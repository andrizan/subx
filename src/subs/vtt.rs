use anyhow::{Context, Result};

use super::{Event, SubFile, SubFormat, time};

/// Parse WebVTT. Everything before the first cue (WEBVTT header, NOTE/STYLE/
/// REGION blocks) is preserved verbatim in `header`. Cue identifiers and
/// cue settings are kept per event.
pub fn parse(text: &str) -> Result<SubFile> {
    let norm = text
        .strip_prefix('\u{FEFF}')
        .unwrap_or(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let lines: Vec<&str> = norm.lines().collect();

    let first = lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim())
        .unwrap_or("");
    if !first.starts_with("WEBVTT") {
        anyhow::bail!("missing WEBVTT header");
    }

    let mut file = SubFile::new(SubFormat::Vtt);
    let mut i = 0;
    // Header: everything up to the first cue.
    while i < lines.len() && !lines[i].contains("-->") {
        file.header.push(lines[i].to_string());
        i += 1;
    }
    // Drop trailing blank lines of the header block.
    while file
        .header
        .last()
        .map(|l| l.trim().is_empty())
        .unwrap_or(false)
    {
        file.header.pop();
    }

    while i < lines.len() {
        if lines[i].trim().is_empty() {
            i += 1;
            continue;
        }
        // Optional cue identifier (a non-blank line before the timing line).
        let mut name = String::new();
        if !lines[i].contains("-->") {
            name = lines[i].to_string();
            i += 1;
            if i >= lines.len() {
                break;
            }
        }
        if !lines[i].contains("-->") {
            i += 1;
            continue;
        }
        let (start, end, settings) = match parse_timing(lines[i]) {
            Some(t) => t,
            None => {
                i += 1;
                continue;
            }
        };
        i += 1;
        let mut body = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() {
            body.push(lines[i]);
            i += 1;
        }
        let body = body.join("\n");
        file.events.push(Event {
            start_ms: start,
            end_ms: end,
            text: body,
            name,
            extra: settings,
            ..Default::default()
        });
    }
    if file.events.is_empty() {
        anyhow::bail!("no VTT cues found");
    }
    Ok(file)
}

fn parse_timing(line: &str) -> Option<(i64, i64, String)> {
    let (a, rest) = line.split_once("-->")?;
    let mut parts = rest.trim().splitn(2, char::is_whitespace);
    let b = parts.next()?.trim();
    let settings = parts.next().unwrap_or("").trim().to_string();
    Some((time::parse_vtt_time(a)?, time::parse_vtt_time(b)?, settings))
}

/// Serialize back to WebVTT.
pub fn serialize(file: &SubFile) -> String {
    let mut out = String::new();
    for h in &file.header {
        out.push_str(h);
        out.push('\n');
    }
    out.push('\n');
    for e in &file.events {
        if !e.name.is_empty() {
            out.push_str(&e.name);
            out.push('\n');
        }
        if e.extra.is_empty() {
            out.push_str(&format!(
                "{} --> {}\n{}\n\n",
                time::format_vtt_time(e.start_ms),
                time::format_vtt_time(e.end_ms),
                e.text
            ));
        } else {
            out.push_str(&format!(
                "{} --> {} {}\n{}\n\n",
                time::format_vtt_time(e.start_ms),
                time::format_vtt_time(e.end_ms),
                e.extra,
                e.text
            ));
        }
    }
    out
}

/// Load a VTT file from disk.
pub fn load(path: &std::path::Path) -> Result<SubFile> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse(&text).with_context(|| format!("cannot parse VTT {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "WEBVTT\n\n00:00:01.000 --> 00:00:03.500 align:center\nHello <i>world</i>\n\n00:14.250 --> 00:16.000\nSecond cue\n\n";

    #[test]
    fn parses_cues() {
        let file = parse(SAMPLE).unwrap();
        assert_eq!(file.events.len(), 2);
        assert_eq!(file.events[0].start_ms, 1000);
        assert_eq!(file.events[0].extra, "align:center");
        assert_eq!(file.events[1].start_ms, 14250);
    }

    #[test]
    fn roundtrips() {
        let file = parse(SAMPLE).unwrap();
        let text = serialize(&file);
        // MM:SS.mmm input is canonicalized to HH:MM:SS.mmm.
        assert!(text.contains("00:00:14.250 --> 00:00:16.000"));
        let reparsed = parse(&text).unwrap();
        assert_eq!(reparsed.events.len(), 2);
    }

    #[test]
    fn rejects_non_vtt() {
        assert!(parse("1\n00:00:01,000 --> 00:00:02,000\nHi\n").is_err());
    }
}
