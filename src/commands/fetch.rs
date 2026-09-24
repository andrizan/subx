use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

/// `subx fetch <URL>` — download subtitles via yt-dlp.
#[derive(Debug, Args)]
pub struct FetchArgs {
    /// Video/playlist URL.
    pub url: String,

    /// Subtitle language code.
    #[arg(long, default_value = "id")]
    pub lang: String,

    /// Output folder.
    #[arg(long, default_value = "Subs")]
    pub out: PathBuf,

    /// Subtitle format preference (yt-dlp `--sub-format`).
    #[arg(long, default_value = "srt/ass")]
    pub format: String,
}

fn find_ytdlp() -> Result<PathBuf> {
    which::which("yt-dlp").context(
        "yt-dlp not found on PATH. Install via `winget install yt-dlp` / `scoop install yt-dlp` or `pip install yt-dlp`",
    )
}

fn build_args(url: &str, lang: &str, format: &str, out: &Path) -> Vec<OsString> {
    let template = format!(
        "{}/%(uploader)s - %(title)s [%(id)s].%(ext)s",
        out.display()
    );
    [
        "--yes-playlist",
        "--skip-download",
        "--write-subs",
        "--write-auto-subs",
        "--sub-lang",
        lang,
        "--sub-format",
        format,
        "-o",
        &template,
        "--no-warnings",
        url,
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

pub async fn run(args: FetchArgs) -> Result<()> {
    let ytdlp = find_ytdlp()?;
    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("cannot create {}", args.out.display()))?;
    println!("Downloading '{}' subtitles for {}", args.lang, args.url);
    let status = tokio::process::Command::new(&ytdlp)
        .args(build_args(&args.url, &args.lang, &args.format, &args.out))
        .status()
        .await
        .with_context(|| "cannot start yt-dlp")?;
    if !status.success() {
        anyhow::bail!("yt-dlp failed (exit={})", status.code().unwrap_or(-1));
    }
    println!("Saved in: {}", args.out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_ytdlp_args() {
        let a: Vec<String> = build_args("URL", "id", "srt/ass", Path::new("Subs"))
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        assert_eq!(
            a,
            [
                "--yes-playlist",
                "--skip-download",
                "--write-subs",
                "--write-auto-subs",
                "--sub-lang",
                "id",
                "--sub-format",
                "srt/ass",
                "-o",
                "Subs/%(uploader)s - %(title)s [%(id)s].%(ext)s",
                "--no-warnings",
                "URL"
            ]
        );
    }
}
