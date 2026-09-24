/// Normalize a language code to 2-letter ISO (1:1 port of extract_subs.go).
pub fn normalize_language(lang_raw: &str) -> String {
    let lang_clean = lang_raw.trim().to_lowercase();
    if lang_clean.is_empty() {
        return "unknown".to_string();
    }
    match lang_clean.as_str() {
        "indonesian" | "ind" | "id" | "ina" | "indo" | "indonesia" => return "id".to_string(),
        "english" | "eng" | "en" | "enm" | "us" | "uk" => return "en".to_string(),
        "japanese" | "jpn" | "jp" | "ja" => return "ja".to_string(),
        "und" | "unknown" | "zxx" | "mis" => return "unknown".to_string(),
        _ => {}
    }
    if lang_clean.len() >= 2 {
        lang_clean[..2].to_string()
    } else {
        "unknown".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_common_variants() {
        assert_eq!(normalize_language("ind"), "id");
        assert_eq!(normalize_language("Indonesian"), "id");
        assert_eq!(normalize_language("eng"), "en");
        assert_eq!(normalize_language("jpn"), "ja");
        assert_eq!(normalize_language("und"), "unknown");
        assert_eq!(normalize_language(""), "unknown");
        assert_eq!(normalize_language("fra"), "fr");
    }
}
