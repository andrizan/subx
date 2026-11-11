use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use regex::Regex;
use serde_json::Value;
use walkdir::WalkDir;

use crate::common::config::AppConfig;
use crate::common::episode::{self, EpisodeId, Kind, Numbering};
use crate::common::tools::{self, Tool};
use crate::media::mkv;

mod native;

/// `subx encode <SOURCE> <DEST>` — encode every video in a folder through a StaxRip template.
///
/// Each video first gets a work copy without embedded subtitles/fonts and with only
/// the chosen audio language. StaxRip is then driven through its command line:
/// load the template, load the work copy, lock the target size, add the external
/// subtitles whose episode number matches, encode, and exit. An episode with no
/// external subtitle uses the text subtitle in the video's language, when it has
/// one; with none at all you are asked before it is encoded. Finished files are
/// verified with mkvmerge. Episodes whose output already exists are skipped.
///
/// With --replace-subs nothing is encoded: the subtitles of the finished files are
/// swapped for the external ones.
#[derive(Debug, Args)]
pub struct EncodeArgs {
    /// Folder with the source videos and external subtitles, or one video (its
    /// subtitles are looked up in its folder).
    pub source: PathBuf,

    /// Output folder.
    pub dest: PathBuf,

    /// StaxRip template (case and punctuation are ignored: "Anime480p.AV1.Opus96").
    /// Default: `[encode] template` in subx.toml.
    #[arg(long)]
    pub template: Option<String>,

    /// Output name prefix such as "[GROUP]"; "" for none. Default: `[encode] prefix`.
    #[arg(long)]
    pub prefix: Option<String>,

    /// Series name in the output file name (default: the name of the DEST
    /// folder, not of the source).
    #[arg(long)]
    pub series: Option<String>,

    /// Tag in parentheses at the end of the output file name; "" for none.
    /// Default: `[encode] tag`.
    #[arg(long)]
    pub tag: Option<String>,

    /// Target size WIDTHxHEIGHT, locked in StaxRip because templates derive the
    /// height from the aspect ratio (848 -> 476). "" keeps the template's size.
    /// Default: `[encode] resolution`.
    #[arg(long)]
    pub resolution: Option<String>,

    /// Only these episodes: comma-separated, ranges allowed ("10-13", "5,8-10,OVA 1-3").
    #[arg(long)]
    pub only: Option<String>,

    /// Subtitle language when the file name has no `.<lang>.` part.
    /// Default: `[encode] sub_lang` (id).
    #[arg(long)]
    pub sub_lang: Option<String>,

    /// Language (ISO 639-2) of the text subtitle inside the video that stands in
    /// when the folder has no external subtitle for an episode; `--sub-lang` is
    /// recognised too. Default: `[encode] embedded_sub_lang` (ind).
    #[arg(long)]
    pub embedded_sub_lang: Option<String>,

    /// The only audio language kept (ISO 639-2). Default: `[encode] audio_lang` (jpn).
    #[arg(long)]
    pub audio_lang: Option<String>,

    /// StaxRip folder (contains StaxRip.exe, Settings, Apps). Default: `[tools] staxrip_dir`.
    #[arg(long)]
    pub staxrip_dir: Option<PathBuf>,

    /// Folder for the per-episode work copies. Default: `[encode] work_dir`,
    /// else <temp>/subx-encode.
    #[arg(long)]
    pub work_dir: Option<PathBuf>,

    /// `native` (ffmpeg + encoder + mkvmerge) or `staxrip` (drive StaxRip).
    /// Default: `[encode] engine` (native).
    #[arg(long)]
    pub engine: Option<String>,

    /// Native engine: `[encode.profiles.*]` entry to use. Default: `[encode] profile`.
    #[arg(long)]
    pub profile: Option<String>,

    /// Native engine: episodes encoded concurrently. Default: `[encode] jobs` (1).
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    /// Native engine: kill an episode whose encoder output stops growing for
    /// this many minutes. Default: `[encode] stall_min` (5).
    #[arg(long)]
    pub stall_min: Option<u64>,

    /// Stop StaxRip when one episode takes longer than this many minutes.
    #[arg(long, default_value_t = 120)]
    pub timeout_min: u64,

    /// Print the plan and the StaxRip command lines without running anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Run even if StaxRip is already running (Jobs.dat/Settings.dat are shared).
    #[arg(long)]
    pub allow_running: bool,

    /// Encode episodes that have no subtitle at all without asking first (for
    /// runs nobody answers).
    #[arg(long)]
    pub allow_no_subs: bool,

    /// Do not encode: replace the subtitle tracks of the results that already
    /// exist with the external subtitles of the source folder (mkvmerge remux,
    /// video and audio untouched). A result is overwritten only after the new file
    /// checks out; a result that does not exist yet, or an episode with no
    /// external subtitle, is skipped.
    #[arg(long)]
    pub replace_subs: bool,
}

const VIDEO_EXTS: &[&str] = &["mkv", "mp4"];
const SUB_EXTS: &[&str] = &["ass", "ssa", "srt"];

static RESOLUTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+x\d+$").unwrap());
static SUB_LANG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.([a-z]{2,3})\.(?:ass|ssa|srt)$").unwrap());

/// A text subtitle track inside a video, to be extracted when the episode has
/// no external subtitle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EmbeddedSub {
    /// mkvmerge track id.
    pub(crate) track: u64,
    /// `ass`, `ssa` or `srt`.
    pub(crate) ext: &'static str,
}

