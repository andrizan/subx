//! Native encode engine: ffmpeg decodes and scales (zimg `zscale`, the same
//! library behind VapourSynth's `resize.Spline36`) into a y4m pipe consumed by
//! SVT-AV1 or x265, the audio track is encoded concurrently, mkvmerge muxes
//! everything with chapters and the source's audio delay, and `mkvmerge -J`
//! verifies the result. Nothing touches the source file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use super::{Episode, check_output, file_name, mkv_info, pick_audio_track, sub_language};
use crate::common::config::{EncodeConfig, Profile};
use crate::common::tools::{self, Tool};
use crate::media::probe;

/// Audio encoder chosen for a profile's codec, in `[encode] aac_encoders` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioEncoder {
    Opusenc(PathBuf),
    FfmpegLibopus,
    Qaac(PathBuf),
    FfmpegFdk,
    Fdkaac(PathBuf),
    FfmpegAac,
}

impl AudioEncoder {
    fn label(&self) -> &'static str {
        match self {
            AudioEncoder::Opusenc(_) => "opusenc",
            AudioEncoder::FfmpegLibopus => "ffmpeg libopus",
            AudioEncoder::Qaac(_) => "qaac",
            AudioEncoder::FfmpegFdk => "ffmpeg libfdk_aac",
            AudioEncoder::Fdkaac(_) => "fdkaac",
            AudioEncoder::FfmpegAac => "ffmpeg aac",
        }
    }

    fn extension(&self) -> &'static str {
        match self {
            AudioEncoder::Opusenc(_) | AudioEncoder::FfmpegLibopus => "opus",
            _ => "m4a",
        }
    }
}

/// Everything the native engine resolves once per run.
#[derive(Debug, Clone)]
pub struct Native {
    pub profile_name: String,
    pub profile: Profile,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub mkvmerge: PathBuf,
    pub mkvextract: PathBuf,
    pub video_encoder: PathBuf,
    pub audio: AudioEncoder,
    pub audio_lang: String,
    pub sub_lang: String,
    pub stall: Duration,
}

/// One external command. `Pipe` streams the producer's stdout into the consumer.
#[derive(Debug, Clone)]
pub enum Step {
    Run {
        name: &'static str,
        program: PathBuf,
        args: Vec<String>,
    },
    Pipe {
        name: &'static str,
        producer: (PathBuf, Vec<String>),
        consumer: (PathBuf, Vec<String>),
        /// File the consumer writes; its growth feeds the stall watchdog.
        output: PathBuf,
    },
}

fn quote(s: &str) -> String {
    if s.is_empty() || s.contains([' ', '"']) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn render_cmd(program: &Path, args: &[String]) -> String {
    let mut out = quote(&program.display().to_string());
    for a in args {
        out.push(' ');
        out.push_str(&quote(a));
    }
    out
}

impl Step {
    pub fn name(&self) -> &'static str {
        match self {
            Step::Run { name, .. } | Step::Pipe { name, .. } => name,
        }
    }

    /// Shell-like rendering for `--dry-run` and the per-episode log.
    pub fn render(&self) -> String {
        match self {
            Step::Run { program, args, .. } => render_cmd(program, args),
            Step::Pipe {
                producer, consumer, ..
            } => format!(
                "{} | {}",
                render_cmd(&producer.0, &producer.1),
                render_cmd(&consumer.0, &consumer.1)
            ),
        }
    }
}

/// Facts about one source file that shape its commands.
#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub audio_track: u64,
    /// ISO 639-2 code mkvmerge reports for that track.
    pub audio_lang: String,
    /// Audio start offset the mux must keep (`--sync`).
    pub delay_ms: i64,
    pub width: u32,
    pub height: u32,
    /// `24000/1001`.
    pub fps: String,
    pub duration_s: f64,
    /// Chapter entries in the source (0 = none); the output must carry the same number.
    pub chapter_entries: u64,
}

/// Total chapter entries across all editions of a `mkvmerge -J` report.
fn chapter_entries(info: &Value) -> u64 {
    info["chapters"].as_array().map_or(0, |c| {
        c.iter()
            .map(|e| e["num_entries"].as_u64().unwrap_or(0))
            .sum()
    })
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Display aspect ratio from the picture size (square pixels assumed).
fn dar(width: u32, height: u32) -> String {
    let g = gcd(width, height).max(1);
    format!("{}/{}", width / g, height / g)
}

/// Colour signalling per profile: `(svt args, x265 args)`.
fn color_args(color: &str) -> Result<(Vec<String>, Vec<String>)> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    Ok(match color {
        "bt709" => (
            s(&[
                "--color-primaries",
                "1",
                "--transfer-characteristics",
                "1",
                "--matrix-coefficients",
                "1",
            ]),
            s(&[
                "--colorprim",
                "bt709",
                "--transfer",
                "bt709",
                "--colormatrix",
                "bt709",
            ]),
        ),
        "bt601" => (
            s(&[
                "--color-primaries",
                "6",
                "--transfer-characteristics",
                "6",
                "--matrix-coefficients",
                "6",
            ]),
            s(&[
                "--colorprim",
                "smpte170m",
                "--transfer",
                "smpte170m",
                "--colormatrix",
                "smpte170m",
            ]),
        ),
        "source" => (Vec::new(), Vec::new()),
        other => bail!("profile color '{other}' unknown (bt709 | bt601 | source)"),
    })
}

