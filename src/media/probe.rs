use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Full ffprobe output: container format, streams and chapters.
#[derive(Debug, Deserialize, Serialize)]
pub struct FFProbeOutput {
    #[serde(default)]
    pub streams: Vec<Stream>,
    #[serde(default)]
    pub format: Option<Format>,
    #[serde(default)]
    pub chapters: Vec<Chapter>,
}

/// Container-level metadata (`-show_format`).
#[derive(Debug, Deserialize, Serialize)]
pub struct Format {
    pub filename: Option<String>,
    pub format_name: Option<String>,
    pub duration: Option<Value>,
    pub size: Option<Value>,
    pub bit_rate: Option<Value>,
    #[serde(default)]
    pub tags: HashMap<String, String>,
}

/// One media stream (`-show_streams`).
#[derive(Debug, Deserialize, Serialize)]
pub struct Stream {
    pub index: i32,
    pub codec_name: Option<String>,
    pub codec_type: Option<String>,
    pub width: Option<Value>,
    pub height: Option<Value>,
    pub avg_frame_rate: Option<String>,
    pub pix_fmt: Option<String>,
    pub sample_rate: Option<Value>,
    pub channels: Option<Value>,
    #[serde(default)]
    pub disposition: HashMap<String, Value>,
    #[serde(default)]
    pub tags: HashMap<String, String>,
}

/// One chapter entry (`-show_chapters`).
#[derive(Debug, Deserialize, Serialize)]
pub struct Chapter {
    pub id: Option<Value>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    #[serde(default)]
    pub tags: HashMap<String, String>,
}

impl Stream {
    pub fn is_subtitle(&self) -> bool {
        self.codec_type.as_deref() == Some("subtitle")
    }

    pub fn language_raw(&self) -> &str {
        self.tags.get("language").map(String::as_str).unwrap_or("")
    }

    pub fn title_raw(&self) -> &str {
        self.tags.get("title").map(String::as_str).unwrap_or("")
    }

    pub fn is_default(&self) -> bool {
        self.disposition.get("default").and_then(Value::as_i64) == Some(1)
    }

    pub fn is_forced(&self) -> bool {
        self.disposition.get("forced").and_then(Value::as_i64) == Some(1)
    }
}

impl Chapter {
    pub fn title(&self) -> &str {
        self.tags.get("title").map(String::as_str).unwrap_or("")
    }
}

