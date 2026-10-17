//! Small shared helpers: token estimates, hashing, time, text utilities.

use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Rough token estimate (≈4 chars per token for code and English text).
pub fn tokens(s: &str) -> i64 {
    ((s.len() as f64) / 4.0).ceil() as i64
}

pub fn tokens_len(bytes: u64) -> i64 {
    ((bytes as f64) / 4.0).ceil() as i64
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

pub fn short_id() -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let h = sha256_hex(format!("{}-{}", n, std::process::id()).as_bytes());
    h[..10].to_string()
}

pub fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".into())
}

pub fn today_start() -> i64 {
    use chrono::{Local, TimeZone};
    let d = Local::now().date_naive();
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .map(|t| t.timestamp())
        .unwrap_or(0)
}

/// Quote a string for POSIX sh.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Truncate a string to at most `max` chars, appending an ellipsis marker.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Keep the text under a token budget, cutting at a line boundary.
pub fn clip_tokens(s: &str, max_tokens: i64) -> (String, bool) {
    if tokens(s) <= max_tokens {
        return (s.to_string(), false);
    }
    let max_bytes = (max_tokens * 4) as usize;
    let mut out = String::new();
    for line in s.lines() {
        if out.len() + line.len() + 1 > max_bytes {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, true)
}

/// Split identifiers like `parseHTTPRequest` / `parse_http_request` into lowercase words.
pub fn split_ident(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = s.chars().collect();
    for i in 0..chars.len() {
        let c = chars[i];
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(cur.to_lowercase());
                cur.clear();
            }
            continue;
        }
        if c.is_uppercase() && !cur.is_empty() {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).map(|n| n.is_lowercase()).unwrap_or(false);
            if prev.is_lowercase() || prev.is_numeric() || (prev.is_uppercase() && next_lower) {
                words.push(cur.to_lowercase());
                cur.clear();
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        words.push(cur.to_lowercase());
    }
    words
}

/// Format lines with 1-based line numbers, like `cat -n`.
pub fn numbered(text: &str, start_line: usize) -> String {
    let mut out = String::new();
    for (i, l) in text.lines().enumerate() {
        out.push_str(&format!("{:>5}  {}\n", start_line + i, l));
    }
    out
}

pub fn human_tokens(n: i64) -> String {
    if n.abs() >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n.abs() >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

pub fn human_bytes(n: u64) -> String {
    let f = n as f64;
    if f >= 1e9 {
        format!("{:.1} GB", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1} MB", f / 1e6)
    } else if f >= 1e3 {
        format!("{:.1} KB", f / 1e3)
    } else {
        format!("{} B", n)
    }
}

/// Read a file as UTF-8 text; returns None for binary/non-UTF-8 content.
pub fn read_text(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(8000).any(|b| *b == 0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Strip ANSI escape sequences.
pub fn strip_ansi(s: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07|\x1b[()][A-Za-z0-9]")
            .unwrap()
    });
    re.replace_all(s, "").into_owned()
}

/// Ask the user a yes/no question on the terminal. Returns `default` when stdin is not a TTY.
pub fn confirm(question: &str, default: bool) -> bool {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return default;
    }
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    eprint!("{} {} ", question, hint);
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return default;
    }
    match line.trim().to_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_identifiers() {
        assert_eq!(
            split_ident("parseHTTPRequest"),
            vec!["parse", "http", "request"]
        );
        assert_eq!(split_ident("brain_query"), vec!["brain", "query"]);
        assert_eq!(split_ident("Foo::bar"), vec!["foo", "bar"]);
    }

    #[test]
    fn quotes_shell() {
        assert_eq!(sh_quote("abc"), "abc");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    }
}
