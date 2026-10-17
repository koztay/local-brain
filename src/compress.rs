//! Output compression for test/build/lint/log output, grep results and the like.
//!
//! Rules first (strip progress bars and ANSI, collapse repeats, count passing tests), the local
//! LLM only when rules are not enough. Errors, warnings, stack traces, failing test names and
//! the exit code are never dropped. The full output is kept under a `raw_id`.

use crate::config::Config;
use crate::llm::Llm;
use crate::paths::Project;
use crate::util;
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub struct Compressed {
    pub text: String,
    pub raw_id: Option<String>,
    pub original_tokens: i64,
    pub tokens: i64,
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

fn is_important(l: &str) -> bool {
    static IMP: OnceLock<Regex> = OnceLock::new();
    static LOC: OnceLock<Regex> = OnceLock::new();
    let imp = re(
        &IMP,
        r#"(?i)(\berror\b|\berrors\b|\berr!|\bfatal\b|\bpanic|\bexception\b|traceback|\bfail(ed|ure|ing|s)?\b|\bwarn(ing)?s?\b|\bassert|segmentation fault|\bundefined\b|cannot find|could not|not found|\bexpected\b|\bunexpected\b|\bdenied\b|\babort|\btimed? ?out\b|^\s*E\s{2,}|^\s*-->\s|^\s*at\s+\S+.*[:(]\d+|^\s*File "[^"]+", line \d+|^error\[|^\s*(✗|✘|×|●|FAIL)\s|\bexit (code|status)\b|^\s*\^+\s*$)"#,
    );
    if imp.is_match(l) {
        // "0 errors", "0 failed", "warnings: 0" are success noise, not problems.
        static ZERO: OnceLock<Regex> = OnceLock::new();
        let zero = re(
            &ZERO,
            r"(?i)\b0 (errors?|failed|failures?|warnings?)\b|(errors?|failures?|warnings?):?\s*0\b",
        );
        return !(zero.is_match(l) && !l.to_lowercase().contains("error[") && !l.contains("FAIL"));
    }
    let loc = re(
        &LOC,
        r"[\w./\\-]+\.(rs|py|ts|tsx|js|jsx|go|java|kt|swift|rb|c|cc|cpp|h|cs|php):\d+",
    );
    loc.is_match(l) && l.len() < 400
}

fn starts_trace(l: &str) -> bool {
    let t = l.trim_start();
    t.starts_with("Traceback")
        || t.contains("panicked at")
        || t.starts_with("stack backtrace")
        || t.starts_with("Caused by")
        || t.starts_with("Exception in")
        || t.contains("Error:")
        || t.starts_with("goroutine ")
}

fn is_pass(l: &str) -> bool {
    static P: OnceLock<Regex> = OnceLock::new();
    re(
        &P,
        r"(?i)(^test .+ \.\.\. ok$|^\s*(✓|√|✔)\s|^\s*PASS\s|^ok\s+\S+\s|\bPASSED\b|^\s*\d+ passing|^\s+ok \d+ )",
    )
    .is_match(l)
}

fn category(l: &str) -> &'static str {
    static NOISE: OnceLock<Regex> = OnceLock::new();
    let t = l.trim_start();
    if t.is_empty() {
        return "blank";
    }
    if is_pass(l) {
        return "passing tests";
    }
    let noise = re(
        &NOISE,
        r"(?i)^(compiling|downloading|downloaded|fetching|collecting|installing|building|checking|resolving|requirement already satisfied|using cached|unpacking|preparing|added \d+ packages|npm (http|timing)|\[\d+/\d+\]|progress)",
    );
    if noise.is_match(t) {
        return "progress/build noise";
    }
    "other output"
}

/// Normalize digits/hex so lines that differ only in numbers collapse together.
fn shape(l: &str) -> String {
    static D: OnceLock<Regex> = OnceLock::new();
    re(&D, r"0x[0-9a-fA-F]+|\d+(\.\d+)?")
        .replace_all(l.trim(), "#")
        .into_owned()
}

