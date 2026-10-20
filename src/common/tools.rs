//! External tool discovery, verification and installation.
//!
//! Resolution order, first hit wins: `[tools] dir` → `tools/` next to the
//! executable → the `Apps` tree of `[tools] staxrip_dir` → `PATH`. Download
//! sources are the projects' official channels, or the Patman releases that
//! StaxRip itself designates for SVT-AV1 and x265 (docs/DESIGN.md, "External
//! binaries"). Nothing is forked, rebuilt or mirrored.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};

use anyhow::{Context, Result, bail};
use regex::Regex;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::common::config::ToolsConfig;
use crate::common::progress;

/// Every external program subx can call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    Ffmpeg,
    Ffprobe,
    Mkvmerge,
    Mkvextract,
    SvtAv1,
    X265,
    Qaac,
    Opusenc,
    Fdkaac,
    Exhale,
    Vspipe,
    YtDlp,
}

/// How an archive is unpacked after download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    Zip,
    SevenZ,
    /// Unpacked with the system `tar` (Linux/macOS ship tar + xz).
    TarXz,
    /// A bare executable.
    Raw,
}

/// Publisher checksum for a download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checksum {
    /// URL of a `.sha256` / `SHA2-256SUMS` text file (`<hex>  <name>` lines).
    File(String),
    /// The publisher offers none; the downloaded hash is printed instead.
    None,
}

/// A pinned download for the current OS and CPU.
#[derive(Debug, Clone)]
pub struct Source {
    pub url: String,
    pub archive: Archive,
    pub checksum: Checksum,
    pub note: Option<&'static str>,
}

/// Where a tool was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    ToolsDir,
    ExeDir,
    StaxRip,
    Path,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Origin::ToolsDir => "tools dir",
            Origin::ExeDir => "next to subx",
            Origin::StaxRip => "StaxRip bundle",
            Origin::Path => "PATH",
        })
    }
}

/// A located tool with its probe result.
#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub origin: Origin,
    pub version: Option<String>,
    /// False when the binary exists but cannot run (qaac without Apple CoreAudio).
    pub usable: bool,
    pub detail: Option<String>,
}

/// ffmpeg release branch: BtbN publishes `ffmpeg-n<branch>-latest-*` with the
/// newest patch of that line, so this pins the major.minor without going stale.
const FFMPEG_BRANCH: &str = "9.0";
const YTDLP_VERSION: &str = "2026.08.19";
const MKVTOOLNIX_VERSION: &str = "103.0";

impl Tool {
    pub const ALL: [Tool; 12] = [
        Tool::Ffmpeg,
        Tool::Ffprobe,
        Tool::Mkvmerge,
        Tool::Mkvextract,
        Tool::SvtAv1,
        Tool::X265,
        Tool::Qaac,
        Tool::Opusenc,
        Tool::Fdkaac,
        Tool::Exhale,
        Tool::Vspipe,
        Tool::YtDlp,
    ];

