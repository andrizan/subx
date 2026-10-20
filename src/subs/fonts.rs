//! Font references inside ASS/SSA scripts and font names inside TTF/OTF/TTC
//! files, so a release can be checked for missing fonts before muxing.

use std::collections::BTreeSet;

fn push(out: &mut BTreeSet<String>, name: &str) {
    // `@Name` selects the vertical variant of the same family.
    let n = name.trim().trim_start_matches('@').trim();
    if !n.is_empty() {
        out.insert(n.to_string());
    }
}

/// Font families an ASS/SSA script asks for: every `Style:` Fontname plus the
/// `\fn` overrides inside Dialogue text (Comment lines are not rendered).
pub fn ass_font_refs(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut in_styles = false;
    let mut fontname_col = 1usize;
    for raw in text.lines() {
        let line = raw.trim_start_matches('\u{feff}').trim();
        if line.starts_with('[') {
            in_styles = line.eq_ignore_ascii_case("[V4+ Styles]")
                || line.eq_ignore_ascii_case("[V4 Styles]");
            continue;
        }
        if in_styles {
            if let Some(rest) = line.strip_prefix("Format:") {
                if let Some(i) = rest
                    .split(',')
                    .position(|f| f.trim().eq_ignore_ascii_case("Fontname"))
                {
                    fontname_col = i;
                }
            } else if let Some(rest) = line.strip_prefix("Style:")
                && let Some(name) = rest.split(',').nth(fontname_col)
            {
                push(&mut out, name);
            }
        } else if let Some(rest) = line.strip_prefix("Dialogue:") {
            let text = rest.splitn(10, ',').nth(9).unwrap_or("");
            for block in text.split('{').skip(1) {
                let block = block.split('}').next().unwrap_or("");
                for tag in block.split('\\').skip(1) {
                    if let Some(name) = tag.strip_prefix("fn") {
                        // A font name ends at the next tag; `\t(...)` transforms
                        // cannot change the font, so parentheses never belong to it.
                        push(&mut out, name.split(['(', ')']).next().unwrap_or(""));
                    }
                }
            }
        }
    }
    out
}

