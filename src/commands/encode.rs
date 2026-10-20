use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use regex::Regex;
use serde_json::Value;
use walkdir::WalkDir;

use crate::common::config::AppConfig;
use crate::common::episode::{self, EpisodeId};
use crate::common::tools::{self, Tool};

mod native;

/// `subx encode <SOURCE> <DEST>` — encode every video in a folder through a StaxRip template.
///
/// Each video first gets a work copy without embedded subtitles/fonts and with only
/// the chosen audio language. StaxRip is then driven through its command line:
/// load the template, load the work copy, lock the target size, add the external
/// subtitles whose episode number matches, encode, and exit. Finished files are
/// verified with mkvmerge. Episodes whose output already exists are skipped.
#[derive(Debug, Args)]
pub struct EncodeArgs {
    /// Folder with the source videos and external subtitles.
    pub source: PathBuf,

    /// Output folder.
    pub dest: PathBuf,

    /// StaxRip template (case and punctuation are ignored: "Anime480p.AV1.Opus96").
    /// Default: `[encode] template` in subx.toml.
    #[arg(long)]
    pub template: Option<String>,

    /// Output name prefix such as "[GROUP]"; "" for none. Default: `[encode] prefix`.
    #[arg(long)]
    pub prefix: Option<String>,

    /// Series name in the output file name (default: the source folder name).
    #[arg(long)]
    pub series: Option<String>,

    /// Tag in parentheses at the end of the output file name; "" for none.
    /// Default: `[encode] tag`.
    #[arg(long)]
    pub tag: Option<String>,

    /// Target size WIDTHxHEIGHT, locked in StaxRip because templates derive the
    /// height from the aspect ratio (848 -> 476). "" keeps the template's size.
    /// Default: `[encode] resolution`.
    #[arg(long)]
    pub resolution: Option<String>,

    /// Only these episodes: comma-separated, ranges allowed ("10-13", "5,8-10,OVA 1-3").
    #[arg(long)]
    pub only: Option<String>,

    /// Subtitle language when the file name has no `.<lang>.` part.
    /// Default: `[encode] sub_lang` (id).
    #[arg(long)]
    pub sub_lang: Option<String>,

    /// The only audio language kept (ISO 639-2). Default: `[encode] audio_lang` (jpn).
    #[arg(long)]
    pub audio_lang: Option<String>,

    /// StaxRip folder (contains StaxRip.exe, Settings, Apps). Default: `[tools] staxrip_dir`.
    #[arg(long)]
    pub staxrip_dir: Option<PathBuf>,

    /// Folder for the per-episode work copies. Default: `[encode] work_dir`,
    /// else <temp>/subx-encode.
    #[arg(long)]
    pub work_dir: Option<PathBuf>,

    /// `native` (ffmpeg + encoder + mkvmerge) or `staxrip` (drive StaxRip).
    /// Default: `[encode] engine` (native).
    #[arg(long)]
    pub engine: Option<String>,

    /// Native engine: `[encode.profiles.*]` entry to use. Default: `[encode] profile`.
    #[arg(long)]
    pub profile: Option<String>,

    /// Native engine: episodes encoded concurrently. Default: `[encode] jobs` (1).
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    /// Native engine: kill an episode whose encoder output stops growing for
    /// this many minutes. Default: `[encode] stall_min` (5).
    #[arg(long)]
    pub stall_min: Option<u64>,

    /// Stop StaxRip when one episode takes longer than this many minutes.
    #[arg(long, default_value_t = 120)]
    pub timeout_min: u64,

    /// Print the plan and the StaxRip command lines without running anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Run even if StaxRip is already running (Jobs.dat/Settings.dat are shared).
    #[arg(long)]
    pub allow_running: bool,
}

const VIDEO_EXTS: &[&str] = &["mkv", "mp4"];
const SUB_EXTS: &[&str] = &["ass", "ssa", "srt"];

static RESOLUTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+x\d+$").unwrap());
static SUB_LANG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.([a-z]{2,3})\.(?:ass|ssa|srt)$").unwrap());

