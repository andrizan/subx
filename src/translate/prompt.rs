use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};

static NUM_PREFIX: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*\d+\s*[.)]\s?").unwrap());

/// Build the system prompt for subtitle translation, injecting glossary rules.
pub fn system_prompt(from: &str, to: &str, glossary: &[(String, String)]) -> String {
    let mut p = format!(
        "You translate subtitles from {from} to {to}. Rules:\n\
         - Reply with the same numbered lines and nothing else.\n\
         - Never alter timing, numbers, or ⟦n⟧ placeholders; keep them exactly.\n\
         - Keep each translation on one line; do not merge or split lines.\n\
         - Natural {to}, matching spoken dialogue."
    );
    if !glossary.is_empty() {
        p.push_str("\n- Always translate these terms exactly:");
        for (a, b) in glossary {
            p.push_str(&format!("\n  - \"{a}\" -> \"{b}\""));
        }
    }
    p
}

/// Format one batch as numbered target lines with labeled context that the
/// model must read but not translate.
pub fn user_block(targets: &[String], before: &[String], after: &[String]) -> String {
    let mut u = String::from(
        "Translate the numbered lines below. Reply with the same numbering and nothing else.\n",
    );
    if !before.is_empty() {
        u.push_str("\nPrevious context (reference only, do not translate):\n");
        for c in before {
            u.push_str(&format!("- {c}\n"));
        }
    }
    if !after.is_empty() {
        u.push_str("\nFollowing context (reference only, do not translate):\n");
        for c in after {
            u.push_str(&format!("- {c}\n"));
        }
    }
    u.push_str("\nLines to translate:\n");
    for (i, t) in targets.iter().enumerate() {
        u.push_str(&format!("{}. {t}\n", i + 1));
    }
    u
}

/// Parse `1. text` reply lines; the count must equal `expected`.
pub fn parse_numbered(text: &str, expected: usize) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push(NUM_PREFIX.replace(line, "").into_owned());
    }
    if out.len() != expected {
        anyhow::bail!("AI returned {} line(s), expected {expected}", out.len());
    }
    Ok(out)
}

/// Parse `--glossary ore=aku` entries.
pub fn parse_glossary(raw: &[String]) -> Result<Vec<(String, String)>> {
    raw.iter()
        .map(|s| {
            let (a, b) = s
                .split_once('=')
                .with_context(|| format!("bad glossary entry '{s}' (want src=dst)"))?;
            let (a, b) = (a.trim(), b.trim());
            if a.is_empty() || b.is_empty() {
                anyhow::bail!("bad glossary entry '{s}' (want src=dst)");
            }
            Ok((a.to_string(), b.to_string()))
        })
        .collect()
}

/// Compile word-boundary, case-insensitive post-replacements.
pub fn compile_glossary(pairs: &[(String, String)]) -> Result<Vec<(Regex, String)>> {
    pairs
        .iter()
        .map(|(a, b)| {
            let re = RegexBuilder::new(&format!(r"\b{}\b", regex::escape(a)))
                .case_insensitive(true)
                .build()
                .with_context(|| format!("bad glossary term '{a}'"))?;
            Ok((re, b.clone()))
        })
        .collect()
}

/// Apply compiled glossary replacements.
pub fn apply_glossary(text: &str, compiled: &[(Regex, String)]) -> String {
    let mut out = text.to_string();
    for (re, dst) in compiled {
        out = re.replace_all(&out, dst.as_str()).into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_glossary_into_prompt() {
        let p = system_prompt("en", "id", &[("ore".into(), "aku".into())]);
        assert!(p.contains("from en to id"));
        assert!(p.contains("\"ore\" -> \"aku\""));
    }

    #[test]
    fn parses_numbered_replies() {
        let out = parse_numbered("1. Halo\n2) Dunia\n\n", 2).unwrap();
        assert_eq!(out, ["Halo", "Dunia"]);
        assert!(parse_numbered("1. Halo\n", 2).is_err());
    }

    #[test]
    fn glossary_replaces_words_only() {
        let pairs = parse_glossary(&["ore=aku".to_string()]).unwrap();
        let compiled = compile_glossary(&pairs).unwrap();
        assert_eq!(apply_glossary("ORE wa forest", &compiled), "aku wa forest");
        assert!(parse_glossary(&["bogus".to_string()]).is_err());
    }
}