impl EmbeddedSub {
    /// Where the track is extracted to.
    fn file_in(&self, work: &Path) -> PathBuf {
        work.join(format!("sub.{}", self.ext))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Episode {
    /// The number the episode is matched, selected (`--only`) and logged by: a
    /// season-tagged episode is counted on from the seasons before it.
    pub(crate) id: EpisodeId,
    pub(crate) video: PathBuf,
    /// External subtitles of the episode.
    pub(crate) subs: Vec<PathBuf>,
    /// Stands in when `subs` is empty.
    pub(crate) embedded: Option<EmbeddedSub>,
    pub(crate) target: PathBuf,
}

impl Episode {
    /// Subtitle tracks the finished file carries.
    pub(crate) fn sub_count(&self) -> usize {
        if self.subs.is_empty() {
            usize::from(self.embedded.is_some())
        } else {
            self.subs.len()
        }
    }

    /// The episode as it is encoded: the embedded subtitle, when it stands in,
    /// is a file in `work` (see [`extract_embedded`]).
    pub(crate) fn with_subs_in(&self, work: &Path) -> Episode {
        let mut run = self.clone();
        if let (true, Some(sub)) = (self.subs.is_empty(), &self.embedded) {
            run.subs = vec![sub.file_in(work)];
        }
        run
    }
}

/// Settings shared by every episode of one run.
struct Ctx {
    exe: PathBuf,
    mkvmerge: PathBuf,
    /// Only needed (and looked up) when an episode uses an embedded subtitle.
    mkvextract: Option<PathBuf>,
    template: String,
    resolution: String,
    audio_lang: String,
    sub_lang: String,
    timeout: Duration,
}

fn has_ext(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

pub(crate) fn file_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|n| n.to_str())
}

/// `[Group] Series - 09 (Tag).mkv`; `label` is [`EpisodeId::file_label`].
fn target_name(prefix: &str, series: &str, label: &str, tag: &str) -> String {
    let title = [prefix, series]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if tag.is_empty() {
        format!("{title} {label}.mkv")
    } else {
        format!("{title} {label} ({tag}).mkv")
    }
}

/// Pair every video in `source` (a folder, or one video whose folder holds the
/// subtitles) with the external subtitles of the same episode.
fn build_plan(
    source: &Path,
    dest: &Path,
    prefix: &str,
    series: &str,
    tag: &str,
    only: &[String],
) -> Result<Vec<Episode>> {
    let (dir, videos) = if source.is_file() {
        let parent = source.parent().filter(|p| !p.as_os_str().is_empty());
        (
            parent.unwrap_or(Path::new(".")).to_path_buf(),
            vec![source.to_path_buf()],
        )
    } else {
        (source.to_path_buf(), Vec::new())
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let videos: Vec<PathBuf> = if source.is_file() {
        videos
    } else {
        files
            .iter()
            .filter(|p| has_ext(p, VIDEO_EXTS))
            .cloned()
            .collect()
    };

    // Seasons number on from one another, so a subtitle named "S2 ... 01" pairs
    // with the video that is season 2 episode 1, not with season 1's.
    let numbering = Numbering::for_videos(videos.iter().filter_map(|v| file_name(v)));
    let mut subs_by_key: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for sub in files.iter().filter(|p| has_ext(p, SUB_EXTS)) {
        if std::fs::metadata(sub).map_or(0, |m| m.len()) == 0 {
            continue;
        }
        if let Some(id) = file_name(sub).and_then(EpisodeId::from_file_name) {
            subs_by_key
                .entry(numbering.resolve(&id).key())
                .or_default()
                .push(sub.clone());
        }
    }

    let mut plan: Vec<Episode> = Vec::new();
    let mut ovas = 0;
    for video in &videos {
        let name = file_name(video).unwrap_or_default();
        let parsed = EpisodeId::from_file_name(name);
        // A release that calls itself an OVA without numbering it ("S1 - 14
        // (OVA)": 14 is the TV numbering) is numbered in name order, as the
        // subtitles of OVAs are.
        let own =
            if episode::mentions_ova(name) && parsed.as_ref().is_none_or(|p| p.kind != Kind::Ova) {
                ovas += 1;
                Some(EpisodeId {
                    kind: Kind::Ova,
                    season: None,
                    number: ovas,
                    version: None,
                })
            } else {
                parsed
            };
        let Some(own) = own else {
            tracing::warn!("Episode number not detected, skipped: {name}");
            continue;
        };
        let id = numbering.resolve(&own);
        let key = id.key();
        if !only.is_empty() && !only.contains(&key) && !only.contains(&own.key()) {
            continue;
        }
        if let Some(prev) = plan.iter().find(|e| e.id.key() == key) {
            anyhow::bail!(
                "Two videos share episode {id}: {} and {name}",
                file_name(&prev.video).unwrap_or_default()
            );
        }
        plan.push(Episode {
            subs: subs_by_key.get(&key).cloned().unwrap_or_default(),
            embedded: None,
            target: dest.join(target_name(prefix, series, &own.file_label(), tag)),
            video: video.clone(),
            id,
        });
    }
    if plan.is_empty() {
        anyhow::bail!("No matching videos to encode in {}", source.display());
    }
    Ok(plan)
}

/// The text subtitle of a video to use when the folder has none: in one of
/// `langs`, a text track (a picture-based PGS/VobSub cannot be a subtitle
/// file), the one that is not forced when there is a choice.
pub(crate) fn pick_embedded_sub(info: &Value, langs: &[String]) -> Option<EmbeddedSub> {
    let tracks = info["tracks"].as_array().map_or(&[][..], Vec::as_slice);
    let in_lang = |t: &&Value| langs.iter().any(|l| track_matches_lang(t, l));
    let subs: Vec<&Value> = tracks
        .iter()
        .filter(|t| t["type"] == "subtitles")
        .filter(in_lang)
        .collect();
    let ext_of = |t: &Value| match t["properties"]["codec_id"].as_str() {
        Some("S_TEXT/ASS") => Some("ass"),
        Some("S_TEXT/SSA") => Some("ssa"),
        Some("S_TEXT/UTF8") => Some("srt"),
        _ => None,
    };
    let text: Vec<&Value> = subs
        .iter()
        .copied()
        .filter(|t| ext_of(t).is_some())
        .collect();
    if !subs.is_empty() && text.is_empty() {
        tracing::warn!("The embedded subtitle is not text (PGS/VobSub) and was skipped.");
    }
    let pick = text
        .iter()
        .find(|t| t["properties"]["forced_track"] != true)
        .or(text.first())?;
    Some(EmbeddedSub {
        track: pick["id"].as_u64()?,
        ext: ext_of(pick)?,
    })
}

/// Look in the video of every episode without an external subtitle for one it
/// can use instead. A video mkvmerge cannot read just has none.
async fn attach_embedded(plan: &mut [Episode], mkvmerge: &Path, langs: &[String]) {
    for ep in plan
        .iter_mut()
        .filter(|e| e.subs.is_empty() && !e.target.exists())
    {
        match mkv_info(mkvmerge, &ep.video).await {
            Ok(info) => ep.embedded = pick_embedded_sub(&info, langs),
            Err(e) => tracing::warn!(
                "Embedded subtitles not readable for {}: {e:#}",
                file_name(&ep.video).unwrap_or_default()
            ),
        }
    }
}

/// Extract the embedded subtitle that stands in for the external ones into
/// `work` (where [`Episode::with_subs_in`] expects it).
pub(crate) async fn extract_embedded(
    mkvextract: Option<&Path>,
    ep: &Episode,
    work: &Path,
) -> Result<()> {
    let (true, Some(sub)) = (ep.subs.is_empty(), &ep.embedded) else {
        return Ok(());
    };
    let mkvextract = mkvextract.context("mkvextract is needed for an embedded subtitle")?;
    let out = sub.file_in(work);
    let status = tokio::process::Command::new(mkvextract)
        .arg("tracks")
        .arg(&ep.video)
        .arg(format!("{}:{}", sub.track, out.display()))
        .stdout(Stdio::null())
        .status()
        .await
        .with_context(|| "cannot start mkvextract")?;
    let written = std::fs::metadata(&out).map_or(0, |m| m.len());
    if status.code().unwrap_or(2) >= 2 || written == 0 {
        anyhow::bail!(
            "mkvextract could not extract subtitle track {} of {}",
            sub.track,
            ep.video.display()
        );
    }
    Ok(())
}

/// Find the template by name, ignoring case and punctuation. Returns the name
/// StaxRip's `-LoadTemplate` expects (path under Settings/Templates, no extension).
fn resolve_template(staxrip_dir: &Path, name: &str) -> Result<String> {
    let root = staxrip_dir.join("Settings").join("Templates");
    if !root.is_dir() {
        anyhow::bail!("Template folder not found: {}", root.display());
    }
    let norm = |s: &str| {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_lowercase()
    };

    let mut all: Vec<String> = WalkDir::new(&root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && has_ext(e.path(), &["srip"]))
        .map(|e| {
            let rel = e.path().strip_prefix(&root).unwrap_or(e.path());
            rel.with_extension("").to_string_lossy().into_owned()
        })
        .collect();
    all.sort();

    let wanted = norm(name);
    let hits: Vec<&String> = all.iter().filter(|t| norm(t) == wanted).collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        _ => anyhow::bail!(
            "Template '{name}' not found or ambiguous in {}. Available: {}",
            root.display(),
            all.join(", ")
        ),
    }
}

/// Subtitle language from `<name>.<lang>.<ext>`, else the default.
pub(crate) fn sub_language(path: &Path, default: &str) -> String {
    file_name(path)
        .and_then(|n| SUB_LANG.captures(n))
        .map_or_else(|| default.to_string(), |c| c[1].to_string())
}

/// StaxRip command line (one raw string: StaxRip parses it itself, so quotes must
/// not be re-escaped). Order matters: loading the source resets the target size.
fn staxrip_cmdline(
    template: &str,
    work: &Path,
    resolution: &str,
    subs: &[PathBuf],
    default_lang: &str,
    target: &Path,
) -> String {
    let q = |s: &str| format!("\"{s}\"");
    let mut parts = vec![
        "-NoFocus".to_string(),
        format!("-LoadTemplate:{}", q(template)),
        "-SetHideDialogsOption:true".to_string(),
        format!("-LoadSourceFile:{}", q(&work.display().to_string())),
    ];
    if !resolution.is_empty() {
        parts.push(format!(
            "-SetTargetImageSize:{}",
            resolution.replace('x', ",")
        ));
    }
    for (i, sub) in subs.iter().enumerate() {
        // StaxRip drops empty parameters, so the (empty) name is written as "".
        parts.push(format!(
            "-AddSubtitle:true,{},\"\",{},false,false,false,{}",
            sub_language(sub, default_lang),
            i == 0,
            q(&sub.display().to_string())
        ));
    }
    parts.push(format!(
        "-SetTargetFile:{}",
        q(&target.display().to_string())
    ));
    parts.push("-StartEncoding".to_string());
    parts.push("-ExitWithoutSaving".to_string());
    parts.join(" ")
}

pub(crate) async fn mkv_info(mkvmerge: &Path, file: &Path) -> Result<Value> {
    crate::media::mkv::info(mkvmerge, file).await
}

fn track_matches_lang(track: &Value, lang: &str) -> bool {
    track["properties"]["language"] == lang || track["properties"]["language_ietf"] == lang
}

/// The audio track to keep, and what to expect of it in the finished file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AudioPick {
    /// mkvmerge track id.
    pub(crate) id: u64,
    /// Language to write on the track: it has none (`und`) and StaxRip leaves
    /// out audio without one.
    pub(crate) relabel: Option<String>,
    /// Language the finished file must report; `None` = the only audio track was
    /// kept whatever its language, so there is nothing to compare with.
    pub(crate) expected: Option<String>,
}

