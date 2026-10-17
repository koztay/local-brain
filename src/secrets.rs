//! Secret masking for every piece of text local-brain hands to an agent or stores raw.

use regex::Regex;
use std::sync::OnceLock;

fn patterns() -> &'static Vec<(Regex, &'static str)> {
    static P: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    P.get_or_init(|| {
        let raw: Vec<(&str, &str)> = vec![
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----", "[private key masked]"),
            (r"\bAKIA[0-9A-Z]{16}\b", "[aws key masked]"),
            (r"\bgh[pousr]_[A-Za-z0-9]{30,}\b", "[github token masked]"),
            (r"\bgithub_pat_[A-Za-z0-9_]{40,}\b", "[github token masked]"),
            (r"\bsk-ant-[A-Za-z0-9_\-]{20,}", "[anthropic key masked]"),
            (r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{20,}", "[api key masked]"),
            (r"\bxox[abprs]-[A-Za-z0-9\-]{10,}", "[slack token masked]"),
            (r"\bAIza[0-9A-Za-z_\-]{35}\b", "[google key masked]"),
            (r"\b(?:sk|rk|pk)_live_[0-9a-zA-Z]{20,}\b", "[stripe key masked]"),
            (r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\b", "[jwt masked]"),
            (r"(?i)\b(bearer)\s+[A-Za-z0-9_\-\.=]{20,}", "$1 [token masked]"),
            (r"(?i)([a-z][a-z0-9+.\-]*://[^:/\s]+:)[^@/\s]{3,}@", "${1}[password masked]@"),
            (
                r#"(?i)\b([A-Z0-9_]*(?:api[_-]?key|secret|token|passwd|password|private[_-]?key|access[_-]?key)[A-Z0-9_]*)\s*([:=])\s*["']?[^\s"',;]{8,}["']?"#,
                "$1$2 [masked]",
            ),
        ];
        raw.into_iter().filter_map(|(p, r)| Regex::new(p).ok().map(|re| (re, r))).collect()
    })
}

pub fn mask(text: &str) -> String {
    let mut out = std::borrow::Cow::Borrowed(text);
    for (re, rep) in patterns() {
        if re.is_match(&out) {
            out = std::borrow::Cow::Owned(re.replace_all(&out, *rep).into_owned());
        }
    }
    out.into_owned()
}

#[cfg(test)]
mod tests {
    use super::mask;

    #[test]
    fn masks_common_secrets() {
        assert!(!mask("token ghp_abcdefghijklmnopqrstuvwxyz0123456789").contains("ghp_abc"));
        assert!(!mask("AWS AKIAABCDEFGHIJKLMNOP").contains("AKIAABCD"));
        assert!(!mask("OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwx").contains("abcdefghijkl"));
        assert!(!mask("postgres://user:hunter2pass@db:5432/x").contains("hunter2"));
        assert!(mask("password = \"correcthorsebattery\"").contains("[masked]"));
        assert_eq!(mask("nothing to see here"), "nothing to see here");
        assert_eq!(mask("let token_count = 5;"), "let token_count = 5;");
    }
}