/// `left,top,right,bottom` crop or `none`.
fn crop_filter(spec: &str) -> Result<Option<String>> {
    let spec = spec.trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    if spec.eq_ignore_ascii_case("auto") {
        bail!("crop = \"auto\" is not implemented yet; use none or left,top,right,bottom");
    }
    let v: Vec<u32> = spec
        .split(',')
        .map(|n| n.trim().parse::<u32>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("crop '{spec}' must be none or left,top,right,bottom"))?;
    let [l, t, r, b] = v[..] else {
        bail!("crop '{spec}' must have four values");
    };
    if l + t + r + b == 0 {
        return Ok(None);
    }
    Ok(Some(format!("crop=iw-{}:ih-{}:{l}:{t}", l + r, t + b)))
}

/// Encoder names from `ffmpeg -encoders`.
async fn ffmpeg_encoders(ffmpeg: &Path) -> Result<HashSet<String>> {
    let out = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| "cannot run ffmpeg -encoders")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .collect())
}

/// Pick the first usable encoder for `codec` following `order`.
fn pick_audio_encoder(
    codec: &str,
    order: &[String],
    ffmpeg_encoders: &HashSet<String>,
) -> Result<AudioEncoder> {
    let resolver = tools::resolver();
    let mut tried = Vec::new();
    match codec {
        "opus" => {
            if let Some((p, _)) = resolver.locate(Tool::Opusenc) {
                return Ok(AudioEncoder::Opusenc(p));
            }
            if ffmpeg_encoders.contains("libopus") {
                return Ok(AudioEncoder::FfmpegLibopus);
            }
            bail!(
                "no Opus encoder: install opusenc (subx tools install opusenc) or an ffmpeg with libopus"
            );
        }
        "aac-lc" | "he-aac" | "he-aac-v2" => {
            for name in order {
                match name.as_str() {
                    "qaac" if codec != "he-aac-v2" => match resolver.probe(Tool::Qaac) {
                        Some(f) if f.usable => return Ok(AudioEncoder::Qaac(f.path)),
                        Some(f) => tried.push(format!(
                            "qaac ({})",
                            f.detail.as_deref().unwrap_or("unusable")
                        )),
                        None => tried.push("qaac (not found; subx tools install qaac)".into()),
                    },
                    "qaac" => tried.push("qaac (Apple's encoder has no HE-AACv2)".into()),
                    "ffmpeg-fdk" => {
                        if ffmpeg_encoders.contains("libfdk_aac") {
                            return Ok(AudioEncoder::FfmpegFdk);
                        }
                        tried.push("ffmpeg libfdk_aac (not in this ffmpeg build)".into());
                    }
                    "fdkaac" => {
                        if let Some((p, _)) = resolver.locate(Tool::Fdkaac) {
                            return Ok(AudioEncoder::Fdkaac(p));
                        }
                        tried.push("fdkaac (not found)".into());
                    }
                    "ffmpeg-aac" if codec == "aac-lc" => {
                        if ffmpeg_encoders.contains("aac") {
                            return Ok(AudioEncoder::FfmpegAac);
                        }
                        tried.push("ffmpeg aac (not in this ffmpeg build)".into());
                    }
                    "ffmpeg-aac" => tried.push("ffmpeg aac (AAC-LC only)".into()),
                    other => tried.push(format!("{other} (unknown entry in aac_encoders)")),
                }
            }
            bail!(
                "no encoder for {codec}; tried: {}. See `subx tools check`.",
                tried.join(", ")
            );
        }
        other => bail!("audio codec '{other}' unknown (opus | aac-lc | he-aac | he-aac-v2)"),
    }
}

impl Native {
    /// Resolve profile, tools and encoders before any episode starts.
    pub async fn setup(
        cfg: &EncodeConfig,
        profile_name: &str,
        audio_lang: &str,
        sub_lang: &str,
        stall_min: u64,
    ) -> Result<Self> {
        let profile = cfg.profiles.get(profile_name).cloned().with_context(|| {
            format!(
                "profile '{profile_name}' not found in [encode.profiles]; known: {}",
                if cfg.profiles.is_empty() {
                    "none".to_string()
                } else {
                    cfg.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            )
        })?;
        if profile.source != "ffmpeg" {
            bail!(
                "profile source '{}' is not implemented yet (ffmpeg only)",
                profile.source
            );
        }
        if (profile.width == 0) != (profile.height == 0) {
            bail!("profile width and height must both be set or both be 0");
        }
        color_args(&profile.color)?;
        crop_filter(&profile.crop)?;
        let encoder_tool = match profile.encoder.as_str() {
            "svt-av1" => Tool::SvtAv1,
            "x265" => Tool::X265,
            other => bail!("profile encoder '{other}' unknown (svt-av1 | x265)"),
        };

        let mut paths = tools::require(&[
            Tool::Ffmpeg,
            Tool::Ffprobe,
            Tool::Mkvmerge,
            Tool::Mkvextract,
            encoder_tool,
        ])?;
        let video_encoder = paths.remove(4);
        let mkvextract = paths.remove(3);
        let mkvmerge = paths.remove(2);
        let ffprobe = paths.remove(1);
        let ffmpeg = paths.remove(0);

        let encoders = ffmpeg_encoders(&ffmpeg).await?;
        let audio = pick_audio_encoder(&profile.audio.codec, &cfg.aac_encoders, &encoders)?;

        Ok(Self {
            profile_name: profile_name.to_string(),
            profile,
            ffmpeg,
            ffprobe,
            mkvmerge,
            mkvextract,
            video_encoder,
            audio,
            audio_lang: audio_lang.to_string(),
            sub_lang: sub_lang.to_string(),
            stall: Duration::from_secs(stall_min.max(1) * 60),
        })
    }

    /// `WxH` the verifier expects, or empty when the source size is kept.
    pub fn resolution(&self) -> String {
        if self.profile.width == 0 {
            String::new()
        } else {
            format!("{}x{}", self.profile.width, self.profile.height)
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "engine native, profile {} ({}, {} {} kbps via {}), {}",
            self.profile_name,
            self.profile.encoder,
            self.profile.audio.codec,
            self.profile.audio.bitrate,
            self.audio.label(),
            if self.profile.width == 0 {
                "source size".to_string()
            } else {
                self.resolution()
            }
        )
    }

    /// Read what the source needs us to know.
    pub async fn inspect(&self, video: &Path) -> Result<SourceInfo> {
        let mkv = mkv_info(&self.mkvmerge, video).await?;
        let audio_track = pick_audio_track(&mkv, &self.audio_lang)
            .with_context(|| format!("while reading {}", video.display()))?;
        let track = mkv["tracks"]
            .as_array()
            .and_then(|t| t.iter().find(|t| t["id"].as_u64() == Some(audio_track)))
            .cloned()
            .unwrap_or(Value::Null);
        let audio_lang = track["properties"]["language"]
            .as_str()
            .unwrap_or(&self.audio_lang)
            .to_string();
        let delay_ms = track["properties"]["minimum_timestamp"]
            .as_i64()
            .map_or(0, |ns| (ns as f64 / 1_000_000.0).round() as i64);
        let chapter_entries = chapter_entries(&mkv);

        let full = probe::probe_full(&self.ffprobe, video).await?;
        let v = full
            .streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("video"))
            .context("no video stream")?;
        let width = probe::val_f64(v.width.as_ref()).unwrap_or(0.0) as u32;
        let height = probe::val_f64(v.height.as_ref()).unwrap_or(0.0) as u32;
        let fps = v
            .avg_frame_rate
            .clone()
            .filter(|f| f != "0/0")
            .unwrap_or_else(|| "24000/1001".to_string());
        let duration_s = full
            .format
            .as_ref()
            .and_then(|f| probe::val_f64(f.duration.as_ref()))
            .unwrap_or(0.0);
        Ok(SourceInfo {
            audio_track,
            audio_lang,
            delay_ms,
            width,
            height,
            fps,
            duration_s,
            chapter_entries,
        })
    }

