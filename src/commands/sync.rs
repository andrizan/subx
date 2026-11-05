use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::common::episode::EpisodeId;
use crate::common::tools::{self, Tool};
use crate::media::{mkv, speech};
use crate::subs::sync::{self, Alignment, Onsets};
use crate::subs::{batch, shift, supported_exts};

/// Video and audio containers ffmpeg can listen to.
const MEDIA_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "mov", "webm", "avi", "ts", "m2ts", "mka", "m4a", "aac", "flac", "opus",
    "ogg", "mp3", "wav",
];

/// `subx sync <VIDEO>` retimes the subtitles inside a video against its own
/// audio; `subx sync <SUBTITLE> <VIDEO>` retimes an external subtitle.
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// What to retime: an .mkv video (its own subtitle tracks), a subtitle
    /// file, or a folder of either.
    pub input: PathBuf,

    /// The video (or audio file, or already-timed subtitle) the subtitle
    /// belongs to; a folder pairs files by episode number. Required for a
    /// subtitle INPUT; a video INPUT listens to its own audio unless given another.
    pub reference: Option<PathBuf>,

    /// Where to write (a folder when INPUT is a folder). Default: a
    /// `<name>.synced.<ext>` file next to each input.
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Overwrite INPUT instead of writing a `.synced` copy.
    #[arg(long, conflicts_with = "output")]
    pub in_place: bool,

    /// Audio stream to listen to (0 = first).
    #[arg(long, default_value_t = 0)]
    pub audio_track: u32,

    /// Only this subtitle track (0 = first) of a video INPUT; default: every
    /// text track.
    #[arg(long)]
    pub sub_track: Option<u32>,

    /// Largest offset to search for, in seconds (either direction).
    #[arg(long, default_value_t = 120.0)]
    pub max_shift: f64,

    /// Leave alone whatever has an offset confidence (0..1) lower than this;
    /// 0 always retimes.
    #[arg(long, default_value_t = 0.2)]
    pub min_confidence: f64,

    /// Report the offsets without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Recurse into subfolders (folder INPUT).
    #[arg(short, long)]
    pub recursive: bool,

    /// Subtitle extensions to process (folder of subtitles).
    #[arg(long, default_value = ".srt,.ass,.ssa,.vtt")]
    pub ext: String,
}

fn has_ext(path: &Path, wanted: impl Fn(&str) -> bool) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| wanted(&e.to_lowercase()))
}

fn is_subtitle(path: &Path) -> bool {
    has_ext(path, |e| supported_exts().contains(&e))
}

/// Videos whose embedded subtitles can be retimed (mkvmerge `--sync` rewrites
/// Matroska; other containers would change format).
fn is_video(path: &Path) -> bool {
    has_ext(path, |e| e == "mkv")
}

/// A copy `sync` wrote earlier (`Ep01.synced.mkv`); never read back as input.
fn is_synced(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.ends_with(".synced"))
}

fn episode_key(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(EpisodeId::from_file_name)
        .map(|id| id.key())
}

/// `Ep01.en.srt` -> `Ep01.en.synced.srt`, in the same folder; `ext` overrides
/// the extension (`mkv` for any video).
fn synced_path(input: &Path, ext: Option<&str>) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = ext
        .map(str::to_string)
        .or_else(|| input.extension().map(|e| e.to_string_lossy().into_owned()))
        .unwrap_or_default();
    input.with_file_name(format!("{stem}.synced.{ext}"))
}

/// Where the result for `input` goes. `rel` is its path inside a folder INPUT
/// (None for a single file); `ext` forces the extension of the written file.
fn destination(args: &SyncArgs, input: &Path, rel: Option<&Path>, ext: Option<&str>) -> PathBuf {
    if args.in_place {
        return input.to_path_buf();
    }
    match (&args.output, rel) {
        (Some(out), Some(rel)) => {
            let path = out.join(rel);
            match ext {
                Some(ext) => path.with_extension(ext),
                None => path,
            }
        }
        (Some(out), None) => out.clone(),
        (None, _) => synced_path(input, ext),
    }
}

