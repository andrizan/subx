pub mod ai;
pub mod cache;
pub mod libre;
pub mod prompt;
pub mod segment;

use anyhow::Result;

/// Source/target language pair, e.g. "en" -> "id".
#[derive(Debug, Clone)]
pub struct LangPair {
    pub from: String,
    pub to: String,
}

/// Translator contract (new providers plug in without refactors).
pub trait Translator: Send + Sync {
    async fn translate_batch(&self, texts: &[String], pair: &LangPair) -> Result<Vec<String>>;
}

/// Mask an API key for logs: `sk-or-abc123` -> `sk-...23`.
pub fn mask_key(key: &str) -> String {
    if key.len() <= 5 {
        return "***".to_string();
    }
    format!("{}...{}", &key[..3], &key[key.len() - 2..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_keys() {
        assert_eq!(mask_key("sk-or-abc123"), "sk-...23");
        assert_eq!(mask_key("abc"), "***");
    }
}