    /// CLI name (`subx tools install <name>`).
    pub fn name(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::Ffprobe => "ffprobe",
            Tool::Mkvmerge => "mkvmerge",
            Tool::Mkvextract => "mkvextract",
            Tool::SvtAv1 => "svt-av1",
            Tool::X265 => "x265",
            Tool::Qaac => "qaac",
            Tool::Opusenc => "opusenc",
            Tool::Fdkaac => "fdkaac",
            Tool::Exhale => "exhale",
            Tool::Vspipe => "vspipe",
            Tool::YtDlp => "yt-dlp",
        }
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| {
            t.name().eq_ignore_ascii_case(name) || t.exe_stem().eq_ignore_ascii_case(name)
        })
    }

    fn exe_stem(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::Ffprobe => "ffprobe",
            Tool::Mkvmerge => "mkvmerge",
            Tool::Mkvextract => "mkvextract",
            Tool::SvtAv1 => "SvtAv1EncApp",
            Tool::X265 => "x265",
            Tool::Qaac if cfg!(windows) => "qaac64",
            Tool::Qaac => "qaac",
            Tool::Opusenc => "opusenc",
            Tool::Fdkaac => "fdkaac",
            Tool::Exhale => "exhale",
            Tool::Vspipe => "vspipe",
            Tool::YtDlp => "yt-dlp",
        }
    }

    /// Executable file name on this OS.
    pub fn exe(self) -> String {
        if cfg!(windows) {
            format!("{}.exe", self.exe_stem())
        } else {
            self.exe_stem().to_string()
        }
    }

    /// Folder under the tools dir; bundles share one (ffmpeg+ffprobe, mkvmerge+mkvextract).
    fn install_dir(self) -> &'static str {
        match self {
            Tool::Ffmpeg | Tool::Ffprobe => "ffmpeg",
            Tool::Mkvmerge | Tool::Mkvextract => "mkvtoolnix",
            Tool::SvtAv1 => "svt-av1",
            Tool::X265 => "x265",
            Tool::Qaac => "qaac",
            Tool::Opusenc => "opusenc",
            Tool::Fdkaac => "fdkaac",
            Tool::Exhale => "exhale",
            Tool::Vspipe => "vapoursynth",
            Tool::YtDlp => "yt-dlp",
        }
    }

    /// The tool whose download provides this one.
    pub fn install_target(self) -> Tool {
        match self {
            Tool::Ffprobe => Tool::Ffmpeg,
            Tool::Mkvextract => Tool::Mkvmerge,
            other => other,
        }
    }

    /// Sub-folder of a StaxRip `Apps` tree searched first, so the intended
    /// build wins over stray copies (eac3to ships its own old ffmpeg).
    fn staxrip_hint(self) -> &'static str {
        match self {
            Tool::Ffmpeg | Tool::Ffprobe => "FrameServer",
            Tool::Mkvmerge | Tool::Mkvextract => "Support/MKVToolNix",
            Tool::SvtAv1 => "Encoders/SvtAv1EncApp",
            Tool::X265 => "Encoders/x265",
            Tool::Qaac => "Audio/qaac",
            Tool::Opusenc => "Audio/opus",
            Tool::Fdkaac => "Audio/fdkaac",
            Tool::Exhale => "Audio/exhale",
            Tool::Vspipe => "FrameServer/VapourSynth",
            Tool::YtDlp => "Support",
        }
    }

    pub fn purpose(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "decode, scale, audio (every media command)",
            Tool::Ffprobe => "media metadata (probe, extract, mux)",
            Tool::Mkvmerge => "muxing and MKV inspection",
            Tool::Mkvextract => "track, font and chapter extraction",
            Tool::SvtAv1 => "AV1 video encoding",
            Tool::X265 => "HEVC video encoding",
            Tool::Qaac => "AAC-LC / HE-AAC audio (Apple encoder)",
            Tool::Opusenc => "Opus audio (exact StaxRip chain; ffmpeg libopus otherwise)",
            Tool::Fdkaac => "AAC-LC / HE-AAC / HE-AACv2 audio (libfdk-aac CLI)",
            Tool::Exhale => "xHE-AAC (USAC) audio",
            Tool::Vspipe => "VapourSynth script source",
            Tool::YtDlp => "subtitle download (fetch)",
        }
    }

    /// Official page shown when the tool is missing.
    pub fn link(self) -> &'static str {
        match self {
            Tool::Ffmpeg | Tool::Ffprobe => "https://ffmpeg.org/download.html",
            Tool::Mkvmerge | Tool::Mkvextract => "https://mkvtoolnix.download/downloads.html",
            Tool::SvtAv1 => "https://github.com/Patman86/SVT-AV1-Mods-by-Patman/releases",
            Tool::X265 => "https://github.com/Patman86/x265-Mod-by-Patman/releases",
            Tool::Qaac => "https://github.com/nu774/qaac/releases",
            Tool::Opusenc => "https://opus-codec.org/downloads/",
            Tool::Fdkaac => "https://github.com/nu774/fdkaac",
            Tool::Exhale => "https://gitlab.com/ecodis/exhale/-/releases",
            Tool::Vspipe => "https://github.com/vapoursynth/vapoursynth/releases",
            Tool::YtDlp => "https://github.com/yt-dlp/yt-dlp/releases",
        }
    }

    fn version_args(self) -> &'static [&'static str] {
        match self {
            Tool::Ffmpeg | Tool::Ffprobe => &["-version"],
            Tool::Qaac => &["--check"],
            _ => &["--version"],
        }
    }

    /// Pinned download for this OS/CPU, if an accepted channel publishes one.
    pub fn source(self) -> Option<Source> {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let win_x64 = os == "windows" && arch == "x86_64";
        match self.install_target() {
            Tool::Ffmpeg => {
                // BtbN builds (linked from ffmpeg.org), GPL static: zscale,
                // libsvtav1, libopus, libx265. Release-branch asset, not master.
                const BASE: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/latest/download/";
                let (platform, ext, archive) = match (os, arch) {
                    ("windows", "x86_64") => ("win64", "zip", Archive::Zip),
                    ("windows", "aarch64") => ("winarm64", "zip", Archive::Zip),
                    ("linux", "x86_64") => ("linux64", "tar.xz", Archive::TarXz),
                    ("linux", "aarch64") => ("linuxarm64", "tar.xz", Archive::TarXz),
                    _ => return None,
                };
                Some(Source {
                    url: format!(
                        "{BASE}ffmpeg-n{FFMPEG_BRANCH}-latest-{platform}-gpl-{FFMPEG_BRANCH}.{ext}"
                    ),
                    archive,
                    checksum: Checksum::File(format!("{BASE}checksums.sha256")),
                    note: Some("BtbN release-branch build linked from ffmpeg.org; has zscale, libsvtav1, libopus, libx265"),
                })
            }
            Tool::Mkvmerge if win_x64 => {
                let url = format!(
                    "https://mkvtoolnix.download/windows/releases/{MKVTOOLNIX_VERSION}/mkvtoolnix-64-bit-{MKVTOOLNIX_VERSION}.zip"
                );
                Some(Source {
                    checksum: Checksum::File(format!("{url}.sha256")),
                    url,
                    archive: Archive::Zip,
                    note: None,
                })
            }
            Tool::SvtAv1 if win_x64 => Some(Source {
                url: "https://github.com/Patman86/SVT-AV1-Mods-by-Patman/releases/download/2026-07-20/SVT-AV1-EncApp-v4.2.0%2B15%2B86-1393de3cd-.Mod-by-Patman.-x64-clang22.1.8.7z".to_string(),
                archive: Archive::SevenZ,
                checksum: Checksum::None,
                note: Some("the build StaxRip ships; upstream GitLab publishes source only"),
            }),
            Tool::X265 if win_x64 => Some(Source {
                url: "https://github.com/Patman86/x265-Mod-by-Patman/releases/download/4.3%2B22%2B73/x265-4.3%2B22%2B73-1e85ca6eb-.Mod-by-Patman.-x64-avx2-clang22.1.8.7z".to_string(),
                archive: Archive::SevenZ,
                checksum: Checksum::None,
                note: Some("AVX2 build; the build StaxRip ships, upstream publishes source only"),
            }),
            Tool::Qaac if win_x64 => Some(Source {
                url: "https://github.com/nu774/qaac/releases/download/v3.07/qaac_3.07.zip".to_string(),
                archive: Archive::Zip,
                checksum: Checksum::None,
                note: Some("also needs Apple CoreAudio (Apple Application Support, or the makeportable script in the qaac wiki)"),
            }),
            Tool::Opusenc if os == "windows" => Some(Source {
                url: "https://archive.mozilla.org/pub/opus/win32/opus-tools-0.2-opus-1.3.zip".to_string(),
                archive: Archive::Zip,
                checksum: Checksum::None,
                note: Some("opus-tools 0.2 with libopus 1.3; a StaxRip bundle ships a newer build"),
            }),
            Tool::YtDlp => {
                let asset = match (os, arch) {
                    ("windows", _) => "yt-dlp.exe",
                    ("linux", "x86_64") => "yt-dlp_linux",
                    ("linux", "aarch64") => "yt-dlp_linux_aarch64",
                    ("macos", _) => "yt-dlp_macos",
                    _ => return None,
                };
                let base = format!("https://github.com/yt-dlp/yt-dlp/releases/download/{YTDLP_VERSION}/");
                Some(Source {
                    url: format!("{base}{asset}"),
                    archive: Archive::Raw,
                    checksum: Checksum::File(format!("{base}SHA2-256SUMS")),
                    note: None,
                })
            }
            _ => None,
        }
    }
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error for tools a command needs but cannot find; `main` maps it to exit code 3.
#[derive(Debug)]
pub struct Missing(pub Vec<Tool>);

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "missing external tool(s):")?;
        for tool in &self.0 {
            writeln!(f, "  {:<11} {}", tool.name(), tool.purpose())?;
            if tool.source().is_some() {
                writeln!(
                    f,
                    "             install: subx tools install {}",
                    tool.name()
                )?;
            }
            writeln!(f, "             download: {}", tool.link())?;
        }
        write!(
            f,
            "Put the binary on PATH, in tools/<name>/ next to subx, or point [tools] staxrip_dir at a StaxRip bundle."
        )
    }
}

