use std::path::PathBuf;

use anyhow::{Context, Result};

/// Locate ffmpeg/ffprobe: (1) next to the binary, (2) on PATH.
/// Returns (ffmpeg, ffprobe).
pub fn find_ffmpeg() -> Result<(PathBuf, PathBuf)> {
    // 1. Next to the executable (e.g. portable ffmpeg.exe).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        #[cfg(windows)]
        let candidates = (dir.join("ffmpeg.exe"), dir.join("ffprobe.exe"));
        #[cfg(not(windows))]
        let candidates = (dir.join("ffmpeg"), dir.join("ffprobe"));
        if candidates.0.exists() && candidates.1.exists() {
            return Ok(candidates);
        }
    }

    // 2. System PATH via `which`.
    let ffmpeg =
        which::which("ffmpeg").context("ffmpeg not found on PATH or next to the binary. Install via `winget install ffmpeg` / `scoop install ffmpeg` or download from https://www.gyan.dev/ffmpeg/builds/")?;
    let ffprobe = which::which("ffprobe").context(
        "ffprobe not found. It usually ships together with ffmpeg (gyan.dev essentials).",
    )?;
    Ok((ffmpeg, ffprobe))
}
