//! Episode identity shared by every command that pairs files of one episode
//! (`encode`, `mux --batch-dir`). One parser, one comparison key.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Episode,
    Ova,
    Special,
    Movie,
}

/// What a file name says about the episode it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EpisodeId {
    pub kind: Kind,
    /// Only set when the name carries `S01E05`-style numbering.
    pub season: Option<u16>,
    pub number: u16,
    /// `v2` release revisions; ignored for matching.
    pub version: Option<u8>,
}

static BRACKETS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]]*\]|\([^)]*\)").unwrap());
static SEASON_EP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bS(\d{1,2})\s*E0*(\d{1,3})(?:v(\d))?\b").unwrap());
static OVA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bOVA\s*0*(\d{1,2})\b").unwrap());
static SPECIAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:SP|Special)\s*0*(\d{1,2})\b").unwrap());
static MOVIE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:movie|film|gekijou?-?ban)\b(?:\s*0*(\d{1,2})\b)?").unwrap()
});
/// Words that make a name an OVA or a film, wherever they stand (brackets too).
static OVA_OR_FILM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:OVA|movie|film|gekijou?-?ban)\b").unwrap());
/// `(OVA)` or `[OVA]`: a release that says what it is without numbering it.
static OVA_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)[(\[]\s*OVA\s*[)\]]").unwrap());
static OVA_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bOVA\b").unwrap());
/// A season tag with no episode part: `Show S2 BD - 05`.
static SEASON_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bS0*(\d{1,2})\b").unwrap());
/// The episode number after a ` - ` or a run of spaces (what ` - ` becomes in
/// `Show_S2_BD___01`).
static AFTER_SEASON_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\s-\s*|\s{2,})0*(\d{1,3})(?:v(\d))?\b").unwrap());
static DASH_EP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s-\s*0*(\d{1,3})(?:v(\d))?\b").unwrap());
static EP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bEP?\s*0*(\d{1,3})(?:v(\d))?\b").unwrap());
static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b0*(\d{1,3})\b").unwrap());
static RANGE: LazyLock<Regex> =
    // `10-13`, `OVA 1-3`, `OVA1-OVA2`, `S2E1-3`: a repeated prefix after the dash is ignored.
    LazyLock::new(|| Regex::new(r"^(.*?)0*(\d+)\s*-\s*\D*0*(\d+)$").unwrap());

fn num<T: std::str::FromStr>(c: &regex::Captures<'_>, i: usize) -> Option<T> {
    c.get(i).and_then(|m| m.as_str().parse().ok())
}

impl EpisodeId {
    /// Parse a file name (any extension chain, e.g. `Show - 09.S0.id.ass`).
    pub fn from_file_name(name: &str) -> Option<Self> {
        let stem = Path::new(name).file_stem()?.to_str()?;
        Self::parse(stem)
    }

