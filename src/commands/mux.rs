use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use crate::common::ffmpeg_find;

/// `subx mux --video V --audio A --subs S -o OUT.mkv`
#[derive(Debug, Args)]
pub struct MuxArgs {
    /// Main video file.
    #[arg(long)]
    pub video: PathBuf,

    /// Extra audio files (repeatable, space-separated ok).
    #[arg(long, num_args = 1..)]
    pub audio: Vec<PathBuf>,

    /// Subtitle files (repeatable, space-separated ok).
    #[arg(long, num_args = 1..)]
    pub subs: Vec<PathBuf>,

    /// Output file (.mkv/.mp4).
    #[arg(short, long)]
    pub out: PathBuf,

    /// Hardsub (burn-in) using this subtitle + re-encode.
    #[arg(long)]
    pub hardsub: Option<PathBuf>,

    /// MKV title.
    #[arg(long)]
    pub title: Option<String>,

    /// Batch mode: mux every video in DIR with same-stem audio/subs.
    #[arg(long)]
    pub batch_dir: Option<PathBuf>,

    /// Output folder for batch mode (default: same folder, `<stem>.muxed.mkv`).
    #[arg(long)]
    pub out_dir: Option<PathBuf>,
}

const VIDEO_EXTS: &[&str] = &["mkv", "mp4"];
const SUB_EXTS: &[&str] = &["ass", "ssa", "srt", "vtt", "sup"];
const AUDIO_EXTS: &[&str] = &[
    "mka", "m4a", "aac", "ac3", "eac3", "mp3", "flac", "ogg", "opus", "wav", "dts",
];

struct SubInput {
    path: PathBuf,
    /// ISO 639-2 language derived from the filename, if recognizable.
    lang: Option<String>,
}

struct MuxSpec {
    video: PathBuf,
    audios: Vec<PathBuf>,
    subs: Vec<SubInput>,
    out: PathBuf,
    hardsub: Option<PathBuf>,
    title: Option<String>,
}

/// Derive an ISO 639-2 language from `<stem>.<lang>.<ext>`, e.g. `Ep01.id.ass`.
fn sub_lang_639_2(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let code = stem.rsplit('.').next()?;
    if !code.bytes().all(|b| b.is_ascii_alphabetic()) || !(2..=3).contains(&code.len()) {
        return None;
    }
    match code.to_lowercase().as_str() {
        "id" | "ind" => Some("ind".to_string()),
        "en" | "eng" => Some("eng".to_string()),
        "ja" | "jpn" => Some("jpn".to_string()),
        _ => None,
    }
}

/// Escape a subtitle path for the ffmpeg `subtitles` filter: forward slashes
/// (no drive-letter backslashes), and every `' : , [ ] ;` prefixed with `\\`.
/// Double backslashes are required because escaping is consumed once by the
/// filtergraph parser and once by the filter's own option parser.
fn escape_filter_path(p: &Path) -> String {
    let forward = p.to_string_lossy().replace('\\', "/");
    let mut out = String::with_capacity(forward.len());
    for ch in forward.chars() {
        if matches!(ch, '\'' | ':' | ',' | '[' | ']' | ';') {
            out.push_str("\\\\");
        }
        out.push(ch);
    }
    out
}

fn build_args(spec: &MuxSpec) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec!["-y".into()];
    // Input order: video, extra audios, subs.
    a.push("-i".into());
    a.push(spec.video.clone().into());
    for x in &spec.audios {
        a.push("-i".into());
        a.push(x.clone().into());
    }
    for s in &spec.subs {
        a.push("-i".into());
        a.push(s.path.clone().into());
    }
    // Maps: keep source video (+ audio if present), then every extra input.
    a.push("-map".into());
    a.push("0:v".into());
    a.push("-map".into());
    a.push("0:a?".into());
    let mut idx = 1;
    for _ in &spec.audios {
        a.push("-map".into());
        a.push(format!("{idx}:a").into());
        idx += 1;
    }
    for (n, s) in spec.subs.iter().enumerate() {
        a.push("-map".into());
        a.push(format!("{}:s", idx + n).into());
        if let Some(lang) = &s.lang {
            a.push(format!("-metadata:s:s:{n}").into());
            a.push(format!("language={lang}").into());
        }
    }
    if let Some(t) = &spec.title {
        a.push("-metadata".into());
        a.push(format!("title={t}").into());
    }
    if let Some(burn) = &spec.hardsub {
        a.push("-vf".into());
        a.push(format!("subtitles={}", escape_filter_path(burn)).into());
        a.extend(
            [
                "-c:v", "libx264", "-crf", "18", "-preset", "fast", "-c:a", "copy", "-c:s", "copy",
            ]
            .into_iter()
            .map(OsString::from),
        );
    } else {
        a.push("-c".into());
        a.push("copy".into());
    }
    a.push(spec.out.clone().into());
    a
}