#[derive(Debug)]
pub(crate) struct Episode {
    pub(crate) id: EpisodeId,
    pub(crate) video: PathBuf,
    pub(crate) subs: Vec<PathBuf>,
    pub(crate) target: PathBuf,
}

/// Settings shared by every episode of one run.
struct Ctx {
    exe: PathBuf,
    mkvmerge: PathBuf,
    template: String,
    resolution: String,
    audio_lang: String,
    sub_lang: String,
    timeout: Duration,
}

fn has_ext(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

pub(crate) fn file_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|n| n.to_str())
}

fn target_name(prefix: &str, series: &str, key: &str, tag: &str) -> String {
    let title = [prefix, series]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if tag.is_empty() {
        format!("{title} - {key}.mkv")
    } else {
        format!("{title} - {key} ({tag}).mkv")
    }
}

/// Pair every video in `source` with the external subtitles of the same episode.
fn build_plan(
    source: &Path,
    dest: &Path,
    prefix: &str,
    series: &str,
    tag: &str,
    only: &[String],
) -> Result<Vec<Episode>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(source)
        .with_context(|| format!("cannot read {}", source.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort();

    let mut subs_by_key: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for sub in files.iter().filter(|p| has_ext(p, SUB_EXTS)) {
        if std::fs::metadata(sub).map_or(0, |m| m.len()) == 0 {
            continue;
        }
        if let Some(id) = file_name(sub).and_then(EpisodeId::from_file_name) {
            subs_by_key.entry(id.key()).or_default().push(sub.clone());
        }
    }

    let mut plan: Vec<Episode> = Vec::new();
    for video in files.iter().filter(|p| has_ext(p, VIDEO_EXTS)) {
        let name = file_name(video).unwrap_or_default();
        let Some(id) = EpisodeId::from_file_name(name) else {
            tracing::warn!("Episode number not detected, skipped: {name}");
            continue;
        };
        let key = id.key();
        if !only.is_empty() && !only.contains(&key) {
            continue;
        }
        if let Some(prev) = plan.iter().find(|e| e.id.key() == key) {
            anyhow::bail!(
                "Two videos share episode {id}: {} and {name}",
                file_name(&prev.video).unwrap_or_default()
            );
        }
        plan.push(Episode {
            subs: subs_by_key.get(&key).cloned().unwrap_or_default(),
            target: dest.join(target_name(prefix, series, &id.to_string(), tag)),
            video: video.clone(),
            id,
        });
    }
    if plan.is_empty() {
        anyhow::bail!("No matching videos to encode in {}", source.display());
    }
    Ok(plan)
}

/// Find the template by name, ignoring case and punctuation. Returns the name
/// StaxRip's `-LoadTemplate` expects (path under Settings/Templates, no extension).
fn resolve_template(staxrip_dir: &Path, name: &str) -> Result<String> {
    let root = staxrip_dir.join("Settings").join("Templates");
    if !root.is_dir() {
        anyhow::bail!("Template folder not found: {}", root.display());
    }
    let norm = |s: &str| {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_lowercase()
    };

    let mut all: Vec<String> = WalkDir::new(&root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && has_ext(e.path(), &["srip"]))
        .map(|e| {
            let rel = e.path().strip_prefix(&root).unwrap_or(e.path());
            rel.with_extension("").to_string_lossy().into_owned()
        })
        .collect();
    all.sort();

    let wanted = norm(name);
    let hits: Vec<&String> = all.iter().filter(|t| norm(t) == wanted).collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        _ => anyhow::bail!(
            "Template '{name}' not found or ambiguous in {}. Available: {}",
            root.display(),
            all.join(", ")
        ),
    }
}

/// Subtitle language from `<name>.<lang>.<ext>`, else the default.
pub(crate) fn sub_language(path: &Path, default: &str) -> String {
    file_name(path)
        .and_then(|n| SUB_LANG.captures(n))
        .map_or_else(|| default.to_string(), |c| c[1].to_string())
}

