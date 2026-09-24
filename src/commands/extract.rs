use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Args;
use tokio::sync::Semaphore;

use crate::common::{ffmpeg_find, fs, lang};
use crate::media::probe;

/// `subx extract <FOLDER>` — extract subtitles (and optionally audio).
#[derive(Debug, Args)]
pub struct ExtractArgs {
    /// Folder containing video files.
    pub folder: PathBuf,

    /// Output folder (default: `<FOLDER>/subs`).
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Parallel worker count.
    #[arg(short = 'j', long, default_value_t = 8)]
    pub jobs: usize,

    /// Video extensions to process (comma-separated).
    #[arg(long, default_value = "mkv,mp4,mka")]
    pub ext: String,

    /// Overwrite output files if they exist.
    #[arg(long)]
    pub overwrite: bool,

    /// Also extract audio tracks (stream copy, no re-encode).
    #[arg(long)]
    pub audio: bool,

    /// Audio output folder (default: `<FOLDER>/audio`).
    #[arg(long)]
    pub audio_out: Option<PathBuf>,
}

/// One subtitle or audio track picked from ffprobe output.
#[derive(Debug, Clone)]
struct SubTrack {
    index: i32,
    lang: String,
    codec: String,
    title: String,
}

/// Image-based subtitle codecs that cannot convert to text.
/// They are copied untouched with a `.sup` extension.
const BITMAP_CODECS: &[&str] = &[
    "hdmv_pgs_subtitle",
    "pgs",
    "dvd_subtitle",
    "dvdsub",
    "dvb_subtitle",
    "xsub",
];

/// Running totals shared between worker tasks.
#[derive(Debug, Default)]
struct Stats {
    total_files: usize,
    files_with_subs: usize,
    total_subs: usize,
    failed_subs: usize,
    skipped_tracks: usize,
    total_audio: usize,
    failed_audio: usize,
    skipped_audio: usize,
    lang_count: HashMap<String, usize>,
}

/// Parse `--ext "mkv,mp4,.mka"` into lowercase extensions without dots.
fn parse_exts(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Output extension for a subtitle codec. Bitmap subs are copied as `.sup`.
fn ext_for_codec(codec: &str) -> &'static str {
    let c = codec.to_lowercase();
    if c == "ass" || c == "ssa" {
        "ass"
    } else if c == "vtt" || c == "webvtt" {
        "vtt"
    } else if BITMAP_CODECS.contains(&c.as_str()) {
        "sup"
    } else {
        "srt"
    }
}

/// Container extension for an audio codec. Stream copy always lands safely
/// in Matroska, so unknown codecs fall back to `.mka`.
fn audio_ext(codec: &str) -> &'static str {
    match codec.to_lowercase().as_str() {
        "aac" | "alac" => "m4a",
        "mp3" => "mp3",
        "opus" => "opus",
        "vorbis" => "ogg",
        "flac" => "flac",
        "ac3" => "ac3",
        "eac3" => "eac3",
        "dts" => "dts",
        "truehd" => "thd",
        "pcm_s16le" | "pcm_s24le" | "pcm_s32le" | "pcm_f32le" | "pcm_u8" => "wav",
        _ => "mka",
    }
}

/// Build `<safe>.<kind><index>[.<title>]`, title capped at 50 chars, total at 150.
/// `kind` is `S` for subtitles, `A` for audio.
fn output_stem(safe: &str, kind: char, index: i32, title: &str) -> String {
    let stem = if title.trim().is_empty() {
        format!("{safe}.{kind}{index}")
    } else {
        let clean: String = fs::sanitize_filename(title).chars().take(50).collect();
        if clean.is_empty() {
            format!("{safe}.{kind}{index}")
        } else {
            format!("{safe}.{kind}{index}.{clean}")
        }
    };
    if stem.chars().count() > 150 {
        stem.chars().take(150).collect()
    } else {
        stem
    }
}

/// Sort subtitle tracks: priority language (`id`) first, then stream index.
fn sort_tracks(tracks: &mut [SubTrack]) {
    tracks.sort_by(|a, b| {
        (a.lang != "id")
            .cmp(&(b.lang != "id"))
            .then(a.index.cmp(&b.index))
    });
}

fn bump_failed(stats: &Arc<Mutex<Stats>>) {
    stats.lock().expect("stats lock poisoned").failed_subs += 1;
}