/// The first audio track in `lang`. A video with a single audio track that is
/// not in `lang` keeps that track anyway (with a warning), and relabels it when
/// its language is unset.
pub(crate) fn pick_audio(info: &Value, lang: &str) -> Result<AudioPick> {
    let tracks = info["tracks"].as_array().map_or(&[][..], Vec::as_slice);
    let all: Vec<&Value> = tracks.iter().filter(|t| t["type"] == "audio").collect();
    let hits: Vec<&&Value> = all.iter().filter(|t| track_matches_lang(t, lang)).collect();
    if hits.len() > 1 {
        tracing::warn!(
            "{} '{lang}' audio tracks found, using the first",
            hits.len()
        );
    }
    if let Some(first) = hits.first() {
        return Ok(AudioPick {
            id: first["id"].as_u64().context("audio track without an id")?,
            relabel: None,
            expected: Some(lang.to_string()),
        });
    }
    let [only] = all.as_slice() else {
        anyhow::bail!("no '{lang}' audio track");
    };
    let found = only["properties"]["language"].as_str().unwrap_or("und");
    tracing::warn!("No '{lang}' audio track, keeping the only audio track (language: {found}).");
    let unset = found == "und";
    Ok(AudioPick {
        id: only["id"].as_u64().context("audio track without an id")?,
        relabel: unset.then(|| lang.to_string()),
        expected: unset.then(|| lang.to_string()),
    })
}

/// Copy without subtitles and fonts, keeping only the chosen audio language.
/// Video, chapters and timing are untouched. Returns the audio language the
/// finished file must report (`None` = do not check it).
async fn make_clean_copy(
    mkvmerge: &Path,
    input: &Path,
    output: &Path,
    audio_lang: &str,
) -> Result<Option<String>> {
    let info = mkv_info(mkvmerge, input).await?;
    let audio = pick_audio(&info, audio_lang)
        .with_context(|| format!("while reading {}", input.display()))?;
    let mut cmd = tokio::process::Command::new(mkvmerge);
    cmd.arg("-o")
        .arg(output)
        .args(["--no-subtitles", "--no-attachments"]);
    if let Some(lang) = &audio.relabel {
        cmd.arg("--language").arg(format!("{}:{lang}", audio.id));
    }
    let status = cmd
        .arg("--audio-tracks")
        .arg(audio.id.to_string())
        .arg(input)
        .stdout(Stdio::null())
        .status()
        .await
        .with_context(|| "cannot start mkvmerge")?;
    if status.code().unwrap_or(2) >= 2 {
        anyhow::bail!(
            "mkvmerge failed to make the work copy of {}",
            input.display()
        );
    }
    Ok(audio.expected)
}

/// What is wrong with a finished file (empty when it matches expectations).
/// `audio_lang` `None` skips the language check.
pub(crate) fn check_output(
    info: &Value,
    expected_subs: usize,
    audio_lang: Option<&str>,
    resolution: &str,
) -> Vec<String> {
    let tracks = info["tracks"].as_array().map_or(&[][..], Vec::as_slice);
    let of = |kind: &str| -> Vec<&Value> { tracks.iter().filter(|t| t["type"] == kind).collect() };
    let (video, audio, subs) = (of("video"), of("audio"), of("subtitles"));
    let fonts = info["attachments"].as_array().map_or(0, Vec::len);

    let mut problems = Vec::new();
    if video.len() != 1 {
        problems.push(format!("video={}", video.len()));
    } else if !resolution.is_empty() && video[0]["properties"]["pixel_dimensions"] != resolution {
        let got = video[0]["properties"]["pixel_dimensions"]
            .as_str()
            .unwrap_or("?");
        problems.push(format!("resolution={got} (expected {resolution})"));
    }
    if audio.len() != 1 {
        problems.push(format!("audio={}", audio.len()));
    } else if let Some(lang) = audio_lang
        && !track_matches_lang(audio[0], lang)
    {
        let got = audio[0]["properties"]["language"].as_str().unwrap_or("?");
        problems.push(format!("audio language={got} (expected {lang})"));
    }
    if subs.len() != expected_subs {
        problems.push(format!(
            "subtitles={} (expected {expected_subs})",
            subs.len()
        ));
    }
    if fonts != 0 {
        problems.push(format!("fonts={fonts}"));
    }
    problems
}

