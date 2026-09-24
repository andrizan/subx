use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;

use crate::common::lang;
use crate::media::probe;

/// `subx probe <FILE>` — show full media metadata (format, streams, chapters).
#[derive(Debug, Args)]
pub struct ProbeArgs {
    /// Media file.
    pub file: PathBuf,

    /// Raw JSON output.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: ProbeArgs) -> Result<()> {
    let (_ffmpeg, ffprobe) = crate::common::ffmpeg_find::find_ffmpeg()?;
    let info = probe::probe_full(&ffprobe, &args.file).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }
    print_human(&args.file, &info);
    Ok(())
}

fn print_human(file: &Path, info: &probe::FFProbeOutput) {
    println!("File: {}", file.display());
    if let Some(f) = &info.format {
        println!(
            "Format: {} | duration {} | size {} | bitrate {}",
            f.format_name.as_deref().unwrap_or("?"),
            probe::human_duration(probe::val_f64(f.duration.as_ref())),
            probe::human_size(probe::val_f64(f.size.as_ref())),
            probe::human_bitrate(probe::val_f64(f.bit_rate.as_ref())),
        );
    }

    println!("\nStreams ({}):", info.streams.len());
    for s in &info.streams {
        let kind = s.codec_type.as_deref().unwrap_or("?");
        let detail = match kind {
            "video" => {
                let fps = probe::fps(s.avg_frame_rate.as_deref())
                    .map(|f| format!("{f:.2} fps "))
                    .unwrap_or_default();
                format!(
                    "{}x{} {}{}",
                    probe::val(s.width.as_ref()),
                    probe::val(s.height.as_ref()),
                    fps,
                    s.pix_fmt.as_deref().unwrap_or("")
                )
                .trim()
                .to_string()
            }
            "audio" => format!(
                "{} Hz, {} ch",
                probe::val(s.sample_rate.as_ref()),
                probe::val(s.channels.as_ref())
            ),
            "subtitle" => format!(
                "lang='{}'->'{}' title='{}'",
                s.language_raw(),
                lang::normalize_language(s.language_raw()),
                s.title_raw()
            ),
            _ => String::new(),
        };
        let mut flags = String::new();
        if s.is_default() {
            flags.push_str(" [default]");
        }
        if s.is_forced() {
            flags.push_str(" [forced]");
        }
        println!(
            "  #{} {}/{kind} {detail}{flags}",
            s.index,
            s.codec_name.as_deref().unwrap_or("?")
        );
    }

    if !info.chapters.is_empty() {
        println!("\nChapters ({}):", info.chapters.len());
        for (n, c) in info.chapters.iter().enumerate() {
            println!(
                "  #{n} {} --> {} '{}'",
                c.start_time.as_deref().unwrap_or("?"),
                c.end_time.as_deref().unwrap_or("?"),
                c.title()
            );
        }
    }

    if let Some(f) = &info.format
        && !f.tags.is_empty()
    {
        println!("\nTags:");
        let mut tags: Vec<_> = f.tags.iter().collect();
        tags.sort();
        for (k, v) in tags {
            println!("  {k} = {v}");
        }
    }
}
