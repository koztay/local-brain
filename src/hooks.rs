//! Claude Code hook handler: `local-brain hook <event>` reads the hook JSON on stdin.
//!
//! * PostToolUse Read — images/PDFs are replaced by their local description and large files by
//!   their outline. It runs after Claude Code's permission check, so local-brain never reads a
//!   file Claude Code was not allowed to read.
//! * PreToolUse Grep/Glob — oversized result sets are ranked with the graph and trimmed.
//! * PostToolUse Bash — long test/build/log output is compressed (`updatedToolOutput`).
//! * SessionStart — a short project brief is injected.
//! * UserPromptSubmit — the files most relevant to the prompt are added as context, so Claude
//!   starts from the right code instead of exploring.
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
        ("pre-tool-use", "Grep") if cfg.hooks.grep => {
            pre_grep(&project, &cfg, &store, &session, input, v)
        }
        ("pre-tool-use", "Glob") if cfg.hooks.glob => {
            pre_glob(&project, &cfg, &store, &session, input, v)
        }
        ("post-tool-use", "Bash") if cfg.hooks.bash => {
            post_bash(&project, &cfg, &store, &session, input, v)
        }
        ("post-tool-use", "Read") => {
            post_read(&project, &cfg, &store, &session, input, &v["tool_response"])
        }
        ("session-start", _) if cfg.hooks.session_brief => {
            session_start(&project, &cfg, &store, &session)
        }
        ("user-prompt-submit", _) if cfg.hooks.prompt_context => prompt_context(
            &project,
            &cfg,
            &store,
            &session,
            v["prompt"].as_str().unwrap_or(""),
        ),
        ("stop", _) if cfg.hooks.session_memory => {
            remember_pending(&store, &session, v["transcript_path"].as_str())?;
            Ok(None)
        }
        ("session-end", _) if cfg.hooks.session_memory => {
            if let Some(t) = v["transcript_path"].as_str() {
                // Claude Code writes no transcript for a session without messages.
                if Path::new(t).is_file() {
                    spawn_ingest(&project, t, &session);
                }
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

/// True when `path` is a file whose real location (symlinks and `..` resolved) is inside the
/// project.
fn inside_project(project: &Project, path: &Path) -> bool {
    match (
        std::fs::canonicalize(path),
        std::fs::canonicalize(&project.root),
    ) {
        (Ok(p), Ok(root)) => p.is_file() && p.starts_with(root),
        _ => false,
    }
}

/// A replacement for a Read result, in the shape of a text-file Read (required by Claude Code).
fn text_result(file_path: &str, content: String) -> Value {
    let lines = content.lines().count();
    json!({"hookSpecificOutput": {
        "hookEventName": "PostToolUse",
        "updatedToolOutput": {"type": "text", "file": {
            "filePath": file_path, "content": content,
            "numLines": lines, "startLine": 1, "totalLines": lines
        }}
    }})
}

/// PostToolUse Read. Runs after Claude Code's permission check, so local-brain only ever
/// processes files Claude Code itself was allowed to read:
/// * images/PDFs (in the project, or outside it when `read_outside_docs`) are replaced by their
///   local description;
/// * large text files in the project are replaced by their outline.
///
/// Reading the same file again within the retry window returns the original.
fn post_read(
    project: &Project,
    cfg: &Config,
    store: &Store,
    session: &str,
    input: &Value,
    resp: &Value,
) -> Result<Option<Value>> {
    let Some(fp) = input["file_path"].as_str() else {
        return Ok(None);
    };
    // An explicit offset/limit is the way to get the original: Claude Code itself rejects an
    // identical repeated Read, so "read it again" cannot be the escape hatch.
    if input.get("offset").map(|o| !o.is_null()).unwrap_or(false)
        || input.get("limit").map(|l| !l.is_null()).unwrap_or(false)
    {
        return Ok(None);
    }
    let abs = PathBuf::from(fp);
    let inside = inside_project(project, &abs);
    let name = abs.to_string_lossy().to_string();
    let is_doc = crate::docs::is_image(&name) || crate::docs::is_pdf(&name);

    if is_doc {
        if !cfg.hooks.read_docs
            || (!inside && !cfg.hooks.read_outside_docs)
            || resp["type"] == "text"
        {
            return Ok(None);
        }
        let path = if inside { project.rel(&abs) } else { name };
        let key = format!("read:{}", path);
        if escape(store, session, &key)? {
            return Ok(None);
        }
        // The prompt's note already carried this document's local description; a Read after
        // that is a request for the pixels.
        if store.recently_denied(session, &format!("noted:{}", path), RETRY_WINDOW)? {
            return Ok(None);
        }
        let mut c = cfg.clone();
        c.runtime.timeout_secs = c.runtime.timeout_secs.min(100);
        let llm = Llm::new(&c);
        let d = match crate::docs::read_doc(store, project, &c, &llm, &path, None) {
            Ok(d) => d,
            Err(e) => {
                log(&format!("read_doc {}: {}", path, e));
                return Ok(None);
            }
        };
        // A small image costs Claude less than its description would; let it through.
        if crate::docs::is_image(&path) && d.baseline_tokens <= util::tokens(&d.text) + 60 {
            return Ok(None);
        }
        store.record_denial(session, &key)?;
        store.record_metric(
            "hook",
            "read_doc",
            d.baseline_tokens,
            util::tokens(&d.text),
            Some(session),
        )?;
        return Ok(Some(text_result(
            fp,
            format!(
                "[local-brain] Read locally (no image tokens). For exact text, numbers or layout, \
                 Read again with offset: 1 to get the original.\n\n{}\n",
                d.text
            ),
        )));
    }

    // Large text files in the project → outline.
    if !inside || !cfg.hooks.read_large || resp["type"] != "text" {
        return Ok(None);
    }
    let Some(text) = resp["file"]["content"].as_str() else {
        return Ok(None);
    };
    let lines = resp["file"]["totalLines"]
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or_else(|| text.lines().count());
    let toks = util::tokens(text);
    if lines <= cfg.hooks.large_file_lines && toks <= cfg.hooks.large_file_tokens {
        return Ok(None);
    }
    let rel = project.rel(&abs);
    let key = format!("read:{}", rel);
    if escape(store, session, &key)? {
        return Ok(None);
    }
    let skel = crate::slice::skeleton(store, project, &rel)?;
    let (skel, _) = util::clip_tokens(&skel, 2500);
    store.record_denial(session, &key)?;
    store.record_metric(
        "hook",
        "read_large",
        toks,
        util::tokens(&skel),
        Some(session),
    )?;
    Ok(Some(text_result(
        fp,
        format!(
            "[local-brain] {} is large ({} lines, ~{} tokens). Its outline is below. Fetch only what \
             you need with brain_slice(path=\"{}\", symbol=\"…\") or Read with offset/limit. To read \
             the whole file anyway, read it in ranges (offset: 1, limit: 1000, then offset: 1001, …).\n\n{}\n",
            rel,
            lines,
            util::human_tokens(toks),
            rel,
            skel
        ),
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

// ---- Bash reads ------------------------------------------------------------------------------
//
// Claude often reads and searches through Bash (`cat`, `grep -rn`) instead of Read and Grep, and
// in bypass-permissions mode Claude Code tells it to. These get the same treatment as the tools:
// a large file is replaced by its outline, an oversized recursive search is ranked and trimmed.
// Narrow reads (`sed -n`, `head`, piped output) pass through, and repeating the identical command
// within the retry window returns the original.

/// Shell words of one simple command: whitespace-separated, quotes and backslash escapes removed.
fn shell_words(seg: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = seg.chars();
    let mut started = false;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    started = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The simple commands of a command line (split on `;`, `&&`, `||` and newlines). Returns None
/// when the line uses constructs whose output we cannot attribute (subshells, redirections into
/// the output, loops).
fn simple_commands(cmd: &str) -> Option<Vec<String>> {
    if cmd.contains("$(") || cmd.contains('`') || cmd.contains("<(") {
        return None;
    }
    let mut segs = Vec::new();
    for line in cmd.lines() {
        for a in line.split("&&") {
            for b in a.split("||") {
                for c in b.split(';') {
                    let t = c.trim();
                    if !t.is_empty() {
                        segs.push(t.to_string());
                    }
                }
            }
        }
    }
    if segs.iter().any(|s| {
        let w = s.split_whitespace().next().unwrap_or("");
        matches!(
            w,
            "for" | "while" | "do" | "done" | "if" | "then" | "fi" | "case"
        )
    }) {
        return None;
    }
    Some(segs)
}

/// Files a `cat` prints whole: `cat a.ts b.ts` without a pipe. Paths resolved against `cwd`.
fn cat_files(cmd: &str, cwd: &Path) -> Vec<PathBuf> {
    let Some(segs) = simple_commands(cmd) else {
        return vec![];
    };
    let mut out = Vec::new();
    for seg in segs {
        if seg.contains('|') || seg.contains('>') {
            continue;
        }
        let words = shell_words(&seg);
        if words.first().map(|w| w != "cat").unwrap_or(true) {
            continue;
        }
        for w in &words[1..] {
            if w.starts_with('-') || w.contains('*') || w.contains('?') {
                continue;
            }
            let p = if Path::new(w).is_absolute() {
                PathBuf::from(w)
            } else {
                cwd.join(w)
            };
            out.push(p);
        }
    }
    out
}

/// A whole-file `cat` of large project files: each such file's text in the output is replaced
/// by its outline (the same rule and limits as a Read). Returns the new output.
fn bash_cat_outline(
    project: &Project,
    cfg: &Config,
    store: &Store,
    session: &str,
    cmd: &str,
    cwd: &Path,
    stdout: &str,
) -> Result<Option<String>> {
    let files = cat_files(cmd, cwd);
    if files.is_empty() {
        return Ok(None);
    }
    let mut out = stdout.to_string();
    let mut before = 0;
    let mut after = 0;
    for abs in files {
        if !inside_project(project, &abs) {
            continue;
        }
        let Some(text) = util::read_text(&abs) else {
            continue;
        };
        let lines = text.lines().count();
        let toks = util::tokens(&text);
        if lines <= cfg.hooks.large_file_lines && toks <= cfg.hooks.large_file_tokens {
            continue;
        }
        let Some(at) = out.find(text.as_str()) else {
            continue;
        };
        let rel = project.rel(&abs);
        let skel = crate::slice::skeleton(store, project, &rel)?;
        let (skel, _) = util::clip_tokens(&skel, 2500);
        let note = format!(
            "[local-brain] {} is large ({} lines, ~{} tokens); its outline is below. Read the part \
             you need with `sed -n 'START,ENDp' {}` (or brain_slice); run the identical command \
             again for the full text.\n{}\n",
            rel,
            lines,
            util::human_tokens(toks),
            rel,
            skel.trim_end()
        );
        before += toks;
        after += util::tokens(&note);
        out.replace_range(at..at + text.len(), &note);
    }
    if after == 0 || after >= before {
        return Ok(None);
    }
    store.record_metric("hook", "bash_cat", before, after, Some(session))?;
    Ok(Some(out))
}

/// `grep -rn …`, `rg …`, `git grep …` as the only command (pipes into another grep are fine).
fn is_search_command(cmd: &str) -> bool {
    let Some(segs) = simple_commands(cmd) else {
        return false;
    };
    if segs.len() != 1 {
        return false;
    }
    let first = before_pipe(&segs[0]).trim().to_string();
    let words = shell_words(&first);
    match words.first().map(String::as_str) {
        Some("rg") => true,
        Some("git") => words.get(1).map(|w| w == "grep").unwrap_or(false),
        Some("grep" | "egrep" | "fgrep") => {
            words.iter().any(|w| {
                w.starts_with('-') && !w.starts_with("--") && (w.contains('r') || w.contains('R'))
            }) || words.iter().any(|w| w == "--recursive")
        }
        _ => false,
    }
}

/// The first command of a pipeline: everything before the first `|` outside quotes.
fn before_pipe(cmd: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in cmd.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, c) {
            (_, '\\') if quote != Some('\'') => escaped = true,
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None, '|') => return &cmd[..i],
            _ => {}
        }
    }
    cmd
}

/// The pattern of a search command: the `-e` value, or the first word that is not an option.
/// grep's basic-regex `\|` alternation is turned into `|`.
fn search_pattern(cmd: &str) -> Option<String> {
    let first = before_pipe(cmd);
    let words = shell_words(first);
    let skip = if words.first().map(|w| w == "git").unwrap_or(false) {
        2
    } else {
        1
    };
    let mut it = words.iter().skip(skip);
    while let Some(w) = it.next() {
        if w == "-e" || w == "--regexp" || w == "--" {
            return it.next().map(|p| p.replace("\\|", "|"));
        }
        if w.starts_with('-') {
            // Options that take a value.
            if matches!(
                w.as_str(),
                "-A" | "-B"
                    | "-C"
                    | "-m"
                    | "-g"
                    | "-t"
                    | "--include"
                    | "--exclude"
                    | "--glob"
                    | "--type"
            ) {
                it.next();
            }
            continue;
        }
        return Some(w.replace("\\|", "|"));
    }
    None
}

/// An oversized recursive search: ranked with the code graph (definitions first, tests and
/// generated files last) and trimmed like a Grep call; the full output goes to brain_raw.
fn bash_search_trim(
    project: &Project,
    cfg: &Config,
    store: &Store,
    session: &str,
    cmd: &str,
    cwd: &Path,
    stdout: &str,
) -> Result<Option<String>> {
    if !is_search_command(cmd) {
        return Ok(None);
    }
    use std::sync::OnceLock;
    static LINE: OnceLock<Regex> = OnceLock::new();
    let line_re = LINE.get_or_init(|| Regex::new(r"^([^:\n]+?)[:-](\d+)[:-](.*)$").unwrap());
    let all: Vec<&str> = stdout.lines().collect();
    if all.len() <= cfg.hooks.grep_max_results {
        return Ok(None);
    }
    let mut hits: Vec<Hit> = Vec::new();
    let mut other = 0;
    for l in &all {
        let Some(c) = line_re.captures(l) else {
            other += 1;
            continue;
        };
        let path = c[1].to_string();
        let n: usize = c[2].parse().unwrap_or(0);
        let abs = if Path::new(&path).is_absolute() {
            PathBuf::from(&path)
        } else {
            cwd.join(&path)
        };
        let rel = project.rel(&abs);
        match hits.iter_mut().find(|h| h.rel == rel) {
            Some(h) => {
                h.lines.push((n, c[3].to_string()));
                h.count += 1;
            }
            None => hits.push(Hit {
                rel,
                lines: vec![(n, c[3].to_string())],
                count: 1,
            }),
        }
    }
    // Only output that is mostly `path:line:text` is a search result we can rank.
    if hits.is_empty() || other * 5 > all.len() {
        return Ok(None);
    }
    let pattern =
        search_pattern(cmd).and_then(|p| RegexBuilder::new(&p).case_insensitive(true).build().ok());
    rank_hits(store, pattern.as_ref(), &mut hits);
    let raw_id = crate::raw::save(
        Some(project),
        &crate::secrets::mask(stdout),
        cfg.compress.raw_ttl_days,
    )?;
    let mut shown = Vec::new();
    'outer: for h in &hits {
        for (n, l) in h.lines.iter().take(4) {
            if shown.len() >= cfg.hooks.grep_show {
                break 'outer;
            }
            shown.push(format!("{}:{}:{}", h.rel, n, util::clip(l.trim_end(), 220)));
        }
    }
    let out = format!(
        "[local-brain] {} matching lines in {} files (~{} tokens). The most relevant are below \
         (ranked with the code graph: definitions first, tests and generated files last). Full \
         output: brain_raw(raw_id=\"{}\", grep=…), or run the identical command again.\n{}\n",
        all.len() - other,
        hits.len(),
        util::human_tokens(util::tokens(stdout)),
        raw_id,
        crate::secrets::mask(&shown.join("\n"))
    );
    store.record_metric(
        "hook",
        "bash_grep",
        util::tokens(stdout),
        util::tokens(&out),
        Some(session),
    )?;
    Ok(Some(out))
}

fn post_bash(
    project: &Project,
    cfg: &Config,
    store: &Store,
    session: &str,
    input: &Value,
    v: &Value,
) -> Result<Option<Value>> {
    let resp = &v["tool_response"];
    let cmd = input["command"].as_str().unwrap_or("");
    // Reads and searches through Bash, like Read and Grep.
    if let Some(stdout) = resp["stdout"].as_str() {
        let cwd = v["cwd"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| project.root.clone());
        let key = format!("bash:{}", util::sha256_hex(cmd.as_bytes()));
        if !escape(store, session, &key)? {
            let mut replaced = None;
            if cfg.hooks.read_large {
                replaced = bash_cat_outline(project, cfg, store, session, cmd, &cwd, stdout)?;
            }
            if replaced.is_none() && cfg.hooks.grep {
                replaced = bash_search_trim(project, cfg, store, session, cmd, &cwd, stdout)?;
            }
            if let Some(new_out) = replaced {
                store.record_denial(session, &key)?;
                let mut new = resp.as_object().cloned().unwrap_or_default();
                new.insert("stdout".into(), Value::String(new_out));
                return Ok(Some(json!({"hookSpecificOutput": {
                    "hookEventName": "PostToolUse", "updatedToolOutput": Value::Object(new)
                }})));
            }
        } else {
            return Ok(None);
        }
    }
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

/// Tags Claude Code wraps around prompts it generates itself (background-task and monitor
/// notifications, local command output, slash-command echoes). They reach UserPromptSubmit like
/// a typed prompt but nobody asked for code context.
const MACHINE_PROMPT_TAGS: &[&str] = &[
    "<task-notification>",
    "<system-reminder>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
    "<command-name>",
    "<command-message>",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
];

/// Prompts that are not a task: confirmations, slash commands, very short replies, and
/// prompts Claude Code generated itself.
fn is_task_prompt(prompt: &str) -> bool {
    let p = prompt.trim();
    if p.starts_with('/') || p.starts_with('!') || p.starts_with('#') {
        return false;
    }
    if MACHINE_PROMPT_TAGS.iter().any(|t| p.starts_with(t)) {
        return false;
    }
    let words = p.split_whitespace().count();
    if words < 3 || p.chars().count() < 15 {
        return false;
    }
    let lower = p.to_lowercase();
    let filler = [
        "yes",
        "no",
        "ok",
        "okay",
        "continue",
        "go ahead",
        "proceed",
        "thanks",
        "thank you",
        "evet",
        "hayır",
        "tamam",
        "devam",
        "teşekkürler",
    ];
    !(words <= 4 && filler.iter().any(|f| lower.starts_with(f)))
}

/// Generic task words that say nothing about *where* in the code to look.
const TASK_WORDS: &[&str] = &[
    "need",
    "needs",
    "improve",
    "analyze",
    "analyse",
    "make",
    "look",
    "looks",
    "looking",
    "page",
    "pages",
    "file",
    "files",
    "code",
    "component",
    "components",
    "their",
    "them",
    "this",
    "that",
    "with",
    "from",
    "should",
    "please",
    "want",
    "into",
    "also",
    "like",
    "update",
    "change",
    "better",
    "work",
    "working",
    "using",
    "add",
    "create",
    "implement",
    "feature",
    "screen",
    "view",
    "fix",
    "bug",
    "issue",
    "make",
    "some",
    "more",
    "less",
    "every",
    "each",
    "there",
    "where",
    "what",
    "when",
    "which",
    "while",
    "have",
    "does",
    "about",
    "then",
    "than",
];

/// Words of a prompt that may name a place in the code ("admin", "checkout", "auth").
fn place_terms(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in prompt
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
    {
        if w.chars().count() >= 4 && !TASK_WORDS.contains(&w.as_str()) && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// A directory name matches a term when it contains it, or when the term is the name plus a
/// suffix (Turkish "panelinin" → "panel").
fn segment_matches(seg: &str, term: &str) -> bool {
    let seg = seg.to_lowercase();
    let seg = seg.trim_matches(|c: char| !c.is_alphanumeric());
    seg.chars().count() >= 4 && (seg.contains(term) || term.starts_with(seg))
}

fn is_test_path(p: &str) -> bool {
    let l = p.to_lowercase();
    l.contains("test") || l.contains("spec") || l.contains("e2e") || l.contains("__mocks__")
}

struct Area {
    dir: String,
    files: usize,
    children: Vec<String>,
}

/// Directories whose name matches a prompt term, with how much code they hold.
fn matching_areas(store: &Store, terms: &[String]) -> Result<Vec<Area>> {
    let files = store.files()?;
    let code: Vec<&str> = files
        .iter()
        .filter(|f| f.kind == "code" && !is_test_path(&f.path))
        .map(|f| f.path.as_str())
        .collect();
    let mut count: std::collections::BTreeMap<String, usize> = Default::default();
    for path in &code {
        let parts: Vec<&str> = path.split('/').collect();
        for depth in 1..parts.len() {
            let seg = parts[depth - 1];
            if terms.iter().any(|t| segment_matches(seg, t)) {
                *count.entry(parts[..depth].join("/")).or_default() += 1;
            }
        }
    }
    // Keep the outermost matching directory of each nest.
    let dirs: Vec<String> = count.keys().cloned().collect();
    let mut areas: Vec<Area> = dirs
        .iter()
        .filter(|d| {
            !dirs
                .iter()
                .any(|o| o != *d && d.starts_with(&format!("{}/", o)))
        })
        .map(|d| {
            let prefix = format!("{}/", d);
            let mut children: Vec<String> = Vec::new();
            for path in &code {
                if let Some(rest) = path.strip_prefix(&prefix) {
                    let child = match rest.split_once('/') {
                        Some((dir, _)) => format!("{}/", dir),
                        None => rest.to_string(),
                    };
                    if !children.contains(&child) {
                        children.push(child);
                    }
                }
            }
            children.sort_by_key(|c| (!c.ends_with('/'), c.clone()));
            Area {
                dir: d.clone(),
                files: count[d],
                children,
            }
        })
        .collect();
    areas.sort_by_key(|a| std::cmp::Reverse(a.files));
    areas.truncate(3);
    Ok(areas)
}

/// Code identifiers a prompt names: everything in backticks, plus bare words shaped like
/// identifiers (camelCase, snake_case, Type.member). Returns (identifier, was_backticked).
fn prompt_identifiers(prompt: &str) -> Vec<(String, bool)> {
    use std::sync::OnceLock;
    static TICK: OnceLock<Regex> = OnceLock::new();
    static WORD: OnceLock<Regex> = OnceLock::new();
    let tick = TICK.get_or_init(|| Regex::new(r"`([^`\s]{2,80})`").unwrap());
    let word = WORD.get_or_init(|| Regex::new(r"\b[A-Za-z_][A-Za-z0-9_.]{2,79}\b").unwrap());
    let shaped = |w: &str| {
        let inner_upper = w.chars().skip(1).any(|c| c.is_ascii_uppercase());
        let lower = w.chars().any(|c| c.is_ascii_lowercase());
        (w.contains('_') && w.chars().any(|c| c.is_alphanumeric()))
            || (w.contains('.')
                && !w.ends_with('.')
                && w.chars()
                    .next()
                    .map(|c| c.is_ascii_uppercase())
                    .unwrap_or(false))
            || (inner_upper && lower)
    };
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut push = |s: &str, ticked: bool| {
        let s = s
            .trim()
            .trim_end_matches("()")
            .trim_matches(|c: char| c == ',' || c == '.' || c == ';' || c == ':');
        if s.chars().count() >= 2 && !out.iter().any(|(o, _)| o == s) && out.len() < 6 {
            out.push((s.to_string(), ticked));
        }
    };
    for m in tick.captures_iter(prompt) {
        push(&m[1], true);
    }
    for m in word.find_iter(prompt) {
        if shaped(m.as_str()) {
            push(m.as_str(), false);
        }
    }
    out
}

/// A symbol the prompt names, resolved in the code graph.
struct Named {
    defs: Vec<crate::store::SymbolRow>,
    callers: Vec<crate::store::EdgeRow>,
}

fn resolve_identifiers(store: &Store, ids: &[(String, bool)]) -> Result<Vec<Named>> {
    let mut out: Vec<Named> = Vec::new();
    let ident = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    };
    for (id, ticked) in ids {
        if id.contains('/') {
            continue; // a file path, handled by prompt_paths
        }
        let mut defs = store.symbols_best(id)?;
        if defs.is_empty() && *ticked {
            // `Parser.parse` → parse, preferably inside Parser.
            if let Some((parent, member)) =
                id.rsplit_once('.').filter(|(p, m)| ident(p) && ident(m))
            {
                let all = store.symbols_best(member)?;
                let inside: Vec<_> = all
                    .iter()
                    .filter(|s| s.parent.as_deref() == Some(parent))
                    .cloned()
                    .collect();
                defs = if inside.is_empty() { all } else { inside };
            }
        }
        if defs.is_empty() {
            continue;
        }
        if out.iter().any(|n| {
            n.defs
                .first()
                .map(|d| d.name == defs[0].name)
                .unwrap_or(false)
        }) {
            continue;
        }
        defs.sort_by_key(|s| (is_test_path(&s.path), s.path.clone(), s.start_line));
        let mut callers = store.edges_to(&defs[0].name, "call")?;
        callers.sort_by_key(|e| (is_test_path(&e.path), e.path.clone(), e.line));
        out.push(Named { defs, callers });
        if out.len() >= 4 {
            break;
        }
    }
    Ok(out)
}

fn asks_for_callers(prompt: &str) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(call|calls|caller|callers|called|calling|use|uses|used|usage|usages|reference|references|referenced|import|imports|imported|invoke|invokes|invoked|depends? on|kullan\w*|çağ\w*)\b").unwrap()
    })
    .is_match(prompt)
}

fn asks_for_architecture(prompt: &str) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(architecture|architectural|overview|high.level|structure of|structured|organi[sz]ed|organi[sz]ation|main (directories|modules|parts|components)|entry points?|how (is|does) (this|the) (repo|repository|project|codebase)|mimari\w*|yapısı\w*)\b").unwrap()
    })
    .is_match(prompt)
}