/// StaxRip command line (one raw string: StaxRip parses it itself, so quotes must
/// not be re-escaped). Order matters: loading the source resets the target size.
fn staxrip_cmdline(
    template: &str,
    work: &Path,
    resolution: &str,
    subs: &[PathBuf],
    default_lang: &str,
    target: &Path,
) -> String {
    let q = |s: &str| format!("\"{s}\"");
    let mut parts = vec![
        "-NoFocus".to_string(),
        format!("-LoadTemplate:{}", q(template)),
        "-SetHideDialogsOption:true".to_string(),
        format!("-LoadSourceFile:{}", q(&work.display().to_string())),
    ];
    if !resolution.is_empty() {
        parts.push(format!(
            "-SetTargetImageSize:{}",
            resolution.replace('x', ",")
        ));
    }
    for (i, sub) in subs.iter().enumerate() {
        // StaxRip drops empty parameters, so the (empty) name is written as "".
        parts.push(format!(
            "-AddSubtitle:true,{},\"\",{},false,false,false,{}",
            sub_language(sub, default_lang),
            i == 0,
            q(&sub.display().to_string())
        ));
    }
    parts.push(format!(
        "-SetTargetFile:{}",
        q(&target.display().to_string())
    ));
    parts.push("-StartEncoding".to_string());
    parts.push("-ExitWithoutSaving".to_string());
    parts.join(" ")
}

pub(crate) async fn mkv_info(mkvmerge: &Path, file: &Path) -> Result<Value> {
    crate::media::mkv::info(mkvmerge, file).await
}

fn track_matches_lang(track: &Value, lang: &str) -> bool {
    track["properties"]["language"] == lang || track["properties"]["language_ietf"] == lang
}

/// Track id of the first audio track in `lang`.
pub(crate) fn pick_audio_track(info: &Value, lang: &str) -> Result<u64> {
    let tracks = info["tracks"].as_array().map_or(&[][..], Vec::as_slice);
    let hits: Vec<&Value> = tracks
        .iter()
        .filter(|t| t["type"] == "audio" && track_matches_lang(t, lang))
        .collect();
    if hits.len() > 1 {
        tracing::warn!(
            "{} '{lang}' audio tracks found, using the first",
            hits.len()
        );
    }
    hits.first()
        .and_then(|t| t["id"].as_u64())
        .with_context(|| format!("no '{lang}' audio track"))
}

/// Copy without subtitles and fonts, keeping only the chosen audio language.
/// Video, chapters and timing are untouched.
async fn make_clean_copy(
    mkvmerge: &Path,
    input: &Path,
    output: &Path,
    audio_lang: &str,
) -> Result<()> {
    let info = mkv_info(mkvmerge, input).await?;
    let audio_id = pick_audio_track(&info, audio_lang)
        .with_context(|| format!("while reading {}", input.display()))?;
    let status = tokio::process::Command::new(mkvmerge)
        .arg("-o")
        .arg(output)
        .args(["--no-subtitles", "--no-attachments", "--audio-tracks"])
        .arg(audio_id.to_string())
        .arg(input)
        .stdout(Stdio::null())
        .status()
        .await
        .with_context(|| "cannot start mkvmerge")?;
    if status.code().unwrap_or(2) >= 2 {
        anyhow::bail!(
            "mkvmerge failed to make the work copy of {}",
            input.display()
        );
    }
    Ok(())
}

/// What is wrong with a finished file (empty when it matches expectations).
pub(crate) fn check_output(
    info: &Value,
    expected_subs: usize,
    audio_lang: &str,
    resolution: &str,
) -> Vec<String> {
    let tracks = info["tracks"].as_array().map_or(&[][..], Vec::as_slice);
    let of = |kind: &str| -> Vec<&Value> { tracks.iter().filter(|t| t["type"] == kind).collect() };
    let (video, audio, subs) = (of("video"), of("audio"), of("subtitles"));
    let fonts = info["attachments"].as_array().map_or(0, Vec::len);

    let mut problems = Vec::new();
    if video.len() != 1 {
        problems.push(format!("video={}", video.len()));
    } else if !resolution.is_empty() && video[0]["properties"]["pixel_dimensions"] != resolution {
        let got = video[0]["properties"]["pixel_dimensions"]
            .as_str()
            .unwrap_or("?");
        problems.push(format!("resolution={got} (expected {resolution})"));
    }
    if audio.len() != 1 {
        problems.push(format!("audio={}", audio.len()));
    } else if !track_matches_lang(audio[0], audio_lang) {
        let got = audio[0]["properties"]["language"].as_str().unwrap_or("?");
        problems.push(format!("audio language={got} (expected {audio_lang})"));
    }
    if subs.len() != expected_subs {
        problems.push(format!(
            "subtitles={} (expected {expected_subs})",
            subs.len()
        ));
    }
    if fonts != 0 {
        problems.push(format!("fonts={fonts}"));
    }
    problems
}

