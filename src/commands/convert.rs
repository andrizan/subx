use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;

use crate::subs::convert;

/// `subx convert <IN> <OUT>` — convert srt <-> ass <-> vtt.
#[derive(Debug, Args)]
pub struct ConvertArgs {
    /// Input subtitle file.
    pub input: PathBuf,
    /// Output file (extension decides the target format).
    pub output: PathBuf,

    /// FPS for frame-based subtitles (reserved, currently informational).
    #[arg(long)]
    pub fps: Option<f64>,
}

pub async fn run(args: ConvertArgs) -> Result<()> {
    if !args.input.is_file() {
        anyhow::bail!(
            "Input file not found (convert works file-to-file): {}",
            args.input.display()
        );
    }
    if let Some(fps) = args.fps {
        println!(
            "Note: --fps ({fps}) is reserved for frame-based formats and ignored for time-based input."
        );
    }
    let s = convert::convert_file(&args.input, &args.output)
        .with_context(|| format!("failed to convert {}", args.input.display()))?;
    println!("Converted {} event(s): {} -> {}", s.events, s.from, s.to);
    println!("Saved: {}", args.output.display());
    Ok(())
}
