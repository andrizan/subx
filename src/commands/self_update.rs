use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Deserialize;

use crate::common::tools::{self, Archive};

/// GitHub repository whose Releases carry the prebuilt archives.
const REPO: &str = "andrizan/subx";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Binary name inside the release archives.
const BIN: &str = if cfg!(windows) { "subx.exe" } else { "subx" };

/// `subx self-update` — replace this binary with the latest GitHub release.
#[derive(Debug, Args)]
pub struct SelfUpdateArgs {
    /// Only report whether a newer release exists; install nothing.
    #[arg(long)]
    pub check: bool,

    /// Install this release (e.g. v1.0.0, older ones too) instead of the latest.
    #[arg(long, value_name = "TAG")]
    pub to: Option<String>,

    /// Reinstall even when already on that version.
    #[arg(long)]
    pub force: bool,

    /// Skip the SHA256SUMS verification of the download.
    #[arg(long)]
    pub no_verify: bool,
}

/// `MAJOR.MINOR.PATCH[-pre]`, with or without a leading `v`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    pre: Option<String>,
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        let text = text.split('+').next()?; // build metadata never orders
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre.to_string())),
            None => (text, None),
        };
        let mut parts = core.split('.');
        let mut next = || parts.next()?.parse().ok();
        let version = Self {
            core: [next()?, next()?, next()?],
            pre,
        };
        parts.next().is_none().then_some(version)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater, // 1.2.0 is newer than 1.2.0-rc1
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// Tag without the leading `v`, as used in archive names.
    fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }
}

/// Build target and archive type of the release matching this OS and CPU
/// (the matrix in `.github/workflows/build.yml`).
fn release_target(os: &str, arch: &str) -> Option<(&'static str, Archive)> {
    match (os, arch) {
        ("windows", "x86_64") => Some(("x86_64-pc-windows-msvc", Archive::Zip)),
        ("windows", "aarch64") => Some(("aarch64-pc-windows-msvc", Archive::Zip)),
        ("linux", "x86_64") => Some(("x86_64-unknown-linux-musl", Archive::TarXz)),
        ("linux", "aarch64") => Some(("aarch64-unknown-linux-musl", Archive::TarXz)),
        ("macos", "aarch64") => Some(("aarch64-apple-darwin", Archive::TarXz)),
        _ => None,
    }
}

/// `subx-<version>-<target>.zip|tar.xz`, the name `release.yml` packages.
fn asset_name(version: &str, target: &str, archive: Archive) -> String {
    let ext = if archive == Archive::Zip {
        "zip"
    } else {
        "tar.xz"
    };
    format!("subx-{version}-{target}.{ext}")
}

async fn fetch_release(client: &reqwest::Client, tag: Option<&str>) -> Result<Release> {
    let url = match tag {
        None => format!("https://api.github.com/repos/{REPO}/releases/latest"),
        Some(tag) => {
            let tag = if tag.starts_with('v') {
                tag.to_string()
            } else {
                format!("v{tag}")
            };
            format!("https://api.github.com/repos/{REPO}/releases/tags/{tag}")
        }
    };
    client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("cannot reach GitHub ({url})"))?
        .error_for_status()
        .with_context(|| match tag {
            Some(tag) => format!("no release '{tag}' in {REPO}"),
            None => format!("release lookup failed for {REPO}"),
        })?
        .json()
        .await
        .context("unexpected release data from GitHub")
}

/// Unpack a release archive into `dest` and return the `subx` binary inside it.
fn unpack_binary(archive: &Path, kind: Archive, dest: &Path) -> Result<PathBuf> {
    tools::extract(archive, kind, dest, BIN)?;
    tools::find_under(dest, BIN, 3)
        .with_context(|| format!("{BIN} not found inside {}", archive.display()))
}

/// `<exe>.old`: where Windows keeps the previous binary, which cannot be deleted while it runs.
fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// Put `new` in place of `current`, the executable that is running right now.
///
/// The binary is first copied next to `current` (same volume), then swapped in:
/// an atomic rename on Unix; on Windows the running file is renamed aside and
/// restored if the swap fails, so a failed update never leaves no subx behind.
fn replace_exe(current: &Path, new: &Path) -> Result<()> {
    let dir = current.parent().context("executable has no folder")?;
    let name = current.file_name().context("executable has no name")?;
    let incoming = dir.join(format!(".{}.new", name.to_string_lossy()));
    std::fs::copy(new, &incoming).with_context(|| {
        format!(
            "cannot write to {}; run from a writable install or with elevated permissions",
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o755))?;
    }
    let swapped = swap(current, &incoming);
    if swapped.is_err() {
        let _ = std::fs::remove_file(&incoming);
    }
    swapped
}