/// Number of running StaxRip processes (Windows only; elsewhere always 0).
async fn staxrip_running() -> usize {
    #[cfg(windows)]
    {
        let out = tokio::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq StaxRip.exe", "/NH"])
            .output()
            .await;
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.to_ascii_lowercase().starts_with("staxrip.exe"))
                .count(),
            Err(_) => 0,
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Run StaxRip with a raw command line and wait for it to exit. The exit code is
/// ignored on purpose: the verified output file decides success.
#[cfg(windows)]
async fn run_staxrip(exe: &Path, cmdline: &str, timeout: Duration) -> Result<()> {
    let mut child = tokio::process::Command::new(exe)
        .current_dir(exe.parent().unwrap_or(Path::new(".")))
        .raw_arg(cmdline)
        .spawn()
        .with_context(|| format!("cannot start {}", exe.display()))?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => {
            status.with_context(|| "waiting for StaxRip failed")?;
            Ok(())
        }
        Err(_) => {
            if let Some(pid) = child.id() {
                let _ = tokio::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .status()
                    .await;
            }
            anyhow::bail!(
                "StaxRip exceeded {} minutes and was stopped",
                timeout.as_secs() / 60
            )
        }
    }
}

#[cfg(not(windows))]
async fn run_staxrip(_exe: &Path, _cmdline: &str, _timeout: Duration) -> Result<()> {
    anyhow::bail!("StaxRip runs on Windows only")
}

async fn encode_one(ctx: &Ctx, ep: &Episode, work_dir: &Path, cmdline: &str) -> Result<()> {
    if work_dir.exists() {
        std::fs::remove_dir_all(work_dir)
            .with_context(|| format!("cannot clear {}", work_dir.display()))?;
    }
    std::fs::create_dir_all(work_dir)
        .with_context(|| format!("cannot create {}", work_dir.display()))?;
    if let Some(parent) = ep.target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    make_clean_copy(
        &ctx.mkvmerge,
        &ep.video,
        &work_dir.join("source.mkv"),
        &ctx.audio_lang,
    )
    .await?;
    run_staxrip(&ctx.exe, cmdline, ctx.timeout).await?;

    if !ep.target.is_file() {
        anyhow::bail!("Output file was not created (see the log in StaxRip's Settings\\Log Files)");
    }
    let info = mkv_info(&ctx.mkvmerge, &ep.target).await?;
    let problems = check_output(&info, ep.subs.len(), &ctx.audio_lang, &ctx.resolution);
    if !problems.is_empty() {
        anyhow::bail!("Output does not match: {}", problems.join("; "));
    }
    // Only a verified episode drops its work copy; failures keep it for inspection.
    std::fs::remove_dir_all(work_dir).ok();
    Ok(())
}

fn print_plan(plan: &[Episode]) {
    for ep in plan {
        let status = if ep.target.exists() {
            "SKIP (output exists)"
        } else {
            "ENCODE"
        };
        println!(
            "{:<7} {:<22} subs={} -> {}",
            ep.id.to_string(),
            status,
            ep.subs.len(),
            file_name(&ep.target).unwrap_or_default()
        );
    }
}

