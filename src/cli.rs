use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// All-in-one subx toolkit (extract / clean / shift / mux / translate ...).
#[derive(Debug, Parser)]
#[command(name = "subx", version, about, propagate_version = true)]
pub struct Cli {
    /// Path to subx.toml (optional).
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Verbose logging (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Quiet mode (errors only).
    #[arg(short, long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Extract subtitles from MKV/MP4.
    Extract(crate::commands::extract::ExtractArgs),
    /// Clean ASS files: keep karaoke, drop junk, normalize the font.
    Clean(crate::commands::clean::CleanArgs),
    /// Shift timestamps by ±ms and drop empty events.
    Shift(crate::commands::shift::ShiftArgs),
    /// Delete lines containing keywords.
    Filter(crate::commands::filter::FilterArgs),
    /// Merge video+audio+subs (softmux copy / hardsub burn).
    Mux(crate::commands::mux::MuxArgs),
    /// Convert srt <-> ass <-> vtt.
    Convert(crate::commands::convert::ConvertArgs),
    /// Translate via LibreTranslate or an AI model (preserve ASS tags, cache, glossary).
    Translate(crate::commands::translate::TranslateArgs),
    /// Show full media metadata (format, streams, chapters).
    Probe(crate::commands::probe::ProbeArgs),
    /// Download subtitles via yt-dlp.
    Fetch(crate::commands::fetch::FetchArgs),
    /// QC stats: reading speed, durations, overlaps.
    Stats(crate::commands::stats::StatsArgs),
}
