use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// Persistent translation cache: resume cheaply, never pay twice.
/// The cache stores raw translations (pre-glossary) so glossary edits
/// take effect on re-runs without re-calling any service.
pub struct Cache {
    path: Option<PathBuf>,
    map: HashMap<String, String>,
}

impl Cache {
    /// In-memory only (used with `--no-cache`).
    pub fn disabled() -> Self {
        Self {
            path: None,
            map: HashMap::new(),
        }
    }

    /// Load from disk; a missing or corrupt file starts empty.
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            map,
        }
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.map.get(key)
    }

    pub fn put(&mut self, key: String, value: String) {
        self.map.insert(key, value);
    }

    pub fn save(&self) -> Result<()> {
        if let Some(p) = &self.path {
            if let Some(parent) = p.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(p, serde_json::to_string_pretty(&self.map)?)
                .with_context(|| format!("cannot write {}", p.display()))?;
        }
        Ok(())
    }

    /// Stable (deterministic across runs) cache key.
    pub fn key(engine: &str, model: &str, from: &str, to: &str, src: &str) -> String {
        let raw = format!("{engine}|{model}|{from}|{to}|{src}");
        format!("{:016x}", fnv1a_64(&raw))
    }
}

fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_and_unique() {
        let a = Cache::key("ai", "m", "en", "id", "Hello");
        assert_eq!(a, Cache::key("ai", "m", "en", "id", "Hello"));
        assert_ne!(a, Cache::key("ai", "m", "en", "id", "World"));
        assert_ne!(a, Cache::key("libre", "libre", "en", "id", "Hello"));
    }

    #[test]
    fn roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cache.json");
        let mut c = Cache::load(p.clone());
        c.put("k".to_string(), "v".to_string());
        c.save().unwrap();
        let c2 = Cache::load(p);
        assert_eq!(c2.get("k").map(String::as_str), Some("v"));
    }
}