fn offset_text(ms: i64) -> String {
    let sign = if ms > 0 { "+" } else { "" };
    if ms.abs() <= sync::IN_SYNC_MS {
        format!("{sign}{ms} ms (already in sync)")
    } else if ms > 0 {
        format!("+{ms} ms (delayed)")
    } else {
        format!("{ms} ms (advanced)")
    }
}

fn quality_text(a: &Alignment) -> String {
    format!(
        "{:.2} (peak {:.1} sigma, {} cue starts)",
        a.confidence, a.peak_sigma, a.cues
    )
}

fn reference_text(path: &Path, audio_track: u32) -> String {
    if is_subtitle(path) {
        "subtitle cues".to_string()
    } else {
        format!("speech in audio track {audio_track}")
    }
}

fn max_shift_ms(args: &SyncArgs) -> i64 {
    (args.max_shift * 1000.0).round() as i64
}

/// The reference signal: cue starts of a subtitle, else speech onsets in the audio.
async fn reference_onsets(path: &Path, audio_track: u32) -> Result<Onsets> {
    if is_subtitle(path) {
        let file = batch::load_any(path)?;
        return Ok(Onsets::from_starts(&sync::cue_starts(&file.events)));
    }
    let ffmpeg = tools::require_one(Tool::Ffmpeg)?;
    Ok(Onsets::from_strength(
        speech::detect(&ffmpeg, path, audio_track).await?,
    ))
}

struct Outcome {
    alignment: Alignment,
    /// Offset was trusted.
    trusted: bool,
    /// A file was written.
    written: bool,
}

/// Align one external subtitle with its reference and write `dest` when the
/// offset is trusted. A subtitle that is already in sync is only copied when
/// an explicit `--output` asked for the file.
async fn sync_one(input: &Path, dest: &Path, reference: &Path, args: &SyncArgs) -> Result<Outcome> {
    // Fully loaded before writing, so in-place runs (dest == input) are safe.
    let mut file = batch::load_any(input)?;
    let starts = sync::cue_starts(&file.events);
    let reference = reference_onsets(reference, args.audio_track)
        .await
        .with_context(|| format!("cannot read the reference {}", reference.display()))?;

    let alignment = sync::align(&reference, &starts, max_shift_ms(args))?;
    let trusted = alignment.confidence >= args.min_confidence;
    let write = trusted
        && !args.dry_run
        && (!alignment.in_sync() || (args.output.is_some() && !args.in_place));
    if write {
        if !alignment.in_sync() {
            shift::shift_events(&mut file.events, alignment.shift_ms as i32);
        }
        batch::save_any(&file, dest)?;
    }
    Ok(Outcome {
        alignment,
        trusted,
        written: write,
    })
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
        .filter(|p| p.is_file() && (is_subtitle(p) || has_ext(p, |e| MEDIA_EXTS.contains(&e))))
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

/// `.mkv` files of a folder with their path relative to it, sorted.
fn collect_videos(dir: &Path, recursive: bool) -> Vec<(PathBuf, Option<PathBuf>)> {
    let depth = if recursive { usize::MAX } else { 1 };
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .min_depth(1)
        .max_depth(depth)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .filter(|p| p.is_file() && is_video(p) && !is_synced(p))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let rel = p.strip_prefix(dir).ok().map(Path::to_path_buf);
            (p, rel)
        })
        .collect()
}