/// Run `ffmpeg -map 0:{index}` (+ optional extra args) and verify the output
/// exists and is non-empty. Returns the failure reason on error.
async fn extract_track(
    ffmpeg: &Path,
    file: &Path,
    index: i32,
    extra: &[&str],
    out_file: &Path,
) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-y", "-v", "error", "-i"]).arg(file);
    cmd.args(["-map", &format!("0:{index}")]);
    cmd.args(extra);
    cmd.arg(out_file);
    let status = cmd.status().await.map_err(|e| format!("spawn({e})"))?;
    if !status.success() {
        let _ = std::fs::remove_file(out_file);
        return Err(format!("ffmpeg(exit={})", status.code().unwrap_or(-1)));
    }
    match std::fs::metadata(out_file) {
        Ok(m) if m.len() > 0 => Ok(()),
        _ => {
            let _ = std::fs::remove_file(out_file);
            Err("empty_output".to_string())
        }
    }
}

pub async fn run(args: ExtractArgs) -> Result<()> {
    let (ffmpeg, ffprobe) = ffmpeg_find::find_ffmpeg()?;

    if !args.folder.is_dir() {
        anyhow::bail!("Folder not found: {}", args.folder.display());
    }
    let out_base = args.out.clone().unwrap_or_else(|| args.folder.join("subs"));
    std::fs::create_dir_all(&out_base)
        .with_context(|| format!("cannot create {}", out_base.display()))?;
    let audio_base = if args.audio {
        let dir = args
            .audio_out
            .clone()
            .unwrap_or_else(|| args.folder.join("audio"));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        Some(dir)
    } else {
        None
    };

    let exts = parse_exts(&args.ext);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&args.folder)
        .with_context(|| format!("cannot read {}", args.folder.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| exts.contains(&e.to_lowercase()))
                    .unwrap_or(false)
        })
        .collect();
    files.sort();

    if files.is_empty() {
        anyhow::bail!(
            "No video files (*{{{}}}) found in {}",
            exts.join(","),
            args.folder.display()
        );
    }

    let jobs = args.jobs.clamp(1, 32).min(files.len());
    println!(
        "Found {} video file(s). Extracting with {} worker(s)...\n",
        files.len(),
        jobs
    );

    let sem = Arc::new(Semaphore::new(jobs));
    let stats = Arc::new(Mutex::new(Stats::default()));
    let bar = crate::common::progress::bar(files.len() as u64, "Extracting");

    // Tasks return their status lines; printing happens sequentially below
    // so output stays in input order instead of completion order.
    let mut handles = Vec::with_capacity(files.len());
    for file in files {
        let sem = Arc::clone(&sem);
        let stats = Arc::clone(&stats);
        let bar = bar.clone();
        let ffmpeg = ffmpeg.clone();
        let ffprobe = ffprobe.clone();
        let out_base = out_base.clone();
        let audio_base = audio_base.clone();
        let overwrite = args.overwrite;
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore closed");
            let line = process_file(
                &ffmpeg,
                &ffprobe,
                &file,
                &out_base,
                audio_base.as_deref(),
                overwrite,
                &stats,
            )
            .await;
            bar.inc(1);
            line
        }));
    }
    let mut lines = Vec::with_capacity(handles.len());
    for h in handles {
        lines.push(h.await.expect("extract worker panicked"));
    }
    bar.finish_and_clear();
    for line in lines {
        println!("{line}");
    }

    let stats = stats.lock().expect("stats lock poisoned");
    print_stats(&stats, &out_base, audio_base.as_deref());

    if stats.total_subs == 0
        && stats.total_audio == 0
        && stats.skipped_tracks == 0
        && stats.skipped_audio == 0
    {
        anyhow::bail!("Nothing was extracted.");
    }
    Ok(())
}

