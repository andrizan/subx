use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{SubFile, supported_exts};

/// Load any supported subtitle file by extension.
pub fn load_any(path: &Path) -> Result<SubFile> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "srt" => super::srt::load(path),
        "ass" | "ssa" => super::ass::load(path),
        "vtt" => super::vtt::load(path),
        _ => anyhow::bail!(
            "unsupported subtitle format: {} (supported: {})",
            path.display(),
            supported_exts().join(", ")
        ),
    }
}

/// Save a subtitle file, creating parent folders and writing atomically
/// (temp file + rename) so interrupted runs never leave half files.
pub fn save_any(file: &SubFile, path: &Path) -> Result<()> {
    let text = match file.format {
        super::SubFormat::Srt => super::srt::serialize(file),
        super::SubFormat::Ass => super::ass::serialize(file),
        super::SubFormat::Vtt => super::vtt::serialize(file),
    };
    crate::common::fs::write_atomic(path, &text)
}

/// Collect subtitle files in a folder, optionally recursing.
/// Extensions are matched without dots, case-insensitively; output is sorted.
pub fn collect_subs(dir: &Path, exts: &[String], recursive: bool) -> Result<Vec<PathBuf>> {
    let wanted: Vec<String> = exts
        .iter()
        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
        .filter(|e| supported_exts().contains(&e.as_str()))
        .collect();
    if wanted.is_empty() {
        anyhow::bail!(
            "no supported subtitle extensions requested (supported: {})",
            supported_exts().join(", ")
        );
    }
    let depth = if recursive { usize::MAX } else { 1 };
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .min_depth(1)
        .max_depth(depth)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| wanted.contains(&e.to_lowercase()))
                    .unwrap_or(false)
        })
        .collect();
    files.sort();
    Ok(files)
}

/// Split `--ext ".srt,.ass"` into normalized extensions (no dots, lowercase).
pub fn parse_ext_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}