impl std::error::Error for Missing {}

/// Finds tools in the configured locations.
#[derive(Debug, Clone)]
pub struct Resolver {
    roots: Vec<(Origin, PathBuf)>,
    use_path: bool,
}

impl Resolver {
    pub fn new(cfg: &ToolsConfig, exe_dir: Option<&Path>, use_path: bool) -> Self {
        let mut roots = Vec::new();
        if let Some(dir) = &cfg.dir {
            roots.push((Origin::ToolsDir, dir.clone()));
        }
        if let Some(dir) = exe_dir {
            roots.push((Origin::ExeDir, dir.join("tools")));
        }
        if let Some(dir) = &cfg.staxrip_dir {
            roots.push((Origin::StaxRip, dir.join("Apps")));
        }
        Self { roots, use_path }
    }

    /// Path and origin of a tool, following the resolution order.
    pub fn locate(&self, tool: Tool) -> Option<(PathBuf, Origin)> {
        let exe = tool.exe();
        for (origin, root) in &self.roots {
            let hit = match origin {
                Origin::StaxRip => find_under(&root.join(tool.staxrip_hint()), &exe, 8)
                    .or_else(|| find_under(root, &exe, 8)),
                _ => find_under(&root.join(tool.install_dir()), &exe, 6)
                    .or_else(|| find_under(root, &exe, 6)),
            };
            if let Some(path) = hit {
                return Some((path, *origin));
            }
        }
        if self.use_path
            && let Ok(path) = which::which(&exe)
        {
            return Some((path, Origin::Path));
        }
        None
    }