async fn process_file(
    ffmpeg: &Path,
    ffprobe: &Path,
    file: &Path,
    out_base: &Path,
    audio_base: Option<&Path>,
    overwrite: bool,
    stats: &Arc<Mutex<Stats>>,
) -> String {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    stats.lock().expect("stats lock poisoned").total_files += 1;

    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.clone());
    let safe = fs::sanitize_filename(&stem);

    let streams = match probe::probe_streams(ffprobe, file).await {
        Ok(s) => s,
        Err(e) => return format!("{name} FAILED: {e:#}"),
    };
    let track_of = |s: &probe::Stream| SubTrack {
        index: s.index,
        lang: lang::normalize_language(s.language_raw()),
        codec: s.codec_name.clone().unwrap_or_default(),
        title: s.title_raw().to_string(),
    };

    let mut tracks: Vec<SubTrack> = streams
        .iter()
        .filter(|s| s.is_subtitle())
        .map(&track_of)
        .collect();

    if tracks.is_empty() && audio_base.is_none() {
        return format!("{name} -- no subtitles found.");
    }
    if !tracks.is_empty() {
        stats.lock().expect("stats lock poisoned").files_with_subs += 1;
    }

    sort_tracks(&mut tracks);
    let mut debug: Vec<String> = tracks
        .iter()
        .map(|t| {
            if t.title.is_empty() {
                format!("S{}:'{}'[{}]", t.index, t.lang, t.codec)
            } else {
                format!("S{}:'{}'[{}] ({})", t.index, t.lang, t.codec, t.title)
            }
        })
        .collect();

    let mut extracted = 0usize;
    let mut problems: Vec<String> = Vec::new();

    for t in &tracks {
        let ext = ext_for_codec(&t.codec);
        let stem = output_stem(&safe, 'S', t.index, &t.title);
        let out_file = if t.lang == "id" {
            out_base.join(format!("{stem}.id.{ext}"))
        } else {
            let dir = out_base.join(&t.lang);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                problems.push(format!("S{}:mkdir({e})", t.index));
                bump_failed(stats);
                continue;
            }
            dir.join(format!("{stem}.{}.{ext}", t.lang))
        };

        if out_file.exists() && !overwrite {
            problems.push(format!("S{}:skipped(exists)", t.index));
            stats.lock().expect("stats lock poisoned").skipped_tracks += 1;
            continue;
        }

        match extract_track(ffmpeg, file, t.index, &[], &out_file).await {
            Ok(()) => {
                extracted += 1;
                let mut st = stats.lock().expect("stats lock poisoned");
                st.total_subs += 1;
                *st.lang_count.entry(t.lang.clone()).or_insert(0) += 1;
            }
            Err(reason) => {
                problems.push(format!("S{}:{reason}", t.index));
                bump_failed(stats);
            }
        }
    }

    let mut audio_done = 0usize;
    let mut audio_total = 0usize;
    if let Some(adir) = audio_base {
        let audio_tracks: Vec<SubTrack> = streams
            .iter()
            .filter(|s| s.codec_type.as_deref() == Some("audio"))
            .map(&track_of)
            .collect();
        audio_total = audio_tracks.len();
        for t in &audio_tracks {
            debug.push(format!("A{}:'{}'[{}]", t.index, t.lang, t.codec));
            let ext = audio_ext(&t.codec);
            let stem = output_stem(&safe, 'A', t.index, &t.title);
            let out_file = adir.join(format!("{stem}.{}.{ext}", t.lang));

            if out_file.exists() && !overwrite {
                problems.push(format!("A{}:skipped(exists)", t.index));
                stats.lock().expect("stats lock poisoned").skipped_audio += 1;
                continue;
            }

            match extract_track(ffmpeg, file, t.index, &["-c", "copy"], &out_file).await {
                Ok(()) => {
                    audio_done += 1;
                    stats.lock().expect("stats lock poisoned").total_audio += 1;
                }
                Err(reason) => {
                    problems.push(format!("A{}:{reason}", t.index));
                    stats.lock().expect("stats lock poisoned").failed_audio += 1;
                }
            }
        }
    }

    let mut line = format!("{name} OK {extracted}/{} subtitle(s)", tracks.len());
    if audio_base.is_some() {
        line.push_str(&format!(" + {audio_done}/{audio_total} audio"));
    }
    line.push_str(&format!(" [{}]", debug.join(" | ")));
    if !problems.is_empty() {
        line.push_str(&format!(" -- {}", problems.join(", ")));
    }
    line
}