pub async fn run(args: SyncArgs) -> Result<()> {
    if !(args.max_shift > 0.0 && args.max_shift <= 3600.0) {
        bail!("--max-shift must be between 0 and 3600 seconds.");
    }
    if !(0.0..=1.0).contains(&args.min_confidence) {
        bail!("--min-confidence must be between 0 and 1.");
    }
    if args.input.is_file() {
        if is_subtitle(&args.input) {
            return run_file(&args).await;
        }
        if is_video(&args.input) {
            return run_videos(&args, vec![(args.input.clone(), None)]).await;
        }
        bail!(
            "INPUT must be a subtitle file or an .mkv video: {}",
            args.input.display()
        );
    }
    if !args.input.is_dir() {
        bail!("Input not found: {}", args.input.display());
    }
    if args.reference.is_some() {
        return run_folder(&args).await;
    }
    let videos = collect_videos(&args.input, args.recursive);
    if videos.is_empty() {
        bail!(
            "No .mkv files found in {}. For a folder of subtitles, give the folder of videos as the second argument.",
            args.input.display()
        );
    }
    run_videos(&args, videos).await
}

/// One external subtitle file with its video.
async fn run_file(args: &SyncArgs) -> Result<()> {
    let Some(reference) = args.reference.as_deref() else {
        bail!("A subtitle needs the video it belongs to: subx sync <SUBTITLE> <VIDEO>");
    };
    if !reference.is_file() {
        bail!(
            "The second argument must be a file when INPUT is a file: {}",
            reference.display()
        );
    }
    let dest = destination(args, &args.input, None, None);
    let out = sync_one(&args.input, &dest, reference, args)
        .await
        .with_context(|| format!("failed on {}", args.input.display()))?;

    println!("Input file  : {}", args.input.display());
    println!(
        "Reference   : {} ({})",
        reference.display(),
        reference_text(reference, args.audio_track)
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
             Try another --audio-track, a subtitle as the reference, or lower --min-confidence.",
            out.alignment.confidence,
            args.min_confidence
        );
    }
    if out.written {
        println!("Output file : {}", dest.display());
    } else {
        println!("Output file : (none, already in sync)");
    }
    Ok(())
}