fn asks_for_docs(prompt: &str) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(doc|docs|document|documentation|documented|spec|specs|specification|plan|plans|readme|guide|handoff|design|notes?|belge\w*|döküman\w*|dokümantasyon)\b").unwrap()
    })
    .is_match(prompt)
}

/// The note for symbols the prompt names: where each is defined (kind, file:lines, signature)
/// and, when the prompt asks about uses, where it is called from. Nothing else: Claude can go
/// straight to the code instead of searching for it.
fn render_symbols(project: &Project, named: &[Named], want_callers: bool, budget: i64) -> String {
    let mut out = String::from(
        "local-brain (code index) resolved the symbols named in this prompt; no search needed. \
         More code: Read with offset/limit, or brain_slice.\n",
    );
    let fits = |out: &str, line: &str| util::tokens(out) + util::tokens(line) <= budget;
    for n in named {
        for (i, d) in n.defs.iter().take(3).enumerate() {
            let sig = util::clip(d.signature.trim(), 110);
            let line = format!(
                "- `{}` — {}{} at {}:{}-{}{}\n",
                d.name,
                d.kind,
                d.parent
                    .as_deref()
                    .map(|p| format!(" in {}", p))
                    .unwrap_or_default(),
                d.path,
                d.start_line,
                d.end_line,
                if sig.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", sig)
                }
            );
            if !fits(&out, &line) {
                return out;
            }
            out.push_str(&line);
            // A short definition is quoted whole: the question is usually answered right there.
            if i == 0 && d.end_line - d.start_line < 14 {
                if let Some(src) = short_source(project, &d.path, d.start_line, d.end_line) {
                    if fits(&out, &src) {
                        out.push_str(&src);
                    }
                }
            }
        }
        if n.defs.len() > 3 {
            let line = format!(
                "  (+{} more definitions named the same)\n",
                n.defs.len() - 3
            );
            if fits(&out, &line) {
                out.push_str(&line);
            }
        }
        if n.callers.is_empty() {
            continue;
        }
        // Call sites grouped by file, code before tests.
        let mut groups: Vec<(String, Vec<String>, Option<String>)> = Vec::new();
        for e in &n.callers {
            match groups.iter_mut().find(|g| g.0 == e.path) {
                Some(g) => g.1.push(e.line.to_string()),
                None => groups.push((e.path.clone(), vec![e.line.to_string()], e.source.clone())),
            }
        }
        let max = if want_callers { 12 } else { 3 };
        let parts: Vec<String> = groups
            .iter()
            .take(max)
            .map(|(p, lines, src)| {
                format!(
                    "{}:{}{}{}",
                    p,
                    lines.join(","),
                    src.as_deref()
                        .filter(|s| !s.is_empty())
                        .map(|s| format!(" (in {})", s))
                        .unwrap_or_default(),
                    if is_test_path(p) { " [test]" } else { "" }
                )
            })
            .collect();
        let line = format!(
            "  called from {} place{} in {} file{}: {}{}\n",
            n.callers.len(),
            if n.callers.len() == 1 { "" } else { "s" },
            groups.len(),
            if groups.len() == 1 { "" } else { "s" },
            parts.join("; "),
            if groups.len() > max {
                format!("; +{} more files", groups.len() - max)
            } else {
                String::new()
            }
        );
        if fits(&out, &line) {
            out.push_str(&line);
        }
    }
    out
}