pub async fn run(args: EncodeArgs, cfg: &AppConfig) -> Result<()> {
    // Flags win over `[tools]` / `[encode]` in subx.toml.
    if !args.source.exists() {
        anyhow::bail!("Not found: {}", args.source.display());
    }
    let engine = args
        .engine
        .clone()
        .unwrap_or_else(|| cfg.encode.engine.clone());
    let prefix = args
        .prefix
        .clone()
        .unwrap_or_else(|| cfg.encode.prefix.clone());
    let audio_lang = args
        .audio_lang
        .clone()
        .unwrap_or_else(|| cfg.encode.audio_lang.clone());
    let sub_lang = args
        .sub_lang
        .clone()
        .unwrap_or_else(|| cfg.encode.sub_lang.clone());
    let series = args.series.clone().unwrap_or_else(|| {
        args.source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let only = args
        .only
        .as_deref()
        .map(episode::expand_selection)
        .transpose()?
        .unwrap_or_default();
    let work_root = args
        .work_dir
        .clone()
        .or_else(|| cfg.encode.work_dir.clone())
        .unwrap_or_else(|| std::env::temp_dir().join("subx-encode"));

    match engine.as_str() {
        "native" => {
            let profile_name = args
                .profile
                .clone()
                .or_else(|| cfg.encode.profile.clone())
                .context("profile unknown: pass --profile or set [encode] profile in subx.toml")?;
            let stall_min = args.stall_min.unwrap_or(cfg.encode.stall_min);
            let native = native::Native::setup(
                &cfg.encode,
                &profile_name,
                &audio_lang,
                &sub_lang,
                stall_min,
            )
            .await?;
            let tag = args
                .tag
                .clone()
                .or_else(|| native.profile.tag.clone())
                .unwrap_or_else(|| cfg.encode.tag.clone());
            let plan = build_plan(&args.source, &args.dest, &prefix, &series, &tag, &only)?;
            print_plan(&plan);
            if args.dry_run {
                return native::dry_run(&native, &plan, &work_root).await;
            }
            let jobs = args.jobs.unwrap_or(cfg.encode.jobs).max(1);
            let failed = native::run_all(&native, &plan, &work_root, jobs).await?;
            if failed > 0 {
                anyhow::bail!("{failed} episode(s) failed.");
            }
            return Ok(());
        }
        "staxrip" => {}
        other => anyhow::bail!("unknown engine '{other}' (staxrip | native)"),
    }

    // --- StaxRip engine --------------------------------------------------------
    let staxrip_dir = args
        .staxrip_dir
        .clone()
        .or_else(|| cfg.tools.staxrip_dir.clone())
        .context(
            "StaxRip folder unknown: pass --staxrip-dir or set [tools] staxrip_dir in subx.toml",
        )?;
    let template_name = args
        .template
        .clone()
        .or_else(|| cfg.encode.template.clone())
        .context("template unknown: pass --template or set [encode] template in subx.toml")?;
    let tag = args.tag.clone().unwrap_or_else(|| cfg.encode.tag.clone());
    let resolution = args
        .resolution
        .clone()
        .unwrap_or_else(|| cfg.encode.resolution.clone())
        .trim()
        .to_lowercase();

    let exe = staxrip_dir.join("StaxRip.exe");
    if !exe.exists() {
        anyhow::bail!("Not found: {}", exe.display());
    }
    let mkvmerge = tools::require_one(Tool::Mkvmerge)?;
    if !resolution.is_empty() && !RESOLUTION.is_match(&resolution) {
        anyhow::bail!("--resolution must look like WIDTHxHEIGHT, e.g. 848x480");
    }
    let template = resolve_template(&staxrip_dir, &template_name)?;

    if !args.dry_run && !args.allow_running {
        let running = staxrip_running().await;
        if running > 0 {
            anyhow::bail!(
                "StaxRip is still running ({running} processes). Close it first or pass --allow-running."
            );
        }
    }

    let plan = build_plan(&args.source, &args.dest, &prefix, &series, &tag, &only)?;
    print_plan(&plan);

    let ctx = Ctx {
        exe,
        mkvmerge,
        template,
        resolution,
        audio_lang,
        sub_lang,
        timeout: Duration::from_secs(args.timeout_min * 60),
    };

    let (mut ok, mut skipped, mut failed) = (0, 0, 0);
    for ep in &plan {
        if ep.target.exists() {
            skipped += 1;
            continue;
        }
        if ep.subs.is_empty() {
            tracing::warn!("Episode {}: no matching subtitle in the folder.", ep.id);
        }
        let work_dir = work_root.join(ep.id.key());
        let cmdline = staxrip_cmdline(
            &ctx.template,
            &work_dir.join("source.mkv"),
            &ctx.resolution,
            &ep.subs,
            &ctx.sub_lang,
            &ep.target,
        );

        if args.dry_run {
            println!(
                "\n[{}] work copy: {}",
                ep.id,
                work_dir.join("source.mkv").display()
            );
            println!("[{}] StaxRip.exe {cmdline}", ep.id);
            continue;
        }

        println!("\n[{}] {}", ep.id, file_name(&ep.video).unwrap_or_default());
        match encode_one(&ctx, ep, &work_dir, &cmdline).await {
            Ok(()) => {
                ok += 1;
                println!("[OK] {} -> {}", ep.id, ep.target.display());
            }
            Err(e) => {
                failed += 1;
                println!("[FAILED] {} -> {e:#}", ep.id);
                println!("Work copy kept in {}", work_dir.display());
            }
        }
    }

    if !args.dry_run {
        println!("--------------------------------------------------");
        println!("Episodes : {}", plan.len());
        println!("Encoded  : {ok}");
        println!("Skipped  : {skipped}");
        println!("Failed   : {failed}");
        if failed > 0 {
            anyhow::bail!("{failed} episode(s) failed.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_target_names() {
        assert_eq!(
            target_name("[ASUT]", "Akatsuki no Yona", "09", "BD 480p AV1.Opus"),
            "[ASUT] Akatsuki no Yona - 09 (BD 480p AV1.Opus).mkv"
        );
        assert_eq!(
            target_name("", "Show", "OVA 1", "T"),
            "Show - OVA 1 (T).mkv"
        );
        assert_eq!(target_name("", "Show", "01", ""), "Show - 01.mkv");
    }

    #[test]
    fn reads_subtitle_language() {
        let d = "id";
        assert_eq!(sub_language(Path::new("a.S0.id.ass"), d), "id");
        assert_eq!(sub_language(Path::new("a.EN.srt"), d), "EN");
        assert_eq!(sub_language(Path::new("Show - 01.ass"), d), "id");
    }

    #[test]
    fn builds_staxrip_cmdline() {
        let line = staxrip_cmdline(
            "Anime-480p.AV1.Opus96",
            Path::new("W/10/source.mkv"),
            "848x480",
            &[
                PathBuf::from("S/ep10.id.ass"),
                PathBuf::from("S/ep10.en.srt"),
            ],
            "id",
            Path::new("D/[ASUT] Show - 10 (T).mkv"),
        );
        assert_eq!(
            line,
            "-NoFocus -LoadTemplate:\"Anime-480p.AV1.Opus96\" -SetHideDialogsOption:true \
             -LoadSourceFile:\"W/10/source.mkv\" -SetTargetImageSize:848,480 \
             -AddSubtitle:true,id,\"\",true,false,false,false,\"S/ep10.id.ass\" \
             -AddSubtitle:true,en,\"\",false,false,false,false,\"S/ep10.en.srt\" \
             -SetTargetFile:\"D/[ASUT] Show - 10 (T).mkv\" -StartEncoding -ExitWithoutSaving"
        );

        let bare = staxrip_cmdline("T", Path::new("w"), "", &[], "id", Path::new("t.mkv"));
        assert!(!bare.contains("-SetTargetImageSize") && !bare.contains("-AddSubtitle"));
    }

    fn mkv(video_dims: &str, audio_lang: &str, subs: usize, fonts: usize) -> Value {
        let mut tracks = vec![
            json!({"id": 0, "type": "video", "properties": {"pixel_dimensions": video_dims}}),
            json!({"id": 1, "type": "audio",
                   "properties": {"language": audio_lang, "language_ietf": "ja"}}),
        ];
        for i in 0..subs {
            tracks.push(json!({"id": 2 + i, "type": "subtitles", "properties": {}}));
        }
        json!({"tracks": tracks, "attachments": vec![json!({}); fonts]})
    }

    #[test]
    fn picks_audio_track_by_language() {
        let info = json!({"tracks": [
            {"id": 0, "type": "video", "properties": {}},
            {"id": 1, "type": "audio", "properties": {"language": "eng", "language_ietf": "en"}},
            {"id": 2, "type": "audio", "properties": {"language": "jpn", "language_ietf": "ja"}},
        ]});
        assert_eq!(pick_audio_track(&info, "jpn").unwrap(), 2);
        assert_eq!(pick_audio_track(&info, "ja").unwrap(), 2);
        assert!(pick_audio_track(&info, "ind").is_err());
    }

    #[test]
    fn verifies_finished_files() {
        assert!(check_output(&mkv("848x480", "jpn", 1, 0), 1, "jpn", "848x480").is_empty());
        assert!(check_output(&mkv("848x476", "jpn", 1, 0), 1, "jpn", "").is_empty());

        let bad = check_output(&mkv("848x476", "eng", 2, 3), 1, "jpn", "848x480");
        assert_eq!(
            bad,
            [
                "resolution=848x476 (expected 848x480)",
                "audio language=eng (expected jpn)",
                "subtitles=2 (expected 1)",
                "fonts=3",
            ]
        );
    }

    fn touch(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn plans_episodes_with_matching_subs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path();
        touch(&src.join("[Cleo]Show_-_09_(BD1080p).mkv"), "");
        touch(&src.join("[Cleo]Show_-_10_(BD1080p).mkv"), "");
        touch(&src.join("[Grp] Show - OVA 1 [576p].mkv"), "");
        touch(&src.join("_Sub__Show_BD___10_480p.S0.id.ass"), "x");
        touch(&src.join("_Sub__Show_BD___09_480p.S0.id.ass"), "");
        touch(&src.join("_Sub__Show_OVA01_480p.id.ass"), "x");
        touch(&src.join("notes.txt"), "x");

        let dest = Path::new("out");
        let plan = build_plan(src, dest, "[P]", "Show", "T", &[]).unwrap();
        let summary: Vec<String> = plan
            .iter()
            .map(|e| format!("{}:{}", e.id, e.subs.len()))
            .collect();
        // Plan follows file-name order; empty subtitle files are ignored, so 09 has none.
        assert_eq!(summary, ["09:0", "10:1", "OVA 1:1"]);
        assert_eq!(plan[1].target, dest.join("[P] Show - 10 (T).mkv"));

        let only = episode::expand_selection("10").unwrap();
        let picked = build_plan(src, dest, "[P]", "Show", "T", &only).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].id.to_string(), "10");
        assert!(build_plan(src, dest, "[P]", "Show", "T", &["99".to_string()]).is_err());
    }

    #[test]
    fn rejects_duplicate_episode_numbers() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Show - 01 [a].mkv"), "");
        touch(&dir.path().join("Show - 01 [b].mp4"), "");
        let err = build_plan(dir.path(), Path::new("out"), "", "Show", "T", &[]).unwrap_err();
        assert!(err.to_string().contains("share episode 01"), "{err}");
    }

    #[test]
    fn resolves_templates_loosely() {
        let dir = tempfile::tempdir().unwrap();
        let tpl = dir.path().join("Settings").join("Templates");
        std::fs::create_dir_all(&tpl).unwrap();
        touch(&tpl.join("Anime-480p.AV1.Opus96.srip"), "");
        touch(&tpl.join("Anime-720p.AV1.Opus96.srip"), "");

        assert_eq!(
            resolve_template(dir.path(), "Anime480p.AV1.Opus96").unwrap(),
            "Anime-480p.AV1.Opus96"
        );
        let err = resolve_template(dir.path(), "nope")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Anime-720p.AV1.Opus96"), "{err}");
        assert!(resolve_template(&dir.path().join("missing"), "x").is_err());
    }
}
