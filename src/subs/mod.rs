pub mod ass;
pub mod batch;
pub mod clean;
pub mod convert;
pub mod filter;
pub mod shift;
pub mod srt;
pub mod stats;
pub mod time;
pub mod vtt;

/// Subtitle container format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubFormat {
    Srt,
    Ass,
    Vtt,
}

/// One subtitle event (cue/dialogue) with timing in milliseconds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Event {
    pub start_ms: i64,
    pub end_ms: i64,
    /// ASS style name (empty for SRT/VTT).
    pub style: String,
    /// ASS actor/name field.
    pub name: String,
    /// ASS effect field.
    pub effect: String,
    /// Raw cue text, formatting tags included.
    pub text: String,
    /// ASS layer.
    pub layer: i32,
    /// ASS margins: [MarginL, MarginR, MarginV].
    pub margins: [String; 3],
    /// VTT cue settings (the part after the timestamps).
    pub extra: String,
}

/// A parsed subtitle file; the header is preserved verbatim for round trips.
#[derive(Debug, Clone)]
pub struct SubFile {
    pub format: SubFormat,
    pub events: Vec<Event>,
    /// Raw header lines (ASS lines outside Dialogue, WEBVTT prelude).
    pub header: Vec<String>,
    /// ASS column order (lowercase names); standard order by default.
    pub ass_columns: Vec<String>,
}

impl SubFile {
    pub fn new(format: SubFormat) -> Self {
        Self {
            format,
            events: Vec::new(),
            header: Vec::new(),
            ass_columns: standard_columns(),
        }
    }
}

/// Canonical ASS column order (lowercase).
pub fn standard_columns() -> Vec<String> {
    [
        "layer", "start", "end", "style", "name", "marginl", "marginr", "marginv", "effect", "text",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Strip `{...}` override blocks and `\N` / `\n` / `\h` breaks.
/// Used for emptiness checks so tag-only events count as empty.
pub fn plaintext(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0u32;
    for ch in text.chars() {
        match ch {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.replace("\\N", "")
        .replace("\\n", "")
        .replace("\\h", " ")
        .trim()
        .to_string()
}

/// File extensions (no dot, lowercase) this toolkit can parse.
pub fn supported_exts() -> &'static [&'static str] {
    &["srt", "ass", "ssa", "vtt"]
}
