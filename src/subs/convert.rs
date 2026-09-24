use std::path::Path;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;

use super::{SubFile, SubFormat};

static VTT_TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>\n]*>").unwrap());

pub struct ConvertSummary {
    pub events: usize,
    pub from: &'static str,
    pub to: &'static str,
}

fn format_name(f: SubFormat) -> &'static str {
    match f {
        SubFormat::Srt => "srt",
        SubFormat::Ass => "ass",
        SubFormat::Vtt => "vtt",
    }
}

fn target_format(path: &Path) -> Result<SubFormat> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "srt" => Ok(SubFormat::Srt),
        "ass" | "ssa" => Ok(SubFormat::Ass),
        "vtt" => Ok(SubFormat::Vtt),
        ext => anyhow::bail!("unsupported target format '.{ext}' (supported: srt, ass, vtt)"),
    }
}

/// ASS `{...}` blocks removed, `\N` breaks restored as real newlines.
fn ass_to_plain(text: &str) -> String {
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
    out.replace("\\N", "\n")
        .replace("\\n", "\n")
        .replace("\\h", " ")
}

/// VTT cue-payload tags (`<v Speaker>`, `<i>`, `<c.color>`) removed.
fn strip_vtt_tags(text: &str) -> String {
    VTT_TAG.replace_all(text, "").into_owned()
}

fn map_text(from: SubFormat, target: SubFormat, text: &str) -> String {
    let plain = match from {
        SubFormat::Ass => ass_to_plain(text),
        SubFormat::Vtt => strip_vtt_tags(text),
        SubFormat::Srt => text.to_string(),
    };
    if target == SubFormat::Ass {
        plain.replace('\n', "\\N")
    } else {
        plain
    }
}

fn ass_header() -> Vec<String> {
    vec![
        "[Script Info]".to_string(),
        "Title: Converted".to_string(),
        "ScriptType: v4.00+".to_string(),
        "WrapStyle: 0".to_string(),
        "ScaledBorderAndShadow: yes".to_string(),
        String::new(),
        "[V4+ Styles]".to_string(),
        "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding".to_string(),
        "Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,2,2,10,10,10,1".to_string(),
        String::new(),
        "[Events]".to_string(),
        "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text".to_string(),
    ]
}

/// Convert parsed subtitles to the target format, remapping line breaks,
/// tags and styles. Timing is preserved exactly.
pub fn convert(mut file: SubFile, target: SubFormat) -> SubFile {
    let from = file.format;
    if from != target {
        for e in &mut file.events {
            e.text = map_text(from, target, &e.text);
            if target == SubFormat::Ass && e.style.is_empty() {
                e.style = "Default".to_string();
            }
            e.name.clear();
            e.effect.clear();
            e.extra.clear();
            e.layer = 0;
            e.margins = Default::default();
        }
        if target == SubFormat::Ass {
            file.header = ass_header();
            file.ass_columns = super::standard_columns();
        } else {
            file.header = if target == SubFormat::Vtt {
                vec!["WEBVTT".to_string()]
            } else {
                Vec::new()
            };
        }
        file.format = target;
    }
    file
}

/// Convert a subtitle file on disk, guessing formats from extensions.
pub fn convert_file(input: &Path, output: &Path) -> Result<ConvertSummary> {
    let file = super::batch::load_any(input)?;
    let from = format_name(file.format);
    let target = target_format(output)?;
    let to = format_name(target);
    let events = file.events.len();
    let converted = convert(file, target);
    super::batch::save_any(&converted, output)
        .with_context(|| format!("cannot write {}", output.display()))?;
    Ok(ConvertSummary { events, from, to })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subs::{ass, srt, vtt};

    #[test]
    fn srt_to_ass_maps_breaks_and_style() {
        let file = srt::parse("1\n00:00:01,000 --> 00:00:02,000\nLine1\nLine2\n\n").unwrap();
        let out = convert(file, SubFormat::Ass);
        assert_eq!(out.events[0].text, "Line1\\NLine2");
        assert_eq!(out.events[0].style, "Default");
        let text = ass::serialize(&out);
        assert!(text.contains("Title: Converted"));
        // Timing survives the round trip.
        let back = ass::parse(&text).unwrap();
        assert_eq!(
            (back.events[0].start_ms, back.events[0].end_ms),
            (1000, 2000)
        );
    }

    #[test]
    fn ass_to_srt_restores_breaks() {
        let file = ass::parse(
            "[Script Info]\nTitle: x\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\i1}A\\NB{\\i0}\n",
        )
        .unwrap();
        let out = convert(file, SubFormat::Srt);
        assert_eq!(out.events[0].text, "A\nB");
    }

    #[test]
    fn vtt_to_srt_strips_tags() {
        let file = vtt::parse(
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<v Speaker>Hello <i>world</i>\n\n",
        )
        .unwrap();
        let out = convert(file, SubFormat::Srt);
        assert_eq!(out.events[0].text, "Hello world");
    }

    #[test]
    fn rejects_bad_target() {
        assert!(target_format(Path::new("out.txt")).is_err());
    }
}