/// Number of running StaxRip processes (Windows only; elsewhere always 0).
async fn staxrip_running() -> usize {
    #[cfg(windows)]
    {
        let out = tokio::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq StaxRip.exe", "/NH"])
            .output()
            .await;
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.to_ascii_lowercase().starts_with("staxrip.exe"))
                .count(),
            Err(_) => 0,
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Run StaxRip with a raw command line and wait for it to exit. The exit code is
/// ignored on purpose: the verified output file decides success.
#[cfg(windows)]
async fn run_staxrip(exe: &Path, cmdline: &str, timeout: Duration) -> Result<()> {
    let mut child = tokio::process::Command::new(exe)
        .current_dir(exe.parent().unwrap_or(Path::new(".")))
        .raw_arg(cmdline)
        .spawn()
        .with_context(|| format!("cannot start {}", exe.display()))?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => {
            status.with_context(|| "waiting for StaxRip failed")?;
            Ok(())
        }
        Err(_) => {
            if let Some(pid) = child.id() {
                let _ = tokio::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .status()
                    .await;
            }
            anyhow::bail!(
                "StaxRip exceeded {} minutes and was stopped",
                timeout.as_secs() / 60
            )
        }
    }
}

#[cfg(not(windows))]
async fn run_staxrip(_exe: &Path, _cmdline: &str, _timeout: Duration) -> Result<()> {
    anyhow::bail!("StaxRip runs on Windows only")
}

async fn encode_one(ctx: &Ctx, ep: &Episode, work_dir: &Path, cmdline: &str) -> Result<()> {
    if work_dir.exists() {
        std::fs::remove_dir_all(work_dir)
            .with_context(|| format!("cannot clear {}", work_dir.display()))?;
    }
    std::fs::create_dir_all(work_dir)
        .with_context(|| format!("cannot create {}", work_dir.display()))?;
    if let Some(parent) = ep.target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    let expected_audio = make_clean_copy(
        &ctx.mkvmerge,
        &ep.video,
        &work_dir.join("source.mkv"),
        &ctx.audio_lang,
    )
    .await?;
    extract_embedded(ctx.mkvextract.as_deref(), ep, work_dir).await?;
    run_staxrip(&ctx.exe, cmdline, ctx.timeout).await?;

    if !ep.target.is_file() {
        anyhow::bail!("Output file was not created (see the log in StaxRip's Settings\\Log Files)");
    }
    let info = mkv_info(&ctx.mkvmerge, &ep.target).await?;
    let problems = check_output(
        &info,
        ep.sub_count(),
        expected_audio.as_deref(),
        &ctx.resolution,
    );
    if !problems.is_empty() {
        anyhow::bail!("Output does not match: {}", problems.join("; "));
    }
    // Only a verified episode drops its work copy; failures keep it for inspection.
    std::fs::remove_dir_all(work_dir).ok();
    Ok(())
}

fn print_plan(plan: &[Episode], replace_subs: bool) {
    for ep in plan {
        let status = if replace_subs {
            if !ep.target.exists() {
                "SKIP (no result yet)"
            } else if ep.subs.is_empty() {
                "SKIP (no external subs)"
            } else {
                "REPLACE SUBS"
            }
        } else if ep.target.exists() {
            "SKIP (output exists)"
        } else {
            "ENCODE"
        };
        let subs = if !ep.subs.is_empty() {
            ep.subs.len().to_string()
        } else if ep.embedded.is_some() {
            "1 (embedded)".to_string()
        } else {
            "0".to_string()
        };
        println!(
            "{:<7} {:<24} subs={} -> {}",
            ep.id.to_string(),
            status,
            subs,
            file_name(&ep.target).unwrap_or_default()
        );
    }
}

fn confirms(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "ya" | "yes"
    )
}

/// Episodes about to be encoded with no subtitle at all (none beside the video,
/// none inside it): say so, and ask before going on unless told not to. An
/// unanswered question (no terminal) is a no.
fn confirm_no_subs(plan: &[Episode], dry_run: bool, allow: bool) -> Result<()> {
    let missing: Vec<String> = plan
        .iter()
        .filter(|e| e.sub_count() == 0 && !e.target.exists())
        .map(|e| e.id.to_string())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    println!(
        "Warning: no subtitle (in the folder, or inside the video) for episode(s): {}",
        missing.join(", ")
    );
    if dry_run || allow {
        return Ok(());
    }
    print!("Encode them without subtitles anyway? [y/N] ");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).ok();
    if !confirms(&answer) {
        anyhow::bail!("Cancelled, nothing was encoded (--allow-no-subs skips the question).");
    }
    Ok(())
}

/// mkvmerge arguments that copy `file` to `tmp` with its subtitle tracks
/// replaced by `subs` (the first one default).
fn swap_args(file: &Path, tmp: &Path, subs: &[PathBuf], default_lang: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-o".into(),
        tmp.into(),
        "--no-subtitles".into(),
        file.into(),
    ];
    for (i, sub) in subs.iter().enumerate() {
        args.push("--language".into());
        args.push(format!("0:{}", sub_language(sub, default_lang)).into());
        args.push("--default-track-flag".into());
        args.push(format!("0:{}", u8::from(i == 0)).into());
        args.push(sub.into());
    }
    args
}