    /// Every command for one episode, in execution order (video and audio run
    /// concurrently; chapters and mux follow).
    pub fn steps(&self, ep: &Episode, src: &SourceInfo, work: &Path) -> Result<Vec<Step>> {
        let p = &self.profile;
        let s = |x: &str| x.to_string();
        let path = |x: &Path| x.display().to_string();
        let source = path(&ep.video);

        // --- video -----------------------------------------------------------
        let mut filters: Vec<String> = Vec::new();
        if let Some(crop) = crop_filter(&p.crop)? {
            filters.push(crop);
        }
        if p.width > 0 {
            filters.push(format!(
                "zscale=w={}:h={}:filter=spline36",
                p.width, p.height
            ));
        }
        // zscale keeps the display aspect by changing the SAR; the mux sets
        // the display size instead, exactly as StaxRip does.
        filters.push(s("setsar=1"));
        let mut producer = vec![
            s("-v"),
            s("error"),
            s("-nostdin"),
            s("-i"),
            source.clone(),
            s("-map"),
            s("0:v:0"),
            s("-vf"),
            filters.join(","),
            s("-pix_fmt"),
            s("yuv420p10le"),
            s("-f"),
            s("yuv4mpegpipe"),
            s("-strict"),
            s("-1"),
            s("-"),
        ];
        let (svt_color, x265_color) = color_args(&p.color)?;
        let (video_out, mut consumer) = match p.encoder.as_str() {
            "svt-av1" => {
                let out = work.join("video.ivf");
                let mut a = vec![
                    s("-i"),
                    s("-"),
                    s("-b"),
                    path(&out),
                    s("--progress"),
                    s("2"),
                    s("--color-range"),
                    s("0"),
                    s("--chroma-sample-position"),
                    s("left"),
                ];
                a.extend(svt_color);
                (out, a)
            }
            _ => {
                let out = work.join("video.hevc");
                let mut a = vec![
                    s("--y4m"),
                    s("--input"),
                    s("-"),
                    s("--output"),
                    path(&out),
                    s("--output-depth"),
                    s("10"),
                    s("--range"),
                    s("limited"),
                    s("--log-level"),
                    s("info"),
                ];
                a.extend(x265_color);
                (out, a)
            }
        };
        consumer.extend(p.video.iter().cloned());
        producer.shrink_to_fit();
        let video = Step::Pipe {
            name: "video",
            producer: (self.ffmpeg.clone(), producer),
            consumer: (self.video_encoder.clone(), consumer),
            output: video_out.clone(),
        };

        // --- audio -----------------------------------------------------------
        let audio_out = work.join(format!("audio.{}", self.audio.extension()));
        let kbps = p.audio.bitrate.to_string();
        let decode = |fmt: &str| {
            vec![
                s("-v"),
                s("error"),
                s("-nostdin"),
                s("-i"),
                source.clone(),
                s("-map"),
                format!("0:{}", src.audio_track),
                s("-vn"),
                s("-sn"),
                s("-dn"),
                s("-f"),
                s(fmt),
                s("-"),
            ]
        };
        let encode_direct = |codec_args: Vec<String>| {
            let mut a = vec![
                s("-v"),
                s("error"),
                s("-nostdin"),
                s("-y"),
                s("-i"),
                source.clone(),
                s("-map"),
                format!("0:{}", src.audio_track),
                s("-vn"),
                s("-sn"),
                s("-dn"),
                // Keep the source's chapters/tags out of the audio file: the
                // mux adds the chapters itself, and MP4 audio would otherwise
                // carry a second copy.
                s("-map_chapters"),
                s("-1"),
                s("-map_metadata"),
                s("-1"),
            ];
            a.extend(codec_args);
            a.push(path(&audio_out));
            a
        };
        let fdk_profile = match p.audio.codec.as_str() {
            "he-aac" => ("aac_he", "5"),
            "he-aac-v2" => ("aac_he_v2", "29"),
            _ => ("aac_low", "2"),
        };
        let audio = match &self.audio {
            AudioEncoder::Opusenc(exe) => Step::Pipe {
                name: "audio",
                producer: (self.ffmpeg.clone(), decode("flac")),
                consumer: (
                    exe.clone(),
                    vec![
                        s("--vbr"),
                        s("--bitrate"),
                        kbps.clone(),
                        s("--no-phase-inv"),
                        s("-"),
                        path(&audio_out),
                    ],
                ),
                output: audio_out.clone(),
            },
            AudioEncoder::FfmpegLibopus => Step::Run {
                name: "audio",
                program: self.ffmpeg.clone(),
                args: encode_direct(vec![
                    s("-c:a"),
                    s("libopus"),
                    s("-b:a"),
                    format!("{kbps}k"),
                    s("-vbr"),
                    s("on"),
                    s("-apply_phase_inv"),
                    s("0"),
                ]),
            },
            AudioEncoder::Qaac(exe) => {
                let mut a = vec![s("--ignorelength")];
                if p.audio.codec == "he-aac" {
                    a.extend([s("--he"), s("--cbr"), kbps.clone()]);
                } else {
                    a.extend([s("--cvbr"), kbps.clone()]);
                }
                a.extend([s("-o"), path(&audio_out), s("-")]);
                Step::Pipe {
                    name: "audio",
                    producer: (self.ffmpeg.clone(), decode("wav")),
                    consumer: (exe.clone(), a),
                    output: audio_out.clone(),
                }
            }
            AudioEncoder::FfmpegFdk => Step::Run {
                name: "audio",
                program: self.ffmpeg.clone(),
                args: encode_direct(vec![
                    s("-c:a"),
                    s("libfdk_aac"),
                    s("-profile:a"),
                    s(fdk_profile.0),
                    s("-b:a"),
                    format!("{kbps}k"),
                ]),
            },
            AudioEncoder::Fdkaac(exe) => Step::Pipe {
                name: "audio",
                producer: (self.ffmpeg.clone(), decode("wav")),
                consumer: (
                    exe.clone(),
                    vec![
                        s("--ignorelength"),
                        s("-p"),
                        s(fdk_profile.1),
                        s("-b"),
                        kbps.clone(),
                        s("-o"),
                        path(&audio_out),
                        s("-"),
                    ],
                ),
                output: audio_out.clone(),
            },
            AudioEncoder::FfmpegAac => Step::Run {
                name: "audio",
                program: self.ffmpeg.clone(),
                args: encode_direct(vec![s("-c:a"), s("aac"), s("-b:a"), format!("{kbps}k")]),
            },
        };

        // --- chapters + mux --------------------------------------------------
        let mut steps = vec![video, audio];
        let chapters = work.join("chapters.xml");
        if src.chapter_entries > 0 {
            steps.push(Step::Run {
                name: "chapters",
                program: self.mkvextract.clone(),
                args: vec![source.clone(), s("chapters"), path(&chapters)],
            });
        }
        let mut mux = vec![
            s("-o"),
            path(&ep.target),
            s("--aspect-ratio"),
            format!("0:{}", dar(src.width, src.height)),
        ];
        if video_out.extension().is_some_and(|e| e == "hevc") {
            // Raw HEVC carries no timing; mkvmerge needs the frame rate.
            mux.extend([s("--default-duration"), format!("0:{}fps", src.fps)]);
        }
        mux.push(path(&video_out));
        mux.extend([
            // Whatever container the audio encoder wrote, only its track is wanted.
            s("--no-chapters"),
            s("--no-global-tags"),
            s("--language"),
            format!("0:{}", src.audio_lang),
            s("--sync"),
            format!("0:{}", src.delay_ms),
            s("--default-track-flag"),
            s("0:1"),
            path(&audio_out),
        ]);
        for (i, sub) in ep.subs.iter().enumerate() {
            mux.extend([
                s("--language"),
                format!("0:{}", sub_language(sub, &self.sub_lang)),
                s("--default-track-flag"),
                format!("0:{}", u8::from(i == 0)),
                path(sub),
            ]);
        }
        if src.chapter_entries > 0 {
            mux.extend([s("--chapters"), path(&chapters)]);
        }
        steps.push(Step::Run {
            name: "mux",
            program: self.mkvmerge.clone(),
            args: mux,
        });
        Ok(steps)
    }
}