/// Lines `start..=end` of a project file, indented as a quote, when they are small enough to be
/// worth more than a pointer (≤ 900 characters, secrets masked).
fn short_source(project: &Project, rel: &str, start: i64, end: i64) -> Option<String> {
    let text = util::read_text(&project.root.join(rel))?;
    let lines: Vec<&str> = text
        .lines()
        .skip((start.max(1) - 1) as usize)
        .take((end - start + 1).max(1) as usize)
        .collect();
    if lines.is_empty() {
        return None;
    }
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let body: Vec<String> = lines
        .iter()
        .map(|l| {
            format!(
                "    {}",
                util::clip(l.get(indent..).unwrap_or("").trim_end(), 160)
            )
        })
        .collect();
    let src = body.join("\n");
    if src.len() > 900 {
        return None;
    }
    Some(format!("{}\n", crate::secrets::mask(&src)))
}

/// The note for "how is this repository organized" prompts: the architecture map the index
/// already has (languages, entry points, modules), so Claude does not have to explore the tree.
fn render_architecture(store: &Store, budget: i64) -> Result<Option<String>> {
    let Some((arch, ..)) = store.summary("architecture", ".")? else {
        return Ok(None);
    };
    let mut out = String::from("local-brain (local code index): repository map.\n");
    if let Some((s, ..)) = store.summary("project", ".")? {
        let first = crate::memory::first_sentences(&s, 240);
        if !first.is_empty() {
            out.push_str(&format!("Overview: {}\n", first));
        }
    }
    // The map is the whole answer here, so it may use half again the usual budget.
    let budget = budget * 3 / 2;
    let fits = |out: &str, line: &str| util::tokens(out) + util::tokens(line) <= budget;
    for l in arch.lines().filter(|l| !l.starts_with("Most referenced")) {
        let line = if let Some(rest) = l.strip_prefix("  ") {
            // "  apps/ (631 · 0 · 0)" → the module with what it is for.
            let dir = rest.split(' ').next().unwrap_or("").trim_end_matches('/');
            let files = rest
                .split('(')
                .nth(1)
                .and_then(|x| x.split([' ', ')']).next())
                .unwrap_or("");
            let what = store
                .summary("module", dir)?
                .map(|(t, ..)| crate::memory::first_sentences(&t, 150))
                .unwrap_or_default();
            if what.is_empty() && dir.contains('.') {
                continue; // a top-level file, not a module
            }
            format!("  {}/ ({} files) — {}\n", dir, files, what)
        } else if l.starts_with("Modules") {
            "Modules:\n".to_string()
        } else {
            format!("{}\n", l.trim_end())
        };
        if !fits(&out, &line) {
            break;
        }
        out.push_str(&line);
    }
    Ok(Some(out))
}

