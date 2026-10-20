//! Matroska facts from `mkvmerge -J`, the parts ffprobe reports poorly:
//! per-track start offsets, attachments and chapter editions.

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
}
