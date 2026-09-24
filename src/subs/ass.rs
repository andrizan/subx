use anyhow::{Context, Result};

use super::{Event, SubFile, SubFormat, standard_columns, time};

/// Parse ASS/SSA. The `Format:` column order is honored, so files with
/// reordered columns still parse. Every non-Dialogue line (including the
/// `Format:` line and `Comment:` lines) is preserved verbatim in `header`.
pub fn parse(text: &str) -> Result<SubFile> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut file = SubFile::new(SubFormat::Ass);
    let mut columns = standard_columns();
    let mut in_events = false;

    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_events = trimmed.eq_ignore_ascii_case("[events]");
            file.header.push(line.to_string());
            continue;
        }
        if in_events && let Some((key, _)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case("format") {
                columns = parse_format_columns(line);
                file.ass_columns = columns.clone();
                file.header.push(line.to_string());
                continue;
            }
            if key.trim().eq_ignore_ascii_case("dialogue") {
                if let Some(ev) = parse_dialogue(line, &columns) {
                    file.events.push(ev);
                }
                continue;
            }
        }
        file.header.push(line.to_string());
    }
    if file.events.is_empty() {
        anyhow::bail!("no ASS dialogues found");
    }
    Ok(file)
}

fn parse_format_columns(line: &str) -> Vec<String> {
    match line.split_once(':') {
        Some((_, rest)) => {
            let cols: Vec<String> = rest.split(',').map(|c| c.trim().to_lowercase()).collect();
            if cols.contains(&"start".to_string()) && cols.contains(&"text".to_string()) {
                cols
            } else {
                standard_columns()
            }
        }
        None => standard_columns(),
    }
}

fn parse_dialogue(line: &str, columns: &[String]) -> Option<Event> {
    let body = line.split_once(':')?.1;
    let parts: Vec<&str> = body.splitn(columns.len(), ',').collect();
    if parts.len() < columns.len() {
        return None;
    }
    let get = |name: &str| -> &str {
        columns
            .iter()
            .position(|c| c == name)
            .map(|i| parts[i].trim())
            .unwrap_or("")
    };
    // Cue text keeps its original spacing (no trim: the comma is the delimiter).
    let text = columns
        .iter()
        .position(|c| c == "text")
        .map(|i| parts[i].to_string())
        .unwrap_or_default();
    Some(Event {
        start_ms: time::parse_ass_time(get("start"))?,
        end_ms: time::parse_ass_time(get("end"))?,
        style: get("style").to_string(),
        name: get("name").to_string(),
        effect: get("effect").to_string(),
        text,
        layer: get("layer").parse().unwrap_or(0),
        margins: [
            get("marginl").to_string(),
            get("marginr").to_string(),
            get("marginv").to_string(),
        ],
        ..Default::default()
    })
}

fn field(e: &Event, col: &str) -> String {
    match col {
        "layer" => e.layer.to_string(),
        "start" => time::format_ass_time(e.start_ms),
        "end" => time::format_ass_time(e.end_ms),
        "style" => e.style.clone(),
        "name" => e.name.clone(),
        "marginl" => e.margins[0].clone(),
        "marginr" => e.margins[1].clone(),
        "marginv" => e.margins[2].clone(),
        "effect" => e.effect.clone(),
        "text" => e.text.clone(),
        _ => String::new(),
    }
}

/// Serialize back to ASS: preserved header lines plus rebuilt Dialogue lines
/// emitted in the file's own column order.
pub fn serialize(file: &SubFile) -> String {
    let mut out = String::new();
    for h in &file.header {
        out.push_str(h);
        out.push('\n');
    }
    for e in &file.events {
        let parts: Vec<String> = file.ass_columns.iter().map(|c| field(e, c)).collect();
        out.push_str("Dialogue: ");
        out.push_str(&parts.join(","));
        out.push('\n');
    }
    out
}

/// Load an ASS/SSA file from disk.
pub fn load(path: &std::path::Path) -> Result<SubFile> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse(&text).with_context(|| format!("cannot parse ASS {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "[Script Info]\nTitle: Sample\nScriptType: v4.00+\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,2,2,10,10,10,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello {\\i1}world\nComment: 0,0:00:02.00,0:00:03.00,Default,,0,0,0,,kept as header\nDialogue: 1,0:00:04.50,0:00:06.00,Karaoke,,10,10,20,,{\\k40}la\n";

    #[test]
    fn parses_dialogues() {
        let file = parse(SAMPLE).unwrap();
        assert_eq!(file.events.len(), 2);
        assert_eq!(file.events[0].start_ms, 1000);
        assert_eq!(file.events[0].text, "Hello {\\i1}world");
        assert_eq!(file.events[1].layer, 1);
        assert_eq!(file.events[1].margins, ["10", "10", "20"]);
        // Comment: lines stay in the header.
        assert!(file.header.iter().any(|l| l.starts_with("Comment:")));
    }

    #[test]
    fn roundtrips_standard_layout() {
        let file = parse(SAMPLE).unwrap();
        let text = serialize(&file);
        // Comment: lines travel with the header; dialogues keep file order.
        assert!(text.contains("Comment: 0,0:00:02.00"));
        let d1 = "Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello {\\i1}world";
        let d2 = "Dialogue: 1,0:00:04.50,0:00:06.00,Karaoke,,10,10,20,,{\\k40}la";
        let p1 = text.find(d1).expect("first dialogue");
        let p2 = text.find(d2).expect("second dialogue");
        assert!(p1 < p2);
        // And the events re-parse identically.
        let reparsed = parse(&text).unwrap();
        assert_eq!(reparsed.events, file.events);
    }
}
