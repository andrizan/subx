use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::common::episode::EpisodeId;
use crate::common::tools::{self, Tool};
use crate::media::speech;
use crate::subs::sync::{self, Alignment, Bits};
use crate::subs::{batch, shift, supported_exts};

/// Video and audio containers ffmpeg can listen to.
const MEDIA_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "mov", "webm", "avi", "ts", "m2ts", "mka", "m4a", "aac", "flac", "opus",
    "ogg", "mp3", "wav",
];

/// `subx sync <IN> <OUT> --ref <VIDEO|SUB>` — find the offset automatically and retime.
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Subtitle file or folder to retime.
    pub input: PathBuf,
    /// Output file or folder (may equal INPUT; optional with --dry-run).
    #[arg(required_unless_present = "dry_run")]
    pub output: Option<PathBuf>,

    /// Reference to line up with: a video/audio file (speech is detected in its
    /// audio), an already-timed subtitle, or a folder of either (paired with
    /// INPUT's files by episode number).
    #[arg(long = "ref", value_name = "PATH")]
    pub reference: PathBuf,

    /// Audio stream of the reference to listen to (0 = first).
    #[arg(long, default_value_t = 0)]
    pub audio_track: u32,

    /// Largest offset to search for, in seconds (either direction).
    #[arg(long, default_value_t = 120.0)]
    pub max_shift: f64,

    /// Do not write files whose offset confidence (0..1) is lower than this;
    /// 0 always writes.
    #[arg(long, default_value_t = 0.2)]
    pub min_confidence: f64,

    /// Report the offsets without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Recurse into subfolders (folder mode).
    #[arg(short, long)]
    pub recursive: bool,

    /// Subtitle extensions to process (folder mode).
    #[arg(long, default_value = ".srt,.ass,.ssa,.vtt")]
    pub ext: String,
}

struct Outcome {
    alignment: Alignment,
    /// Offset was trusted (and written unless `--dry-run`).
    trusted: bool,
}

fn is_subtitle(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| supported_exts().contains(&e.to_lowercase().as_str()))
}

fn episode_key(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(EpisodeId::from_file_name)
        .map(|id| id.key())
}

fn offset_text(ms: i64) -> String {
    match ms {
        0 => "0 ms (already in sync)".to_string(),
        m if m > 0 => format!("+{m} ms (delayed)"),
        m => format!("{m} ms (advanced)"),
    }
}

fn quality_text(a: &Alignment) -> String {
    format!(
        "{:.2} (peak {:.1} sigma), {:.0}% of cue time on speech",
        a.confidence,
        a.peak_sigma,
        a.coverage * 100.0
    )
}

/// The reference signal: cues of a subtitle, else speech in the audio.
async fn reference_bits(path: &Path, audio_track: u32) -> Result<Bits> {
    if is_subtitle(path) {
        let file = batch::load_any(path)?;
        return Ok(Bits::from_ranges(sync::cue_ranges(&file.events)));
    }
    let ffmpeg = tools::require_one(Tool::Ffmpeg)?;
    Ok(Bits::from_ranges(
        speech::detect(&ffmpeg, path, audio_track).await?,
    ))
}

/// Align one subtitle with one reference; writes `output` when the offset is
/// trusted and this is not a dry run.
async fn sync_one(
    input: &Path,
    output: Option<&Path>,
    reference: &Path,
    args: &SyncArgs,
) -> Result<Outcome> {
    // Fully loaded before writing, so in-place runs (input == output) are safe.
    let mut file = batch::load_any(input)?;
    let subs = Bits::from_ranges(sync::cue_ranges(&file.events));
    let reference = reference_bits(reference, args.audio_track)
        .await
        .with_context(|| format!("cannot read the reference {}", reference.display()))?;

    let max_shift_ms = (args.max_shift * 1000.0).round() as i64;
    let alignment = sync::align(&reference, &subs, max_shift_ms)?;
    let trusted = alignment.confidence >= args.min_confidence;
    if let (true, false, Some(output)) = (trusted, args.dry_run, output) {
        if alignment.shift_ms != 0 {
            shift::shift_events(&mut file.events, alignment.shift_ms as i32);
        }
        batch::save_any(&file, output)?;
    }
    Ok(Outcome { alignment, trusted })
}

/// Reference files of a folder keyed by episode; a video beats a subtitle of the same episode.
fn collect_references(dir: &Path, recursive: bool) -> HashMap<String, PathBuf> {
    let depth = if recursive { usize::MAX } else { 1 };
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .min_depth(1)
        .max_depth(depth)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .filter(|p| {
            p.is_file()
                && (is_subtitle(p)
                    || p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| MEDIA_EXTS.contains(&e.to_lowercase().as_str())))
        })
        .collect();
    files.sort_by_key(|p| (is_subtitle(p), p.clone()));
    let mut by_episode = HashMap::new();
    for p in files {
        if let Some(key) = episode_key(&p) {
            by_episode.entry(key).or_insert(p);
        }
    }
    by_episode
}

