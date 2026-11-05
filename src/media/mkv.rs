//! Matroska facts from `mkvmerge -J`, the parts ffprobe reports poorly:
//! per-track start offsets, attachments and chapter editions.

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// Parsed `mkvmerge -J` output. Exit code 1 means warnings only.
pub async fn info(mkvmerge: &Path, file: &Path) -> Result<Value> {
    let out = tokio::process::Command::new(mkvmerge)
        .arg("-J")
        .arg(file)
        .output()
        .await
        .with_context(|| "cannot start mkvmerge")?;
    if out.status.code().unwrap_or(2) >= 2 {
        anyhow::bail!("mkvmerge -J failed for {}", file.display());
    }
    serde_json::from_slice(&out.stdout).with_context(|| "cannot parse mkvmerge JSON")
}

/// One subtitle track as listed by `mkvmerge -J`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubTrack {
    /// mkvmerge track id, the number `--sync` takes.
    pub id: u64,
    /// Position among the file's subtitle tracks, i.e. ffmpeg's `-map 0:s:N`.
    pub ordinal: u32,
    pub codec_id: String,
    pub language: String,
    pub name: String,
}

impl SubTrack {
    /// Extension a text track can be extracted as; `None` for bitmap subtitles.
    pub fn text_ext(&self) -> Option<&'static str> {
        let id = self.codec_id.to_ascii_uppercase();
        match id.as_str() {
            "S_TEXT/ASS" | "S_TEXT/SSA" | "S_ASS" | "S_SSA" => Some("ass"),
            s if s.starts_with("S_TEXT/") => Some("srt"),
            _ => None,
        }
    }

    /// `[ind] Indonesian (ASS)` for messages.
    pub fn label(&self) -> String {
        let codec = self.codec_id.rsplit('/').next().unwrap_or(&self.codec_id);
        let lang = if self.language.is_empty() {
            "und"
        } else {
            &self.language
        };
        if self.name.is_empty() {
            format!("[{lang}] ({codec})")
        } else {
            format!("[{lang}] {} ({codec})", self.name)
        }
    }
}

