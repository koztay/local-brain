//! Claude Code hook handler: `local-brain hook <event>` reads the hook JSON on stdin.
//!
//! * PreToolUse Read  — images/PDFs are read locally; large files get a skeleton instead.
//! * PreToolUse Grep/Glob — oversized result sets are ranked with the graph and trimmed.
//! * PostToolUse Bash — long test/build/log output is compressed (`updatedToolOutput`).
//! * SessionStart — a short project brief is injected.
//! * Stop / SessionEnd — the session is summarized into memory in the background.
//!
//! Escape hatch: a blocked Read/Grep/Glob is allowed when Claude repeats the same call.
//! Hooks never fail the agent: any error results in "no opinion" (exit 0, no output).

use crate::config::Config;
use crate::llm::Llm;
use crate::paths::{self, Project};
use crate::store::Store;
use crate::util;
use anyhow::Result;
use globset::{GlobBuilder, GlobMatcher};
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

const RETRY_WINDOW: i64 = 900;

pub fn log(msg: &str) {
    use std::io::Write;
    let p = paths::log_file("hooks.log");
    if let Ok(md) = std::fs::metadata(&p) {
        if md.len() > 5_000_000 {
            let _ = std::fs::remove_file(&p);
        }
    }
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = writeln!(f, "{} {}", util::fmt_ts(util::now()), msg);
    }
}

pub fn run(event: &str) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let v: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    match handle(event, &v) {
        Ok(Some(out)) => println!("{}", out),
        Ok(None) => {}
        Err(e) => log(&format!("{} error: {:#}", event, e)),
    }
    Ok(())
}

fn project_for(v: &Value) -> Option<Project> {
    let cwd = v["cwd"].as_str().map(PathBuf::from);
    if let Ok(d) = std::env::var("CLAUDE_PROJECT_DIR") {
        if let Some(p) = paths::find_project(Path::new(&d)) {
            return Some(p);
        }
    }
    cwd.and_then(|c| paths::find_project(&c))
}

pub fn handle(event: &str, v: &Value) -> Result<Option<Value>> {
    let Some(project) = project_for(v) else {
        return Ok(None);
    };
    let cfg = Config::load(Some(&project));
    let store = Store::open(&project)?;
    let session = v["session_id"].as_str().unwrap_or("unknown").to_string();
    let tool = v["tool_name"].as_str().unwrap_or("");
    let input = &v["tool_input"];
    match (event, tool) {
        ("pre-tool-use", "Read") => pre_read(&project, &cfg, &store, &session, input),
        ("pre-tool-use", "Grep") if cfg.hooks.grep => {
            pre_grep(&project, &cfg, &store, &session, input, v)
        }
        ("pre-tool-use", "Glob") if cfg.hooks.glob => {
            pre_glob(&project, &cfg, &store, &session, input, v)
        }
        ("post-tool-use", "Bash") if cfg.hooks.bash => {
            post_bash(&project, &cfg, &store, &session, input, &v["tool_response"])
        }
        ("session-start", _) if cfg.hooks.session_brief => {
            session_start(&project, &cfg, &store, &session)
        }
        ("stop", _) if cfg.hooks.session_memory => {
            remember_pending(&store, &session, v["transcript_path"].as_str())?;
            Ok(None)
        }
        ("session-end", _) if cfg.hooks.session_memory => {
            if let Some(t) = v["transcript_path"].as_str() {
                spawn_ingest(&project, t, &session);
                forget_pending(&store, &session)?;
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn deny(reason: String) -> Option<Value> {
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }
    }))
}

/// Shared escape hatch: the second identical attempt within the window goes through.
fn escape(store: &Store, session: &str, key: &str) -> Result<bool> {
    if store.recently_denied(session, key, RETRY_WINDOW)? {
        store.clear_denial(session, key)?;
        return Ok(true);
    }
    Ok(false)
}

// ---- Read ----------------------------------------------------------------------------------