/// Remove ANSI sequences and carriage-return progress redraws.
pub fn clean(input: &str) -> String {
    let s = util::strip_ansi(input);
    let mut out = String::with_capacity(s.len());
    for line in s.split('\n') {
        let l = line.trim_end_matches('\r');
        let l = l.rsplit('\r').next().unwrap_or(l);
        out.push_str(l);
        out.push('\n');
    }
    if !input.ends_with('\n') {
        out.pop();
    }
    out
}

/// Deterministic compression. Returns the compressed body (without footer).
pub fn compress_rules(text: &str, target_tokens: i64) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let mut keep = vec![false; n];
    let mut important = vec![false; n];
    let head = 6.min(n);
    let tail = 20.min(n);
    for k in keep.iter_mut().take(head) {
        *k = true;
    }
    for k in keep.iter_mut().skip(n - tail) {
        *k = true;
    }
    let mut i = 0;
    while i < n {
        if is_important(lines[i]) {
            important[i] = true;
            keep[i] = true;
            if i > 0 && category(lines[i - 1]) == "other output" {
                keep[i - 1] = true;
            }
            if i + 1 < n {
                keep[i + 1] = true;
            }
        }
        if starts_trace(lines[i]) {
            // Keep the whole stack trace: following indented / frame lines.
            let mut j = i + 1;
            while j < n && j - i < 80 {
                let l = lines[j];
                let frame = l.starts_with(' ')
                    || l.starts_with('\t')
                    || l.trim_start().starts_with("at ")
                    || l.trim()
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false);
                if !frame || l.trim().is_empty() {
                    break;
                }
                keep[j] = true;
                important[j] = true;
                j += 1;
            }
            important[i] = true;
            keep[i] = true;
        }
        i += 1;
    }
    // Passing tests and noise inside the head/tail are summarized too.
    for idx in 0..n {
        if keep[idx]
            && !important[idx]
            && category(lines[idx]) != "other output"
            && idx >= head
            && idx < n - tail
        {
            keep[idx] = false;
        }
    }

    let mut out = String::new();
    let mut i = 0;
    let mut last_shape = String::new();
    let mut repeat = 0;
    while i < n {
        if keep[i] {
            let sh = shape(lines[i]);
            if sh == last_shape && !important[i] {
                repeat += 1;
                i += 1;
                continue;
            }
            if repeat > 0 {
                out.push_str(&format!(
                    "    … (previous line repeated {}× with different numbers)\n",
                    repeat
                ));
                repeat = 0;
            }
            out.push_str(lines[i]);
            out.push('\n');
            last_shape = sh;
            i += 1;
            continue;
        }
        if repeat > 0 {
            out.push_str(&format!(
                "    … (previous line repeated {}× with different numbers)\n",
                repeat
            ));
            repeat = 0;
        }
        let start = i;
        let mut cats: BTreeMap<&str, usize> = BTreeMap::new();
        while i < n && !keep[i] {
            *cats.entry(category(lines[i])).or_default() += 1;
            i += 1;
        }
        cats.remove("blank");
        if cats.is_empty() {
            continue;
        }
        let parts: Vec<String> = cats.iter().map(|(k, v)| format!("{} {}", v, k)).collect();
        out.push_str(&format!(
            "[… {} lines omitted: {}]\n",
            i - start,
            parts.join(", ")
        ));
        last_shape.clear();
    }
    if repeat > 0 {
        out.push_str(&format!(
            "    … (previous line repeated {}× with different numbers)\n",
            repeat
        ));
    }

    // Still too large: trim non-important "other output" from the middle, never important lines.
    if util::tokens(&out) > target_tokens * 2 {
        let ls: Vec<&str> = out.lines().collect();
        let mut trimmed = String::new();
        let budget = (target_tokens * 4) as usize;
        let mut dropped = 0;
        let keep_tail = 15.min(ls.len());
        for (idx, l) in ls.iter().enumerate() {
            let essential =
                is_important(l) || l.starts_with("[…") || idx >= ls.len() - keep_tail || idx < head;
            if essential || trimmed.len() + l.len() < budget {
                if dropped > 0 {
                    trimmed.push_str(&format!("[… {} more lines omitted]\n", dropped));
                    dropped = 0;
                }
                trimmed.push_str(l);
                trimmed.push('\n');
            } else {
                dropped += 1;
            }
        }
        if dropped > 0 {
            trimmed.push_str(&format!("[… {} more lines omitted]\n", dropped));
        }
        out = trimmed;
    }
    out
}