fn u16be(bytes: &[u8], at: usize) -> Option<u16> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn u32be(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Family (1), full (4) and typographic family (16) names from a TTF, OTF or
/// TTC file, in file order without case-insensitive duplicates. Only the
/// `name` table is read, so any other table may be absent or broken.
pub fn font_names(bytes: &[u8]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let fonts: Vec<usize> = if bytes.starts_with(b"ttcf") {
        let n = u32be(bytes, 8).unwrap_or(0).min(64) as usize;
        (0..n)
            .filter_map(|i| u32be(bytes, 12 + 4 * i).map(|v| v as usize))
            .collect()
    } else {
        vec![0]
    };
    for base in fonts {
        let Some(num_tables) = u16be(bytes, base + 4) else {
            continue;
        };
        for i in 0..num_tables as usize {
            let rec = base + 12 + 16 * i;
            if bytes.get(rec..rec + 4) != Some(b"name") {
                continue;
            }
            if let Some(off) = u32be(bytes, rec + 8) {
                read_name_table(bytes, off as usize, &mut names);
            }
        }
    }
    names
}

fn read_name_table(bytes: &[u8], off: usize, out: &mut Vec<String>) {
    let (Some(count), Some(strings)) = (u16be(bytes, off + 2), u16be(bytes, off + 4)) else {
        return;
    };
    for i in 0..count as usize {
        let r = off + 6 + 12 * i;
        let (Some(platform), Some(name_id), Some(len), Some(soff)) = (
            u16be(bytes, r),
            u16be(bytes, r + 6),
            u16be(bytes, r + 8),
            u16be(bytes, r + 10),
        ) else {
            return;
        };
        if !matches!(name_id, 1 | 4 | 16) {
            continue;
        }
        let start = off + strings as usize + soff as usize;
        let Some(raw) = bytes.get(start..start + len as usize) else {
            continue;
        };
        let text = match platform {
            // Unicode and Windows platforms store UTF-16BE.
            0 | 3 => {
                let units: Vec<u16> = raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_be_bytes(*c))
                    .collect();
                String::from_utf16_lossy(&units)
            }
            // Macintosh platform: Mac Roman, ASCII for the names that matter.
            1 => raw.iter().map(|&b| b as char).collect(),
            _ => continue,
        };
        let text = text.trim();
        if !text.is_empty() && !out.iter().any(|n| n.eq_ignore_ascii_case(text)) {
            out.push(text.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_style_and_override_fonts() {
        let ass = "\u{feff}[Script Info]\nTitle: x\n\n[V4+ Styles]\n\
            Format: Name, Fontname, Fontsize, PrimaryColour\n\
            Style: Default,Noto Sans,48,&H00FFFFFF\n\
            Style: Sign,@Yu Gothic UI,40,&H00FFFFFF\n\n\
            [Events]\n\
            Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\fnArial Black\\fs20}Hi, {\\b1}there\n\
            Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\fn}reset {\\t(0,100,\\fs30)}x\n\
            Comment: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\fnIgnored}note\n";
        let refs: Vec<String> = ass_font_refs(ass).into_iter().collect();
        assert_eq!(refs, ["Arial Black", "Noto Sans", "Yu Gothic UI"]);
    }

    #[test]
    fn honours_a_reordered_format_line() {
        let ass = "[V4 Styles]\nFormat: Name, Fontsize, Fontname\nStyle: A,20,Lato\n";
        assert_eq!(ass_font_refs(ass).into_iter().collect::<Vec<_>>(), ["Lato"]);
        assert!(ass_font_refs("[Events]\nDialogue: 0,0,0,D,,0,0,0,,plain\n").is_empty());
    }

    /// Minimal sfnt with only a `name` table carrying UTF-16BE Windows names.
    /// `base` is where the font starts inside the file (table offsets are absolute).
    fn fake_font(names: &[(u16, &str)], base: u32) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut records = Vec::new();
        for (id, text) in names {
            let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
            records.extend_from_slice(&3u16.to_be_bytes()); // platform: Windows
            records.extend_from_slice(&1u16.to_be_bytes()); // encoding
            records.extend_from_slice(&0x0409u16.to_be_bytes()); // language
            records.extend_from_slice(&id.to_be_bytes());
            records.extend_from_slice(&(utf16.len() as u16).to_be_bytes());
            records.extend_from_slice(&(strings.len() as u16).to_be_bytes());
            strings.extend_from_slice(&utf16);
        }
        let mut name_table = Vec::new();
        name_table.extend_from_slice(&0u16.to_be_bytes()); // format
        name_table.extend_from_slice(&(names.len() as u16).to_be_bytes());
        name_table.extend_from_slice(&((6 + records.len()) as u16).to_be_bytes());
        name_table.extend_from_slice(&records);
        name_table.extend_from_slice(&strings);

        let mut font = Vec::new();
        font.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // sfnt version
        font.extend_from_slice(&1u16.to_be_bytes()); // numTables
        font.extend_from_slice(&[0u8; 6]); // searchRange etc. (unused)
        font.extend_from_slice(b"name");
        font.extend_from_slice(&0u32.to_be_bytes()); // checksum
        font.extend_from_slice(&(28 + base).to_be_bytes()); // header 12 + one record 16, absolute
        font.extend_from_slice(&(name_table.len() as u32).to_be_bytes());
        font.extend_from_slice(&name_table);
        font
    }

    #[test]
    fn reads_names_from_sfnt_and_collections() {
        let names = [
            (1, "Test Sans"),
            (2, "Bold"),
            (4, "Test Sans Bold"),
            (16, "Test Sans"),
        ];
        let font = fake_font(&names, 0);
        assert_eq!(font_names(&font), ["Test Sans", "Test Sans Bold"]);

        // TTC: header + one offset pointing at the embedded font.
        let mut ttc = Vec::new();
        ttc.extend_from_slice(b"ttcf");
        ttc.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        ttc.extend_from_slice(&1u32.to_be_bytes());
        ttc.extend_from_slice(&16u32.to_be_bytes());
        ttc.extend_from_slice(&fake_font(&names, 16));
        assert_eq!(font_names(&ttc), ["Test Sans", "Test Sans Bold"]);

        assert!(font_names(b"not a font at all").is_empty());
        assert!(font_names(&[]).is_empty());
    }
}