fn swap(current: &Path, incoming: &Path) -> Result<()> {
    if !cfg!(windows) {
        return std::fs::rename(incoming, current)
            .with_context(|| format!("cannot replace {}", current.display()));
    }
    let old = old_path(current);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(current, &old)
        .with_context(|| format!("cannot move {} aside", current.display()))?;
    if let Err(e) = std::fs::rename(incoming, current) {
        let _ = std::fs::rename(&old, current);
        return Err(e).with_context(|| format!("cannot install {}", current.display()));
    }
    Ok(())
}

/// Run the downloaded binary's `--version` before it replaces a working one,
/// so a corrupt or wrong-architecture download is caught here.
async fn check_runs(binary: &Path, version: &str) -> Result<()> {
    let out = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(binary)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("the downloaded binary did not answer --version in time")?
    .context("the downloaded binary cannot run on this machine")?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !text.contains(version) {
        bail!(
            "the downloaded binary did not report version {version} (got '{}')",
            text.trim()
        );
    }
    Ok(())
}

/// Download, verify, unpack and swap in `release`; `staging` is scratch space
/// beside the executable and is the caller's to delete.
async fn install(
    release: &Release,
    exe: &Path,
    staging: &Path,
    no_verify: bool,
    client: &reqwest::Client,
) -> Result<()> {
    let (target, kind) = release_target(std::env::consts::OS, std::env::consts::ARCH)
        .with_context(|| {
            format!(
                "no prebuilt release for {}-{}; build from source instead",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
    let name = asset_name(release.version(), target, kind);
    let asset = release
        .asset(&name)
        .with_context(|| format!("release {} has no asset {name}", release.tag_name))?;

    std::fs::create_dir_all(staging).with_context(|| {
        format!(
            "cannot write to {}; run from a writable install or with elevated permissions",
            staging.display()
        )
    })?;
    let archive = staging.join(&name);
    println!("Downloading {name}");
    let digest = tools::download(client, &asset.browser_download_url, &archive).await?;

    if no_verify {
        println!("Checksum skipped (--no-verify); sha256 = {digest}");
    } else {
        let sums = release.asset("SHA256SUMS").with_context(|| {
            format!(
                "release {} has no SHA256SUMS; use --no-verify to install unchecked",
                release.tag_name
            )
        })?;
        let text = client
            .get(&sums.browser_download_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let expected = tools::parse_checksum(&text, &name)
            .with_context(|| format!("no entry for {name} in SHA256SUMS"))?;
        if expected != digest {
            bail!("checksum mismatch for {name}: expected {expected}, got {digest}");
        }
        println!("sha256 verified: {digest}");
    }

    let binary = unpack_binary(&archive, kind, &staging.join("unpacked"))?;
    check_runs(&binary, release.version()).await?;
    replace_exe(exe, &binary)
}

pub async fn run(args: SelfUpdateArgs) -> Result<()> {
    let current = Version::parse(CURRENT).context("this build has an unreadable version")?;
    let api = reqwest::Client::builder()
        .user_agent(concat!("subx/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()?;

    let release = fetch_release(&api, args.to.as_deref()).await?;
    let wanted = Version::parse(&release.tag_name)
        .with_context(|| format!("unreadable release tag '{}'", release.tag_name))?;
    println!("Installed: {CURRENT}");
    println!("Release  : {}", release.version());

    let newer = wanted > current;
    if args.check {
        if newer {
            println!("A newer release is available; run `subx self-update`.");
        } else {
            println!("subx is up to date.");
        }
        return Ok(());
    }
    // The latest release is only installed when newer; an explicit --to always is.
    let skip = if args.to.is_some() {
        wanted == current
    } else {
        !newer
    };
    if skip && !args.force {
        println!("Nothing to do; use --force to reinstall.");
        return Ok(());
    }

    let exe = std::env::current_exe().context("cannot locate the running executable")?;
    let _ = std::fs::remove_file(old_path(&exe)); // left behind by the previous update
    let staging = exe
        .parent()
        .context("executable has no folder")?
        .join(".subx-update");
    let _ = std::fs::remove_dir_all(&staging);

    // Downloads get their own client: a whole-request timeout would cut slow links.
    let downloader = reqwest::Client::builder()
        .user_agent(concat!("subx/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()?;
    let result = install(&release, &exe, &staging, args.no_verify, &downloader).await;
    let _ = std::fs::remove_dir_all(&staging);
    result?;

    println!(
        "Updated subx {CURRENT} -> {} ({})",
        release.version(),
        exe.display()
    );
    if cfg!(windows) {
        println!(
            "The previous binary stays as {} until the next update.",
            old_path(&exe).display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("cannot parse {text}"))
    }

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("1.2.3+build5"), v("1.2.3"));
        assert!(v("1.10.0") > v("1.9.9"));
        assert!(v("v2.0.0") > v("1.99.99"));
        assert!(v("1.2.0") > v("1.2.0-rc1"));
        assert!(v("1.2.0-rc2") > v("1.2.0-rc1"));
        assert!(v("1.1.0") < v("1.1.1"));
        for bad in ["", "abc", "1.2", "1.2.x", "1.2.3.4", "v"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn targets_follow_the_release_matrix() {
        assert_eq!(
            release_target("windows", "x86_64"),
            Some(("x86_64-pc-windows-msvc", Archive::Zip))
        );
        assert_eq!(
            release_target("linux", "aarch64"),
            Some(("aarch64-unknown-linux-musl", Archive::TarXz))
        );
        assert_eq!(
            release_target("macos", "aarch64"),
            Some(("aarch64-apple-darwin", Archive::TarXz))
        );
        assert_eq!(release_target("macos", "x86_64"), None);
        assert_eq!(release_target("freebsd", "x86_64"), None);
        assert_eq!(
            asset_name("1.1.0", "x86_64-pc-windows-msvc", Archive::Zip),
            "subx-1.1.0-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            asset_name("1.1.0", "aarch64-apple-darwin", Archive::TarXz),
            "subx-1.1.0-aarch64-apple-darwin.tar.xz"
        );
    }

    #[test]
    fn reads_the_github_release_payload() {
        let json = r#"{
            "tag_name": "v1.1.0", "prerelease": false, "draft": false,
            "assets": [
                {"name": "SHA256SUMS", "size": 535,
                 "browser_download_url": "https://example.test/SHA256SUMS"},
                {"name": "subx-1.1.0-x86_64-pc-windows-msvc.zip", "size": 3308684,
                 "browser_download_url": "https://example.test/win.zip"}
            ]
        }"#;
        let release: Release = serde_json::from_str(json).unwrap();
        assert_eq!(release.version(), "1.1.0");
        assert_eq!(
            release.asset("SHA256SUMS").unwrap().browser_download_url,
            "https://example.test/SHA256SUMS"
        );
        assert!(
            release
                .asset("subx-1.1.0-aarch64-apple-darwin.tar.xz")
                .is_none()
        );
    }

    #[test]
    fn unpacks_the_binary_from_a_release_zip() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("subx-1.2.0-x86_64-pc-windows-msvc.zip");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (path, body) in [
                (
                    "subx-1.2.0-x86_64-pc-windows-msvc/README.md",
                    &b"readme"[..],
                ),
                (
                    &format!("subx-1.2.0-x86_64-pc-windows-msvc/{BIN}"),
                    b"binary",
                ),
            ] {
                zip.start_file(path, opts).unwrap();
                zip.write_all(body).unwrap();
            }
            zip.finish().unwrap();
        }
        let found = unpack_binary(&archive, Archive::Zip, &dir.path().join("out")).unwrap();
        assert_eq!(std::fs::read(found).unwrap(), b"binary");

        // A file that is not a zip is an error, not a panic.
        let broken = dir.path().join("broken.zip");
        std::fs::write(&broken, b"not a zip").unwrap();
        assert!(unpack_binary(&broken, Archive::Zip, &dir.path().join("out2")).is_err());
    }

    #[test]
    fn replaces_the_executable_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join(BIN);
        let new = dir.path().join("download");
        std::fs::write(&current, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();

        replace_exe(&current, &new).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new");
        if cfg!(windows) {
            assert_eq!(std::fs::read(old_path(&current)).unwrap(), b"old");
        }
        // No half-installed scratch file is left beside it.
        assert!(!dir.path().join(format!(".{BIN}.new")).exists());

        // A second update replaces the stale `.old` instead of failing on it.
        std::fs::write(&new, b"newer").unwrap();
        replace_exe(&current, &new).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"newer");
    }

    #[test]
    fn a_failed_replace_keeps_the_current_binary() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join(BIN);
        std::fs::write(&current, b"old").unwrap();
        let missing = dir.path().join("no-such-download");
        assert!(replace_exe(&current, &missing).is_err());
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
    }

    /// Real GitHub lookup: the latest release has this platform's archive and
    /// SHA256SUMS. Needs network — run explicitly: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "network"]
    async fn e2e_latest_release_has_this_platform() {
        let api = reqwest::Client::builder()
            .user_agent("subx-test")
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        let release = fetch_release(&api, None).await.unwrap();
        assert!(Version::parse(&release.tag_name).is_some());
        assert!(release.asset("SHA256SUMS").is_some());
        if let Some((target, kind)) = release_target(std::env::consts::OS, std::env::consts::ARCH) {
            let name = asset_name(release.version(), target, kind);
            assert!(release.asset(&name).is_some(), "missing {name}");
        }
        assert!(fetch_release(&api, Some("v0.0.0-nope")).await.is_err());
    }
}