/// Compress `input` if it exceeds the threshold. `label` describes the source (e.g. the command).
pub fn compress(
    project: Option<&Project>,
    cfg: &Config,
    llm: Option<&Llm>,
    input: &str,
    label: &str,
    exit_code: Option<i32>,
) -> Compressed {
    let cleaned = crate::secrets::mask(&clean(input));
    let original_tokens = util::tokens(input);
    let cleaned_tokens = util::tokens(&cleaned);
    if cleaned_tokens <= cfg.compress.threshold_tokens {
        return Compressed {
            tokens: cleaned_tokens,
            text: cleaned,
            raw_id: None,
            original_tokens,
        };
    }
    let raw_id = crate::raw::save(project, &cleaned, cfg.compress.raw_ttl_days).ok();
    let mut body = compress_rules(&cleaned, cfg.compress.target_tokens);
    let mut how = "rules";
    if util::tokens(&body) > cfg.compress.target_tokens * 3 / 2 && cfg.compress.llm_summary {
        if let Some(llm) = llm.filter(|l| l.has_text()) {
            let (excerpt, _) = util::clip_tokens(&cleaned, 6000);
            let prompt = format!(
                "Summarize this command output in at most 5 short bullet points: what ran, the outcome, key counts and the most important problems. Do not invent anything. Output only the bullets.\n\nCommand: {}\nOutput:\n{}",
                label, excerpt
            );
            if let Ok(s) = llm.text(&prompt, 250) {
                let important: Vec<&str> = body.lines().filter(|l| is_important(l)).collect();
                let (imp, cut) = util::clip_tokens(
                    &important.join("\n"),
                    cfg.compress.target_tokens.max(600) * 2,
                );
                body = format!(
                    "Summary (local model):\n{}\n\nKey lines:\n{}{}",
                    s.trim(),
                    imp,
                    if cut {
                        "[… more key lines in raw output]\n"
                    } else {
                        ""
                    }
                );
                how = "rules+llm";
            }
        }
    }
    let mut text = body;
    let footer = format!(
        "\n[local-brain: output compressed {} → {} tokens ({}){}. Full output: brain_raw raw_id=\"{}\" (supports grep/range).]",
        util::human_tokens(original_tokens),
        util::human_tokens(util::tokens(&text)),
        how,
        exit_code.map(|c| format!(", exit code {}", c)).unwrap_or_default(),
        raw_id.clone().unwrap_or_else(|| "unavailable".into())
    );
    text.push_str(&footer);
    Compressed {
        tokens: util::tokens(&text),
        text,
        raw_id,
        original_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_errors_and_counts_passes() {
        let mut s = String::from("running 500 tests\n");
        for i in 0..500 {
            s.push_str(&format!("test module::case_{} ... ok\n", i));
        }
        s.push_str("test module::broken ... FAILED\n");
        s.push_str("thread 'broken' panicked at src/lib.rs:10:5:\n  assertion failed: x == 2\n");
        s.push_str("test result: FAILED. 500 passed; 1 failed\n");
        let out = compress_rules(&s, 500);
        assert!(out.contains("broken ... FAILED"));
        assert!(out.contains("panicked at src/lib.rs:10:5"));
        assert!(out.contains("assertion failed"));
        assert!(out.contains("passing tests"));
        assert!(util::tokens(&out) < util::tokens(&s) / 5);
    }

    #[test]
    fn strips_progress_redraws() {
        assert_eq!(clean("10%\r50%\r100% done\nnext"), "100% done\nnext");
    }

    #[test]
    fn zero_errors_is_not_important() {
        assert!(!is_important("Finished: 0 errors, 0 warnings"));
        assert!(is_important("error[E0425]: cannot find value `x`"));
        assert!(is_important("  File \"app.py\", line 3, in <module>"));
    }
}