async fn run_mux(ffmpeg: &Path, spec: &MuxSpec) -> Result<()> {
    let args = build_args(spec);
    let status = tokio::process::Command::new(ffmpeg)
        .args(&args)
        .status()
        .await
        .with_context(|| "cannot start ffmpeg")?;
    if !status.success() {
        anyhow::bail!("ffmpeg failed (exit={})", status.code().unwrap_or(-1));
    }
    Ok(())
}

fn ensure_parent(out: &Path) -> Result<()> {
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    Ok(())
}

/// True when `candidate` belongs to the episode stem: exact match or
/// `<stem>.<suffix>` (e.g. `Ep01.id.ass` belongs to `Ep01`).
fn matches_stem(candidate: &Path, video_stem: &str, video_path: &Path) -> bool {
    if candidate == video_path {
        return false;
    }
    let cs = candidate
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let vs = video_stem.to_lowercase();
    cs == vs || cs.starts_with(&format!("{vs}."))
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

async fn run_batch(ffmpeg: &Path, args: &MuxArgs, dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        anyhow::bail!("Batch folder not found: {}", dir.display());
    }
    let out_dir = args.out_dir.clone().unwrap_or_else(|| dir.to_path_buf());
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("cannot create {}", out_dir.display()))?;

    let mut videos: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && VIDEO_EXTS.contains(&ext_of(p).as_str()))
        .collect();
    videos.sort();
    if videos.is_empty() {
        anyhow::bail!("No video files found in {}", dir.display());
    }

    let siblings: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();

    let mut ok = 0;
    let mut failed = 0;
    for video in &videos {
        let stem = video
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("out")
            .to_string();
        let audios: Vec<PathBuf> = siblings
            .iter()
            .filter(|p| AUDIO_EXTS.contains(&ext_of(p).as_str()) && matches_stem(p, &stem, video))
            .cloned()
            .collect();
        let subs: Vec<SubInput> = siblings
            .iter()
            .filter(|p| SUB_EXTS.contains(&ext_of(p).as_str()) && matches_stem(p, &stem, video))
            .map(|p| SubInput {
                path: p.clone(),
                lang: sub_lang_639_2(p),
            })
            .collect();
        if audios.is_empty() && subs.is_empty() {
            println!("[SKIP] {stem}: no extra tracks");
            continue;
        }
        let out = out_dir.join(format!("{stem}.muxed.mkv"));
        let spec = MuxSpec {
            video: video.clone(),
            audios,
            subs,
            out: out.clone(),
            hardsub: None,
            title: args.title.clone(),
        };
        match run_mux(ffmpeg, &spec).await {
            Ok(()) => {
                ok += 1;
                println!("[OK] {stem} -> {}", out.display());
            }
            Err(e) => {
                failed += 1;
                println!("[FAILED] {stem} -> {e:#}");
            }
        }
    }
    println!("--------------------------------------------------");
    println!("Episodes : {}", videos.len());
    println!("Muxed    : {ok}");
    println!("Failed   : {failed}");
    if ok == 0 {
        anyhow::bail!("All episodes failed.");
    }
    Ok(())
}

