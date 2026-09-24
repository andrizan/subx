use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};

use super::{Event, plaintext};

/// Compile user keywords into case-insensitive regexes (grep -E flavor,
/// like the legacy cleaner.sh).
pub fn compile_keywords(keywords: &[String]) -> Result<Vec<Regex>> {
    keywords
        .iter()
        .map(|k| {
            RegexBuilder::new(k)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("invalid keyword regex: {k}"))
        })
        .collect()
}

/// Drop events whose visible text matches any keyword.
/// Returns the kept events plus drop count.
pub fn filter_events(events: Vec<Event>, patterns: &[Regex]) -> (Vec<Event>, usize) {
    let before = events.len();
    let kept = events
        .into_iter()
        .filter(|e| {
            let t = plaintext(&e.text);
            !patterns.iter().any(|re| re.is_match(&t))
        })
        .collect::<Vec<_>>();
    let removed = before - kept.len();
    (kept, removed)
}

#[cfg(test)]
mod tests {
    use super::super::Event;
    use super::*;

    fn event(text: &str) -> Event {
        Event {
            text: text.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn filters_case_insensitively() {
        let res = compile_keywords(&["opening".to_string(), "end.*song".to_string()]).unwrap();
        let events = vec![
            event("Here comes the OPENING theme"),
            event("Just a normal line"),
            event("EndCredit Song starts"),
        ];
        let (kept, removed) = filter_events(events, &res);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text, "Just a normal line");
        assert_eq!(removed, 2);
    }

    #[test]
    fn rejects_bad_regex() {
        assert!(compile_keywords(&["(unclosed".to_string()]).is_err());
    }
}
