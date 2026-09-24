use once_cell::sync::Lazy;
use regex::Regex;

static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{[^}]*\}|\\[Nnh]").unwrap());
static RESTORE: Lazy<Regex> = Lazy::new(|| Regex::new(r"⟦(\d+)⟧").unwrap());

/// Protect formatting (`{...}` override blocks, `\N` / `\h` breaks) as
/// `⟦n⟧` placeholders so MT engines neither translate nor mangle them.
pub fn protect(text: &str) -> (String, Vec<String>) {
    let mut table = Vec::new();
    let out = TAG
        .replace_all(text, |caps: &regex::Captures| {
            table.push(caps[0].to_string());
            format!("⟦{}⟧", table.len() - 1)
        })
        .into_owned();
    (out, table)
}

/// Restore placeholders. Unknown indices are left untouched so a mangled
/// reply degrades visibly instead of silently.
pub fn restore(text: &str, table: &[String]) -> String {
    RESTORE
        .replace_all(text, |caps: &regex::Captures| {
            let i: usize = caps[1].parse().unwrap_or(usize::MAX);
            table.get(i).cloned().unwrap_or_else(|| caps[0].to_string())
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_tags() {
        let (p, t) = protect(r"{\i1}Hello\Nworld{\i0} and \h more");
        assert_eq!(p, "⟦0⟧Hello⟦1⟧world⟦2⟧ and ⟦3⟧ more");
        assert_eq!(restore(&p, &t), r"{\i1}Hello\Nworld{\i0} and \h more");
    }

    #[test]
    fn leaves_plain_text_alone() {
        let (p, t) = protect("Just text");
        assert_eq!(p, "Just text");
        assert!(t.is_empty());
    }

    #[test]
    fn keeps_unknown_placeholders() {
        assert_eq!(restore("a ⟦9⟧ b", &[]), "a ⟦9⟧ b");
    }
}