pub async fn run(args: MuxArgs) -> Result<()> {
    let (ffmpeg, _) = ffmpeg_find::find_ffmpeg()?;
    if let Some(dir) = args.batch_dir.clone() {
        return run_batch(&ffmpeg, &args, &dir).await;
    }
    if !args.video.is_file() {
        anyhow::bail!("Video not found: {}", args.video.display());
    }
    for p in args.audio.iter().chain(args.subs.iter()) {
        if !p.is_file() {
            anyhow::bail!("Input not found: {}", p.display());
        }
    }
    if let Some(burn) = &args.hardsub
        && !burn.is_file()
    {
        anyhow::bail!("Hardsub file not found: {}", burn.display());
    }
    ensure_parent(&args.out)?;
    let spec = MuxSpec {
        video: args.video.clone(),
        audios: args.audio.clone(),
        subs: args
            .subs
            .iter()
            .map(|p| SubInput {
                path: p.clone(),
                lang: sub_lang_639_2(p),
            })
            .collect(),
        out: args.out.clone(),
        hardsub: args.hardsub.clone(),
        title: args.title.clone(),
    };
    run_mux(&ffmpeg, &spec).await?;
    println!("Muxed: {}", args.out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::probe;

    fn args_str(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|s| s.to_str().unwrap()).collect()
    }

    #[test]
    fn builds_softmux_args() {
        let spec = MuxSpec {
            video: PathBuf::from("v.mkv"),
            audios: vec![PathBuf::from("a.mka")],
            subs: vec![SubInput {
                path: PathBuf::from("ep.en.srt"),
                lang: Some("eng".to_string()),
            }],
            out: PathBuf::from("o.mkv"),
            hardsub: None,
            title: Some("T".to_string()),
        };
        let built = build_args(&spec);
        assert_eq!(
            args_str(&built),
            [
                "-y",
                "-i",
                "v.mkv",
                "-i",
                "a.mka",
                "-i",
                "ep.en.srt",
                "-map",
                "0:v",
                "-map",
                "0:a?",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-metadata:s:s:0",
                "language=eng",
                "-metadata",
                "title=T",
                "-c",
                "copy",
                "o.mkv"
            ]
        );
    }

    #[test]
    fn builds_hardsub_args() {
        let spec = MuxSpec {
            video: PathBuf::from("v.mkv"),
            audios: vec![],
            subs: vec![],
            out: PathBuf::from("o.mkv"),
            hardsub: Some(PathBuf::from("sub.ass")),
            title: None,
        };
        let built = build_args(&spec);
        let a = args_str(&built);
        assert!(a.contains(&"-vf"));
        let vf = a[a.iter().position(|x| *x == "-vf").unwrap() + 1];
        assert!(vf.starts_with("subtitles=") && vf.ends_with("sub.ass"));
        for flag in ["libx264", "18", "fast"] {
            assert!(a.contains(&flag), "missing {flag}");
        }
    }

    #[test]
    fn escapes_filter_paths() {
        assert_eq!(
            escape_filter_path(Path::new("C:\\A'B:C.ass")),
            "C\\\\:/A\\\\'B\\\\:C.ass"
        );
        assert_eq!(
            escape_filter_path(Path::new("Ep 01 [480p].ass")),
            "Ep 01 \\\\[480p\\\\].ass"
        );
        assert_eq!(escape_filter_path(Path::new("plain.ass")), "plain.ass");
    }

    #[test]
    fn derives_sub_languages() {
        assert_eq!(
            sub_lang_639_2(Path::new("Ep01.id.ass")),
            Some("ind".to_string())
        );
        assert_eq!(
            sub_lang_639_2(Path::new("Ep01.ENG.srt")),
            Some("eng".to_string())
        );
        assert_eq!(sub_lang_639_2(Path::new("Show - 01 [480p].srt")), None);
        assert_eq!(sub_lang_639_2(Path::new("noext")), None);
    }

    #[test]
    fn matches_episode_stems() {
        let v = Path::new("Ep01.mkv");
        assert!(matches_stem(Path::new("Ep01.id.ass"), "Ep01", v));
        assert!(matches_stem(Path::new("Ep01.mka"), "Ep01", v));
        assert!(!matches_stem(Path::new("Ep02.id.ass"), "Ep01", v));
        assert!(!matches_stem(v, "Ep01", v));
    }

    /// Generate shared fixtures: silent video + tone audio + en/id subs.
    fn fixtures(dir: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        use std::process::Command;

        let video = dir.join("ep.mp4");
        let audio = dir.join("ep.mka");
        let en_sub = dir.join("ep.en.srt");
        let id_sub = dir.join("ep.id.srt");
        std::fs::write(&en_sub, "1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
        std::fs::write(&id_sub, "1\n00:00:00,000 --> 00:00:01,000\nHalo\n").unwrap();

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
                    "-c:v",
                    vcodec,
                    "-pix_fmt",
                    "yuv420p",
                    video.to_str().expect("utf8 path"),
                ])
                .status()
                .expect("ffmpeg binary missing");
            if status.success() {
                ok = true;
                break;
            }
        }
        assert!(ok, "ffmpeg could not make the sample video");
        run_ffmpeg(&[
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:a",
            "aac",
            audio.to_str().expect("utf8 path"),
        ]);
        (video, audio, en_sub, id_sub)
    }

    fn run_ffmpeg(args: &[&str]) {
        let status = std::process::Command::new("ffmpeg")
            .args(args)
            .status()
            .expect("ffmpeg binary missing");
        assert!(status.success(), "ffmpeg failed: {args:?}");
    }

    /// End-to-end softmux: 1 video + 1 audio + 2 subs -> 1v/1a/2s with tags.
    /// Needs ffmpeg/ffprobe binaries — run explicitly: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs ffmpeg/ffprobe binaries"]
    async fn e2e_softmux() {
        let dir = tempfile::tempdir().unwrap();
        let (video, audio, en_sub, id_sub) = fixtures(dir.path());
        let out = dir.path().join("out.mkv");
        run(MuxArgs {
            video,
            audio: vec![audio],
            subs: vec![en_sub, id_sub],
            out: out.clone(),
            hardsub: None,
            title: Some("T".to_string()),
            batch_dir: None,
            out_dir: None,
        })
        .await
        .expect("mux failed");

        let (_, ffprobe) = ffmpeg_find::find_ffmpeg().unwrap();
        let streams = probe::probe_streams(&ffprobe, &out).await.unwrap();
        let (mut v, mut a, mut s) = (0, 0, 0);
        let mut langs = Vec::new();
        for st in &streams {
            match st.codec_type.as_deref() {
                Some("video") => v += 1,
                Some("audio") => a += 1,
                Some("subtitle") => {
                    s += 1;
                    langs.push(st.tags.get("language").cloned().unwrap_or_default());
                }
                _ => {}
            }
        }
        assert_eq!((v, a, s), (1, 1, 2));
        langs.sort();
        assert_eq!(langs, ["eng", "ind"]);
    }

    /// End-to-end hardsub burn-in (re-encode, no soft tracks kept).
    /// Needs ffmpeg/ffprobe binaries — run explicitly: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs ffmpeg/ffprobe binaries"]
    async fn e2e_hardsub() {
        let dir = tempfile::tempdir().unwrap();
        let (video, _, _, id_sub) = fixtures(dir.path());
        let out = dir.path().join("burned.mkv");
        run(MuxArgs {
            video,
            audio: vec![],
            subs: vec![],
            out: out.clone(),
            hardsub: Some(id_sub),
            title: None,
            batch_dir: None,
            out_dir: None,
        })
        .await
        .expect("hardsub failed");

        assert!(std::fs::metadata(&out).unwrap().len() > 0);
        let (_, ffprobe) = ffmpeg_find::find_ffmpeg().unwrap();
        let streams = probe::probe_streams(&ffprobe, &out).await.unwrap();
        assert!(
            streams
                .iter()
                .any(|s| s.codec_type.as_deref() == Some("video")),
            "burned file has no video stream"
        );
    }

    /// End-to-end batch mode with same-stem matching.
    /// Needs ffmpeg/ffprobe binaries — run explicitly: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs ffmpeg/ffprobe binaries"]
    async fn e2e_batch() {
        let dir = tempfile::tempdir().unwrap();
        let batch = dir.path().join("batch");
        std::fs::create_dir_all(&batch).unwrap();
        let (video, audio, en_sub, _) = fixtures(&batch);
        // Rename fixtures to episode-stem layout.
        std::fs::rename(&video, batch.join("Ep01.mp4")).unwrap();
        std::fs::rename(&audio, batch.join("Ep01.mka")).unwrap();
        std::fs::rename(&en_sub, batch.join("Ep01.en.srt")).unwrap();
        let out_dir = dir.path().join("done");
        run(MuxArgs {
            video: PathBuf::new(),
            audio: vec![],
            subs: vec![],
            out: PathBuf::new(),
            hardsub: None,
            title: None,
            batch_dir: Some(batch),
            out_dir: Some(out_dir.clone()),
        })
        .await
        .expect("batch mux failed");

        let out = out_dir.join("Ep01.muxed.mkv");
        assert!(out.is_file(), "batch output missing");
        let (_, ffprobe) = ffmpeg_find::find_ffmpeg().unwrap();
        let streams = probe::probe_streams(&ffprobe, &out).await.unwrap();
        let subs = streams
            .iter()
            .filter(|s| s.codec_type.as_deref() == Some("subtitle"))
            .count();
        assert_eq!(subs, 1);
    }
}