/// Files a prompt names (`docs/brand/preview.png`, `src/main.rs`), as paths the index knows.
/// Absolute paths are tried with their leading directories stripped.
fn prompt_paths(store: &Store, prompt: &str) -> Result<Vec<String>> {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?:^|[\s`'"(])((?:\./)?[A-Za-z0-9_@.\-\[\]~]+(?:/[A-Za-z0-9_@.\-\[\]]+)+\.[A-Za-z0-9]{1,8})"#).unwrap()
    });
    let mut out: Vec<String> = Vec::new();
    for c in re.captures_iter(prompt) {
        let raw = c[1].trim_start_matches("./").to_string();
        let mut candidates = vec![raw.clone()];
        let mut rest = raw.as_str();
        while let Some((_, tail)) = rest.split_once('/') {
            candidates.push(tail.to_string());
            rest = tail;
        }
        for cand in candidates {
            let known = store.file(&cand)?.is_some() || store.doc(&cand)?.is_some();
            if known {
                if !out.contains(&cand) {
                    out.push(cand);
                }
                break;
            }
        }
        if out.len() >= 4 {
            break;
        }
    }
    Ok(out)
}

/// Append what the index knows about each named file: a document's local description (so the
/// image or PDF need not be sent at all), or a code file's summary and key symbols.
fn render_paths(
    store: &Store,
    session: &str,
    paths: &[String],
    out: &mut String,
    budget: i64,
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    if out.is_empty() {
        out.push_str("local-brain (local code index): the files named in this prompt.\n");
    }
    for p in paths {
        let room = ((budget - util::tokens(out)) * 4).max(200) as usize;
        if let Some((_, text, model)) = store.doc(p)? {
            let kind = if p.to_lowercase().ends_with(".pdf") {
                "PDF"
            } else {
                "image"
            };
            // A Read of this file in this session then returns the original, not this again.
            store.record_denial(session, &format!("noted:{}", p))?;
            // Images get the gist only: Claude's own look at an image costs ~50–1,600 tokens
            // and is more reliable than a small model's transcription, so a long description
            // here would be paid twice. PDFs (text plus ~1,500 tokens a page) get the room.
            let max = if kind == "PDF" { room.min(1600) } else { 300 };
            out.push_str(&format!(
                "- {} ({}, described locally by {}; Read the file to see it): {}\n",
                p,
                kind,
                model,
                util::clip(&crate::memory::first_sentences(&text, max), max)
            ));
            continue;
        }
        if let Some(f) = store.file(p)? {
            let summary = store
                .summary("file", p)?
                .map(|(t, ..)| crate::memory::first_sentences(&t, 160))
                .unwrap_or_default();
            let syms: Vec<String> = store
                .symbols_in_file(f.id)?
                .iter()
                .filter(|s| s.parent.is_none())
                .take(4)
                .map(|s| format!("`{}` ({}-{})", s.name, s.start_line, s.end_line))
                .collect();
            let line = format!(
                "- {} ({} lines) — {}{}\n",
                p,
                f.lines,
                summary,
                if syms.is_empty() {
                    String::new()
                } else {
                    format!(" Key: {}.", syms.join(", "))
                }
            );
            if util::tokens(out) + util::tokens(&line) <= budget {
                out.push_str(&line);
            }
        }
    }
    Ok(())
}

/// UserPromptSubmit: tell Claude where the code for this request lives, within
/// `prompt_context_tokens`. Symbols named in the prompt are resolved exactly in the code graph
/// (definition and call sites); architecture questions get the repository map; other task
/// prompts get the matching areas of the tree and the most relevant code files.
fn prompt_context(
    project: &Project,
    cfg: &Config,
    store: &Store,
    session: &str,
    prompt: &str,
) -> Result<Option<Value>> {
    if !is_task_prompt(prompt) || store.meta("last_learn")?.is_none() {
        return Ok(None);
    }
    let budget = cfg.hooks.prompt_context_tokens.max(150);
    let done = |out: String| -> Result<Option<Value>> {
        store.record_metric(
            "hook",
            "prompt_context",
            0,
            util::tokens(&out),
            Some(session),
        )?;
        Ok(Some(json!({
            "hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": out}
        })))
    };
    let named = resolve_identifiers(store, &prompt_identifiers(prompt))?;
    let paths = prompt_paths(store, prompt)?;
    if !named.is_empty() || !paths.is_empty() {
        let mut out = if named.is_empty() {
            String::new()
        } else {
            render_symbols(project, &named, asks_for_callers(prompt), budget)
        };
        render_paths(store, session, &paths, &mut out, budget)?;
        return done(out);
    }
    if asks_for_architecture(prompt) {
        if let Some(out) = render_architecture(store, budget)? {
            return done(out);
        }
    }
    let mut c = cfg.clone();
    // Embedding the prompt must not hold the user up; lexical search still works without it.
    c.runtime.timeout_secs = c.runtime.timeout_secs.min(8);
    let q = util::clip(prompt, 2000);
    let hits = crate::query::search(store, &c, &q, None, &[], 40)?;
    let terms = place_terms(&q);
    let lower_prompt = q.to_lowercase();
    // Areas the prompt already names as paths ("in apps/ and services/") tell Claude nothing.
    let areas: Vec<Area> = matching_areas(store, &terms)?
        .into_iter()
        .filter(|a| {
            let last = a.dir.rsplit('/').next().unwrap_or(&a.dir).to_lowercase();
            !lower_prompt.contains(&format!("{}/", last))
        })
        .collect();
    // A file is worth naming only when it shares a specific word with the prompt; lexical
    // search otherwise surfaces whatever shares common vocabulary ("page", "service").
    let strong: Vec<&String> = terms
        .iter()
        .filter(|t| t.chars().count() >= 5 && !lower_prompt.contains(&format!("{}/", t)))
        .collect();
    let relevant = |path: &str, summary: &str| -> bool {
        let hay = format!("{} {}", path, summary).to_lowercase();
        hay.split(|c: char| !c.is_alphanumeric())
            .any(|w| strong.iter().any(|t| w == t.as_str()))
    };
    if hits.is_empty() && areas.is_empty() {
        return Ok(None);
    }

    // Files ranked by their chunks' scores, with up to two key symbols each.
    let mut order: Vec<String> = Vec::new();
    let mut score: std::collections::HashMap<String, f32> = Default::default();
    let mut syms: std::collections::HashMap<String, Vec<String>> = Default::default();
    for (ch, s) in &hits {
        if !order.contains(&ch.path) {
            order.push(ch.path.clone());
        }
        *score.entry(ch.path.clone()).or_default() += s;
        let e = syms.entry(ch.path.clone()).or_default();
        if e.len() < 2 {
            let label = match &ch.symbol {
                Some(sym) => format!("`{}` ({}-{})", sym, ch.start_line, ch.end_line),
                None => format!("lines {}-{}", ch.start_line, ch.end_line),
            };
            if !e.contains(&label) {
                e.push(label);
            }
        }
    }
    order.sort_by(|a, b| {
        score[b]
            .partial_cmp(&score[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let kind_of: std::collections::HashMap<String, String> = store
        .files()?
        .into_iter()
        .map(|f| (f.path, f.kind))
        .collect();
    let in_area = |p: &str| areas.iter().any(|a| p.starts_with(&format!("{}/", a.dir)));
    let code: Vec<&String> = order
        .iter()
        .filter(|p| kind_of.get(*p).map(|k| k == "code").unwrap_or(false))
        .filter(|p| !is_test_path(p))
        .collect();
    let docs: Vec<&String> = order
        .iter()
        .filter(|p| kind_of.get(*p).map(|k| k != "code").unwrap_or(false))
        .collect();

    let mut out = String::from(
        "local-brain (local code index): where the code for this request lives. Start here; \
         `brain_query` finds more, `brain_slice` reads one symbol.\n",
    );
    let fits = |out: &str, line: &str| util::tokens(out) + util::tokens(line) <= budget;
    let mut shown = 0;
    let file_line = |path: &str| -> Result<String> {
        let summary = store
            .summary("file", path)?
            .map(|(t, ..)| util::clip(&crate::memory::first_sentences(&t, 140), 140))
            .unwrap_or_default();
        Ok(format!(
            "- {} — {}{}\n",
            path,
            summary,
            syms.get(path)
                .filter(|v| !v.is_empty())
                .map(|v| format!(" Key: {}.", v.join(", ")))
                .unwrap_or_default()
        ))
    };

    for a in &areas {
        let summary = store
            .summary("module", &a.dir)?
            .map(|(t, ..)| t.lines().next().unwrap_or("").to_string())
            .map(|t| util::clip(&t, 160))
            .unwrap_or_default();
        let mut kids = a.children.clone();
        let more = kids.len().saturating_sub(16);
        kids.truncate(16);
        let line = format!(
            "- area {}/ ({} code files) — {} Contains: {}{}\n",
            a.dir,
            a.files,
            summary,
            kids.join(", "),
            if more > 0 {
                format!(", +{} more", more)
            } else {
                String::new()
            }
        );
        if fits(&out, &line) {
            out.push_str(&line);
            shown += 1;
        }
    }
    let best = code.first().map(|p| score[*p]).unwrap_or(0.0);
    for (i, path) in code.iter().filter(|p| !in_area(p)).take(3).enumerate() {
        if i >= 1 && score[*path] < best * 0.35 {
            break;
        }
        let line = file_line(path)?;
        if !relevant(path, &line) {
            continue;
        }
        if !fits(&out, &line) {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    // Inside a matching area, the best few files are still worth naming.
    for path in code.iter().filter(|p| in_area(p)).take(3) {
        let line = file_line(path)?;
        if !relevant(path, &line) {
            continue;
        }
        if !fits(&out, &line) {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    // Documents only when the prompt is about them: plans and specs are long and rarely the
    // place to start a code task.
    if asks_for_docs(prompt) {
        for path in docs.iter().take(1) {
            let line = file_line(path)?.replacen("- ", "- doc ", 1);
            if !fits(&out, &line) {
                break;
            }
            out.push_str(&line);
            shown += 1;
        }
    }
    if shown == 0 {
        return Ok(None);
    }
    done(out)
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
            if let Some(p) = path.as_str().filter(|p| Path::new(p).is_file()) {
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
    fn task_prompts_are_recognised() {
        assert!(is_task_prompt(
            "We need to improve the admin panel looking, analyze the pages"
        ));
        assert!(is_task_prompt(
            "admin panelinin görünümünü iyileştirelim lütfen"
        ));
        assert!(!is_task_prompt("yes"));
        assert!(!is_task_prompt("ok go ahead"));
        assert!(!is_task_prompt("evet devam et"));
        assert!(!is_task_prompt("/mcp"));
        assert!(!is_task_prompt("fix it"));
        assert!(!is_task_prompt(
            "<task-notification>\n<task-id>bqw4cyox5</task-id>\n<summary>Monitor event: \"model processes + memory pressure\"</summary>"
        ));
        assert!(!is_task_prompt(
            "<local-command-stdout>Compacted conversation summary of the session</local-command-stdout>"
        ));
        // Pasted markup from the user is still a task.
        assert!(is_task_prompt(
            "<div class=\"card\"> this card overflows on mobile, fix the admin layout"
        ));
    }

    #[test]
    fn prompt_identifiers_and_intents() {
        let ids = prompt_identifiers(
            "Where is `createSafeFetch` defined, and who calls listMyRsvps or `Parser.parse()`? The admin page is slow.",
        );
        let names: Vec<&str> = ids.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["createSafeFetch", "Parser.parse", "listMyRsvps"]
        );
        assert!(
            prompt_identifiers("Summarize `docs/brand/preview.png`")
                .iter()
                .all(|(n, _)| n.contains('/')),
            "paths are left to prompt_paths"
        );
        assert!(ids[0].1 && !ids[2].1, "backticks are remembered");
        assert!(prompt_identifiers("improve the admin panel pages").is_empty());
        assert!(asks_for_callers("Which files call `listMyRsvps`?"));
        assert!(!asks_for_callers("Where is `rateHit` defined?"));
        assert!(asks_for_architecture(
            "Describe the architecture of this repository: main directories and entry points"
        ));
        assert!(!asks_for_architecture("Where is `rateHit` defined?"));
        assert!(asks_for_docs("Which spec covers the RSVP flow?"));
        assert!(!asks_for_docs("fix the rateHit rate limiter"));
    }

    #[test]
    fn place_terms_and_segments() {
        let t = place_terms("We need to improve the admin panel looking, analyze the pages");
        assert_eq!(t, vec!["admin", "panel"]);
        assert!(segment_matches("admin", "admin"));
        assert!(segment_matches("[locale]", "locale"));
        assert!(segment_matches("panel", "panelinin"));
        assert!(!segment_matches("app", "application"));
        assert!(is_test_path("apps/web/e2e/admin.spec.ts"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_project_are_outside() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("proj");
        let outside = d.path().join("secret");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("a.png"), b"x").unwrap();
        std::fs::write(outside.join("passport.pdf"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let p = Project::new(root.clone());
        assert!(inside_project(&p, &root.join("a.png")));
        assert!(!inside_project(&p, &root.join("link/passport.pdf")));
        assert!(!inside_project(&p, &root.join("../secret/passport.pdf")));
        assert!(!inside_project(&p, &outside.join("passport.pdf")));
    }

    #[test]
    fn bash_reads_are_recognised() {
        assert_eq!(
            shell_words(r#"cat 'a b.ts' app/\[locale\]/page.tsx "x y""#),
            vec!["cat", "a b.ts", "app/[locale]/page.tsx", "x y"]
        );
        let cwd = Path::new("/p");
        assert_eq!(
            cat_files("cat src/a.ts src/b.ts; grep -n x src/c.ts", cwd),
            vec![PathBuf::from("/p/src/a.ts"), PathBuf::from("/p/src/b.ts")]
        );
        assert!(
            cat_files("cat src/a.ts | head -50", cwd).is_empty(),
            "piped: already narrow"
        );
        assert!(cat_files("sed -n 1,80p src/a.ts", cwd).is_empty());
        assert!(cat_files("for f in *.ts; do cat $f; done", cwd).is_empty());
        assert!(is_search_command("grep -rn \"listMyRsvps\" apps packages"));
        assert!(is_search_command("rg -n createSafeFetch | grep -v test"));
        assert!(is_search_command("git grep -n rateHit"));
        assert!(
            !is_search_command("grep -n x src/a.ts"),
            "one file is a narrow read"
        );
        assert!(
            !is_search_command("ls; grep -rn x ."),
            "mixed output is left alone"
        );
        assert_eq!(
            search_pattern("grep -rn -A 3 'admin\\|yonetim' docs").as_deref(),
            Some("admin|yonetim")
        );
        assert_eq!(search_pattern("rg -e foo src").as_deref(), Some("foo"));
        assert_eq!(
            search_pattern("git grep -n rateHit").as_deref(),
            Some("rateHit")
        );
    }

    #[test]
    fn large_cat_becomes_an_outline_and_repeats_get_the_original() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let big: String = (0..900).map(|i| format!("const v{i} = {i};\n")).collect();
        std::fs::write(root.join("src/big.ts"), &big).unwrap();
        std::fs::write(root.join("src/small.ts"), "export const a = 1;\n").unwrap();
        let project = Project::new(root.clone());
        let store = Store::open(&project).unwrap();
        let cfg = Config::default();
        let stdout = format!("{}export const a = 1;\n", big);
        let v = json!({"cwd": root, "tool_response": {"stdout": stdout, "stderr": ""}});
        let input = json!({"command": "cat src/big.ts src/small.ts"});
        let out = post_bash(&project, &cfg, &store, "s", &input, &v)
            .unwrap()
            .unwrap();
        let text = out["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .unwrap();
        assert!(text.contains("src/big.ts is large (900 lines"), "{text}");
        assert!(
            text.contains("export const a = 1;"),
            "small files stay whole"
        );
        assert!(text.len() < stdout.len() / 4);
        // The identical command again returns the original.
        assert!(post_bash(&project, &cfg, &store, "s", &input, &v)
            .unwrap()
            .is_none());
    }

    #[test]
    fn bash_factors() {
        assert_eq!(bash_factor("cargo test --all"), 1);
        assert_eq!(bash_factor("cat src/main.rs"), 10);
        assert_eq!(bash_factor("ls -la"), 2);
        assert!(bash_factor("local-brain run -- make") > 1000);
    }
}
