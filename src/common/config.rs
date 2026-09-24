use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Global config from `subx.toml` (all fields optional, defaults apply).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Parallel worker count for extract/translate.
    pub workers: usize,
    /// Language priority for extract (first entry goes to the main folder).
    pub lang_priority: Vec<String>,
    /// Default font for `clean`.
    pub default_font: String,
    /// LibreTranslate URL (self-hosting recommended).
    pub libre_url: String,
    /// LibreTranslate API key (optional, for public instances).
    pub libre_api_key: Option<String>,
    /// Default translation engine: `libre` (free) or `ai` (needs an API key).
    pub translate_engine: String,
    /// OpenAI-compatible base URL for the AI engine.
    pub ai_base_url: String,
    /// Default model for the AI engine.
    pub ai_model: String,
    /// AI API key (prefer env: OPENAI_API_KEY / OPENROUTER_API_KEY / SUBX_API_KEY).
    pub ai_api_key: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            workers: 8,
            lang_priority: vec!["id".to_string()],
            default_font: "Arial".to_string(),
            libre_url: "http://localhost:5000".to_string(),
            libre_api_key: None,
            translate_engine: "libre".to_string(),
            ai_base_url: "https://api.openai.com/v1".to_string(),
            ai_model: "gpt-4o-mini".to_string(),
            ai_api_key: None,
        }
    }
}

impl AppConfig {
    /// Best-effort load: missing file / parse failure => defaults + warning.
    /// Priority: explicit path (--config) > subx.local.toml (secret,
    /// gitignored, for API keys) > subx.toml.
    pub fn load(path: Option<&Path>) -> Self {
        let candidate: Option<PathBuf> = match path {
            Some(p) => Some(p.to_path_buf()),
            None => {
                let mut cands = vec![
                    Path::new("subx.local.toml").to_path_buf(),
                    Path::new("subx.toml").to_path_buf(),
                ];
                if let Some(dir) = std::env::current_exe()
                    .ok()
                    .and_then(|e| e.parent().map(|d| d.to_path_buf()))
                {
                    cands.push(dir.join("subx.local.toml"));
                    cands.push(dir.join("subx.toml"));
                }
                cands.into_iter().find(|p| p.exists())
            }
        };

        match candidate {
            Some(p) => match std::fs::read_to_string(&p) {
                Ok(text) => match toml::from_str::<AppConfig>(&text) {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        tracing::warn!("failed to parse {}: {e}, using defaults", p.display());
                        Self::default()
                    }
                },
                Err(e) => {
                    tracing::debug!("cannot read {}: {e}, using defaults", p.display());
                    Self::default()
                }
            },
            None => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.workers, 8);
        assert_eq!(cfg.translate_engine, "libre");
        assert_eq!(cfg.ai_base_url, "https://api.openai.com/v1");
        assert_eq!(cfg.ai_model, "gpt-4o-mini");
        assert!(cfg.ai_api_key.is_none());
    }

    #[test]
    fn missing_file_gives_defaults() {
        let cfg = AppConfig::load(Some(Path::new("definitely-not-here-12345.toml")));
        assert_eq!(cfg.workers, AppConfig::default().workers);
        assert_eq!(cfg.translate_engine, "libre");
    }

    #[test]
    fn parses_valid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("subx.toml");
        std::fs::write(
            &p,
            "workers = 2\ntranslate_engine = \"ai\"\nai_model = \"x/y\"\nai_api_key = \"sk-test\"\n",
        )
        .unwrap();
        let cfg = AppConfig::load(Some(&p));
        assert_eq!(cfg.workers, 2);
        assert_eq!(cfg.translate_engine, "ai");
        assert_eq!(cfg.ai_model, "x/y");
        assert_eq!(cfg.ai_api_key.as_deref(), Some("sk-test"));
        // Unspecified keys keep their defaults.
        assert_eq!(cfg.default_font, "Arial");
        assert_eq!(cfg.libre_url, "http://localhost:5000");
    }

    #[test]
    fn invalid_toml_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("subx.toml");
        std::fs::write(&p, "workers = [unclosed\n").unwrap();
        let cfg = AppConfig::load(Some(&p));
        assert_eq!(cfg.workers, 8);
    }
}