/// Append a line to the episode log (best effort; logging never fails a job).
fn log_line(log: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = writeln!(f, "{line}");
    }
}

async fn drain(reader: Option<impl AsyncReadExt + Unpin>) -> String {
    let Some(mut r) = reader else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

/// Run one step; stdout/stderr go to the log. Pipes are watched: when the
/// consumer's output file stops growing for `stall`, both processes are killed.
async fn run_step(step: &Step, stall: Duration, log: &Path, tag: &str) -> Result<()> {
    let started = Instant::now();
    log_line(
        log,
        &format!("\n### {} ({tag})\n{}", step.name(), step.render()),
    );
    match step {
        Step::Run { program, args, .. } => {
            let out = tokio::process::Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .output()
                .await
                .with_context(|| format!("cannot start {}", program.display()))?;
            log_line(log, &String::from_utf8_lossy(&out.stdout));
            log_line(log, &String::from_utf8_lossy(&out.stderr));
            // mkvmerge exits 1 for warnings; only 2+ (or signals) are failures.
            let code = out.status.code().unwrap_or(-1);
            let ok = if step.name() == "mux" {
                (0..=1).contains(&code)
            } else {
                code == 0
            };
            if !ok {
                bail!(
                    "{} failed (exit={code}): {}",
                    step.name(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
        Step::Pipe {
            producer,
            consumer,
            output,
            ..
        } => {
            let mut prod = tokio::process::Command::new(&producer.0)
                .args(&producer.1)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("cannot start {}", producer.0.display()))?;
            let feed: Stdio = prod
                .stdout
                .take()
                .context("producer stdout missing")?
                .try_into()
                .context("cannot connect pipe")?;
            let mut cons = tokio::process::Command::new(&consumer.0)
                .args(&consumer.1)
                .stdin(feed)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("cannot start {}", consumer.0.display()))?;
            let prod_err = tokio::spawn(drain(prod.stderr.take()));
            let cons_out = tokio::spawn(drain(cons.stdout.take()));
            let cons_err = tokio::spawn(drain(cons.stderr.take()));

            let mut last_size = 0u64;
            let mut last_change = Instant::now();
            let status = loop {
                tokio::select! {
                    s = cons.wait() => break s.with_context(|| "waiting for the encoder failed")?,
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {
                        let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
                        if size > last_size {
                            last_size = size;
                            last_change = Instant::now();
                        } else if last_change.elapsed() >= stall {
                            let _ = cons.kill().await;
                            let _ = prod.kill().await;
                            bail!(
                                "{}: {} stopped growing for {} min, killed",
                                step.name(),
                                file_name(output).unwrap_or("output"),
                                stall.as_secs() / 60
                            );
                        }
                    }
                }
            };
            let prod_status = prod.wait().await?;
            let (pe, co, ce) = (
                prod_err.await.unwrap_or_default(),
                cons_out.await.unwrap_or_default(),
                cons_err.await.unwrap_or_default(),
            );
            log_line(log, &co);
            log_line(log, &ce);
            if !pe.trim().is_empty() {
                log_line(log, &format!("[producer stderr]\n{pe}"));
            }
            if !status.success() {
                bail!(
                    "{} encoder failed (exit={}): {}",
                    step.name(),
                    status.code().unwrap_or(-1),
                    ce.lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                );
            }
            if !prod_status.success() {
                bail!(
                    "{} decoder failed (exit={}): {}",
                    step.name(),
                    prod_status.code().unwrap_or(-1),
                    pe.trim()
                );
            }
        }
    }
    log_line(
        log,
        &format!(
            "### {} done in {:.1}s",
            step.name(),
            started.elapsed().as_secs_f64()
        ),
    );
    Ok(())
}

/// Encode, mux and verify one episode in `work`; the log stays in `work/encode.log`.
pub async fn encode_one(n: &Native, ep: &Episode, work: &Path) -> Result<()> {
    if work.exists() {
        std::fs::remove_dir_all(work)
            .with_context(|| format!("cannot clear {}", work.display()))?;
    }
    std::fs::create_dir_all(work).with_context(|| format!("cannot create {}", work.display()))?;
    if let Some(parent) = ep.target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let log = work.join("encode.log");
    log_line(
        &log,
        &format!(
            "subx {} native encode\nsource: {}\ntarget: {}\n{}",
            env!("CARGO_PKG_VERSION"),
            ep.video.display(),
            ep.target.display(),
            n.describe()
        ),
    );

    let src = n.inspect(&ep.video).await?;
    let steps = n.steps(ep, &src, work)?;
    let tag = ep.id.to_string();
    let (video, audio, rest) = (&steps[0], &steps[1], &steps[2..]);
    tokio::try_join!(
        run_step(video, n.stall, &log, &tag),
        run_step(audio, n.stall, &log, &tag)
    )?;
    for step in rest {
        run_step(step, n.stall, &log, &tag).await?;
    }

    if !ep.target.is_file() {
        bail!("output file was not created");
    }
    let info = mkv_info(&n.mkvmerge, &ep.target).await?;
    let mut problems = check_output(&info, ep.subs.len(), &n.audio_lang, &n.resolution());
    if let Some(ns) = info["container"]["properties"]["duration"].as_f64() {
        let got = ns / 1e9;
        if src.duration_s > 0.0 && (got - src.duration_s).abs() > 1.0 {
            problems.push(format!(
                "duration={got:.2}s (source {:.2}s)",
                src.duration_s
            ));
        }
    }
    let got_chapters = chapter_entries(&info);
    if got_chapters != src.chapter_entries {
        problems.push(format!(
            "chapters={got_chapters} (source {})",
            src.chapter_entries
        ));
    }
    if !problems.is_empty() {
        bail!("output does not match: {}", problems.join("; "));
    }
    log_line(&log, "### verified");
    Ok(())
}

/// Print the plan's command lines without running anything.
pub async fn dry_run(n: &Native, plan: &[Episode], work_root: &Path) -> Result<()> {
    println!("{}", n.describe());
    for ep in plan.iter().filter(|e| !e.target.exists()) {
        let work = work_root.join(ep.id.key());
        let src = n.inspect(&ep.video).await?;
        println!(
            "\n[{}] audio track {} ({}, +{} ms), {}x{} @ {} fps, chapters: {}",
            ep.id,
            src.audio_track,
            src.audio_lang,
            src.delay_ms,
            src.width,
            src.height,
            src.fps,
            src.chapter_entries
        );
        for step in n.steps(ep, &src, &work)? {
            println!("[{}] {}: {}", ep.id, step.name(), step.render());
        }
    }
    Ok(())
}

/// Encode every pending episode, `jobs` at a time; returns the failure count.
pub async fn run_all(n: &Native, plan: &[Episode], work_root: &Path, jobs: usize) -> Result<usize> {
    println!("{}", n.describe());
    let pending: Vec<&Episode> = plan.iter().filter(|e| !e.target.exists()).collect();
    let skipped = plan.len() - pending.len();
    let logs_dir = work_root.join("logs");
    std::fs::create_dir_all(&logs_dir).ok();

    let results: Vec<(String, Result<Duration>)> = futures::stream::iter(pending)
        .map(|ep| {
            let logs_dir = logs_dir.clone();
            async move {
                let work = work_root.join(ep.id.key());
                let started = Instant::now();
                println!(
                    "[{}] start {}",
                    ep.id,
                    file_name(&ep.video).unwrap_or_default()
                );
                let res = encode_one(n, ep, &work).await;
                match &res {
                    Ok(()) => {
                        // Keep the log, drop the intermediates.
                        let kept = logs_dir.join(format!("{}.log", ep.id.key()));
                        std::fs::copy(work.join("encode.log"), &kept).ok();
                        std::fs::remove_dir_all(&work).ok();
                        println!(
                            "[OK] {} -> {} ({:.0}s)",
                            ep.id,
                            ep.target.display(),
                            started.elapsed().as_secs_f64()
                        );
                    }
                    Err(e) => {
                        println!("[FAILED] {} -> {e:#}", ep.id);
                        println!(
                            "[{}] work files and encode.log kept in {}",
                            ep.id,
                            work.display()
                        );
                    }
                }
                (ep.id.to_string(), res.map(|()| started.elapsed()))
            }
        })
        .buffer_unordered(jobs.max(1))
        .collect()
        .await;

    let failed = results.iter().filter(|(_, r)| r.is_err()).count();
    println!("--------------------------------------------------");
    println!("Episodes : {}", plan.len());
    println!("Encoded  : {}", results.len() - failed);
    println!("Skipped  : {skipped}");
    println!("Failed   : {failed}");
    Ok(failed)
}

/// Differences between two `mkvmerge -J` reports that matter for parity:
/// track layout, codecs, sizes, languages, flags, delays, fonts, chapters,
/// duration (1 s tolerance). Empty means the containers are equivalent.
#[cfg(test)]
pub fn container_diff(a: &Value, b: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let tracks = |v: &Value| v["tracks"].as_array().cloned().unwrap_or_default();
    let (ta, tb) = (tracks(a), tracks(b));
    if ta.len() != tb.len() {
        out.push(format!("track count {} vs {}", ta.len(), tb.len()));
        return out;
    }
    for (i, (x, y)) in ta.iter().zip(&tb).enumerate() {
        for key in ["type", "codec"] {
            if x[key] != y[key] {
                out.push(format!("track {i} {key}: {} vs {}", x[key], y[key]));
            }
        }
        for key in [
            "pixel_dimensions",
            "display_dimensions",
            "language",
            "default_track",
            "forced_track",
            "minimum_timestamp",
            "audio_channels",
            "audio_sampling_frequency",
        ] {
            if x["properties"][key] != y["properties"][key] {
                out.push(format!(
                    "track {i} {key}: {} vs {}",
                    x["properties"][key], y["properties"][key]
                ));
            }
        }
    }
    let fonts = |v: &Value| v["attachments"].as_array().map_or(0, Vec::len);
    if fonts(a) != fonts(b) {
        out.push(format!("attachments {} vs {}", fonts(a), fonts(b)));
    }
    let chapters = |v: &Value| {
        v["chapters"].as_array().map_or(0, |c| {
            c.iter()
                .map(|e| e["num_entries"].as_u64().unwrap_or(0))
                .sum::<u64>()
        })
    };
    if chapters(a) != chapters(b) {
        out.push(format!("chapters {} vs {}", chapters(a), chapters(b)));
    }
    let duration = |v: &Value| {
        v["container"]["properties"]["duration"]
            .as_f64()
            .map(|ns| ns / 1e9)
    };
    if let (Some(x), Some(y)) = (duration(a), duration(b))
        && (x - y).abs() > 1.0
    {
        out.push(format!("duration {x:.2}s vs {y:.2}s"));
    }
    out
}

/// `All:` value from ffmpeg's `ssim` filter summary line.
#[cfg(test)]
pub fn parse_ssim(text: &str) -> Option<f64> {
    text.lines().filter(|l| l.contains("SSIM")).find_map(|l| {
        l.split_whitespace()
            .find_map(|t| t.strip_prefix("All:"))
            .and_then(|v| v.parse().ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::AudioSpec;
    use crate::common::episode::EpisodeId;
    use serde_json::json;

    #[test]
    fn container_diff_reports_what_matters() {
        let a = json!({
            "container": {"properties": {"duration": 1421087000000u64}},
            "tracks": [
                {"type": "video", "codec": "AV1", "properties": {"pixel_dimensions": "848x480", "display_dimensions": "853x480"}},
                {"type": "audio", "codec": "Opus", "properties": {"language": "jpn", "default_track": true, "minimum_timestamp": 9000000}},
                {"type": "subtitles", "codec": "SubStationAlpha", "properties": {"language": "ind", "default_track": true}},
            ],
            "attachments": [],
            "chapters": [{"num_entries": 6}],
        });
        assert!(container_diff(&a, &a).is_empty());

        let mut b = a.clone();
        b["tracks"][0]["properties"]["pixel_dimensions"] = json!("848x476");
        b["tracks"][1]["properties"]["language"] = json!("eng");
        b["attachments"] = json!([{}, {}]);
        b["container"]["properties"]["duration"] = json!(1419000000000u64);
        let diff = container_diff(&a, &b);
        assert_eq!(diff.len(), 4, "{diff:?}");
        assert!(diff[0].contains("pixel_dimensions") && diff[1].contains("language"));
        assert!(diff[2].starts_with("attachments 0 vs 2") && diff[3].starts_with("duration"));

        let mut c = a.clone();
        c["tracks"].as_array_mut().unwrap().pop();
        assert_eq!(container_diff(&a, &c), ["track count 3 vs 2"]);
    }

    #[test]
    fn parses_ssim_summary() {
        let text = "frame= 34072 fps=2500\n[Parsed_ssim_0 @ 0x1] SSIM Y:0.998712 (28.90) U:0.999301 (31.55) V:0.999280 (31.43) All:0.998901 (29.59)\n";
        assert_eq!(parse_ssim(text), Some(0.998901));
        assert_eq!(parse_ssim("nothing here"), None);
    }

    /// Parity against a reference encode (for example StaxRip's output of the
    /// same episode). Needs ffmpeg and mkvmerge plus two files in
    /// `SUBX_PARITY_NATIVE` and `SUBX_PARITY_REFERENCE`; run explicitly:
    /// `cargo test -- --ignored parity`. Identical stream MD5s pass outright;
    /// otherwise the decoded video must reach SSIM >= 0.99.
    #[tokio::test]
    #[ignore = "needs ffmpeg/mkvmerge and SUBX_PARITY_NATIVE / SUBX_PARITY_REFERENCE files"]
    async fn parity_with_reference_encode() {
        let (Ok(native), Ok(reference)) = (
            std::env::var("SUBX_PARITY_NATIVE"),
            std::env::var("SUBX_PARITY_REFERENCE"),
        ) else {
            eprintln!("SUBX_PARITY_NATIVE / SUBX_PARITY_REFERENCE not set; nothing compared");
            return;
        };
        let mkvmerge = tools::require_one(Tool::Mkvmerge).unwrap();
        let (ffmpeg, _) = tools::ffmpeg_pair().unwrap();
        let (a, b) = (Path::new(&native), Path::new(&reference));

        let diff = container_diff(
            &mkv_info(&mkvmerge, a).await.unwrap(),
            &mkv_info(&mkvmerge, b).await.unwrap(),
        );
        assert!(diff.is_empty(), "container differences: {diff:?}");

        let md5 = |file: &Path, map: &str| {
            let out = std::process::Command::new(&ffmpeg)
                .args(["-v", "error", "-i"])
                .arg(file)
                .args(["-map", map, "-c", "copy", "-f", "md5", "-"])
                .output()
                .expect("ffmpeg md5");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let same_audio = md5(a, "0:a:0") == md5(b, "0:a:0");
        if md5(a, "0:v:0") == md5(b, "0:v:0") {
            eprintln!("video streams are bit-identical (audio identical: {same_audio})");
            return;
        }

        let out = std::process::Command::new(&ffmpeg)
            .args(["-v", "info", "-i"])
            .arg(a)
            .arg("-i")
            .arg(b)
            .args(["-lavfi", "[0:v][1:v]ssim", "-f", "null", "-"])
            .output()
            .expect("ffmpeg ssim");
        let ssim = parse_ssim(&String::from_utf8_lossy(&out.stderr)).expect("ssim summary");
        eprintln!("SSIM all = {ssim} (audio identical: {same_audio})");
        assert!(ssim >= 0.99, "SSIM {ssim} below 0.99");
    }

    fn native(encoder: &str, audio: AudioEncoder, codec: &str) -> Native {
        Native {
            profile_name: "p".into(),
            profile: Profile {
                tag: None,
                width: 848,
                height: 480,
                crop: "none".into(),
                source: "ffmpeg".into(),
                encoder: encoder.into(),
                video: vec!["--preset".into(), "4".into(), "--crf".into(), "35".into()],
                audio: AudioSpec {
                    codec: codec.into(),
                    bitrate: 96,
                },
                color: "bt709".into(),
                fonts: false,
            },
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            mkvmerge: "mkvmerge".into(),
            mkvextract: "mkvextract".into(),
            video_encoder: encoder.into(),
            audio,
            audio_lang: "jpn".into(),
            sub_lang: "id".into(),
            stall: Duration::from_secs(300),
        }
    }

    fn episode() -> Episode {
        Episode {
            id: EpisodeId::parse("09").unwrap(),
            video: PathBuf::from("S/Show - 09.mkv"),
            subs: vec![PathBuf::from("S/Show - 09.id.ass")],
            target: PathBuf::from("D/Show - 09.mkv"),
        }
    }

    fn source() -> SourceInfo {
        SourceInfo {
            audio_track: 2,
            audio_lang: "jpn".into(),
            delay_ms: 9,
            width: 1920,
            height: 1080,
            fps: "24000/1001".into(),
            duration_s: 1422.0,
            chapter_entries: 6,
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(dar(1920, 1080), "16/9");
        assert_eq!(dar(848, 480), "53/30");
        assert_eq!(crop_filter("none").unwrap(), None);
        assert_eq!(crop_filter("0,0,0,0").unwrap(), None);
        assert_eq!(
            crop_filter("0,140,0,140").unwrap().as_deref(),
            Some("crop=iw-0:ih-280:0:140")
        );
        assert!(crop_filter("auto").is_err());
        assert!(crop_filter("1,2").is_err());
        assert!(color_args("weird").is_err());
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote("plain"), "plain");
    }

    #[test]
    fn builds_svt_opus_pipeline_like_staxrip() {
        let n = native("svt-av1", AudioEncoder::Opusenc("opusenc".into()), "opus");
        let steps = n.steps(&episode(), &source(), Path::new("W")).unwrap();
        let names: Vec<_> = steps.iter().map(Step::name).collect();
        assert_eq!(names, ["video", "audio", "chapters", "mux"]);

        let video = steps[0].render();
        assert!(video.contains("-map 0:v:0 -vf zscale=w=848:h=480:filter=spline36,setsar=1 -pix_fmt yuv420p10le -f yuv4mpegpipe -strict -1 -"), "{video}");
        assert!(video.contains("| svt-av1 -i - -b W\\video.ivf --progress 2 --color-range 0 --chroma-sample-position left --color-primaries 1 --transfer-characteristics 1 --matrix-coefficients 1 --preset 4 --crf 35") || video.contains("| svt-av1 -i - -b W/video.ivf"), "{video}");

        let audio = steps[1].render();
        assert!(
            audio.contains(
                "-map 0:2 -vn -sn -dn -f flac - | opusenc --vbr --bitrate 96 --no-phase-inv -"
            ),
            "{audio}"
        );

        let mux = steps[3].render();
        assert!(
            mux.starts_with("mkvmerge -o \"D/Show - 09.mkv\" --aspect-ratio 0:16/9 "),
            "{mux}"
        );
        assert!(
            mux.contains("--language 0:jpn --sync 0:9 --default-track-flag 0:1 "),
            "{mux}"
        );
        assert!(
            mux.contains("--language 0:id --default-track-flag 0:1 \"S/Show - 09.id.ass\""),
            "{mux}"
        );
        assert!(mux.contains("--chapters "), "{mux}");
        assert!(!mux.contains("--default-duration"), "{mux}");
    }

    #[test]
    fn builds_x265_aac_pipeline_with_frame_rate() {
        let n = native("x265", AudioEncoder::FfmpegAac, "aac-lc");
        let mut src = source();
        src.chapter_entries = 0;
        let steps = n.steps(&episode(), &src, Path::new("W")).unwrap();
        let names: Vec<_> = steps.iter().map(Step::name).collect();
        assert_eq!(names, ["video", "audio", "mux"]);
        // Direct ffmpeg audio must not smuggle the source's chapters into the m4a.
        assert!(
            steps[1]
                .render()
                .contains("-map_chapters -1 -map_metadata -1 -c:a aac")
        );
        assert!(
            steps[2]
                .render()
                .contains("--no-chapters --no-global-tags --language 0:jpn")
        );
        let video = steps[0].render();
        assert!(
            video.contains("| x265 --y4m --input - --output "),
            "{video}"
        );
        assert!(video.contains("--output-depth 10 --range limited --log-level info --colorprim bt709 --transfer bt709 --colormatrix bt709 --preset 4 --crf 35"), "{video}");
        let audio = steps[1].render();
        assert!(audio.contains("-c:a aac -b:a 96k "), "{audio}");
        let mux = steps[2].render();
        assert!(mux.contains("--default-duration 0:24000/1001fps "), "{mux}");
        assert!(!mux.contains("--chapters"), "{mux}");
    }

    #[test]
    fn he_aac_variants_pick_profiles() {
        let n = native("x265", AudioEncoder::FfmpegFdk, "he-aac-v2");
        let steps = n.steps(&episode(), &source(), Path::new("W")).unwrap();
        assert!(
            steps[1]
                .render()
                .contains("-c:a libfdk_aac -profile:a aac_he_v2 -b:a 96k")
        );

        let n = native("x265", AudioEncoder::Qaac("qaac64".into()), "he-aac");
        let steps = n.steps(&episode(), &source(), Path::new("W")).unwrap();
        assert!(
            steps[1]
                .render()
                .contains("-f wav - | qaac64 --ignorelength --he --cbr 96 -o ")
        );

        let n = native("x265", AudioEncoder::Fdkaac("fdkaac".into()), "he-aac");
        let steps = n.steps(&episode(), &source(), Path::new("W")).unwrap();
        assert!(
            steps[1]
                .render()
                .contains("| fdkaac --ignorelength -p 5 -b 96 -o ")
        );
    }

    #[test]
    fn audio_encoder_selection_follows_order_and_capabilities() {
        let order: Vec<String> = ["qaac", "ffmpeg-fdk", "fdkaac", "ffmpeg-aac"]
            .into_iter()
            .map(String::from)
            .collect();
        let fdk: HashSet<String> = ["libfdk_aac", "aac", "libopus"]
            .into_iter()
            .map(String::from)
            .collect();
        let plain: HashSet<String> = ["aac", "libopus"].into_iter().map(String::from).collect();
        // qaac is unusable on the test machine unless Apple CoreAudio exists, so the
        // chain must land on libfdk_aac / aac depending on the ffmpeg build.
        match pick_audio_encoder("he-aac", &order, &fdk).unwrap() {
            AudioEncoder::Qaac(_) | AudioEncoder::FfmpegFdk => {}
            other => panic!("unexpected {other:?}"),
        }
        match pick_audio_encoder("aac-lc", &order, &plain).unwrap() {
            AudioEncoder::Qaac(_) | AudioEncoder::FfmpegAac => {}
            other => panic!("unexpected {other:?}"),
        }
        let err = pick_audio_encoder("he-aac-v2", &order, &plain)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no encoder for he-aac-v2") && err.contains("HE-AACv2"),
            "{err}"
        );
        assert!(pick_audio_encoder("mp3", &order, &plain).is_err());
        assert!(matches!(
            pick_audio_encoder("opus", &order, &plain),
            Ok(AudioEncoder::Opusenc(_) | AudioEncoder::FfmpegLibopus)
        ));
    }
}