    /// Parse free text: a file stem or a user token such as `OVA 01`, `S2E05`, `9`.
    ///
    /// Priority: `S01E05` (`S00E01` is an OVA) > `OVA 1` > `SP 1` / `Special 1` >
    /// `Movie` / `Film` / `Gekijouban` > `S2 ... - 05` (season 2 and up) >
    /// ` - 09` > `EP09` / `E09` > the last standalone 1–3 digit number (so
    /// `480p`, `x265` and years never count). Bracketed groups and underscores
    /// are ignored.
    pub fn parse(text: &str) -> Option<Self> {
        let cleaned = BRACKETS.replace_all(text, " ").replace('_', " ");
        if let Some(c) = SEASON_EP.captures(&cleaned) {
            let number = num(&c, 2)?;
            return Some(match num::<u16>(&c, 1) {
                Some(0) => Self::plain(Kind::Ova, number),
                season => Self {
                    kind: Kind::Episode,
                    season,
                    number,
                    version: num(&c, 3),
                },
            });
        }
        if let Some(c) = OVA.captures(&cleaned) {
            return Some(Self::plain(Kind::Ova, num(&c, 1)?));
        }
        if let Some(c) = SPECIAL.captures(&cleaned) {
            return Some(Self::plain(Kind::Special, num(&c, 1)?));
        }
        if let Some(c) = MOVIE.captures(&cleaned) {
            return Some(Self::plain(Kind::Movie, num(&c, 1).unwrap_or(1)));
        }
        // `Show S2 BD - 05`: a season tag with the number after it. Season 1 (or
        // no tag at all) is just the plain numbering below.
        if !OVA_OR_FILM.is_match(text)
            && let Some(season) = SEASON_TAG
                .captures(&cleaned)
                .and_then(|c| num::<u16>(&c, 1))
            && season >= 2
            && let Some(c) = AFTER_SEASON_TAG.captures(&cleaned)
        {
            return Some(Self {
                kind: Kind::Episode,
                season: Some(season),
                number: num(&c, 1)?,
                version: num(&c, 2),
            });
        }
        for re in [&*DASH_EP, &*EP] {
            if let Some(c) = re.captures(&cleaned) {
                return Some(Self {
                    kind: Kind::Episode,
                    season: None,
                    number: num(&c, 1)?,
                    version: num(&c, 2),
                });
            }
        }
        let last = NUMBER.captures_iter(&cleaned).last()?;
        Some(Self::plain(Kind::Episode, num(&last, 1)?))
    }

    fn plain(kind: Kind, number: u16) -> Self {
        Self {
            kind,
            season: None,
            number,
            version: None,
        }
    }

    /// Comparison key: `05`, `105`, `S2E05`, `OVA1`, `SP1`, `MOVIE1`. The
    /// release version is deliberately left out so `v2` files still pair.
    pub fn key(&self) -> String {
        match (self.kind, self.season) {
            (Kind::Episode, Some(s)) => format!("S{s}E{:02}", self.number),
            (Kind::Episode, None) => format!("{:02}", self.number),
            (Kind::Ova, _) => format!("OVA{}", self.number),
            (Kind::Special, _) => format!("SP{}", self.number),
            (Kind::Movie, _) => format!("MOVIE{}", self.number),
        }
    }
}

/// Human form used in messages: `09`, `S02E05`, `OVA 01`, `SP 1`, `Movie`.
impl fmt::Display for EpisodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.kind, self.season) {
            (Kind::Episode, Some(s)) => write!(f, "S{s:02}E{:02}", self.number),
            (Kind::Episode, None) => write!(f, "{:02}", self.number),
            (Kind::Ova, _) => write!(f, "OVA {:02}", self.number),
            (Kind::Special, _) => write!(f, "SP {}", self.number),
            (Kind::Movie, _) if self.number == 1 => f.write_str("Movie"),
            (Kind::Movie, _) => write!(f, "Movie {}", self.number),
        }
    }
}

impl EpisodeId {
    /// What follows the series name in an output file name: `S2 - 05` for a
    /// season-tagged episode (numbered within its season), else `- 09`,
    /// `- OVA 01`, `- Movie 2`.
    pub fn file_label(&self) -> String {
        match (self.kind, self.season) {
            (Kind::Episode, Some(s)) => format!("S{s} - {:02}", self.number),
            _ => format!("- {self}"),
        }
    }
}

/// A name that calls itself an OVA, in brackets (`[OVA]`, `(OVA)`) or without.
pub fn mentions_ova(name: &str) -> bool {
    let cleaned = BRACKETS.replace_all(name, " ").replace('_', " ");
    OVA_TAG.is_match(name) || OVA_WORD.is_match(&cleaned)
}

/// Season-aware episode numbers for the videos of one folder.
///
/// A folder with several seasons (`S01E01` ... `S02E01` ...) usually comes with
/// subtitles numbered straight through, so season N continues after the last
/// episode of the seasons before it (season 2 episode 1 becomes 13 after a
/// 12-episode season). A folder with one season keeps its numbers as they are.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Numbering {
    /// Season -> episodes in the seasons before it.
    offset: BTreeMap<u16, u32>,
}