/// Subtitle tracks in file order.
pub fn subtitle_tracks(info: &Value) -> Vec<SubTrack> {
    let text = |v: &Value| v.as_str().unwrap_or("").to_string();
    info["tracks"]
        .as_array()
        .map(|tracks| {
            tracks
                .iter()
                .filter(|t| t["type"].as_str() == Some("subtitles"))
                .enumerate()
                .filter_map(|(ordinal, t)| {
                    let p = &t["properties"];
                    Some(SubTrack {
                        id: t["id"].as_u64()?,
                        ordinal: ordinal as u32,
                        codec_id: text(&p["codec_id"]),
                        language: text(&p["language"]),
                        name: text(&p["track_name"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// mkvmerge arguments that copy `input` to `output` unchanged except for a
/// constant delay (ms, negative = earlier) on the listed `(track id, delay)`s.
fn retime_args(input: &Path, output: &Path, delays: &[(u64, i64)]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-o".into(), output.into()];
    for (id, ms) in delays {
        args.push("--sync".into());
        args.push(format!("{id}:{ms}").into());
    }
    args.push(input.into());
    args
}

/// Remux `input` to `output` with the given subtitle delays; every stream is
/// copied, chapters and attachments carry over. Exit code 1 means warnings only.
pub async fn retime(
    mkvmerge: &Path,
    input: &Path,
    output: &Path,
    delays: &[(u64, i64)],
) -> Result<()> {
    let out = tokio::process::Command::new(mkvmerge)
        .args(retime_args(input, output, delays))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| "cannot start mkvmerge")?;
    if out.status.code().unwrap_or(2) >= 2 {
        let text = String::from_utf8_lossy(&out.stdout);
        let detail: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("Error"))
            .take(3)
            .collect();
        anyhow::bail!(
            "mkvmerge failed for {}: {}",
            input.display(),
            if detail.is_empty() {
                "see mkvmerge output".to_string()
            } else {
                detail.join(" | ")
            }
        );
    }
    Ok(())
}

/// Total chapter entries across all editions.
pub fn chapter_entries(info: &Value) -> u64 {
    info["chapters"].as_array().map_or(0, |c| {
        c.iter()
            .map(|e| e["num_entries"].as_u64().unwrap_or(0))
            .sum()
    })
}

/// Attachments that are fonts: `(attachment id, file name)`.
pub fn font_attachments(info: &Value) -> Vec<(u64, String)> {
    const MIME: &[&str] = &[
        "application/x-truetype-font",
        "application/vnd.ms-opentype",
        "application/font-sfnt",
        "application/x-font-ttf",
        "application/x-font-otf",
    ];
    info["attachments"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|att| {
                    let id = att["id"].as_u64()?;
                    let name = att["file_name"].as_str()?.to_string();
                    let mime = att["content_type"].as_str().unwrap_or("").to_lowercase();
                    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
                    let is_font = mime.starts_with("font/")
                        || MIME.contains(&mime.as_str())
                        || matches!(ext.as_str(), "ttf" | "otf" | "ttc");
                    is_font.then_some((id, name))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_chapters_and_picks_font_attachments() {
        let info = json!({
            "chapters": [{"num_entries": 6}, {"num_entries": 2}],
            "attachments": [
                {"id": 1, "file_name": "KozMinPro-Bold.ttf", "content_type": "application/x-truetype-font"},
                {"id": 2, "file_name": "Legacy.otf", "content_type": "application/vnd.ms-opentype"},
                {"id": 3, "file_name": "cover.jpg", "content_type": "image/jpeg"},
                {"id": 4, "file_name": "odd.TTC", "content_type": "application/octet-stream"},
                {"id": 5, "file_name": "Noto.ttf", "content_type": "font/ttf"},
            ]
        });
        assert_eq!(chapter_entries(&info), 8);
        assert_eq!(chapter_entries(&json!({})), 0);
        let fonts = font_attachments(&info);
        let names: Vec<&str> = fonts.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(
            names,
            ["KozMinPro-Bold.ttf", "Legacy.otf", "odd.TTC", "Noto.ttf"]
        );
        assert_eq!(fonts[0].0, 1);
        assert!(font_attachments(&json!({})).is_empty());
    }

    #[test]
    fn lists_subtitle_tracks_in_file_order() {
        let info = json!({"tracks": [
            {"id": 0, "type": "video", "properties": {"codec_id": "V_MPEGH/ISO/HEVC"}},
            {"id": 1, "type": "audio", "properties": {"codec_id": "A_OPUS", "language": "jpn"}},
            {"id": 2, "type": "subtitles", "properties": {
                "codec_id": "S_TEXT/ASS", "language": "ind", "track_name": "Indonesian"}},
            {"id": 3, "type": "subtitles", "properties": {"codec_id": "S_HDMV/PGS"}},
            {"id": 5, "type": "subtitles", "properties": {
                "codec_id": "S_TEXT/UTF8", "language": "eng"}},
        ]});
        let tracks = subtitle_tracks(&info);
        let ids: Vec<(u64, u32)> = tracks.iter().map(|t| (t.id, t.ordinal)).collect();
        // mkvmerge ids skip other track types; ordinals count subtitles only.
        assert_eq!(ids, [(2, 0), (3, 1), (5, 2)]);
        assert_eq!(tracks[0].text_ext(), Some("ass"));
        assert_eq!(tracks[1].text_ext(), None);
        assert_eq!(tracks[2].text_ext(), Some("srt"));
        assert_eq!(tracks[0].label(), "[ind] Indonesian (ASS)");
        assert_eq!(tracks[1].label(), "[und] (PGS)");
        assert!(subtitle_tracks(&json!({})).is_empty());
    }

    #[test]
    fn retime_args_delay_only_the_listed_tracks() {
        let args = retime_args(
            Path::new("in.mkv"),
            Path::new("out.mkv"),
            &[(2, 2370), (5, -1200)],
        );
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "-o", "out.mkv", "--sync", "2:2370", "--sync", "5:-1200", "in.mkv"
            ]
        );
    }
}