/// A folder of external subtitles with a folder of videos.
async fn run_folder(args: &SyncArgs) -> Result<()> {
    let reference_dir = args.reference.as_deref().unwrap_or(&args.input);
    if !reference_dir.is_dir() {
        bail!(
            "The second argument must be a folder when INPUT is a folder: {}",
            reference_dir.display()
        );
    }
    let exts = batch::parse_ext_list(&args.ext);
    let files: Vec<PathBuf> = batch::collect_subs(&args.input, &exts, args.recursive)?
        .into_iter()
        .filter(|f| !is_synced(f))
        .collect();
    if files.is_empty() {
        bail!("No subtitle files found in {}", args.input.display());
    }
    let references = collect_references(reference_dir, args.recursive);
    if references.is_empty() {
        bail!(
            "No video, audio or subtitle files found in {}",
            reference_dir.display()
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
        let dest = destination(args, f, Some(rel), None);
        match sync_one(f, &dest, reference, args).await {
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
                if out.alignment.in_sync() {
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

/// The programs a video needs: ffmpeg to listen and to pull tracks out,
/// mkvmerge to read the tracks and to write the result.
struct VideoTools {
    ffmpeg: PathBuf,
    mkvmerge: PathBuf,
}

/// Scratch folder for extracted tracks, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("subx-sync-{}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What happened to the subtitle tracks of one video.
#[derive(Default)]
struct VideoOutcome {
    /// Subtitle tracks looked at.
    found: usize,
    /// One printable line per track.
    lines: Vec<String>,
    /// Tracks with a trusted, non-zero offset.
    retimed: usize,
    in_sync: usize,
    low: usize,
    /// Bitmap or unreadable tracks, left as they are.
    skipped: usize,
    written: bool,
}

impl VideoOutcome {
    fn status(&self, dry_run: bool) -> &'static str {
        if self.found == 0 {
            "NO SUBTITLES"
        } else if self.retimed > 0 {
            if dry_run { "DRY RUN" } else { "OK" }
        } else if self.in_sync > 0 {
            "IN SYNC"
        } else {
            "SKIPPED"
        }
    }
}

/// Pull one text subtitle track out of `video` into `dest`.
async fn extract_text_track(
    ffmpeg: &Path,
    video: &Path,
    ordinal: u32,
    ext: &str,
    dest: &Path,
) -> Result<()> {
    let codec = if ext == "ass" { "ass" } else { "srt" };
    let out = tokio::process::Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(video)
        .args(["-map", &format!("0:s:{ordinal}"), "-c:s", codec])
        .arg(dest)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("cannot run {}", ffmpeg.display()))?;
    if !out.status.success() || !dest.is_file() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "ffmpeg could not extract subtitle track {ordinal}: {}",
            err.trim().lines().last().unwrap_or("no output")
        );
    }
    Ok(())
}

/// Measure every text subtitle track of `video` against the reference audio
/// and, unless `--dry-run`, write `dest` with those tracks delayed.
async fn sync_video(
    video: &Path,
    dest: &Path,
    args: &SyncArgs,
    bins: &VideoTools,
) -> Result<VideoOutcome> {
    let info = mkv::info(&bins.mkvmerge, video).await?;
    let tracks: Vec<mkv::SubTrack> = mkv::subtitle_tracks(&info)
        .into_iter()
        .filter(|t| args.sub_track.is_none_or(|n| n == t.ordinal))
        .collect();
    let mut out = VideoOutcome {
        found: tracks.len(),
        ..Default::default()
    };
    if !tracks.iter().any(|t| t.text_ext().is_some()) {
        for t in &tracks {
            out.skipped += 1;
            out.lines.push(format!(
                "  S{} {} | image subtitle, cannot be measured; left as it is",
                t.ordinal,
                t.label()
            ));
        }
        return Ok(out);
    }

    // The audio is decoded once and every track is measured against it.
    let reference_path = args.reference.as_deref().unwrap_or(video);
    let reference = reference_onsets(reference_path, args.audio_track)
        .await
        .with_context(|| format!("cannot read the reference {}", reference_path.display()))?;
    let scratch = Scratch::new()?;
    let mut delays: Vec<(u64, i64)> = Vec::new();

    for t in &tracks {
        let head = format!("  S{} {}", t.ordinal, t.label());
        let Some(ext) = t.text_ext() else {
            out.skipped += 1;
            out.lines.push(format!(
                "{head} | image subtitle, cannot be measured; left as it is"
            ));
            continue;
        };
        let file = scratch.0.join(format!("track{}.{ext}", t.ordinal));
        let measured = async {
            extract_text_track(&bins.ffmpeg, video, t.ordinal, ext, &file).await?;
            let subs = batch::load_any(&file)?;
            sync::align(
                &reference,
                &sync::cue_starts(&subs.events),
                max_shift_ms(args),
            )
        }
        .await;
        match measured {
            Err(e) => {
                out.skipped += 1;
                out.lines
                    .push(format!("{head} | skipped, left as it is: {e:#}"));
            }
            Ok(a) if a.confidence < args.min_confidence => {
                out.low += 1;
                out.lines.push(format!(
                    "{head} | skipped, confidence {:.2} < {:.2} (offset would be {})",
                    a.confidence,
                    args.min_confidence,
                    offset_text(a.shift_ms)
                ));
            }
            Ok(a) => {
                if a.in_sync() {
                    out.in_sync += 1;
                } else {
                    out.retimed += 1;
                    delays.push((t.id, a.shift_ms));
                }
                out.lines.push(format!(
                    "{head} | {} | confidence {}",
                    offset_text(a.shift_ms),
                    quality_text(&a)
                ));
            }
        }
    }

    if args.dry_run || delays.is_empty() {
        return Ok(out);
    }
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    // Written beside the destination first, so an interrupted run never
    // leaves a half file (or a broken original when overwriting in place).
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if let Err(e) = mkv::retime(&bins.mkvmerge, video, &tmp, &delays).await {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, dest).with_context(|| format!("cannot write {}", dest.display()))?;
    out.written = true;
    Ok(out)
}

/// Videos whose own subtitle tracks are retimed (a folder run or a single file).
async fn run_videos(args: &SyncArgs, videos: Vec<(PathBuf, Option<PathBuf>)>) -> Result<()> {
    let paths = tools::require(&[Tool::Ffmpeg, Tool::Mkvmerge])?;
    let bins = VideoTools {
        ffmpeg: paths[0].clone(),
        mkvmerge: paths[1].clone(),
    };
    let many = videos.len() > 1;
    let (mut retimed, mut in_sync, mut skipped, mut no_subs, mut failed) = (0, 0, 0, 0, 0);
    let mut last_error = None;

    for (video, rel) in &videos {
        let dest = destination(args, video, rel.as_deref(), Some("mkv"));
        let shown = rel.as_deref().unwrap_or(video).display();
        if !many {
            println!("Input file  : {}", video.display());
            let reference = args.reference.as_deref().unwrap_or(video);
            println!(
                "Reference   : {} ({})",
                reference.display(),
                reference_text(reference, args.audio_track)
            );
        }
        match sync_video(video, &dest, args, &bins).await {
            Ok(o) => {
                let status = o.status(args.dry_run);
                match status {
                    "OK" | "DRY RUN" => retimed += 1,
                    "IN SYNC" => in_sync += 1,
                    "NO SUBTITLES" => no_subs += 1,
                    _ => skipped += 1,
                }
                println!("[{status}] {shown}");
                for line in &o.lines {
                    println!("{line}");
                }
                if o.written {
                    println!("  -> {}", dest.display());
                }
                if !many && matches!(status, "NO SUBTITLES" | "SKIPPED") {
                    last_error = Some(if status == "SKIPPED" {
                        anyhow::anyhow!(
                            "no subtitle track could be retimed with enough confidence; nothing written. \
                             Try another --audio-track or lower --min-confidence."
                        )
                    } else {
                        anyhow::anyhow!("{} has no subtitle track", video.display())
                    });
                }
            }
            Err(e) => {
                failed += 1;
                println!("[FAILED] {shown} -> {e:#}");
                last_error = Some(e);
            }
        }
    }

    if !many {
        return last_error.map_or(Ok(()), Err);
    }
    println!("--------------------------------------------------");
    println!("Total videos   : {}", videos.len());
    println!(
        "Retimed        : {retimed}{}",
        if args.dry_run { " (dry run)" } else { "" }
    );
    println!("Already in sync: {in_sync}");
    println!("Skipped        : {skipped} (low confidence or unreadable tracks)");
    println!("No subtitles   : {no_subs}");
    println!("Failed         : {failed}");
    if retimed + in_sync == 0 {
        bail!("No video could be retimed.");
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

    fn args(input: &str) -> SyncArgs {
        SyncArgs {
            input: PathBuf::from(input),
            reference: None,
            output: None,
            in_place: false,
            audio_track: 0,
            sub_track: None,
            max_shift: 120.0,
            min_confidence: 0.2,
            dry_run: false,
            recursive: false,
            ext: ".srt,.ass,.ssa,.vtt".to_string(),
        }
    }

    #[test]
    fn offsets_read_naturally() {
        assert_eq!(offset_text(0), "0 ms (already in sync)");
        assert_eq!(offset_text(10), "+10 ms (already in sync)");
        assert_eq!(offset_text(-30), "-30 ms (already in sync)");
        assert_eq!(offset_text(1230), "+1230 ms (delayed)");
        assert_eq!(offset_text(-500), "-500 ms (advanced)");
    }

    #[test]
    fn results_default_to_a_synced_copy_beside_the_input() {
        let a = args("Ep01.mkv");
        assert_eq!(
            destination(&a, Path::new("D:/v/Ep01.mkv"), None, Some("mkv")),
            PathBuf::from("D:/v/Ep01.synced.mkv")
        );
        assert_eq!(
            destination(&a, Path::new("subs/Ep01.en.srt"), None, None),
            PathBuf::from("subs/Ep01.en.synced.srt")
        );
    }

    #[test]
    fn output_and_in_place_override_the_default() {
        let mut a = args("Ep01.mkv");
        a.in_place = true;
        assert_eq!(
            destination(&a, Path::new("D:/v/Ep01.mkv"), None, Some("mkv")),
            PathBuf::from("D:/v/Ep01.mkv")
        );

        let mut a = args("Season1");
        a.output = Some(PathBuf::from("out"));
        // A file INPUT writes exactly there; a folder INPUT mirrors its layout.
        assert_eq!(
            destination(&a, Path::new("Season1/Ep01.mkv"), None, Some("mkv")),
            PathBuf::from("out")
        );
        assert_eq!(
            destination(
                &a,
                Path::new("Season1/sub/Ep01.mkv"),
                Some(Path::new("sub/Ep01.mkv")),
                Some("mkv")
            ),
            PathBuf::from("out/sub/Ep01.mkv")
        );
    }

    #[test]
    fn written_copies_are_never_read_back() {
        assert!(is_synced(Path::new("Ep01.synced.mkv")));
        assert!(is_synced(Path::new("Ep01.en.synced.srt")));
        assert!(!is_synced(Path::new("Ep01.mkv")));
        assert!(!is_synced(Path::new("synced.mkv")));
    }

    #[test]
    fn only_mkv_counts_as_a_video_input() {
        assert!(is_video(Path::new("a.MKV")));
        assert!(!is_video(Path::new("a.mp4")));
        assert!(!is_video(Path::new("a.srt")));
        assert!(is_subtitle(Path::new("a.ASS")));
    }

    #[test]
    fn videos_are_collected_without_our_own_output() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "Show - 01.mkv",
            "Show - 01.synced.mkv",
            "Show - 02.mkv",
            "Show - 03.mp4",
            "sub/Show - 04.mkv",
        ] {
            touch(&dir.path().join(name));
        }
        let names = |recursive| -> Vec<String> {
            collect_videos(dir.path(), recursive)
                .into_iter()
                .map(|(_, rel)| rel.unwrap().to_string_lossy().replace('\\', "/"))
                .collect()
        };
        assert_eq!(names(false), ["Show - 01.mkv", "Show - 02.mkv"]);
        assert_eq!(
            names(true),
            ["Show - 01.mkv", "Show - 02.mkv", "sub/Show - 04.mkv"]
        );
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

    #[test]
    fn video_status_reflects_the_tracks() {
        let o = |found, retimed, in_sync| VideoOutcome {
            found,
            retimed,
            in_sync,
            ..Default::default()
        };
        assert_eq!(o(0, 0, 0).status(false), "NO SUBTITLES");
        assert_eq!(o(2, 1, 1).status(false), "OK");
        assert_eq!(o(2, 1, 1).status(true), "DRY RUN");
        assert_eq!(o(2, 0, 1).status(false), "IN SYNC");
        assert_eq!(o(2, 0, 0).status(false), "SKIPPED");
    }

    /// `ms` as an SRT timestamp.
    fn srt_time(ms: i64) -> String {
        format!(
            "{:02}:{:02}:{:02},{:03}",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1000 % 60,
            ms % 1000
        )
    }

    fn srt(spans: &[(i64, i64)], early_ms: i64) -> String {
        spans
            .iter()
            .enumerate()
            .map(|(i, (a, b))| {
                format!(
                    "{}\n{} --> {}\nLine {}\n\n",
                    i + 1,
                    srt_time(a - early_ms),
                    srt_time(b - early_ms),
                    i + 1
                )
            })
            .collect()
    }

    /// First cue start of subtitle track `ordinal` of `mkv`, in ms.
    async fn first_cue(ffmpeg: &Path, mkv: &Path, ordinal: u32, dir: &Path) -> i64 {
        let file = dir.join(format!("check{ordinal}.srt"));
        extract_text_track(ffmpeg, mkv, ordinal, "srt", &file)
            .await
            .unwrap();
        batch::load_any(&file).unwrap().events[0].start_ms
    }

    /// A real mkv with two subtitle tracks (one 2.5 s early, one right), retimed
    /// through the CLI path. Needs ffmpeg and mkvmerge — run explicitly:
    /// `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs ffmpeg and mkvmerge"]
    async fn e2e_retimes_the_embedded_subtitle_of_an_mkv() {
        if tools::require(&[Tool::Mkvmerge]).is_err() {
            eprintln!("MKVToolNix not found; skipping");
            return;
        }
        let (ffmpeg, _) = tools::ffmpeg_pair().expect("ffmpeg missing");
        let dir = tempfile::tempdir().unwrap();

        // Dialogue-like speech spans (ms) over eight minutes, from a fixed seed:
        // a clip much shorter than an episode has too few cues to be trusted.
        let (mut state, mut t) = (0x9E37_79B9_7F4A_7C15u64, 3000i64);
        let mut next = move |n: i64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as i64
        };
        let mut spans: Vec<(i64, i64)> = Vec::new();
        while t < 8 * 60 * 1000 {
            let len = 1000 + next(3000);
            spans.push((t, t + len));
            t += len + 500 + next(5500);
        }
        let rate = 16_000usize;
        let samples: Vec<i16> = (0..rate * 8 * 60)
            .map(|i| {
                let t = i as f64 / rate as f64;
                let on = spans
                    .iter()
                    .any(|&(a, b)| t * 1000.0 >= a as f64 && t * 1000.0 < b as f64);
                let amp = if on { 12000.0 } else { 30.0 };
                let wave = (t * 440.0 * std::f64::consts::TAU).sin()
                    + 0.5 * (t * 1100.0 * std::f64::consts::TAU).sin();
                (amp * wave) as i16
            })
            .collect();
        let wav = dir.path().join("a.wav");
        std::fs::write(&wav, speech::wav_bytes(rate as u32, &samples)).unwrap();
        let (early, right) = (dir.path().join("early.srt"), dir.path().join("right.srt"));
        std::fs::write(&early, srt(&spans, 2500)).unwrap();
        std::fs::write(&right, srt(&spans, 0)).unwrap();

        let video = dir.path().join("Ep01.mkv");
        let made = std::process::Command::new(&ffmpeg)
            .args(["-v", "error", "-y", "-i"])
            .arg(&wav)
            .arg("-i")
            .arg(&early)
            .arg("-i")
            .arg(&right)
            .args(["-map", "0:a", "-map", "1:s", "-map", "2:s"])
            .args(["-c:a", "libopus", "-c:s", "srt"])
            .arg(&video)
            .status()
            .unwrap();
        assert!(made.success());
        // The muxer may nudge the whole timeline by a few ms (Opus priming), so
        // everything below is compared with what the file held before.
        let before = [
            first_cue(&ffmpeg, &video, 0, dir.path()).await,
            first_cue(&ffmpeg, &video, 1, dir.path()).await,
        ];
        let first = spans[0].0;
        assert!((before[0] - (first - 2500)).abs() <= 40 && (before[1] - first).abs() <= 40);

        let mut a = args(video.to_str().unwrap());
        a.max_shift = 10.0;
        run(a).await.unwrap();

        // Default output: a synced copy beside the input; the original is untouched.
        let synced = dir.path().join("Ep01.synced.mkv");
        assert!(synced.is_file());
        assert_eq!(first_cue(&ffmpeg, &video, 0, dir.path()).await, before[0]);
        let fixed = first_cue(&ffmpeg, &synced, 0, dir.path()).await;
        assert!(
            (fixed - (before[0] + 2500)).abs() <= 60,
            "retimed track starts at {fixed}, was {}",
            before[0]
        );
        // The track that was already right is not touched.
        assert_eq!(first_cue(&ffmpeg, &synced, 1, dir.path()).await, before[1]);

        // Running again on the result has nothing left to do and writes nothing.
        let mut again = args(synced.to_str().unwrap());
        again.max_shift = 10.0;
        again.dry_run = true;
        run(again).await.unwrap();
    }
}