fn pre_read(
    project: &Project,
    cfg: &Config,
    store: &Store,
    session: &str,
    input: &Value,
) -> Result<Option<Value>> {
    let Some(fp) = input["file_path"].as_str() else {
        return Ok(None);
    };
    let abs = PathBuf::from(fp);
    if !abs.starts_with(&project.root) || !abs.is_file() {
        return Ok(None);
    }
    let rel = project.rel(&abs);
    let key = format!("read:{}", rel);
    let is_doc = crate::docs::is_image(&rel) || crate::docs::is_pdf(&rel);

    if is_doc && cfg.hooks.read_docs {
        if escape(store, session, &key)? {
            return Ok(None);
        }
        let mut c = cfg.clone();
        c.runtime.timeout_secs = c.runtime.timeout_secs.min(100);
        let llm = Llm::new(&c);
        return match crate::docs::read_doc(store, project, &c, &llm, &rel, None) {
            Ok(d) => {
                store.record_denial(session, &key)?;
                store.record_metric(
                    "hook",
                    "read_doc",
                    d.baseline_tokens,
                    util::tokens(&d.text),
                    Some(session),
                )?;
                Ok(deny(format!(
                    "[local-brain] This file was read locally to save image/PDF tokens; its content is below. \
                     For a specific question use brain_doc(path, question). If you really need the original, call Read on it again.\n\n{}",
                    d.text
                )))
            }
            Err(e) => {
                log(&format!("read_doc {}: {}", rel, e));
                Ok(None)
            }
        };
    }

    if !cfg.hooks.read_large || is_doc {
        return Ok(None);
    }
    if input.get("offset").map(|o| !o.is_null()).unwrap_or(false)
        || input.get("limit").map(|l| !l.is_null()).unwrap_or(false)
    {
        return Ok(None);
    }
    let md = std::fs::metadata(&abs)?;
    if md.len() < (cfg.hooks.large_file_tokens * 4) as u64 / 2 {
        return Ok(None);
    }
    let Some(text) = util::read_text(&abs) else {
        return Ok(None);
    };
    let lines = text.lines().count();
    let toks = util::tokens(&text);
    if lines <= cfg.hooks.large_file_lines && toks <= cfg.hooks.large_file_tokens {
        return Ok(None);
    }
    if escape(store, session, &key)? {
        return Ok(None);
    }
    let skel = crate::slice::skeleton(store, project, &rel)?;
    let (skel, _) = util::clip_tokens(&skel, 2500);
    store.record_denial(session, &key)?;
    // Claude's Read returns at most ~2000 lines / 25k tokens.
    store.record_metric(
        "hook",
        "read_large",
        toks.min(25_000),
        util::tokens(&skel),
        Some(session),
    )?;
    Ok(deny(format!(
        "[local-brain] {} is large ({} lines, ~{} tokens). Its outline is below. Fetch only what you need with \
         brain_slice(path=\"{}\", symbol=\"…\") or Read with offset/limit. To read the whole file anyway, call Read again unchanged.\n\n{}",
        rel, lines, util::human_tokens(toks), rel, skel
    )))
}

// ---- Grep / Glob -----------------------------------------------------------------------------

struct Hit {
    rel: String,
    lines: Vec<(usize, String)>,
    count: usize,
}

fn walk_files(base: &Path, hidden: bool) -> ignore::Walk {
    ignore::WalkBuilder::new(base)
        .hidden(!hidden)
        .git_ignore(true)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git" && e.file_name() != ".local-brain")
        .build()
}