    /// Locate and run the version probe.
    pub fn probe(&self, tool: Tool) -> Option<Found> {
        let (path, origin) = self.locate(tool)?;
        let (version, usable, detail) = probe_version(tool, &path);
        Some(Found {
            path,
            origin,
            version,
            usable,
            detail,
        })
    }
}

/// First file named `exe` under `root` (sorted walk, so `SvtAv1EncApp/` wins
/// over `SvtAv1EncApp-Essential/` in a StaxRip bundle).
fn find_under(root: &Path, exe: &str, max_depth: usize) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    WalkDir::new(root)
        .max_depth(max_depth)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| {
            e.file_type().is_file()
                && e.file_name()
                    .to_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(exe))
        })
        .map(|e| e.into_path())
}

static VERSION_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[vVrRnN]|N-|git-)?\d+[0-9A-Za-z.+_-]*$").unwrap());

/// Version-looking token from a tool's `--version` output: `v103.0`, `2.83`,
/// `4.3+6+70-44ebc4e46`, `R81`, `N-127375-gf4012774b`, `2026.08.19`. Tokens
/// after a copyright notice on the same line are ignored so years never win.
pub fn version_from_output(text: &str) -> Option<String> {
    text.lines()
        .flat_map(|line| {
            line.split_whitespace().take_while(|t| {
                !t.eq_ignore_ascii_case("copyright") && !t.eq_ignore_ascii_case("(c)")
            })
        })
        .map(|t| t.trim_end_matches([',', ';', ':', ')']))
        .find(|t| VERSION_TOKEN.is_match(t))
        .map(str::to_string)
}

/// Run the version command; `(version, usable, detail)`.
fn probe_version(tool: Tool, path: &Path) -> (Option<String>, bool, Option<String>) {
    let output = std::process::Command::new(path)
        .args(tool.version_args())
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(out) = output else {
        return (None, false, Some("cannot be executed".to_string()));
    };
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let version = version_from_output(&text);
    if tool == Tool::Qaac
        && let Some(err) = text.lines().find(|l| l.contains("ERROR"))
    {
        let detail = if err.contains("CoreAudioToolbox") {
            "needs Apple CoreAudio (CoreAudioToolbox.dll); see the qaac wiki".to_string()
        } else {
            err.trim().to_string()
        };
        return (version, false, Some(detail));
    }
    let usable = out.status.success() || version.is_some();
    (version, usable, None)
}