fn print_stats(stats: &Stats, out_base: &Path, audio_base: Option<&Path>) {
    println!("\n========================================");
    println!("             FINAL STATISTICS");
    println!("========================================");
    println!("Total video files found   : {}", stats.total_files);
    println!("Files with subtitles      : {}", stats.files_with_subs);
    println!("Total subtitles extracted : {}", stats.total_subs);
    if stats.failed_subs > 0 {
        println!("Total subtitles FAILED    : {}", stats.failed_subs);
    }
    if stats.skipped_tracks > 0 {
        println!(
            "Skipped tracks (exists, use --overwrite): {}",
            stats.skipped_tracks
        );
    }
    if audio_base.is_some() {
        println!("Total audio extracted       : {}", stats.total_audio);
        if stats.failed_audio > 0 {
            println!("Total audio FAILED          : {}", stats.failed_audio);
        }
        if stats.skipped_audio > 0 {
            println!(
                "Skipped audio (exists, use --overwrite): {}",
                stats.skipped_audio
            );
        }
    }
    if !stats.lang_count.is_empty() {
        println!("\nPer-language breakdown:");
        let mut langs: Vec<_> = stats.lang_count.iter().collect();
        langs.sort_by(|a, b| (a.0 != "id").cmp(&(b.0 != "id")).then(a.0.cmp(b.0)));
        for (l, c) in langs {
            if l == "id" {
                println!("  - {l}: {c} subtitle(s) -> main folder");
            } else {
                println!("  - {l}: {c} subtitle(s) -> subfolder ({l}/)");
            }
        }
    }
    if stats.total_subs > 0 {
        println!("\nSubtitles saved in: {}", out_base.display());
    }
    if stats.total_audio > 0
        && let Some(adir) = audio_base
    {
        println!("Audio saved in: {}", adir.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ext_list() {
        assert_eq!(parse_exts("mkv,mp4,mka"), ["mkv", "mp4", "mka"]);
        assert_eq!(parse_exts("MKV, .mp4 ,"), ["mkv", "mp4"]);
        assert!(parse_exts("").is_empty());
    }

    #[test]
    fn maps_codec_to_extension() {
        assert_eq!(ext_for_codec("ass"), "ass");
        assert_eq!(ext_for_codec("ssa"), "ass");
        assert_eq!(ext_for_codec("subrip"), "srt");
        assert_eq!(ext_for_codec("mov_text"), "srt");
        assert_eq!(ext_for_codec("webvtt"), "vtt");
        assert_eq!(ext_for_codec("hdmv_pgs_subtitle"), "sup");
        assert_eq!(ext_for_codec("dvd_subtitle"), "sup");
        // Unknown codecs fall back to text extraction.
        assert_eq!(ext_for_codec("something_new"), "srt");
    }

    #[test]
    fn maps_audio_codec_to_container() {
        assert_eq!(audio_ext("aac"), "m4a");
        assert_eq!(audio_ext("alac"), "m4a");
        assert_eq!(audio_ext("mp3"), "mp3");
        assert_eq!(audio_ext("opus"), "opus");
        assert_eq!(audio_ext("vorbis"), "ogg");
        assert_eq!(audio_ext("flac"), "flac");
        assert_eq!(audio_ext("ac3"), "ac3");
        assert_eq!(audio_ext("eac3"), "eac3");
        assert_eq!(audio_ext("dts"), "dts");
        assert_eq!(audio_ext("pcm_s16le"), "wav");
        // Unknown codecs land safely in Matroska.
        assert_eq!(audio_ext("truehd"), "thd");
        assert_eq!(audio_ext("something_new"), "mka");
    }

    #[test]
    fn builds_output_stems() {
        assert_eq!(output_stem("Show_01", 'S', 2, ""), "Show_01.S2");
        assert_eq!(
            output_stem("Show_01", 'S', 2, "Forced"),
            "Show_01.S2.Forced"
        );
        assert_eq!(output_stem("Show_01", 'A', 4, ""), "Show_01.A4");
        // Titles that sanitize to nothing fall back to no title part.
        assert_eq!(output_stem("Show_01", 'S', 2, "..."), "Show_01.S2");
        // Long titles are capped at 50 chars.
        let long = "t".repeat(80);
        assert_eq!(
            output_stem("Show_01", 'S', 2, &long).chars().count(),
            "Show_01.S2.".chars().count() + 50
        );
    }

    #[test]
    fn sorts_id_first_then_index() {
        let mut tracks = vec![
            SubTrack {
                index: 3,
                lang: "en".into(),
                codec: "subrip".into(),
                title: String::new(),
            },
            SubTrack {
                index: 2,
                lang: "id".into(),
                codec: "subrip".into(),
                title: String::new(),
            },
            SubTrack {
                index: 1,
                lang: "unknown".into(),
                codec: "subrip".into(),
                title: String::new(),
            },
        ];
        sort_tracks(&mut tracks);
        let order: Vec<i32> = tracks.iter().map(|t| t.index).collect();
        assert_eq!(order, [2, 1, 3]);
    }

    /// Mux one episode with 3 text subtitle tracks (id/en/und) + 1 audio track.
    fn mux_episode(dir: &Path, name: &str) -> PathBuf {
        use std::process::Command;

        for (tag, text) in [("id", "Halo dunia"), ("en", "Hello world"), ("und", "???")] {
            std::fs::write(
                dir.join(format!("{name}.{tag}.srt")),
                format!("1\n00:00:00,000 --> 00:00:01,000\n{text}\n"),
            )
            .unwrap();
        }
        let mkv = dir.join(format!("{name}.mkv"));

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
                    dir.join(format!("{name}.id.srt"))
                        .to_str()
                        .expect("utf8 path"),
                    "-i",
                    dir.join(format!("{name}.en.srt"))
                        .to_str()
                        .expect("utf8 path"),
                    "-i",
                    dir.join(format!("{name}.und.srt"))
                        .to_str()
                        .expect("utf8 path"),
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=2",
                    "-map",
                    "0:v",
                    "-map",
                    "1",
                    "-map",
                    "2",
                    "-map",
                    "3",
                    "-map",
                    "4:a",
                    "-c:v",
                    vcodec,
                    "-pix_fmt",
                    "yuv420p",
                    "-c:s",
                    "srt",
                    "-c:a",
                    "aac",
                    "-metadata:s:s:0",
                    "language=ind",
                    "-metadata:s:s:1",
                    "language=eng",
                    "-metadata:s:s:2",
                    "language=und",
                    "-metadata:s:a:0",
                    "language=jpn",
                    mkv.to_str().expect("utf8 path"),
                ])
                .status()
                .expect("ffmpeg binary missing");
            if status.success() {
                ok = true;
                break;
            }
        }
        assert!(ok, "ffmpeg could not mux {name}");
        mkv
    }

    fn count_ext(dir: &Path, ext: &str) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
            .count()
    }

    fn list_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// End-to-end: extract 2 MKVs x (3 subtitle + 1 audio tracks), check layout.
    /// Needs ffmpeg/ffprobe binaries — run explicitly: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs ffmpeg/ffprobe binaries"]
    async fn e2e_extract_two_episodes() {
        let dir = tempfile::tempdir().unwrap();
        let videos = dir.path().join("videos");
        std::fs::create_dir_all(&videos).unwrap();
        mux_episode(&videos, "Show - 01 [480p]");
        mux_episode(&videos, "Show - 02 [480p]");

        let out = dir.path().join("out");
        let audio_out = dir.path().join("audio");
        run(ExtractArgs {
            folder: videos.clone(),
            out: Some(out.clone()),
            jobs: 2,
            ext: "mkv".to_string(),
            overwrite: false,
            audio: true,
            audio_out: Some(audio_out.clone()),
        })
        .await
        .expect("extract failed");

        // `id` tracks land in the root, others in language subfolders.
        assert_eq!(count_ext(&out, "srt"), 2, "expected 2 id subs in root");
        assert_eq!(count_ext(&out.join("en"), "srt"), 2);
        assert_eq!(count_ext(&out.join("unknown"), "srt"), 2);

        // Audio lands in its own folder, tagged from the source language.
        let audios = list_names(&audio_out);
        assert_eq!(audios.len(), 2, "expected 2 audio files, got {audios:?}");
        assert!(
            audios.iter().all(|n| n.ends_with(".ja.m4a")),
            "unexpected audio names: {audios:?}"
        );

        // Spot-check subtitle content survived the round trip.
        let id_files: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("srt"))
            .collect();
        let text = std::fs::read_to_string(&id_files[0]).unwrap();
        assert!(text.contains("Halo dunia"), "unexpected content: {text}");

        // A second run without --overwrite extracts nothing new but still succeeds.
        run(ExtractArgs {
            folder: videos,
            out: Some(out),
            jobs: 2,
            ext: "mkv".to_string(),
            overwrite: false,
            audio: true,
            audio_out: Some(audio_out),
        })
        .await
        .expect("repeat run should succeed (all skipped)");
    }
}
