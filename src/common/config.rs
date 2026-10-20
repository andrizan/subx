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
    /// External tool locations (`[tools]`).
    pub tools: ToolsConfig,
    /// Defaults for `encode` (`[encode]`); every CLI flag wins over them.
    pub encode: EncodeConfig,
}

/// `[tools]` section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Where `subx tools install` puts binaries and the first place they are
    /// looked up (default: `tools/` next to the executable).
    pub dir: Option<PathBuf>,
    /// Folder containing `StaxRip.exe` (required by `encode`); its `Apps` tree
    /// is also searched for every other tool.
    pub staxrip_dir: Option<PathBuf>,
}

/// `[encode]` section: per-machine naming and language defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EncodeConfig {
    /// StaxRip template name (case and punctuation are ignored).
    pub template: Option<String>,
    /// Output name prefix such as `[GROUP]`; empty for none.
    pub prefix: String,
    /// Tag in parentheses at the end of the output name; empty for none.
    pub tag: String,
    /// Target size `WIDTHxHEIGHT`; empty keeps the template's size.
    pub resolution: String,
    /// The only audio language kept (ISO 639-2).
    pub audio_lang: String,
    /// Subtitle language when the file name has no `.<lang>.` part.
    pub sub_lang: String,
    /// Folder for per-episode work copies (default: `<temp>/subx-encode`).
    pub work_dir: Option<PathBuf>,
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
            tools: ToolsConfig::default(),
            encode: EncodeConfig::default(),
        }
    }
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            template: None,
            prefix: String::new(),
            tag: String::new(),
            resolution: String::new(),
            audio_lang: "jpn".to_string(),
            sub_lang: "id".to_string(),
            work_dir: None,
        }
    }
}

impl AppConfig {
    /// Best-effort load. With `--config` exactly that file is read; otherwise
    /// `subx.toml` and `subx.local.toml` next to the executable and in the
    /// current directory are merged, later files overriding earlier ones:
    /// exe `subx.toml` < exe `subx.local.toml` < cwd `subx.toml` < cwd
    /// `subx.local.toml`. Missing files are skipped; unreadable or invalid
    /// files are skipped with a warning.
    pub fn load(path: Option<&Path>) -> Self {
        let files = match path {
            Some(p) => vec![p.to_path_buf()],
            None => Self::default_files(),
        };
        Self::load_files(&files)
    }

    fn default_files() -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf))
        {
            files.push(dir.join("subx.toml"));
            files.push(dir.join("subx.local.toml"));
        }
        files.push(PathBuf::from("subx.toml"));
        files.push(PathBuf::from("subx.local.toml"));
        files
    }

    /// Merge the given files in order (later wins) on top of the defaults.
    fn load_files(files: &[PathBuf]) -> Self {
        let mut merged = toml::Value::Table(toml::Table::new());
        for file in files.iter().filter(|f| f.exists()) {
            let parsed = std::fs::read_to_string(file)
                .map_err(|e| e.to_string())
                .and_then(|text| toml::from_str::<toml::Value>(&text).map_err(|e| e.to_string()));
            match parsed {
                Ok(value) => merge(&mut merged, value),
                Err(e) => tracing::warn!("skipping config {}: {e}", file.display()),
            }
        }
        match merged.try_into::<AppConfig>() {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("invalid config: {e}, using defaults");
                Self::default()
            }
        }
    }
}

/// Deep-merge `over` into `base`: tables recurse, everything else is replaced.
fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(base), toml::Value::Table(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, over) => *base = over,
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
        assert!(cfg.tools.staxrip_dir.is_none());
        assert!(cfg.encode.template.is_none());
        assert_eq!(cfg.encode.audio_lang, "jpn");
        assert_eq!(cfg.encode.sub_lang, "id");
        assert!(cfg.encode.prefix.is_empty() && cfg.encode.tag.is_empty());
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
            "workers = 2\ntranslate_engine = \"ai\"\nai_model = \"x/y\"\nai_api_key = \"sk-test\"\n\
             [tools]\nstaxrip_dir = \"C:/StaxRip\"\n[encode]\ntemplate = \"Anime480p\"\nprefix = \"[P]\"\n",
        )
        .unwrap();
        let cfg = AppConfig::load(Some(&p));
        assert_eq!(cfg.workers, 2);
        assert_eq!(cfg.translate_engine, "ai");
        assert_eq!(cfg.ai_model, "x/y");
        assert_eq!(cfg.ai_api_key.as_deref(), Some("sk-test"));
        assert_eq!(cfg.tools.staxrip_dir, Some(PathBuf::from("C:/StaxRip")));
        assert_eq!(cfg.encode.template.as_deref(), Some("Anime480p"));
        assert_eq!(cfg.encode.prefix, "[P]");
        // Unspecified keys keep their defaults.
        assert_eq!(cfg.default_font, "Arial");
        assert_eq!(cfg.libre_url, "http://localhost:5000");
        assert_eq!(cfg.encode.audio_lang, "jpn");
    }

    #[test]
    fn invalid_toml_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("subx.toml");
        std::fs::write(&p, "workers = [unclosed\n").unwrap();
        let cfg = AppConfig::load(Some(&p));
        assert_eq!(cfg.workers, 8);
    }

    #[test]
    fn local_file_overrides_base_keys_only() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("subx.toml");
        let local = dir.path().join("subx.local.toml");
        std::fs::write(
            &base,
            "workers = 2\ndefault_font = \"Noto\"\n[encode]\nprefix = \"[BASE]\"\ntag = \"T\"\n",
        )
        .unwrap();
        std::fs::write(
            &local,
            "workers = 4\n[encode]\nprefix = \"[LOCAL]\"\n[tools]\nstaxrip_dir = \"D:/S\"\n",
        )
        .unwrap();
        let cfg = AppConfig::load_files(&[base, local, dir.path().join("absent.toml")]);
        assert_eq!(cfg.workers, 4);
        assert_eq!(cfg.default_font, "Noto");
        assert_eq!(cfg.encode.prefix, "[LOCAL]");
        assert_eq!(cfg.encode.tag, "T");
        assert_eq!(cfg.tools.staxrip_dir, Some(PathBuf::from("D:/S")));
    }
}