static RESOLVER: OnceLock<Resolver> = OnceLock::new();

/// Install the process-wide resolver from `[tools]`; later calls are ignored.
pub fn init(cfg: &ToolsConfig) {
    let _ = RESOLVER.set(Resolver::new(cfg, exe_dir().as_deref(), true));
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
}

pub fn resolver() -> &'static Resolver {
    RESOLVER.get_or_init(|| Resolver::new(&ToolsConfig::default(), exe_dir().as_deref(), true))
}

/// Paths of the given tools, in order, or a `Missing` error listing every absent one.
pub fn require(tools: &[Tool]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::with_capacity(tools.len());
    let mut missing = Vec::new();
    for &tool in tools {
        match resolver().locate(tool) {
            Some((path, _)) => paths.push(path),
            None => missing.push(tool),
        }
    }
    if missing.is_empty() {
        Ok(paths)
    } else {
        Err(Missing(missing).into())
    }
}

pub fn require_one(tool: Tool) -> Result<PathBuf> {
    Ok(require(&[tool])?.remove(0))
}

/// `(ffmpeg, ffprobe)`.
pub fn ffmpeg_pair() -> Result<(PathBuf, PathBuf)> {
    let mut v = require(&[Tool::Ffmpeg, Tool::Ffprobe])?;
    let ffprobe = v.remove(1);
    Ok((v.remove(0), ffprobe))
}

/// Default install root: `[tools] dir`, else `tools/` next to the executable.
pub fn default_install_root(cfg: &ToolsConfig) -> Result<PathBuf> {
    if let Some(dir) = &cfg.dir {
        return Ok(dir.clone());
    }
    exe_dir()
        .map(|d| d.join("tools"))
        .context("cannot determine the executable's folder; pass --tools-dir")
}

/// Expected hex digest for `asset` from a checksum file (`<hex>  <name>` lines,
/// or a bare hex string).
pub fn parse_checksum(text: &str, asset: &str) -> Option<String> {
    let mut single = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hex), name) = (parts.next(), parts.next()) else {
            continue;
        };
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        match name {
            Some(n) if n.trim_start_matches('*') == asset => return Some(hex.to_lowercase()),
            None if single.is_none() => single = Some(hex.to_lowercase()),
            _ => {}
        }
    }
    single
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Download `url` to `dest` while hashing it; returns the SHA-256 hex digest.
async fn download(client: &reqwest::Client, url: &str, dest: &Path) -> Result<String> {
    let mut resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("request failed: {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed: {url}"))?;
    let bar = progress::bar(resp.content_length().unwrap_or(0), "downloading");
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("cannot create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = resp.chunk().await.with_context(|| "download interrupted")? {
        file.write_all(&chunk)?;
        hasher.update(&chunk);
        bar.inc(chunk.len() as u64);
    }
    bar.finish_and_clear();
    file.flush()?;
    Ok(hex(&hasher.finalize()))
}

/// Unpack `archive` into `dest` according to its type.
pub fn extract(archive: &Path, kind: Archive, dest: &Path, exe: &str) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("cannot create {}", dest.display()))?;
    match kind {
        Archive::Zip => {
            let file = std::fs::File::open(archive)?;
            zip::ZipArchive::new(file)
                .with_context(|| "not a zip archive")?
                .extract(dest)
                .with_context(|| "zip extraction failed")?;
        }
        Archive::SevenZ => {
            sevenz_rust::decompress_file(archive, dest)
                .map_err(|e| anyhow::anyhow!("7z extraction failed: {e}"))?;
        }
        Archive::TarXz => {
            let status = std::process::Command::new("tar")
                .arg("-xJf")
                .arg(archive)
                .arg("-C")
                .arg(dest)
                .status()
                .with_context(|| "cannot run tar (needed for .tar.xz)")?;
            if !status.success() {
                bail!("tar failed (exit={})", status.code().unwrap_or(-1));
            }
        }
        Archive::Raw => {
            let target = dest.join(exe);
            std::fs::copy(archive, &target)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
            }
        }
    }
    Ok(())
}