impl Numbering {
    /// From the file names of the videos.
    pub fn for_videos<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let ids: Vec<EpisodeId> = names
            .into_iter()
            .filter_map(EpisodeId::from_file_name)
            .collect();
        let mut last: BTreeMap<u16, u32> = BTreeMap::new();
        for id in &ids {
            if let (Kind::Episode, Some(season)) = (id.kind, id.season) {
                let slot = last.entry(season).or_default();
                *slot = (*slot).max(u32::from(id.number));
            }
        }
        // Untagged episodes next to a tagged "S2" are season 1; without them the
        // second season would collide with the first.
        if !last.is_empty() && !last.contains_key(&1) {
            let first = ids
                .iter()
                .filter(|id| id.kind == Kind::Episode && id.season.is_none())
                .map(|id| u32::from(id.number))
                .max();
            if let Some(first) = first {
                last.insert(1, first);
            }
        }
        let mut offset = BTreeMap::new();
        if last.len() > 1 {
            let mut before = 0;
            for (season, count) in last {
                offset.insert(season, before);
                before += count;
            }
        }
        Self { offset }
    }

    /// The id matching and `--only` work with: a season-tagged episode gets its
    /// number counted on from the earlier seasons and drops the season.
    pub fn resolve(&self, id: &EpisodeId) -> EpisodeId {
        match (id.kind, id.season) {
            (Kind::Episode, Some(season)) => {
                let before = self.offset.get(&season).copied().unwrap_or(0);
                EpisodeId {
                    season: None,
                    number: u16::try_from(before + u32::from(id.number)).unwrap_or(u16::MAX),
                    ..id.clone()
                }
            }
            _ => id.clone(),
        }
    }
}

/// True when both names parse to the same episode.
pub fn same_episode(a: &Path, b: &Path) -> bool {
    let id = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(EpisodeId::from_file_name)
            .map(|e| e.key())
    };
    matches!((id(a), id(b)), (Some(x), Some(y)) if x == y)
}

