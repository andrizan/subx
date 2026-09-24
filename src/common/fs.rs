use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Sanitize a filename to be Windows-safe (port of extract_subs.go).
/// Replaces `<>:"/\|?*[]() '-&%` with `_`, trims trailing `. `, truncates to 150 chars.
pub fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '[' | ']' | '(' | ')' | ' '
            | '-' | '\'' | '&' | '%' => out.push('_'),
            _ => out.push(ch),
        }
    }
    // Trim trailing dots & spaces (illegal on Windows).
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    // Cap at 150 chars (not bytes) to stay clear of MAX_PATH.
    if out.chars().count() > 150 {
        out = out.chars().take(150).collect();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_windows_forbidden_chars() {
        assert_eq!(sanitize_filename("Anime: Ep 01"), "Anime__Ep_01");
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_filename("trailing..."), "trailing");
    }

    #[test]
    fn truncates_long_names() {
        let long = "a".repeat(200);
        assert_eq!(sanitize_filename(&long).chars().count(), 150);
    }
}

/// Write a file atomically (temp file + rename) so interrupted runs never
/// leave half-written subtitles behind.
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let tmp: PathBuf = {
        let mut s = path.as_os_str().to_owned();
        s.push(".tmp");
        PathBuf::from(s)
    };
    std::fs::write(&tmp, content).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod io_tests {
    use super::*;

    #[test]
    fn atomic_write_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("a.srt");
        write_atomic(&p, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello");
        assert!(!dir.path().join("sub").join("a.srt.tmp").exists());
    }
}
