use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use walkdir::WalkDir;

use crate::subs::{batch, fonts};

/// `subx fonts <ASS|FOLDER>` — which fonts the scripts need and where they are.
#[derive(Debug, Args)]
pub struct FontsArgs {
    /// ASS/SSA file, or a folder searched recursively.
    pub input: PathBuf,

    /// Folder with font files (default: `fonts/` next to the input when it exists).
    #[arg(long)]
    pub fonts_dir: Option<PathBuf>,

    /// Ignore the operating system's font folders.
    #[arg(long)]
    pub no_system: bool,

    /// JSON output.
    #[arg(long)]
    pub json: bool,
}

const FONT_EXTS: &[&str] = &["ttf", "otf", "ttc"];

fn system_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    if cfg!(windows) {
        if let Some(w) = env("WINDIR") {
            dirs.push(w.join("Fonts"));
        }
        if let Some(l) = env("LOCALAPPDATA") {
            dirs.push(l.join("Microsoft").join("Windows").join("Fonts"));
        }
    } else if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Some(h) = env("HOME") {
            dirs.push(h.join("Library").join("Fonts"));
        }
    } else {
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        if let Some(h) = env("HOME") {
            dirs.push(h.join(".fonts"));
            dirs.push(h.join(".local").join("share").join("fonts"));
        }
    }
    dirs
}

/// Lower-cased font name -> file, first file wins.
fn index_fonts(dirs: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    let mut index = BTreeMap::new();
    for dir in dirs.iter().filter(|d| d.is_dir()) {
        for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let is_font = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| FONT_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)));
            if !entry.file_type().is_file() || !is_font {
                continue;
            }
            // 64 MB cap: CJK collections are large but nothing legitimate is larger.
            if entry.metadata().map_or(true, |m| m.len() > 64 << 20) {
                continue;
            }
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            for name in fonts::font_names(&bytes) {
                index
                    .entry(name.to_lowercase())
                    .or_insert_with(|| path.to_path_buf());
            }
        }
    }
    index
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Attached,
    System,
    Missing,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Attached => "attached",
            Status::System => "system",
            Status::Missing => "MISSING",
        }
    }
}

pub async fn run(args: FontsArgs) -> Result<()> {
    let files: Vec<PathBuf> = if args.input.is_file() {
        vec![args.input.clone()]
    } else if args.input.is_dir() {
        batch::collect_subs(&args.input, &["ass".into(), "ssa".into()], true)?
    } else {
        anyhow::bail!("Not found: {}", args.input.display());
    };
    if files.is_empty() {
        anyhow::bail!("No ASS/SSA files under {}", args.input.display());
    }

    // font -> scripts that use it
    let mut wanted: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in &files {
        let bytes =
            std::fs::read(file).with_context(|| format!("cannot read {}", file.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        let short = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for font in fonts::ass_font_refs(&text) {
            wanted.entry(font).or_default().push(short.clone());
        }
    }

    let base = if args.input.is_dir() {
        args.input.clone()
    } else {
        args.input
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    };
    let fonts_dir = args
        .fonts_dir
        .clone()
        .or_else(|| Some(base.join("fonts")).filter(|d| d.is_dir()));
    let attached = fonts_dir
        .as_ref()
        .map_or_else(BTreeMap::new, |d| index_fonts(std::slice::from_ref(d)));
    let system = if args.no_system {
        BTreeMap::new()
    } else {
        index_fonts(&system_font_dirs())
    };

    let mut rows = Vec::new();
    let mut missing = 0usize;
    for (font, users) in &wanted {
        let key = font.to_lowercase();
        let (status, path) = if let Some(p) = attached.get(&key) {
            (Status::Attached, Some(p.clone()))
        } else if let Some(p) = system.get(&key) {
            (Status::System, Some(p.clone()))
        } else {
            missing += 1;
            (Status::Missing, None)
        };
        rows.push((font.clone(), status, path, users.clone()));
    }

    if args.json {
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|(font, status, path, users)| {
                serde_json::json!({
                    "font": font,
                    "status": status.label().to_lowercase(),
                    "file": path.as_ref().map(|p| p.display().to_string()),
                    "used_by": users,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        println!(
            "{} script(s), {} font(s) referenced; fonts dir: {}; system fonts: {}",
            files.len(),
            rows.len(),
            fonts_dir
                .as_ref()
                .map_or("none".to_string(), |d| d.display().to_string()),
            if args.no_system {
                "ignored"
            } else {
                "searched"
            }
        );
        println!("{:<32} {:<9} FILE / USED BY", "FONT", "STATUS");
        for (font, status, path, users) in &rows {
            let detail = match path {
                Some(p) => p.display().to_string(),
                None => format!("used by {}", users.join(", ")),
            };
            println!("{:<32} {:<9} {}", font, status.label(), detail);
        }
    }

    if missing > 0 {
        anyhow::bail!("{missing} font(s) missing");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_fonts_by_every_name_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        // Reuse the synthetic font from subs::fonts tests through a real file.
        let font = fonts_test_font();
        std::fs::write(dir.path().join("a.ttf"), &font).unwrap();
        std::fs::write(dir.path().join("ignored.txt"), b"x").unwrap();
        let index = index_fonts(&[dir.path().to_path_buf()]);
        assert_eq!(index.get("test sans"), Some(&dir.path().join("a.ttf")));
        assert_eq!(index.get("test sans bold"), Some(&dir.path().join("a.ttf")));
        assert!(!index.contains_key("ignored"));
        assert!(index_fonts(&[dir.path().join("absent")]).is_empty());
    }

    fn fonts_test_font() -> Vec<u8> {
        let (family, full) = ("Test Sans", "Test Sans Bold");
        let mut strings = Vec::new();
        let mut records = Vec::new();
        for (id, text) in [(1u16, family), (4u16, full)] {
            let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
            for v in [
                3u16,
                1,
                0x0409,
                id,
                utf16.len() as u16,
                strings.len() as u16,
            ] {
                records.extend_from_slice(&v.to_be_bytes());
            }
            strings.extend_from_slice(&utf16);
        }
        let mut name = Vec::new();
        for v in [0u16, 2, (6 + records.len()) as u16] {
            name.extend_from_slice(&v.to_be_bytes());
        }
        name.extend(records);
        name.extend(strings);
        let mut font = Vec::new();
        font.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        font.extend_from_slice(&1u16.to_be_bytes());
        font.extend_from_slice(&[0u8; 6]);
        font.extend_from_slice(b"name");
        font.extend_from_slice(&0u32.to_be_bytes());
        font.extend_from_slice(&28u32.to_be_bytes());
        font.extend_from_slice(&(name.len() as u32).to_be_bytes());
        font.extend(name);
        font
    }
}
