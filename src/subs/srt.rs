use anyhow::{Context, Result};

use super::{Event, SubFile, SubFormat, time};

/// Parse SubRip text into events. Block numbers are validated loosely so
/// slightly malformed files still load; only the timings must parse.
pub fn parse(text: &str) -> Result<SubFile> {
    let norm = text
        .strip_prefix('\u{FEFF}')
        .unwrap_or(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let lines: Vec<&str> = norm.lines().collect();
    let mut file = SubFile::new(SubFormat::Srt);
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim().is_empty() {
            i += 1;
            continue;
        }
        let mut j = i;
        // Optional numeric index line.
        if !lines[j].contains("-->") {
            j += 1;
            if j >= lines.len() {
                break;
            }
        }
        let range = match parse_range(lines[j]) {
            Some(r) => r,
            None => {
                i = j + 1;
                continue;
            }
        };
        j += 1;
        let mut body = Vec::new();
        while j < lines.len() && !lines[j].trim().is_empty() {
            body.push(lines[j]);
            j += 1;
        }
        file.events.push(Event {
            start_ms: range.0,
            end_ms: range.1,
            text: body.join("\n"),
            ..Default::default()
        });
        i = j;
    }
    if file.events.is_empty() {
        anyhow::bail!("no SRT cues found");
    }
    Ok(file)
}

fn parse_range(line: &str) -> Option<(i64, i64)> {
    let (a, b) = line.split_once("-->")?;
    Some((time::parse_srt_time(a)?, time::parse_srt_time(b)?))
}

/// Serialize back to canonical SubRip (renumbered from 1).
pub fn serialize(file: &SubFile) -> String {
    let mut out = String::new();
    for (n, e) in file.events.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            n + 1,
            time::format_srt_time(e.start_ms),
            time::format_srt_time(e.end_ms),
            e.text
        ));
    }
    out
}

/// Load an SRT file from disk.
pub fn load(path: &std::path::Path) -> Result<SubFile> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse(&text).with_context(|| format!("cannot parse SRT {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "1\n00:00:01,000 --> 00:00:03,500\nHello {\\i1}world{\\i0}\n\n2\n00:00:04,000 --> 00:00:06,000\nSecond\\Nline\n\n";

    #[test]
    fn parses_cues() {
        let file = parse(SAMPLE).unwrap();
        assert_eq!(file.events.len(), 2);
        assert_eq!(file.events[0].start_ms, 1000);
        assert_eq!(file.events[0].end_ms, 3500);
        assert_eq!(file.events[0].text, "Hello {\\i1}world{\\i0}");
        assert_eq!(file.events[1].text, "Second\\Nline");
    }

    #[test]
    fn roundtrips() {
        let file = parse(SAMPLE).unwrap();
        assert_eq!(serialize(&file), SAMPLE);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("just some text\nno timings here\n").is_err());
    }
}