pub async fn run(args: SyncArgs) -> Result<()> {
    if !(args.max_shift > 0.0 && args.max_shift <= 3600.0) {
        bail!("--max-shift must be between 0 and 3600 seconds.");
    }
    if !(0.0..=1.0).contains(&args.min_confidence) {
        bail!("--min-confidence must be between 0 and 1.");
    }
    if args.input.is_file() {
        return run_file(&args).await;
    }
    if !args.input.is_dir() {
        bail!("Input not found: {}", args.input.display());
    }
    run_folder(&args).await
}

async fn run_file(args: &SyncArgs) -> Result<()> {
    if !args.reference.is_file() {
        bail!(
            "--ref must be a file when INPUT is a file: {}",
            args.reference.display()
        );
    }
    let out = sync_one(&args.input, args.output.as_deref(), &args.reference, args)
        .await
        .with_context(|| format!("failed on {}", args.input.display()))?;

    println!("Input file  : {}", args.input.display());
    println!(
        "Reference   : {} ({})",
        args.reference.display(),
        if is_subtitle(&args.reference) {
            "subtitle cues".to_string()
        } else {
            format!("speech in audio track {}", args.audio_track)
        }
    );
    println!("Offset      : {}", offset_text(out.alignment.shift_ms));
    println!("Confidence  : {}", quality_text(&out.alignment));
    if args.dry_run {
        let verdict = if out.trusted {
            "would be written"
        } else {
            "would be skipped, confidence too low"
        };
        println!("Dry run     : nothing written ({verdict})");
        return Ok(());
    }
    if !out.trusted {
        bail!(
            "confidence {:.2} is below --min-confidence {:.2}; nothing written. \
             Try another --audio-track, a subtitle as --ref, or lower --min-confidence.",
            out.alignment.confidence,
            args.min_confidence
        );
    }
    if let Some(output) = &args.output {
        println!("Output file : {}", output.display());
    }
    Ok(())
}

async fn run_folder(args: &SyncArgs) -> Result<()> {
    if !args.reference.is_dir() {
        bail!(
            "--ref must be a folder when INPUT is a folder: {}",
            args.reference.display()
        );
    }
    let exts = batch::parse_ext_list(&args.ext);
    let files = batch::collect_subs(&args.input, &exts, args.recursive)?;
    if files.is_empty() {
        bail!("No subtitle files found in {}", args.input.display());
    }
    let references = collect_references(&args.reference, args.recursive);
    if references.is_empty() {
        bail!(
            "No video, audio or subtitle files found in {}",
            args.reference.display()
        );
    }

    let (mut retimed, mut in_sync, mut low, mut unpaired, mut failed) = (0, 0, 0, 0, 0);
    for f in &files {
        let rel = f.strip_prefix(&args.input).unwrap_or(f);
        let Some(reference) = episode_key(f).and_then(|k| references.get(&k)) else {
            unpaired += 1;
            println!("[NO REFERENCE] {}", rel.display());
            continue;
        };
        let dest = args.output.as_ref().map(|o| o.join(rel));
        match sync_one(f, dest.as_deref(), reference, args).await {
            Ok(out) if !out.trusted => {
                low += 1;
                println!(
                    "[SKIPPED] {} -> confidence {:.2} < {:.2} (offset would be {})",
                    rel.display(),
                    out.alignment.confidence,
                    args.min_confidence,
                    offset_text(out.alignment.shift_ms)
                );
            }
            Ok(out) => {
                if out.alignment.shift_ms == 0 {
                    in_sync += 1;
                } else {
                    retimed += 1;
                }
                println!(
                    "[OK] {} -> {} (confidence {:.2})",
                    rel.display(),
                    offset_text(out.alignment.shift_ms),
                    out.alignment.confidence
                );
            }
            Err(e) => {
                failed += 1;
                println!("[FAILED] {} -> {e:#}", rel.display());
            }
        }
    }
    println!("--------------------------------------------------");
    println!("Total files    : {}", files.len());
    println!(
        "Retimed        : {retimed}{}",
        if args.dry_run { " (dry run)" } else { "" }
    );
    println!("Already in sync: {in_sync}");
    println!("Low confidence : {low} (not written)");
    println!("No reference   : {unpaired}");
    println!("Failed         : {failed}");
    if retimed + in_sync == 0 {
        bail!("No file could be retimed.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn offsets_read_naturally() {
        assert_eq!(offset_text(0), "0 ms (already in sync)");
        assert_eq!(offset_text(1230), "+1230 ms (delayed)");
        assert_eq!(offset_text(-500), "-500 ms (advanced)");
    }

    #[test]
    fn references_pair_by_episode_and_prefer_video() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "Show - 01.mkv",
            "Show - 01.ass", // same episode: the video wins
            "Show - 02.ass",
            "notes.txt",
            "Show - 03.mp4",
        ] {
            touch(&dir.path().join(name));
        }
        let refs = collect_references(dir.path(), false);
        let name = |k: &str| refs[k].file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(name("01"), "Show - 01.mkv");
        assert_eq!(name("02"), "Show - 02.ass");
        assert_eq!(name("03"), "Show - 03.mp4");
        assert_eq!(refs.len(), 3);
    }
}
