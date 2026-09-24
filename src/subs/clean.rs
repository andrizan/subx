use once_cell::sync::Lazy;
use regex::Regex;

use super::{Event, shift::shift_events};

static KARAOKE_TAG: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\\k[0-9]|\\K[0-9]|\\kf[0-9]|\\ko[0-9]").unwrap());
static OVERRIDE_BLOCK: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{[^}]*\}").unwrap());
static DRAWING: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^\s*[mlbc]\s+[-0-9.]").unwrap());
static DOMAIN_JUNK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[A-Za-z0-9_]+\.[a-zA-Z]{2,}$").unwrap());
static DEATH_JUNK: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)deaths?").unwrap());
static SYNC_TITLE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)sync.*title|sign.*title").unwrap());
static SYMBOLS_ONLY: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[^a-zA-Z0-9\s]{3,}$").unwrap());

/// True for vertical karaoke: karaoke timing tags, or many tiny lines.
pub fn is_karaoke_vertical(text: &str) -> bool {
    if KARAOKE_TAG.is_match(text) {
        return true;
    }
    let lines: Vec<&str> = text
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    lines.len() > 5 && lines.iter().all(|l| l.chars().count() <= 4)
}

/// Clean one normal dialogue: strip visual tags, drop junk (drawings,
/// bare domains, sync-title credits, symbol-only lines, empties).
/// Returns `None` when the line should be deleted.
pub fn clean_normal_text(text: &str) -> Option<String> {
    let stripped = OVERRIDE_BLOCK.replace_all(text, "");
    let t = stripped.trim();
    if DRAWING.is_match(t) {
        return None;
    }
    let words = t.split_whitespace().count();
    if words <= 3 && DOMAIN_JUNK.is_match(t) {
        return None;
    }
    if t.len() < 30 && DEATH_JUNK.is_match(t) {
        return None;
    }
    if SYNC_TITLE.is_match(t) {
        return None;
    }
    if SYMBOLS_ONLY.is_match(t) {
        return None;
    }
    if t.is_empty() {
        return None;
    }
    Some(t.to_string())
}

/// Regenerated header for cleaned files: everything in Arial, with separate
/// `Default` and `Karaoke` styles (port of trim_ass.py).
pub fn default_header(font: &str) -> Vec<String> {
    vec![
        "[Script Info]".to_string(),
        "Title: Cleaned Dialogs Keep Karaoke".to_string(),
        "ScriptType: v4.00+".to_string(),
        "WrapStyle: 0".to_string(),
        "ScaledBorderAndShadow: yes".to_string(),
        String::new(),
        "[V4+ Styles]".to_string(),
        "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding".to_string(),
        format!("Style: Default,{font},20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,2,2,10,10,10,1"),
        format!("Style: Karaoke,{font},24,&H00FFFFFF,&H0000FFFF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,1,2,10,10,10,1"),
        String::new(),
        "[Events]".to_string(),
        "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text".to_string(),
        String::new(),
    ]
}

#[derive(Debug, Default)]
pub struct CleanStats {
    pub dialogs: usize,
    pub karaoke: usize,
}

/// Clean ASS events: keep vertical karaoke verbatim (style `Karaoke`),
/// scrub normal dialogues (style `Default`), optionally shift timings.
/// (`font` is unused here; the caller builds the header via `default_header`.)
pub fn clean_events(events: &[Event], _font: &str, shift_ms: i32) -> (Vec<Event>, CleanStats) {
    let mut out = Vec::new();
    let mut stats = CleanStats::default();
    for e in events {
        let mut kept = e.clone();
        if shift_ms != 0 {
            shift_events(std::slice::from_mut(&mut kept), shift_ms);
        }
        if is_karaoke_vertical(&kept.text) {
            kept.style = "Karaoke".to_string();
            stats.karaoke += 1;
        } else {
            match clean_normal_text(&kept.text) {
                Some(t) => {
                    kept.text = t;
                    kept.style = "Default".to_string();
                }
                None => continue,
            }
        }
        kept.effect.clear();
        out.push(kept);
        stats.dialogs += 1;
    }
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_karaoke() {
        assert!(is_karaoke_vertical("{\\k40}ka{\\k30}ra"));
        assert!(is_karaoke_vertical("{\\K10}x"));
        assert!(!is_karaoke_vertical("{\\i1}Hello world"));
    }

    #[test]
    fn scrubs_dialogues() {
        assert_eq!(
            clean_normal_text("{\\i1}Hello{\\i0} world"),
            Some("Hello world".into())
        );
        assert_eq!(clean_normal_text("Visit example.com"), None);
        assert_eq!(clean_normal_text("Subbed by DeathFans"), None);
        assert_eq!(clean_normal_text("Sync and Title by XYZ"), None);
        assert_eq!(clean_normal_text("m 0 0 l 100 100"), None);
        assert_eq!(clean_normal_text("***"), None);
        assert_eq!(clean_normal_text(""), None);
        assert_eq!(
            clean_normal_text("Just a normal line here"),
            Some("Just a normal line here".into())
        );
    }

    #[test]
    fn golden_clean_sample() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
        let src = std::fs::read_to_string(dir.join("dirty.ass")).unwrap();
        let file = super::super::ass::parse(&src).unwrap();
        let (events, stats) = clean_events(&file.events, "Arial", 0);
        assert_eq!(stats.dialogs, 2);
        assert_eq!(stats.karaoke, 1);

        let out = super::super::SubFile {
            format: super::super::SubFormat::Ass,
            header: default_header("Arial"),
            ass_columns: super::super::standard_columns(),
            events,
        };
        let text = super::super::ass::serialize(&out);
        let expected = std::fs::read_to_string(dir.join("clean.ass")).unwrap();
        assert_eq!(normalize(&text), normalize(&expected));
    }

    fn normalize(s: &str) -> String {
        s.replace("\r\n", "\n").trim_end().to_string() + "\n"
    }
}