/// Download, verify and unpack one tool into `root/<install dir>/`; returns the
/// probed result. With `verify` off the checksum step is skipped entirely.
pub async fn install(tool: Tool, root: &Path, verify: bool) -> Result<Found> {
    let target = tool.install_target();
    let Some(source) = target.source() else {
        bail!(
            "{} has no download for {}-{} in an accepted channel; get it from {}",
            target.name(),
            std::env::consts::OS,
            std::env::consts::ARCH,
            target.link()
        );
    };
    let asset = source.url.rsplit('/').next().unwrap_or("download");
    let asset = percent_decode(asset);
    let dest = root.join(target.install_dir());
    let staging = root.join(format!(".{}.download", target.install_dir()));
    std::fs::create_dir_all(root).with_context(|| format!("cannot create {}", root.display()))?;

    println!("{}: downloading {}", target.name(), source.url);
    if let Some(note) = source.note {
        println!("  note: {note}");
    }
    let client = reqwest::Client::builder()
        .user_agent(concat!("subx/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let digest = download(&client, &source.url, &staging).await?;

    if verify {
        match &source.checksum {
            Checksum::File(url) => {
                let text = client
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?;
                let expected = parse_checksum(&text, &asset)
                    .with_context(|| format!("no entry for {asset} in {url}"))?;
                if expected != digest {
                    std::fs::remove_file(&staging).ok();
                    bail!("checksum mismatch for {asset}: expected {expected}, got {digest}");
                }
                println!("  sha256 verified: {digest}");
            }
            Checksum::None => {
                println!(
                    "  warning: the publisher provides no checksum; downloaded sha256 = {digest}"
                );
            }
        }
    } else {
        println!("  checksum skipped (--no-verify); sha256 = {digest}");
    }

    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .with_context(|| format!("cannot replace {}", dest.display()))?;
    }
    extract(&staging, source.archive, &dest, &target.exe())?;
    std::fs::remove_file(&staging).ok();

    let cfg = ToolsConfig {
        dir: Some(root.to_path_buf()),
        staxrip_dir: None,
    };
    Resolver::new(&cfg, None, false)
        .probe(tool)
        .with_context(|| {
            format!(
                "{} was unpacked but {} was not found under {}",
                target.name(),
                tool.exe(),
                dest.display()
            )
        })
}

/// Minimal percent-decoding for release asset names (`%2B` -> `+`).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn names_round_trip_and_are_unique() {
        let mut names: Vec<&str> = Tool::ALL.iter().map(|t| t.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Tool::ALL.len());
        for tool in Tool::ALL {
            assert_eq!(Tool::from_name(tool.name()), Some(tool));
        }
        assert_eq!(Tool::from_name("SvtAv1EncApp"), Some(Tool::SvtAv1));
        assert_eq!(Tool::from_name("MKVMERGE"), Some(Tool::Mkvmerge));
        assert_eq!(Tool::from_name("nope"), None);
        assert_eq!(Tool::Ffprobe.install_target(), Tool::Ffmpeg);
        assert_eq!(Tool::Mkvextract.install_target(), Tool::Mkvmerge);
    }

    #[test]
    fn parses_versions_from_tool_output() {
        let cases = [
            (
                "ffmpeg version N-127375-gf4012774b Copyright (c) 2000-2026",
                "N-127375-gf4012774b",
            ),
            (
                "ffmpeg version 7.1.1-essentials_build-www.gyan.dev Copyright",
                "7.1.1-essentials_build-www.gyan.dev",
            ),
            ("mkvmerge v103.0 ('Little Houses') 64-bit", "v103.0"),
            (
                "SVT-AV1 v4.2.0+71+88-17cd99550 [Mod by Patman] (release)",
                "v4.2.0+71+88-17cd99550",
            ),
            (
                "x265 [info]: HEVC encoder version 4.3+6+70-44ebc4e46 [Mod by Patman]",
                "4.3+6+70-44ebc4e46",
            ),
            ("qaac 2.83, CoreAudioToolbox 7.10.9.0", "2.83"),
            (
                "opusenc opus-tools 0.2-39-g9b1ca51 (using libopus 1.6.1-6-ga3f0ec0)",
                "0.2-39-g9b1ca51",
            ),
            (
                "VapourSynth Video Processing Library\nCopyright (c) 2012-2025 Fredrik Mellbin\nCore R81\nAPI R4.1",
                "R81",
            ),
            (
                "ffmpeg version n8.0.2 Copyright (c) 2000-2025 the FFmpeg developers\nbuilt with gcc 16.1.0",
                "n8.0.2",
            ),
            ("2026.08.19", "2026.08.19"),
        ];
        for (text, want) in cases {
            assert_eq!(version_from_output(text).as_deref(), Some(want), "{text}");
        }
        assert_eq!(version_from_output("no digits here"), None);
    }

    #[test]
    fn parses_checksum_files() {
        let multi = "aaaa…\n\
            1fa6733c37ea6fb51c99ad8fe785e7b7e5f3246c9b980230329d4fb72ed8d4d6  yt-dlp\n\
            66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a  yt-dlp.exe\n";
        assert_eq!(
            parse_checksum(multi, "yt-dlp.exe").as_deref(),
            Some("66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a")
        );
        assert_eq!(
            parse_checksum(multi, "yt-dlp").as_deref(),
            Some("1fa6733c37ea6fb51c99ad8fe785e7b7e5f3246c9b980230329d4fb72ed8d4d6")
        );
        assert_eq!(parse_checksum(multi, "missing"), None);
        let single = "C860A10C707CB1EE587D1464F6171842F2ADA3BB6F031DFFF55FEC75542F0417  mkvtoolnix-64-bit-103.0.zip\n";
        assert_eq!(
            parse_checksum(single, "mkvtoolnix-64-bit-103.0.zip").as_deref(),
            Some("c860a10c707cb1ee587d1464f6171842f2ada3bb6f031dfff55fec75542f0417")
        );
        let bare = "c860a10c707cb1ee587d1464f6171842f2ada3bb6f031dfff55fec75542f0417\n";
        assert_eq!(
            parse_checksum(bare, "anything").as_deref(),
            Some(&bare[..64])
        );
        assert_eq!(parse_checksum("not a hash", "x"), None);
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("v4.2.0%2B15%2B86-x.7z"), "v4.2.0+15+86-x.7z");
        assert_eq!(percent_decode("plain.zip"), "plain.zip");
        assert_eq!(percent_decode("bad%"), "bad%");
    }

    #[test]
    fn resolves_in_documented_order() {
        let dir = tempfile::tempdir().unwrap();
        let exe = Tool::Ffmpeg.exe();
        let tools_dir = dir.path().join("cfg-tools");
        let exe_dir = dir.path().join("app");
        let staxrip = dir.path().join("StaxRip");
        let in_tools = tools_dir.join("ffmpeg").join("bin").join(&exe);
        let in_exe = exe_dir.join("tools").join("ffmpeg").join(&exe);
        let in_staxrip = staxrip
            .join("Apps")
            .join("FrameServer")
            .join("AviSynth")
            .join(&exe);
        for p in [&in_tools, &in_exe, &in_staxrip] {
            touch(p);
        }
        let cfg = ToolsConfig {
            dir: Some(tools_dir.clone()),
            staxrip_dir: Some(staxrip.clone()),
        };
        let r = Resolver::new(&cfg, Some(&exe_dir), false);
        assert_eq!(
            r.locate(Tool::Ffmpeg),
            Some((in_tools.clone(), Origin::ToolsDir))
        );

        std::fs::remove_file(&in_tools).unwrap();
        assert_eq!(
            r.locate(Tool::Ffmpeg),
            Some((in_exe.clone(), Origin::ExeDir))
        );

        std::fs::remove_file(&in_exe).unwrap();
        assert_eq!(r.locate(Tool::Ffmpeg), Some((in_staxrip, Origin::StaxRip)));

        // Nothing configured and PATH disabled -> missing.
        let none = Resolver::new(&ToolsConfig::default(), None, false);
        assert_eq!(none.locate(Tool::Ffmpeg), None);
        assert!(none.probe(Tool::Mkvmerge).is_none());
    }

    #[test]
    fn staxrip_bundle_prefers_the_plain_encoder_folder() {
        let dir = tempfile::tempdir().unwrap();
        let exe = Tool::SvtAv1.exe();
        let enc = dir.path().join("Apps").join("Encoders");
        let plain = enc.join("SvtAv1EncApp").join(&exe);
        touch(&enc.join("SvtAv1EncApp-Essential").join(&exe));
        touch(&plain);
        let cfg = ToolsConfig {
            dir: None,
            staxrip_dir: Some(dir.path().to_path_buf()),
        };
        let r = Resolver::new(&cfg, None, false);
        assert_eq!(r.locate(Tool::SvtAv1).map(|(p, _)| p), Some(plain));
    }

    #[test]
    fn staxrip_bundle_skips_stray_ffmpeg_copies() {
        let dir = tempfile::tempdir().unwrap();
        let exe = Tool::Ffmpeg.exe();
        let apps = dir.path().join("Apps");
        touch(&apps.join("Audio").join("eac3to").join(&exe)); // sorts first, must lose
        let wanted = apps.join("FrameServer").join("AviSynth").join(&exe);
        touch(&wanted);
        let cfg = ToolsConfig {
            dir: None,
            staxrip_dir: Some(dir.path().to_path_buf()),
        };
        let r = Resolver::new(&cfg, None, false);
        assert_eq!(r.locate(Tool::Ffmpeg).map(|(p, _)| p), Some(wanted));
    }

    #[test]
    fn extracts_zip_into_tool_dir() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file(format!("bundle/bin/{}", Tool::Ffmpeg.exe()), opts)
                .unwrap();
            zip.write_all(b"fake").unwrap();
            zip.finish().unwrap();
        }
        let root = dir.path().join("tools");
        extract(
            &archive,
            Archive::Zip,
            &root.join("ffmpeg"),
            &Tool::Ffmpeg.exe(),
        )
        .unwrap();
        let cfg = ToolsConfig {
            dir: Some(root.clone()),
            staxrip_dir: None,
        };
        let (path, origin) = Resolver::new(&cfg, None, false)
            .locate(Tool::Ffmpeg)
            .unwrap();
        assert_eq!(origin, Origin::ToolsDir);
        assert_eq!(
            path,
            root.join("ffmpeg")
                .join("bundle")
                .join("bin")
                .join(Tool::Ffmpeg.exe())
        );
    }

    #[test]
    fn raw_extract_copies_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("dl");
        std::fs::write(&src, b"bin").unwrap();
        let dest = dir.path().join("yt-dlp");
        extract(&src, Archive::Raw, &dest, &Tool::YtDlp.exe()).unwrap();
        assert_eq!(std::fs::read(dest.join(Tool::YtDlp.exe())).unwrap(), b"bin");
    }

    #[test]
    fn missing_error_lists_links() {
        let msg = Missing(vec![Tool::SvtAv1, Tool::Vspipe]).to_string();
        assert!(msg.contains("svt-av1") && msg.contains(Tool::SvtAv1.link()));
        assert!(msg.contains("vspipe") && msg.contains(Tool::Vspipe.link()));
        assert!(msg.contains("subx tools install svt-av1") || Tool::SvtAv1.source().is_none());
        assert!(!msg.contains("subx tools install vspipe"));
    }

    /// Real download + sha256 verification + zip extraction.
    /// Needs network — run explicitly: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "network"]
    async fn e2e_install_mkvtoolnix_verifies_checksum() {
        if Tool::Mkvmerge.source().is_none() {
            return; // no official portable build for this OS
        }
        let dir = tempfile::tempdir().unwrap();
        let found = install(Tool::Mkvmerge, dir.path(), true).await.unwrap();
        assert!(found.usable, "{found:?}");
        assert!(
            found
                .version
                .as_deref()
                .unwrap_or("")
                .contains(MKVTOOLNIX_VERSION)
        );
    }

    /// Real 7z download (no publisher checksum) + extraction.
    /// Needs network — run explicitly: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "network"]
    async fn e2e_install_svt_av1_from_7z() {
        if Tool::SvtAv1.source().is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let found = install(Tool::SvtAv1, dir.path(), true).await.unwrap();
        assert!(found.usable, "{found:?}");
        assert!(found.version.as_deref().unwrap_or("").starts_with("v4."));
    }
}