/// Replace the subtitle tracks of a finished file with `subs`, without encoding
/// again: remux to a temporary file beside it, check that video, audio and
/// chapters are as before and the subtitles are the new ones (no fonts), and only
/// then overwrite. Anything wrong leaves the existing file as it was.
async fn swap_subs(
    mkvmerge: &Path,
    file: &Path,
    subs: &[PathBuf],
    default_lang: &str,
) -> Result<()> {
    let before = mkv_info(mkvmerge, file).await?;
    let tracks = before["tracks"].as_array().map_or(&[][..], Vec::as_slice);
    let of = |kind: &str| -> Vec<&Value> { tracks.iter().filter(|t| t["type"] == kind).collect() };
    let (video, audio) = (of("video"), of("audio"));
    let ([video], [audio]) = (video.as_slice(), audio.as_slice()) else {
        anyhow::bail!(
            "unusual file (video={}, audio={}), left alone",
            of("video").len(),
            of("audio").len()
        );
    };
    let resolution = video["properties"]["pixel_dimensions"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let audio_lang = audio["properties"]["language"].as_str().map(str::to_string);
    let chapters = mkv::chapter_entries(&before);

    let mut tmp_name = file.as_os_str().to_owned();
    tmp_name.push(".subswap.tmp");
    let tmp = PathBuf::from(tmp_name);
    let result = async {
        let status = tokio::process::Command::new(mkvmerge)
            .args(swap_args(file, &tmp, subs, default_lang))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .await
            .with_context(|| "cannot start mkvmerge")?;
        if status.code().unwrap_or(2) >= 2 {
            anyhow::bail!("mkvmerge failed to swap the subtitles");
        }
        let after = mkv_info(mkvmerge, &tmp).await?;
        let mut problems = check_output(&after, subs.len(), audio_lang.as_deref(), &resolution);
        let got = mkv::chapter_entries(&after);
        if got != chapters {
            problems.push(format!("chapters={got} (before {chapters})"));
        }
        if !problems.is_empty() {
            anyhow::bail!("the remuxed file does not match: {}", problems.join("; "));
        }
        std::fs::rename(&tmp, file).with_context(|| format!("cannot replace {}", file.display()))
    }
    .await;
    if tmp.exists() {
        std::fs::remove_file(&tmp).ok();
    }
    result
}

/// `--replace-subs`: swap the subtitles of the results that exist.
async fn replace_subs(plan: &[Episode], dry_run: bool, default_lang: &str) -> Result<()> {
    let todo: Vec<&Episode> = plan
        .iter()
        .filter(|e| !e.subs.is_empty() && e.target.exists())
        .collect();
    let names = |ep: &Episode| {
        ep.subs
            .iter()
            .filter_map(|s| file_name(s))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if dry_run {
        for ep in todo {
            println!(
                "[{}] replace the subtitles of {} with: {}",
                ep.id,
                file_name(&ep.target).unwrap_or_default(),
                names(ep)
            );
        }
        return Ok(());
    }
    if todo.is_empty() {
        println!("Nothing to replace.");
        return Ok(());
    }
    let mkvmerge = tools::require_one(Tool::Mkvmerge)?;
    let (mut ok, mut failed) = (0, 0);
    for ep in todo {
        println!("\n[{}] replacing the subtitles with: {}", ep.id, names(ep));
        match swap_subs(&mkvmerge, &ep.target, &ep.subs, default_lang).await {
            Ok(()) => {
                ok += 1;
                println!("[OK] {} -> {}", ep.id, ep.target.display());
            }
            Err(e) => {
                failed += 1;
                println!(
                    "[FAILED] {} -> {e:#} (the existing file is unchanged)",
                    ep.id
                );
            }
        }
    }
    println!("--------------------------------------------------");
    println!("Replaced : {ok}");
    println!("Failed   : {failed}");
    if failed > 0 {
        anyhow::bail!("{failed} episode(s) failed.");
    }
    Ok(())
}

/// Name of the output folder: what the series is called unless `--series` says otherwise.
fn folder_name(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Tag at the end of the output name: the flag, else the native profile's, else
/// `[encode] tag`.
fn output_tag(args: &EncodeArgs, cfg: &AppConfig, engine: &str) -> String {
    args.tag
        .clone()
        .or_else(|| {
            if engine != "native" {
                return None;
            }
            let name = args.profile.as_ref().or(cfg.encode.profile.as_ref())?;
            cfg.encode.profiles.get(name)?.tag.clone()
        })
        .unwrap_or_else(|| cfg.encode.tag.clone())
}

pub async fn run(args: EncodeArgs, cfg: &AppConfig) -> Result<()> {
    // Flags win over `[tools]` / `[encode]` in subx.toml.
    if !args.source.exists() {
        anyhow::bail!("Not found: {}", args.source.display());
    }
    let engine = args
        .engine
        .clone()
        .unwrap_or_else(|| cfg.encode.engine.clone());
    let prefix = args
        .prefix
        .clone()
        .unwrap_or_else(|| cfg.encode.prefix.clone());
    let audio_lang = args
        .audio_lang
        .clone()
        .unwrap_or_else(|| cfg.encode.audio_lang.clone());
    let sub_lang = args
        .sub_lang
        .clone()
        .unwrap_or_else(|| cfg.encode.sub_lang.clone());
    let embedded_langs = vec![
        args.embedded_sub_lang
            .clone()
            .unwrap_or_else(|| cfg.encode.embedded_sub_lang.clone()),
        sub_lang.clone(),
    ];
    let series = args
        .series
        .clone()
        .unwrap_or_else(|| folder_name(&args.dest));
    let only = args
        .only
        .as_deref()
        .map(episode::expand_selection)
        .transpose()?
        .unwrap_or_default();
    let work_root = args
        .work_dir
        .clone()
        .or_else(|| cfg.encode.work_dir.clone())
        .unwrap_or_else(|| std::env::temp_dir().join("subx-encode"));
    let tag = output_tag(&args, cfg, &engine);

    if args.replace_subs {
        let plan = build_plan(&args.source, &args.dest, &prefix, &series, &tag, &only)?;
        print_plan(&plan, true);
        return replace_subs(&plan, args.dry_run, &sub_lang).await;
    }

    match engine.as_str() {
        "native" => {
            let profile_name = args
                .profile
                .clone()
                .or_else(|| cfg.encode.profile.clone())
                .context("profile unknown: pass --profile or set [encode] profile in subx.toml")?;
            let stall_min = args.stall_min.unwrap_or(cfg.encode.stall_min);
            let native = native::Native::setup(
                &cfg.encode,
                &profile_name,
                &audio_lang,
                &sub_lang,
                stall_min,
            )
            .await?;
            let mut plan = build_plan(&args.source, &args.dest, &prefix, &series, &tag, &only)?;
            attach_embedded(&mut plan, &native.mkvmerge, &embedded_langs).await;
            print_plan(&plan, false);
            confirm_no_subs(&plan, args.dry_run, args.allow_no_subs)?;
            if args.dry_run {
                return native::dry_run(&native, &plan, &work_root).await;
            }
            let jobs = args.jobs.unwrap_or(cfg.encode.jobs).max(1);
            let failed = native::run_all(&native, &plan, &work_root, jobs).await?;
            if failed > 0 {
                anyhow::bail!("{failed} episode(s) failed.");
            }
            return Ok(());
        }
        "staxrip" => {}
        other => anyhow::bail!("unknown engine '{other}' (staxrip | native)"),
    }

    // --- StaxRip engine --------------------------------------------------------
    let staxrip_dir = args
        .staxrip_dir
        .clone()
        .or_else(|| cfg.tools.staxrip_dir.clone())
        .context(
            "StaxRip folder unknown: pass --staxrip-dir or set [tools] staxrip_dir in subx.toml",
        )?;
    let template_name = args
        .template
        .clone()
        .or_else(|| cfg.encode.template.clone())
        .context("template unknown: pass --template or set [encode] template in subx.toml")?;
    let resolution = args
        .resolution
        .clone()
        .unwrap_or_else(|| cfg.encode.resolution.clone())
        .trim()
        .to_lowercase();

    let exe = staxrip_dir.join("StaxRip.exe");
    if !exe.exists() {
        anyhow::bail!("Not found: {}", exe.display());
    }
    let mkvmerge = tools::require_one(Tool::Mkvmerge)?;
    if !resolution.is_empty() && !RESOLUTION.is_match(&resolution) {
        anyhow::bail!("--resolution must look like WIDTHxHEIGHT, e.g. 848x480");
    }
    let template = resolve_template(&staxrip_dir, &template_name)?;

    if !args.dry_run && !args.allow_running {
        let running = staxrip_running().await;
        if running > 0 {
            anyhow::bail!(
                "StaxRip is still running ({running} processes). Close it first or pass --allow-running."
            );
        }
    }

    let mut plan = build_plan(&args.source, &args.dest, &prefix, &series, &tag, &only)?;
    attach_embedded(&mut plan, &mkvmerge, &embedded_langs).await;
    print_plan(&plan, false);
    confirm_no_subs(&plan, args.dry_run, args.allow_no_subs)?;

    let mkvextract = if plan.iter().any(|e| e.embedded.is_some()) {
        Some(tools::require_one(Tool::Mkvextract)?)
    } else {
        None
    };
    let ctx = Ctx {
        exe,
        mkvmerge,
        mkvextract,
        template,
        resolution,
        audio_lang,
        sub_lang,
        timeout: Duration::from_secs(args.timeout_min * 60),
    };

    let (mut ok, mut skipped, mut failed) = (0, 0, 0);
    for ep in &plan {
        if ep.target.exists() {
            skipped += 1;
            continue;
        }
        let work_dir = work_root.join(ep.id.key());
        let cmdline = staxrip_cmdline(
            &ctx.template,
            &work_dir.join("source.mkv"),
            &ctx.resolution,
            &ep.with_subs_in(&work_dir).subs,
            &ctx.sub_lang,
            &ep.target,
        );

        if args.dry_run {
            println!(
                "\n[{}] work copy: {}",
                ep.id,
                work_dir.join("source.mkv").display()
            );
            println!("[{}] StaxRip.exe {cmdline}", ep.id);
            continue;
        }

        println!("\n[{}] {}", ep.id, file_name(&ep.video).unwrap_or_default());
        match encode_one(&ctx, ep, &work_dir, &cmdline).await {
            Ok(()) => {
                ok += 1;
                println!("[OK] {} -> {}", ep.id, ep.target.display());
            }
            Err(e) => {
                failed += 1;
                println!("[FAILED] {} -> {e:#}", ep.id);
                println!("Work copy kept in {}", work_dir.display());
            }
        }
    }

    if !args.dry_run {
        println!("--------------------------------------------------");
        println!("Episodes : {}", plan.len());
        println!("Encoded  : {ok}");
        println!("Skipped  : {skipped}");
        println!("Failed   : {failed}");
        if failed > 0 {
            anyhow::bail!("{failed} episode(s) failed.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_target_names() {
        assert_eq!(
            target_name("[ASUT]", "Akatsuki no Yona", "- 09", "BD 480p AV1.Opus"),
            "[ASUT] Akatsuki no Yona - 09 (BD 480p AV1.Opus).mkv"
        );
        assert_eq!(
            target_name("", "Show", "- OVA 01", "T"),
            "Show - OVA 01 (T).mkv"
        );
        assert_eq!(target_name("", "Show", "S2 - 01", ""), "Show S2 - 01.mkv");
    }

    #[test]
    fn reads_subtitle_language() {
        let d = "id";
        assert_eq!(sub_language(Path::new("a.S0.id.ass"), d), "id");
        assert_eq!(sub_language(Path::new("a.EN.srt"), d), "EN");
        assert_eq!(sub_language(Path::new("Show - 01.ass"), d), "id");
    }

    #[test]
    fn builds_staxrip_cmdline() {
        let line = staxrip_cmdline(
            "Anime-480p.AV1.Opus96",
            Path::new("W/10/source.mkv"),
            "848x480",
            &[
                PathBuf::from("S/ep10.id.ass"),
                PathBuf::from("S/ep10.en.srt"),
            ],
            "id",
            Path::new("D/[ASUT] Show - 10 (T).mkv"),
        );
        assert_eq!(
            line,
            "-NoFocus -LoadTemplate:\"Anime-480p.AV1.Opus96\" -SetHideDialogsOption:true \
             -LoadSourceFile:\"W/10/source.mkv\" -SetTargetImageSize:848,480 \
             -AddSubtitle:true,id,\"\",true,false,false,false,\"S/ep10.id.ass\" \
             -AddSubtitle:true,en,\"\",false,false,false,false,\"S/ep10.en.srt\" \
             -SetTargetFile:\"D/[ASUT] Show - 10 (T).mkv\" -StartEncoding -ExitWithoutSaving"
        );

        let bare = staxrip_cmdline("T", Path::new("w"), "", &[], "id", Path::new("t.mkv"));
        assert!(!bare.contains("-SetTargetImageSize") && !bare.contains("-AddSubtitle"));
    }

    fn mkv(video_dims: &str, audio_lang: &str, subs: usize, fonts: usize) -> Value {
        let mut tracks = vec![
            json!({"id": 0, "type": "video", "properties": {"pixel_dimensions": video_dims}}),
            json!({"id": 1, "type": "audio",
                   "properties": {"language": audio_lang, "language_ietf": "ja"}}),
        ];
        for i in 0..subs {
            tracks.push(json!({"id": 2 + i, "type": "subtitles", "properties": {}}));
        }
        json!({"tracks": tracks, "attachments": vec![json!({}); fonts]})
    }

    #[test]
    fn picks_audio_track_by_language() {
        let info = json!({"tracks": [
            {"id": 0, "type": "video", "properties": {}},
            {"id": 1, "type": "audio", "properties": {"language": "eng", "language_ietf": "en"}},
            {"id": 2, "type": "audio", "properties": {"language": "jpn", "language_ietf": "ja"}},
        ]});
        let jpn = |lang: &str| AudioPick {
            id: 2,
            relabel: None,
            expected: Some(lang.to_string()),
        };
        assert_eq!(pick_audio(&info, "jpn").unwrap(), jpn("jpn"));
        assert_eq!(pick_audio(&info, "ja").unwrap(), jpn("ja"));
        assert!(pick_audio(&info, "ind").is_err());
    }

    #[test]
    fn a_lone_audio_track_is_kept_whatever_its_language() {
        let lone = |lang: &str| {
            json!({"tracks": [
                {"id": 0, "type": "video", "properties": {}},
                {"id": 3, "type": "audio", "properties": {"language": lang}},
            ]})
        };
        // Another language: kept as it is, and not compared with the wanted one.
        assert_eq!(
            pick_audio(&lone("eng"), "jpn").unwrap(),
            AudioPick {
                id: 3,
                relabel: None,
                expected: None
            }
        );
        // No language set: StaxRip would leave the track out, so it is labelled.
        assert_eq!(
            pick_audio(&lone("und"), "jpn").unwrap(),
            AudioPick {
                id: 3,
                relabel: Some("jpn".into()),
                expected: Some("jpn".into())
            }
        );
    }

    #[test]
    fn verifies_finished_files() {
        assert!(check_output(&mkv("848x480", "jpn", 1, 0), 1, Some("jpn"), "848x480").is_empty());
        assert!(check_output(&mkv("848x476", "jpn", 1, 0), 1, Some("jpn"), "").is_empty());
        // No language to expect: whatever the track says is fine.
        assert!(check_output(&mkv("848x476", "eng", 1, 0), 1, None, "").is_empty());

        let bad = check_output(&mkv("848x476", "eng", 2, 3), 1, Some("jpn"), "848x480");
        assert_eq!(
            bad,
            [
                "resolution=848x476 (expected 848x480)",
                "audio language=eng (expected jpn)",
                "subtitles=2 (expected 1)",
                "fonts=3",
            ]
        );
    }

    fn touch(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn plans_episodes_with_matching_subs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path();
        touch(&src.join("[Cleo]Show_-_09_(BD1080p).mkv"), "");
        touch(&src.join("[Cleo]Show_-_10_(BD1080p).mkv"), "");
        touch(&src.join("[Grp] Show - OVA 1 [576p].mkv"), "");
        touch(&src.join("_Sub__Show_BD___10_480p.S0.id.ass"), "x");
        touch(&src.join("_Sub__Show_BD___09_480p.S0.id.ass"), "");
        touch(&src.join("_Sub__Show_OVA01_480p.id.ass"), "x");
        touch(&src.join("notes.txt"), "x");

        let dest = Path::new("out");
        let plan = build_plan(src, dest, "[P]", "Show", "T", &[]).unwrap();
        let summary: Vec<String> = plan
            .iter()
            .map(|e| format!("{}:{}", e.id, e.subs.len()))
            .collect();
        // Plan follows file-name order; empty subtitle files are ignored, so 09 has none.
        assert_eq!(summary, ["09:0", "10:1", "OVA 01:1"]);
        assert_eq!(plan[1].target, dest.join("[P] Show - 10 (T).mkv"));

        let only = episode::expand_selection("10").unwrap();
        let picked = build_plan(src, dest, "[P]", "Show", "T", &only).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].id.to_string(), "10");
        assert!(build_plan(src, dest, "[P]", "Show", "T", &["99".to_string()]).is_err());
    }

    #[test]
    fn rejects_duplicate_episode_numbers() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Show - 01 [a].mkv"), "");
        touch(&dir.path().join("Show - 01 [b].mp4"), "");
        let err = build_plan(dir.path(), Path::new("out"), "", "Show", "T", &[]).unwrap_err();
        assert!(err.to_string().contains("share episode 01"), "{err}");
    }

    fn summary(plan: &[Episode]) -> Vec<String> {
        plan.iter()
            .map(|e| format!("{}:{}", e.id, e.subs.len()))
            .collect()
    }

    #[test]
    fn seasons_continue_in_the_plan_and_are_named_per_season() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path();
        touch(&src.join("Show S01E01.mkv"), "");
        touch(&src.join("Show S01E02.mkv"), "");
        touch(&src.join("Show S02E01.mkv"), "");
        // Numbered straight through, or by season: both reach the same episode.
        touch(&src.join("Show - 02.id.ass"), "x");
        touch(&src.join("Show - 03.id.ass"), "x");
        touch(&src.join("_Sub__Show_S2_BD___01.S0.id.ass"), "x");

        let dest = Path::new("out");
        let plan = build_plan(src, dest, "", "Show", "T", &[]).unwrap();
        assert_eq!(summary(&plan), ["01:0", "02:1", "03:2"]);
        assert_eq!(plan[0].target, dest.join("Show S1 - 01 (T).mkv"));
        assert_eq!(plan[2].target, dest.join("Show S2 - 01 (T).mkv"));

        // `--only` takes the straight-through number or the season form.
        for spec in ["3", "S2E1"] {
            let only = episode::expand_selection(spec).unwrap();
            let picked = build_plan(src, dest, "", "Show", "T", &only).unwrap();
            assert_eq!(summary(&picked), ["03:2"], "{spec}");
        }
    }

    #[test]
    fn untagged_releases_that_call_themselves_ova_are_numbered_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path();
        touch(&src.join("Show - 01.mkv"), "");
        touch(&src.join("Show S1 - 14 (OVA).mkv"), "");
        touch(&src.join("Show S2 OVA - 15.mkv"), "");
        touch(&src.join("Show - OVA 01.id.ass"), "x");
        touch(&src.join("Show - OVA 02.id.ass"), "x");

        let dest = Path::new("out");
        let plan = build_plan(src, dest, "", "Show", "T", &[]).unwrap();
        assert_eq!(summary(&plan), ["01:0", "OVA 01:1", "OVA 02:1"]);
        assert_eq!(plan[1].target, dest.join("Show - OVA 01 (T).mkv"));
    }

    #[test]
    fn one_video_as_the_source_finds_its_subtitles_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path();
        touch(&src.join("Show - 05.mkv"), "");
        touch(&src.join("Show - 06.mkv"), "");
        touch(&src.join("Show - 05.id.ass"), "x");
        touch(&src.join("Show - 06.id.ass"), "x");

        let plan = build_plan(
            &src.join("Show - 05.mkv"),
            Path::new("out"),
            "",
            "Show",
            "T",
            &[],
        )
        .unwrap();
        assert_eq!(summary(&plan), ["05:1"]);
    }

    fn sub_track(id: u64, lang: &str, codec: &str, forced: bool) -> Value {
        json!({"id": id, "type": "subtitles",
               "properties": {"language": lang, "codec_id": codec, "forced_track": forced}})
    }

    #[test]
    fn picks_the_text_subtitle_in_the_wanted_language() {
        let langs = ["ind".to_string(), "id".to_string()];
        let info = |subs: Vec<Value>| json!({"tracks": subs});
        let pick = |subs: Vec<Value>| pick_embedded_sub(&info(subs), &langs);

        // Text in the wanted language, the one that is not forced first.
        let found = pick(vec![
            sub_track(2, "eng", "S_TEXT/ASS", false),
            sub_track(3, "ind", "S_HDMV/PGS", false),
            sub_track(4, "ind", "S_TEXT/ASS", true),
            sub_track(5, "ind", "S_TEXT/UTF8", false),
        ]);
        assert_eq!(
            found,
            Some(EmbeddedSub {
                track: 5,
                ext: "srt"
            })
        );
        // Only a forced one: still the best there is.
        let forced = pick(vec![sub_track(4, "ind", "S_TEXT/SSA", true)]);
        assert_eq!(
            forced,
            Some(EmbeddedSub {
                track: 4,
                ext: "ssa"
            })
        );
        // Pictures cannot be a subtitle file, and other languages do not count.
        assert_eq!(pick(vec![sub_track(3, "ind", "S_HDMV/PGS", false)]), None);
        assert_eq!(pick(vec![sub_track(2, "eng", "S_TEXT/ASS", false)]), None);
        assert_eq!(pick(vec![]), None);
    }

    #[test]
    fn an_embedded_subtitle_is_a_file_in_the_work_folder() {
        let mut ep = Episode {
            id: EpisodeId::parse("09").unwrap(),
            video: PathBuf::from("S/Show - 09.mkv"),
            subs: Vec::new(),
            embedded: Some(EmbeddedSub {
                track: 3,
                ext: "ass",
            }),
            target: PathBuf::from("D/Show - 09.mkv"),
        };
        assert_eq!(ep.sub_count(), 1);
        assert_eq!(
            ep.with_subs_in(Path::new("W")).subs,
            [PathBuf::from("W/sub.ass")]
        );
        // External subtitles win; the embedded one is not added to them.
        ep.subs = vec![PathBuf::from("S/Show - 09.id.ass")];
        assert_eq!(ep.sub_count(), 1);
        assert_eq!(ep.with_subs_in(Path::new("W")).subs, ep.subs);
        // Neither: nothing to add.
        ep.subs.clear();
        ep.embedded = None;
        assert_eq!(ep.sub_count(), 0);
        assert!(ep.with_subs_in(Path::new("W")).subs.is_empty());
    }

    #[test]
    fn only_yes_answers_continue() {
        for yes in ["y", "Y", "ya", "YES", " y\r\n"] {
            assert!(confirms(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "yaa", "maybe"] {
            assert!(!confirms(no), "{no:?}");
        }
    }

    #[test]
    fn missing_subtitles_stop_a_run_only_when_asked() {
        let ep = |n: &str, subs: usize| Episode {
            id: EpisodeId::parse(n).unwrap(),
            video: PathBuf::from(format!("S/{n}.mkv")),
            subs: vec![PathBuf::from("x.ass"); subs],
            embedded: None,
            target: PathBuf::from(format!("no/such/dir/{n}.mkv")),
        };
        // Everything has subtitles: nothing to ask.
        assert!(confirm_no_subs(&[ep("01", 1)], false, false).is_ok());
        // A dry run and --allow-no-subs go on without asking.
        assert!(confirm_no_subs(&[ep("01", 0)], true, false).is_ok());
        assert!(confirm_no_subs(&[ep("01", 0)], false, true).is_ok());
    }

    #[test]
    fn builds_the_subtitle_swap_command() {
        let args = swap_args(
            Path::new("D/a.mkv"),
            Path::new("D/a.mkv.subswap.tmp"),
            &[PathBuf::from("S/a.id.ass"), PathBuf::from("S/a.en.srt")],
            "id",
        );
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-o",
                "D/a.mkv.subswap.tmp",
                "--no-subtitles",
                "D/a.mkv",
                "--language",
                "0:id",
                "--default-track-flag",
                "0:1",
                "S/a.id.ass",
                "--language",
                "0:en",
                "--default-track-flag",
                "0:0",
                "S/a.en.srt",
            ]
        );
    }

    /// A 2 s mkv: one video, one Japanese audio track and one Indonesian subtitle.
    fn make_video(ffmpeg: &Path, out: &Path, cue: &str) {
        let srt = out.with_extension("cue.srt");
        std::fs::write(&srt, format!("1\n00:00:00,200 --> 00:00:01,500\n{cue}\n")).unwrap();
        let made = std::process::Command::new(ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=black:s=64x64:d=2")
            .args(["-f", "lavfi", "-i", "sine=d=2", "-i"])
            .arg(&srt)
            .args(["-map", "0:v", "-map", "1:a", "-map", "2:s"])
            .args(["-c:v", "mpeg4", "-c:a", "libopus", "-c:s", "srt"])
            .args(["-metadata:s:a:0", "language=jpn"])
            .args(["-metadata:s:s:0", "language=ind"])
            .arg(out)
            .status()
            .unwrap();
        assert!(made.success());
        std::fs::remove_file(&srt).unwrap();
    }

    /// Real files: swap the subtitle of a finished episode for an external one,
    /// then take an embedded one from a source video. Needs ffmpeg and MKVToolNix —
    /// run explicitly: `cargo test -- --ignored e2e_encode_subtitles`.
    #[tokio::test]
    #[ignore = "needs ffmpeg and mkvmerge"]
    async fn e2e_encode_subtitles() {
        let (Ok(mkvmerge), Ok(mkvextract)) = (
            tools::require_one(Tool::Mkvmerge),
            tools::require_one(Tool::Mkvextract),
        ) else {
            eprintln!("MKVToolNix not found; skipping");
            return;
        };
        let (ffmpeg, _) = tools::ffmpeg_pair().expect("ffmpeg missing");
        let dir = tempfile::tempdir().unwrap();
        let (src, out) = (dir.path().join("src"), dir.path().join("out"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&out).unwrap();

        // The source holds the video (with its own Indonesian subtitle) and a new
        // external one; the finished result is the same video with the old one.
        make_video(&ffmpeg, &src.join("Show - 01.mkv"), "embedded line");
        make_video(&ffmpeg, &out.join("Show - 01.mkv"), "old line");
        touch(
            &src.join("Show - 01.id.srt"),
            "1\n00:00:00,200 --> 00:00:01,500\nnew line\n",
        );

        let plan = build_plan(&src, &out, "", "Show", "", &[]).unwrap();
        assert_eq!(summary(&plan), ["01:1"]);
        let target = plan[0].target.clone();

        // A subtitle that is not there fails and leaves the result as it was.
        let before = std::fs::read(&target).unwrap();
        let missing = [src.join("missing.id.srt")];
        assert!(swap_subs(&mkvmerge, &target, &missing, "id").await.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), before);
        assert!(!target.with_extension("mkv.subswap.tmp").exists());

        replace_subs(&plan, false, "id").await.unwrap();
        let info = mkv_info(&mkvmerge, &target).await.unwrap();
        let subs: Vec<&Value> = info["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["type"] == "subtitles")
            .collect();
        assert_eq!(subs.len(), 1);
        assert!(track_matches_lang(subs[0], "id"), "{subs:?}");
        assert_ne!(std::fs::read(&target).unwrap(), before);
        assert!(check_output(&info, 1, Some("jpn"), "64x64").is_empty());

        // Without the external subtitle the one inside the video stands in.
        std::fs::remove_file(src.join("Show - 01.id.srt")).unwrap();
        std::fs::remove_file(&target).unwrap();
        let mut plan = build_plan(&src, &out, "", "Show", "", &[]).unwrap();
        assert_eq!(plan[0].sub_count(), 0);
        let langs = ["ind".to_string(), "id".to_string()];
        attach_embedded(&mut plan, &mkvmerge, &langs).await;
        assert_eq!(plan[0].sub_count(), 1);
        assert_eq!(plan[0].embedded.as_ref().map(|e| e.ext), Some("srt"));

        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        extract_embedded(Some(&mkvextract), &plan[0], &work)
            .await
            .unwrap();
        let text = std::fs::read_to_string(work.join("sub.srt")).unwrap();
        assert!(text.contains("embedded line"), "{text}");
    }

    #[test]
    fn the_series_is_named_after_the_destination_folder() {
        assert_eq!(
            folder_name(Path::new("D:/Media/Videos/Akatsuki no Yona")),
            "Akatsuki no Yona"
        );
        assert_eq!(folder_name(Path::new("out/")), "out");
    }

    #[test]
    fn resolves_templates_loosely() {
        let dir = tempfile::tempdir().unwrap();
        let tpl = dir.path().join("Settings").join("Templates");
        std::fs::create_dir_all(&tpl).unwrap();
        touch(&tpl.join("Anime-480p.AV1.Opus96.srip"), "");
        touch(&tpl.join("Anime-720p.AV1.Opus96.srip"), "");

        assert_eq!(
            resolve_template(dir.path(), "Anime480p.AV1.Opus96").unwrap(),
            "Anime-480p.AV1.Opus96"
        );
        let err = resolve_template(dir.path(), "nope")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Anime-720p.AV1.Opus96"), "{err}");
        assert!(resolve_template(&dir.path().join("missing"), "x").is_err());
    }
}