/// Expand a `--only`-style selection into comparison keys:
/// `"10,12-14,OVA 1-3,S2E05"` → `["10","12","13","14","OVA1","OVA2","OVA3","S2E05"]`.
pub fn expand_selection(spec: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some(c) = RANGE.captures(part) {
            let prefix = &c[1];
            let from: u32 = c[2]
                .parse()
                .with_context(|| format!("bad range '{part}'"))?;
            let to: u32 = c[3]
                .parse()
                .with_context(|| format!("bad range '{part}'"))?;
            if from > to {
                anyhow::bail!("Range '{part}' is reversed.");
            }
            for n in from..=to {
                let token = format!("{prefix}{n}");
                let id = EpisodeId::parse(&token)
                    .with_context(|| format!("cannot read episode '{token}' in '{part}'"))?;
                out.push(id.key());
            }
        } else {
            let id =
                EpisodeId::parse(part).with_context(|| format!("cannot read episode '{part}'"))?;
            out.push(id.key());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> String {
        EpisodeId::from_file_name(name)
            .map(|e| e.to_string())
            .unwrap_or_else(|| "-".to_string())
    }

    #[test]
    fn parses_real_release_names() {
        // (file name, display form)
        let cases = [
            (
                "[Cleo]Akatsuki_no_Yona_-_09_(Dual Audio_10bit_BD1080p_x265).mkv",
                "09",
            ),
            (
                "_D_Animesub__Akatsuki_no_Yona_BD___06_480p_MegumiNime.S0.id.ass",
                "06",
            ),
            (
                "[AkihitoSubs] Akatsuki no Yona - OVA 1 [576p][HEVC][x265][Opus][10Bit].mkv",
                "OVA 01",
            ),
            (
                "_Kusonime__Akatsuki_no_Yona_OVA02_Mkv480p.S0._Lovelie__Akatsuki_no_Yona_OVA_02.id.ass",
                "OVA 02",
            ),
            ("[ASUT] Akatsuki no Yona - 10 (BD 480p AV1.Opus).mkv", "10"),
            ("[SubsPlease] Frieren - 28 (1080p) [ABCD1234].mkv", "28"),
            (
                "[Erai-raws] Sousou no Frieren - 01v2 [1080p][Multiple Subtitle].mkv",
                "01",
            ),
            ("Show.S02E05.1080p.WEB-DL.mkv", "S02E05"),
            ("Show S2E5.mkv", "S02E05"),
            ("Show - S01E12 - Title.mkv", "S01E12"),
            ("Show EP7 720p.mkv", "07"),
            ("Show E07 [BD].mkv", "07"),
            ("Show 12 1080p x265 2017.mkv", "12"),
            ("Show - 105 [1080p].mkv", "105"),
            ("Show - SP 1 [BD].mkv", "SP 1"),
            ("Show - Special 2.mkv", "SP 2"),
            ("Show - Movie [BD 1080p].mkv", "Movie"),
            ("Show Movie 2 [BD].mkv", "Movie 2"),
            ("Show Gekijouban [BD].mkv", "Movie"),
            ("Show - Film 2 [BD].mkv", "Movie 2"),
            // S00 is the specials season; a season tag counts only from season 2 on.
            ("Show S00E03.mkv", "OVA 03"),
            ("Show S2 BD - 05.mkv", "S02E05"),
            ("_Sub__Show_S2_BD___01.S0.id.ass", "S02E01"),
            ("Show S1 BD - 05.mkv", "05"),
            ("Show S2 OVA - 14.mkv", "14"),
            ("Ep01.mkv", "01"),
            ("Ep01.id.ass", "01"),
            ("ep.en.srt", "-"),
            ("Movie.mkv", "Movie"),
            ("Show - 09.id.ass", "09"),
            ("Show - 09 [480p].en.srt", "09"),
        ];
        for (name, want) in cases {
            assert_eq!(id(name), want, "{name}");
        }
    }

    #[test]
    fn versions_do_not_split_an_episode() {
        let a = EpisodeId::from_file_name("Show - 01v2.mkv").unwrap();
        let b = EpisodeId::from_file_name("Show - 01.id.ass").unwrap();
        assert_eq!(a.version, Some(2));
        assert_eq!(a.key(), b.key());
        assert!(same_episode(
            Path::new("Show - 01v2.mkv"),
            Path::new("Show - 01.id.ass")
        ));
        assert!(!same_episode(
            Path::new("Show - 01.mkv"),
            Path::new("Show - 02.id.ass")
        ));
        assert!(!same_episode(
            Path::new("Show - 01.mkv"),
            Path::new("ep.en.srt")
        ));
        assert!(!same_episode(
            Path::new("Show - 01.mkv"),
            Path::new("Show - OVA 1.mkv")
        ));
        assert!(!same_episode(
            Path::new("Show S1E01.mkv"),
            Path::new("Show S2E01.mkv")
        ));
    }

    #[test]
    fn keys_normalize_user_tokens() {
        for (token, key) in [
            ("9", "09"),
            ("09", "09"),
            ("105", "105"),
            ("OVA 1", "OVA1"),
            ("OVA 01", "OVA1"),
            (" ova1 ", "OVA1"),
            ("sp1", "SP1"),
            ("Movie", "MOVIE1"),
            ("S2E05", "S2E05"),
            ("s02e5", "S2E05"),
        ] {
            assert_eq!(EpisodeId::parse(token).unwrap().key(), key, "{token}");
        }
        assert!(EpisodeId::parse("no number").is_none());
    }

    #[test]
    fn file_labels_follow_the_season_tag() {
        let label = |name: &str| EpisodeId::from_file_name(name).unwrap().file_label();
        assert_eq!(label("Show S2 BD - 05.mkv"), "S2 - 05");
        assert_eq!(label("Show S01E12.mkv"), "S1 - 12");
        assert_eq!(label("Show - 13.mkv"), "- 13");
        assert_eq!(label("Show - OVA 1.mkv"), "- OVA 01");
        assert_eq!(label("Show Movie 2.mkv"), "- Movie 2");
    }

    #[test]
    fn spots_releases_that_call_themselves_ova() {
        assert!(mentions_ova("[Grp] Show S1 - 14 (OVA).mkv"));
        assert!(mentions_ova("Show S2 OVA - 14.mkv"));
        assert!(mentions_ova("Show [OVA] 03.mkv"));
        assert!(!mentions_ova("Show - 14.mkv"));
        assert!(!mentions_ova("Show Nova 14.mkv"));
    }

    fn numbered(numbering: &Numbering, name: &str) -> String {
        let id = EpisodeId::from_file_name(name).unwrap();
        numbering.resolve(&id).to_string()
    }

    #[test]
    fn later_seasons_continue_after_the_earlier_ones() {
        let mut names: Vec<String> = (1..=12).map(|n| format!("Show S01E{n:02}.mkv")).collect();
        names.extend((1..=3).map(|n| format!("Show S02E{n:02}.mkv")));
        let numbering = Numbering::for_videos(names.iter().map(String::as_str));
        assert_eq!(numbered(&numbering, "Show S01E05.mkv"), "05");
        assert_eq!(numbered(&numbering, "Show S02E01.mkv"), "13");
        // Subtitles named the same way pair with the same number.
        assert_eq!(
            numbered(&numbering, "_Sub__Show_S2_BD___03.S0.id.ass"),
            "15"
        );
        // Absolute numbers and OVAs are left alone.
        assert_eq!(numbered(&numbering, "Show - 14.id.ass"), "14");
        assert_eq!(numbered(&numbering, "Show - OVA 1.mkv"), "OVA 01");
    }

    #[test]
    fn untagged_episodes_count_as_season_one() {
        let mut names: Vec<String> = (1..=12).map(|n| format!("Show - {n:02}.mkv")).collect();
        names.extend((1..=2).map(|n| format!("Show S2 BD - {n:02}.mkv")));
        let numbering = Numbering::for_videos(names.iter().map(String::as_str));
        assert_eq!(numbered(&numbering, "Show - 07.mkv"), "07");
        assert_eq!(numbered(&numbering, "Show S2 BD - 01.mkv"), "13");
    }

    #[test]
    fn a_single_season_keeps_its_numbers() {
        let numbering = Numbering::for_videos(["Show S02E01.mkv", "Show S02E02.mkv"]);
        assert_eq!(numbered(&numbering, "Show S02E02.mkv"), "02");
        assert_eq!(numbered(&numbering, "Show - 02.id.ass"), "02");
        assert_eq!(Numbering::for_videos([]), Numbering::default());
    }

    #[test]
    fn expands_selections() {
        assert_eq!(expand_selection("10-13").unwrap(), ["10", "11", "12", "13"]);
        assert_eq!(
            expand_selection("08-09,22,OVA 1-2").unwrap(),
            ["08", "09", "22", "OVA1", "OVA2"]
        );
        assert_eq!(expand_selection("OVA1-OVA2").unwrap(), ["OVA1", "OVA2"]);
        assert_eq!(
            expand_selection("S2E1-3").unwrap(),
            ["S2E01", "S2E02", "S2E03"]
        );
        assert_eq!(expand_selection("5").unwrap(), ["05"]);
        assert_eq!(expand_selection("OVA 01").unwrap(), ["OVA1"]);
        assert!(expand_selection("13-10").is_err());
        assert!(expand_selection("abc").is_err());
    }
}