fn glob_matcher(pat: &str, literal_sep: bool) -> Option<GlobMatcher> {
    GlobBuilder::new(pat)
        .literal_separator(literal_sep)
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

fn grep_files(
    project: &Project,
    base: &Path,
    re: &Regex,
    glob: Option<&str>,
    max_files: usize,
) -> Vec<Hit> {
    let gm = glob.and_then(|g| glob_matcher(g, false));
    let secrets = crate::scan::Filter::new(&[]);
    let mut out = Vec::new();
    let mut scanned = 0;
    for ent in walk_files(base, false).flatten() {
        if !ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        scanned += 1;
        if scanned > max_files {
            break;
        }
        let p = ent.path();
        let rel = project.rel(p);
        if secrets.is_secret(&rel) {
            continue;
        }
        if let Some(m) = &gm {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let rel_base = p.strip_prefix(base).unwrap_or(p);
            if !m.is_match(rel_base) && !m.is_match(&name) {
                continue;
            }
        }
        if ent.metadata().map(|m| m.len() > 2_000_000).unwrap_or(true) {
            continue;
        }
        let Some(text) = util::read_text(p) else {
            continue;
        };
        let mut lines = Vec::new();
        for (i, l) in text.lines().enumerate() {
            if re.is_match(l) {
                lines.push((i + 1, l.to_string()));
            }
        }
        if !lines.is_empty() {
            let count = lines.len();
            out.push(Hit { rel, lines, count });
        }
    }
    out
}

fn rank_hits(store: &Store, re: Option<&Regex>, hits: &mut [Hit]) {
    let score = |h: &Hit| -> f64 {
        let mut s = (1.0 + h.count as f64).ln();
        if let Ok(Some(f)) = store.file(&h.rel) {
            if f.kind == "code" {
                s += 1.0;
            }
            if let Some(re) = re {
                if let Ok(syms) = store.symbols_in_file(f.id) {
                    let defs = syms.iter().filter(|x| re.is_match(&x.name)).count();
                    s += 3.0 * defs.min(3) as f64;
                }
            }
        }
        let lower = h.rel.to_lowercase();
        if lower.contains("test")
            || lower.contains("spec")
            || lower.contains("fixture")
            || lower.contains("mock")
        {
            s -= 0.7;
        }
        if lower.contains("generated")
            || lower.contains(".min.")
            || lower.ends_with(".lock")
            || lower.contains("vendor")
        {
            s -= 2.0;
        }
        s - 0.05 * h.rel.matches('/').count() as f64
    };
    hits.sort_by(|a, b| {
        score(b)
            .partial_cmp(&score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn pre_grep(
    project: &Project,
    cfg: &Config,
    store: &Store,
    session: &str,
    input: &Value,
    v: &Value,
) -> Result<Option<Value>> {
    let Some(pattern) = input["pattern"].as_str() else {
        return Ok(None);
    };
    // Calls that are already narrow, or that we cannot reproduce faithfully, pass through.
    if input["head_limit"].as_u64().map(|n| n > 0).unwrap_or(false)
        || input["multiline"].as_bool() == Some(true)
        || input["type"].as_str().is_some()
    {
        return Ok(None);
    }
    let key = format!("grep:{}", util::sha256_hex(input.to_string().as_bytes()));
    if escape(store, session, &key)? {
        return Ok(None);
    }
    let ci = input["-i"].as_bool().unwrap_or(false);
    let Ok(re) = RegexBuilder::new(pattern).case_insensitive(ci).build() else {
        return Ok(None);
    };
    let base = input["path"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| v["cwd"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| project.root.clone());
    if !base.starts_with(&project.root) {
        return Ok(None);
    }
    if base.is_file() {
        return Ok(None);
    }
    let mode = input["output_mode"]
        .as_str()
        .unwrap_or("files_with_matches");
    let mut hits = grep_files(project, &base, &re, input["glob"].as_str(), 60_000);
    let total_lines: usize = hits.iter().map(|h| h.count).sum();
    let total = if mode == "content" {
        total_lines
    } else {
        hits.len()
    };
    if total <= cfg.hooks.grep_max_results {
        return Ok(None);
    }
    rank_hits(store, Some(&re), &mut hits);
    let full: String = match mode {
        "content" => hits
            .iter()
            .flat_map(|h| {
                h.lines
                    .iter()
                    .map(move |(n, l)| format!("{}:{}:{}", h.rel, n, l))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "count" => hits
            .iter()
            .map(|h| format!("{}:{}", h.rel, h.count))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => hits
            .iter()
            .map(|h| h.rel.clone())
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let full = crate::secrets::mask(&full);
    let raw_id = crate::raw::save(Some(project), &full, cfg.compress.raw_ttl_days)?;
    let show = cfg.hooks.grep_show;
    let shown: String = match mode {
        "content" => {
            let mut out = Vec::new();
            'outer: for h in &hits {
                for (n, l) in h.lines.iter().take(4) {
                    if out.len() >= show {
                        break 'outer;
                    }
                    out.push(format!("{}:{}:{}", h.rel, n, util::clip(l.trim_end(), 220)));
                }
            }
            out.join("\n")
        }
        "count" => hits
            .iter()
            .take(show)
            .map(|h| format!("{}:{}", h.rel, h.count))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => hits
            .iter()
            .take(show)
            .map(|h| format!("{} ({} matches)", h.rel, h.count))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let shown = crate::secrets::mask(&shown);
    store.record_denial(session, &key)?;
    store.record_metric(
        "hook",
        "grep",
        util::tokens(&full),
        util::tokens(&shown) + 120,
        Some(session),
    )?;
    Ok(deny(format!(
        "[local-brain] This search matched {} {} in {} files (~{} tokens of output). The most relevant results are below \
         (ranked with the code graph: definitions first, tests and generated files last). Full results: \
         brain_raw(raw_id=\"{}\", grep=…). Narrow the pattern/path, or use brain_query for conceptual questions. \
         Repeat the identical Grep call to get the unfiltered output.\n\n{}",
        total,
        if mode == "content" { "lines" } else { "files" },
        hits.len(),
        util::human_tokens(util::tokens(&full)),
        raw_id,
        shown
    )))
}

fn pre_glob(
    project: &Project,
    cfg: &Config,
    store: &Store,
    session: &str,
    input: &Value,
    v: &Value,
) -> Result<Option<Value>> {
    let Some(pattern) = input["pattern"].as_str() else {
        return Ok(None);
    };
    let base = input["path"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| v["cwd"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| project.root.clone());
    if !base.starts_with(&project.root) || !base.is_dir() {
        return Ok(None);
    }
    let key = format!("glob:{}", util::sha256_hex(input.to_string().as_bytes()));
    if escape(store, session, &key)? {
        return Ok(None);
    }
    let Some(m) = glob_matcher(pattern, true) else {
        return Ok(None);
    };
    let mut found: Vec<(String, i64)> = Vec::new();
    for ent in walk_files(&base, true).flatten().take(200_000) {
        if !ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let rel_base = ent.path().strip_prefix(&base).unwrap_or(ent.path());
        if m.is_match(rel_base) {
            let mtime = ent
                .metadata()
                .ok()
                .and_then(|md| md.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            found.push((project.rel(ent.path()), mtime));
        }
    }
    if found.len() <= cfg.hooks.glob_max_results {
        return Ok(None);
    }
    // Like the Glob tool: most recently modified first.
    found.sort_by_key(|f| std::cmp::Reverse(f.1));
    let full = found
        .iter()
        .map(|f| f.0.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let raw_id = crate::raw::save(Some(project), &full, cfg.compress.raw_ttl_days)?;
    // Summarize by directory so the shape of the result is visible.
    let mut dirs: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (p, _) in &found {
        let d = Path::new(p)
            .parent()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default();
        *dirs
            .entry(if d.is_empty() { ".".into() } else { d })
            .or_default() += 1;
    }
    let mut dv: Vec<(String, usize)> = dirs.into_iter().collect();
    dv.sort_by_key(|d| std::cmp::Reverse(d.1));
    let shown: Vec<String> = found
        .iter()
        .take(cfg.hooks.grep_show)
        .map(|f| f.0.clone())
        .collect();
    let by_dir: Vec<String> = dv
        .iter()
        .take(15)
        .map(|(d, n)| format!("{}/ ({})", d, n))
        .collect();
    let msg = format!(
        "[local-brain] Glob `{}` matched {} files. {} most recently modified:\n{}\n\nBy directory: {}\n\nFull list: brain_raw(raw_id=\"{}\", grep=…). Use a narrower pattern, or repeat the identical Glob call for the unfiltered list.",
        pattern,
        found.len(),
        shown.len(),
        shown.join("\n"),
        by_dir.join(", "),
        raw_id
    );
    store.record_denial(session, &key)?;
    store.record_metric(
        "hook",
        "glob",
        util::tokens(&full),
        util::tokens(&msg),
        Some(session),
    )?;
    Ok(deny(msg))
}

// ---- Bash ------------------------------------------------------------------------------------

/// How much larger than the threshold an output must be before it is compressed.
fn bash_factor(cmd: &str) -> i64 {
    use std::sync::OnceLock;
    static READ: OnceLock<Regex> = OnceLock::new();
    static NOISY: OnceLock<Regex> = OnceLock::new();
    let read = READ.get_or_init(|| {
        Regex::new(
            r"^\s*(cat|head|less|more|bat|nl|sed\s+-n|awk|jq|git\s+(diff|show)|git\s+log\s+.*-p)\b",
        )
        .unwrap()
    });
    let noisy = NOISY.get_or_init(|| {
        Regex::new(r"(?i)\b(test|tests|pytest|jest|vitest|mocha|karma|cargo|go\s+(test|build|vet)|npm|pnpm|yarn|bun|npx|make|cmake|ninja|gradle|gradlew|mvn|xcodebuild|swift\s+(build|test)|tsc|eslint|ruff|mypy|flake8|pylint|black|prettier|lint|pip|poetry|uv|bundle|rake|docker|kubectl|journalctl|logs?|tail|dotnet|mix|composer|phpunit|rspec|terraform|ansible)\b").unwrap()
    });
    if cmd.contains("local-brain run") {
        return i64::MAX / 4;
    }
    if read.is_match(cmd) {
        10
    } else if noisy.is_match(cmd) {
        1
    } else {
        2
    }
}

fn post_bash(
    project: &Project,
    cfg: &Config,
    store: &Store,
    session: &str,
    input: &Value,
    resp: &Value,
) -> Result<Option<Value>> {
    let cmd = input["command"].as_str().unwrap_or("");
    let factor = bash_factor(cmd);
    let mut c = cfg.clone();
    c.compress.threshold_tokens = cfg.compress.threshold_tokens.saturating_mul(factor);
    c.runtime.timeout_secs = c.runtime.timeout_secs.min(25);
    let llm = Llm::new(&c);
    let label = util::clip(cmd, 200);
    match resp {
        Value::Object(obj) => {
            let stdout = obj.get("stdout").and_then(|s| s.as_str()).unwrap_or("");
            let stderr = obj.get("stderr").and_then(|s| s.as_str()).unwrap_or("");
            if util::tokens(stdout) + util::tokens(stderr) <= c.compress.threshold_tokens {
                return Ok(None);
            }
            let mut new = obj.clone();
            let mut before = 0;
            let mut after = 0;
            for (field, text) in [("stdout", stdout), ("stderr", stderr)] {
                if text.is_empty() {
                    continue;
                }
                let comp =
                    crate::compress::compress(Some(project), &c, Some(&llm), text, &label, None);
                before += comp.original_tokens;
                after += comp.tokens;
                new.insert(field.to_string(), Value::String(comp.text));
            }
            if after >= before {
                return Ok(None);
            }
            store.record_metric("hook", "bash", before, after, Some(session))?;
            Ok(Some(
                json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": Value::Object(new)}}),
            ))
        }
        Value::String(text) => {
            let comp = crate::compress::compress(Some(project), &c, Some(&llm), text, &label, None);
            if comp.raw_id.is_none() {
                return Ok(None);
            }
            store.record_metric(
                "hook",
                "bash",
                comp.original_tokens,
                comp.tokens,
                Some(session),
            )?;
            Ok(Some(
                json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": comp.text}}),
            ))
        }
        _ => Ok(None),
    }
}

// ---- Sessions --------------------------------------------------------------------------------

fn pending(store: &Store) -> serde_json::Map<String, Value> {
    store
        .meta("pending_sessions")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn remember_pending(store: &Store, session: &str, transcript: Option<&str>) -> Result<()> {
    let Some(t) = transcript else { return Ok(()) };
    let mut m = pending(store);
    m.insert(session.to_string(), Value::String(t.to_string()));
    store.set_meta("pending_sessions", &Value::Object(m).to_string())
}

fn forget_pending(store: &Store, session: &str) -> Result<()> {
    let mut m = pending(store);
    m.remove(session);
    store.set_meta("pending_sessions", &Value::Object(m).to_string())
}

pub fn spawn_ingest(project: &Project, transcript: &str, session: &str) {
    let mut cmd = std::process::Command::new(paths::exe());
    cmd.args([
        "memory",
        "ingest",
        "--transcript",
        transcript,
        "--session",
        session,
        "--project",
    ])
    .arg(&project.root)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Err(e) = cmd.spawn() {
        log(&format!("spawn ingest: {}", e));
    }
}

fn session_start(
    project: &Project,
    cfg: &Config,
    store: &Store,
    session: &str,
) -> Result<Option<Value>> {
    // Sessions that ended without SessionEnd (e.g. a closed terminal) are summarized now.
    for (sid, path) in pending(store) {
        if sid != session {
            if let Some(p) = path.as_str() {
                spawn_ingest(project, p, &sid);
            }
            forget_pending(store, &sid)?;
        }
    }
    let mut brief = crate::memory::brief(store, project, cfg.hooks.brief_tokens)?;
    if store.meta("last_learn")?.is_none() {
        brief.push_str("(The index is empty: run `local-brain learn` in the project root.)\n");
    }
    store.record_metric(
        "hook",
        "session_brief",
        0,
        util::tokens(&brief),
        Some(session),
    )?;
    Ok(Some(
        json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": brief}}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_factors() {
        assert_eq!(bash_factor("cargo test --all"), 1);
        assert_eq!(bash_factor("cat src/main.rs"), 10);
        assert_eq!(bash_factor("ls -la"), 2);
        assert!(bash_factor("local-brain run -- make") > 1000);
    }
}