/// Render a JSON number-or-string as plain text (ffprobe mixes both).
pub fn val(v: Option<&Value>) -> String {
    match v {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Parse a JSON number-or-string as f64.
pub fn val_f64(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `30000/1001` -> 29.97.
pub fn fps(s: Option<&str>) -> Option<f64> {
    let s = s?;
    if let Some((a, b)) = s.split_once('/') {
        let (a, b): (f64, f64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
        if b != 0.0 {
            return Some(a / b);
        }
        return None;
    }
    s.trim().parse().ok()
}

/// Bytes -> `12.34 MB`.
pub fn human_size(bytes: Option<f64>) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = match bytes {
        Some(b) => b.max(0.0),
        None => return "?".to_string(),
    };
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{v:.0} {}", UNITS[unit])
    } else {
        format!("{v:.2} {}", UNITS[unit])
    }
}

/// Seconds -> `01:02:03`.
pub fn human_duration(secs: Option<f64>) -> String {
    let s = match secs {
        Some(v) => v.max(0.0),
        None => return "?".to_string(),
    };
    format!(
        "{:02}:{:02}:{:02}",
        s as u64 / 3600,
        s as u64 % 3600 / 60,
        s as u64 % 60
    )
}

/// Bits/sec -> `128 kb/s`.
pub fn human_bitrate(bps: Option<f64>) -> String {
    match bps {
        Some(b) => format!("{:.0} kb/s", b.max(0.0) / 1000.0),
        None => "?".to_string(),
    }
}

async fn run_ffprobe(ffprobe: &Path, file: &Path, sections: &[&str]) -> Result<FFProbeOutput> {
    let mut cmd = tokio::process::Command::new(ffprobe);
    cmd.args(["-v", "quiet", "-print_format", "json"]);
    for s in sections {
        cmd.arg(s);
    }
    let out = cmd
        .arg(file)
        .output()
        .await
        .with_context(|| format!("failed to run ffprobe for {}", file.display()))?;

    if !out.status.success() {
        anyhow::bail!(
            "ffprobe failed for {}: {}",
            file.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    serde_json::from_slice(&out.stdout).context("failed to parse ffprobe JSON")
}

/// List subtitle streams (used by extract/mux).
pub async fn probe_streams(ffprobe: &Path, file: &Path) -> Result<Vec<Stream>> {
    Ok(run_ffprobe(ffprobe, file, &["-show_streams"])
        .await?
        .streams)
}

/// Full metadata: format + streams + chapters.
pub async fn probe_full(ffprobe: &Path, file: &Path) -> Result<FFProbeOutput> {
    run_ffprobe(
        ffprobe,
        file,
        &["-show_format", "-show_streams", "-show_chapters"],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffprobe_sample() {
        let json = serde_json::json!({
            "streams": [
                {"index": 0, "codec_name": "h264", "codec_type": "video", "tags": {}},
                {"index": 1, "codec_name": "aac", "codec_type": "audio"},
                {"index": 2, "codec_name": "subrip", "codec_type": "subtitle",
                 "tags": {"language": "eng", "title": "Full"}},
                {"index": 3, "codec_name": "ass", "codec_type": "subtitle", "tags": {}}
            ]
        });
        let parsed: FFProbeOutput = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.streams.len(), 4);

        let subs: Vec<_> = parsed.streams.iter().filter(|s| s.is_subtitle()).collect();
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].language_raw(), "eng");
        assert_eq!(subs[0].title_raw(), "Full");
        // Missing tags default to empty.
        assert_eq!(subs[1].language_raw(), "");
        assert_eq!(parsed.streams[1].language_raw(), "");
    }

    #[test]
    fn parses_format_and_chapters() {
        let json = serde_json::json!({
            "format": {
                "filename": "Ep01.mkv",
                "format_name": "matroska,webm",
                "duration": "1425.500000",
                "size": "734003200",
                "bit_rate": "4119352",
                "tags": {"title": "Episode 1"}
            },
            "streams": [],
            "chapters": [
                {"id": 0, "start_time": "0.000000", "end_time": "90.000000",
                 "tags": {"title": "Prologue"}}
            ]
        });
        let parsed: FFProbeOutput = serde_json::from_value(json).unwrap();
        let f = parsed.format.unwrap();
        assert_eq!(f.format_name.as_deref(), Some("matroska,webm"));
        assert_eq!(human_duration(val_f64(f.duration.as_ref())), "00:23:45");
        assert_eq!(human_size(val_f64(f.size.as_ref())), "700.00 MB");
        assert_eq!(parsed.chapters.len(), 1);
        assert_eq!(parsed.chapters[0].title(), "Prologue");
    }

    #[test]
    fn formats_display_values() {
        assert_eq!(fps(Some("30000/1001")), Some(29.97002997002997));
        assert_eq!(fps(Some("25/1")), Some(25.0));
        assert_eq!(fps(Some("0/0")), None);
        assert_eq!(fps(None), None);
        assert_eq!(val(Some(&serde_json::json!(320))), "320");
        assert_eq!(val(Some(&serde_json::json!("48000"))), "48000");
        assert_eq!(val(None), "");
        assert_eq!(human_bitrate(Some(128_000.0)), "128 kb/s");
        assert_eq!(human_duration(None), "?");
    }

    /// End-to-end: mux a real MKV with 2 subtitle tracks, then probe it.
    /// Needs ffmpeg/ffprobe binaries — run explicitly: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs ffmpeg/ffprobe binaries"]
    async fn e2e_mkv_with_two_sub_tracks() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let en = dir.path().join("en.srt");
        let id = dir.path().join("id.srt");
        std::fs::write(&en, "1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
        std::fs::write(&id, "1\n00:00:00,000 --> 00:00:01,000\nHalo\n").unwrap();
        let mkv = dir.path().join("sample.mkv");

        // Prefer libx264, fall back to mpeg4 where x264 is unavailable.
        let mut ok = false;
        for vcodec in ["libx264", "mpeg4"] {
            let status = Command::new("ffmpeg")
                .args([
                    "-y",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=duration=2:size=320x240:rate=30",
                    "-i",
                    en.to_str().expect("utf8 path"),
                    "-i",
                    id.to_str().expect("utf8 path"),
                    "-map",
                    "0:v",
                    "-map",
                    "1",
                    "-map",
                    "2",
                    "-c:v",
                    vcodec,
                    "-pix_fmt",
                    "yuv420p",
                    "-c:s",
                    "srt",
                    "-metadata:s:s:0",
                    "language=eng",
                    "-metadata:s:s:1",
                    "language=ind",
                    mkv.to_str().expect("utf8 path"),
                ])
                .status()
                .expect("ffmpeg binary missing");
            if status.success() {
                ok = true;
                break;
            }
        }
        assert!(ok, "ffmpeg could not mux the sample MKV");

        let (_ffmpeg, ffprobe) =
            crate::common::ffmpeg_find::find_ffmpeg().expect("ffprobe missing");
        let streams = probe_streams(&ffprobe, &mkv).await.unwrap();
        let subs: Vec<_> = streams.iter().filter(|s| s.is_subtitle()).collect();
        assert_eq!(subs.len(), 2);

        let mut langs: Vec<String> = subs
            .iter()
            .map(|s| crate::common::lang::normalize_language(s.language_raw()))
            .collect();
        langs.sort();
        assert_eq!(langs, ["en".to_string(), "id".to_string()]);
    }
}
