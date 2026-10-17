//! The `learn` pipeline: scan → AST graph → embeddings → summaries → documents → validation.
//! Every stage is keyed on content hashes, so an interrupted run resumes where it stopped and
//! re-running after edits only reprocesses what changed.

use crate::config::Config;
use crate::embed::Embedder;
use crate::lang::{self, Lang};
use crate::llm::Llm;
use crate::paths::Project;
use crate::scan::{self, Entry, Kind};
use crate::store::Store;
use crate::util;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

/// The step shown as `[n/total]` by progress bars of the current `learn` run.
static STEP: std::sync::Mutex<Option<(usize, usize)>> = std::sync::Mutex::new(None);

fn set_step(n: usize, total: usize) {
    if let Ok(mut s) = STEP.lock() {
        *s = Some((n, total));
    }
}

/// `1m 05s`, `42s`, `1h 03m`.
pub fn fmt_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{}s", secs),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// Seconds left at the average rate so far; `None` until there is enough to go on.
fn eta_secs(done: usize, total: usize, elapsed: f64) -> Option<u64> {
    if done == 0 || done >= total || (done < 3 && elapsed < 3.0) {
        return None;
    }
    let secs = (elapsed / done as f64 * (total - done) as f64).round() as u64;
    (secs > 0).then_some(secs)
}

pub struct Progress {
    label: String,
    total: usize,
    done: usize,
    enabled: bool,
    step: Option<(usize, usize)>,
    started: std::time::Instant,
    last_draw: Option<std::time::Instant>,
}

impl Progress {
    /// A progress line on stderr. Animated only on a terminal; otherwise one line at the end.
    pub fn new(label: &str, total: usize, enabled: bool) -> Progress {
        let mut p = Progress {
            label: label.to_string(),
            total,
            done: 0,
            enabled,
            step: STEP.lock().ok().and_then(|s| *s),
            started: std::time::Instant::now(),
            last_draw: None,
        };
        if total == 0 && enabled && p.step.is_some() {
            // Keep the step sequence readable when a step has nothing to do.
            let step = p
                .step
                .map(|(n, t)| format!("[{}/{}] ", n, t))
                .unwrap_or_default();
            eprintln!("  {}{:<11} up to date", step, p.label);
        }
        p.draw(false);
        p
    }
    pub fn inc(&mut self) {
        self.done += 1;
        let due = self
            .last_draw
            .map(|t| t.elapsed().as_millis() >= 200)
            .unwrap_or(true);
        if due || self.done == self.total {
            self.draw(false);
        }
    }
    fn line(&self, finished: bool) -> String {
        let width = 24;
        let filled = (self.done * width) / self.total.max(1);
        let step = self
            .step
            .map(|(n, t)| format!("[{}/{}] ", n, t))
            .unwrap_or_default();
        let elapsed = self.started.elapsed().as_secs_f64();
        let tail = if finished {
            format!("done in {}", fmt_duration(elapsed.round() as u64))
        } else {
            let rate = if elapsed >= 1.0 && self.done > 0 {
                let r = self.done as f64 / elapsed;
                if r >= 1.0 {
                    format!("{:.0}/s", r)
                } else {
                    format!("{:.1}s each", 1.0 / r)
                }
            } else {
                String::new()
            };
            match eta_secs(self.done, self.total, elapsed) {
                Some(e) => format!("{} · ETA {}", rate, fmt_duration(e)),
                None => rate,
            }
        };
        format!(
            "  {}{:<11} [{}{}] {}/{}  {}",
            step,
            self.label,
            "█".repeat(filled),
            "░".repeat(width - filled),
            self.done,
            self.total,
            tail
        )
    }
    fn tty() -> bool {
        use std::io::IsTerminal;
        std::io::stderr().is_terminal()
    }
    fn draw(&mut self, finished: bool) {
        if !self.enabled || self.total == 0 || !Self::tty() {
            return;
        }
        self.last_draw = Some(std::time::Instant::now());
        // \x1b[K clears what a longer previous line left behind.
        eprint!("\r{}\x1b[K", self.line(finished));
        let _ = std::io::stderr().flush();
    }
    pub fn finish(&mut self) {
        if !self.enabled || self.total == 0 {
            return;
        }
        if Self::tty() {
            self.draw(true);
            eprintln!();
        } else {
            eprintln!("{}", self.line(true));
        }
    }
}

#[derive(Default, Debug)]
pub struct Report {
    pub scanned: usize,
    pub indexed: usize,
    pub removed: usize,
    pub symbols: i64,
    pub edges: i64,
    pub chunks: i64,
    pub embedded: usize,
    pub embed_model: String,
    pub summaries: usize,
    pub llm_summaries: usize,
    /// Model summaries left for the background (time budget reached).
    pub summaries_pending: usize,
    pub flagged: usize,
    pub docs: usize,
    pub archived_memories: usize,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Default)]
pub struct Options {
    /// Skip all LLM work (summaries/docs use deterministic fallbacks).
    pub no_llm: bool,
    /// Only the AST + lexical index (what the watcher does synchronously).
    pub ast_only: bool,
    pub quiet: bool,
    /// Time allowed for model summaries; `None` = no limit. The rest is left for later passes.
    pub summary_budget: Option<std::time::Duration>,
}

/// Index one file's structure and chunks (stage 1). Returns true if anything changed.
pub fn index_file(store: &Store, _project: &Project, e: &Entry) -> Result<bool> {
    // Cheap pre-check: same size and mtime as indexed means unchanged (no read, no hash).
    if let Some((size, mtime, stage)) = store.file_stat(&e.rel)? {
        if size == e.size as i64 && mtime == e.mtime && stage >= 1 {
            return Ok(false);
        }
    }
    let bytes = std::fs::read(&e.abs)?;
    let hash = util::sha256_hex(&bytes);
    if let Some(f) = store.file(&e.rel)? {
        if f.hash == hash && f.stage >= 1 {
            // Touched but unchanged: remember the new mtime so it is not re-hashed or
            // reported as stale again.
            store.touch_file(f.id, e.mtime)?;
            return Ok(false);
        }
    }
    let text = if e.kind == Kind::Doc {
        None
    } else {
        match String::from_utf8(bytes.clone()) {
            Ok(t) => Some(t),
            Err(_) => return Ok(false),
        }
    };
    let lines = text.as_ref().map(|t| t.lines().count()).unwrap_or(0) as i64;
    let lang_label = lang::lang_label(&e.abs);
    let tx = store.conn.unchecked_transaction()?;
    let file_id = store.upsert_file(
        &e.rel,
        lang_label.as_deref(),
        e.kind.as_str(),
        &hash,
        e.size as i64,
        lines,
        e.mtime,
    )?;
    store.clear_file_index(file_id)?;
    if let Some(text) = &text {
        match (e.kind, Lang::from_path(&e.abs)) {
            (Kind::Code, Some(l)) => {
                let parsed = lang::parse(l, text);
                for s in &parsed.symbols {
                    store.insert_symbol(
                        file_id,
                        &s.name,
                        &s.kind,
                        s.parent.as_deref(),
                        s.start as i64,
                        s.end as i64,
                        &s.signature,
                        s.doc.as_deref(),
                    )?;
                }
                for ed in &parsed.edges {
                    store.insert_edge(
                        file_id,
                        ed.kind,
                        ed.source.as_deref(),
                        &ed.target,
                        ed.line as i64,
                    )?;
                }
                for (sym, a, b) in chunk_code(text, &parsed.symbols) {
                    let body = slice_lines(text, a, b);
                    // Skip chunks that are only braces/blank lines.
                    if body.chars().filter(|c| c.is_alphanumeric()).count() < 4 {
                        continue;
                    }
                    store.insert_chunk(
                        file_id,
                        &e.rel,
                        sym.as_deref(),
                        a as i64,
                        b as i64,
                        &body,
                    )?;
                }
            }
            _ => {
                for (title, a, b) in chunk_text(text) {
                    let body = slice_lines(text, a, b);
                    if body.trim().is_empty() {
                        continue;
                    }
                    store.insert_chunk(
                        file_id,
                        &e.rel,
                        title.as_deref(),
                        a as i64,
                        b as i64,
                        &body,
                    )?;
                }
            }
        }
    }
    store.set_stage(file_id, 1)?;
    mark_summaries_stale(store, &e.rel)?;
    tx.commit()?;
    Ok(true)
}

pub fn mark_summaries_stale(store: &Store, rel: &str) -> Result<()> {
    store.mark_stale("file", rel)?;
    let mut dir = Path::new(rel).parent();
    while let Some(d) = dir {
        let key = if d.as_os_str().is_empty() {
            ".".to_string()
        } else {
            d.to_string_lossy().replace('\\', "/")
        };
        store.mark_stale("module", &key)?;
        if key == "." {
            break;
        }
        dir = d.parent();
    }
    store.mark_stale("project", ".")?;
    store.mark_stale("architecture", ".")?;
    Ok(())
}

pub fn slice_lines(text: &str, start: usize, end: usize) -> String {
    text.lines()
        .skip(start.saturating_sub(1))
        .take(end.saturating_sub(start) + 1)
        .collect::<Vec<_>>()
        .join("\n")
}

const MAX_CHUNK: usize = 80;

/// Chunks for code: one per function/method (split when long), headers of types, and windows
/// over code not covered by any symbol.
fn chunk_code(text: &str, syms: &[lang::Sym]) -> Vec<(Option<String>, usize, usize)> {
    let total = text.lines().count();
    let mut covered = vec![false; total + 2];
    let mut out = Vec::new();
    let mut sorted: Vec<&lang::Sym> = syms.iter().collect();
    sorted.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
    for s in &sorted {
        let leaf = matches!(
            s.kind.as_str(),
            "function" | "method" | "macro" | "const" | "static" | "type"
        );
        let (a, b) = if leaf {
            (s.start, s.end)
        } else {
            // A type: its header up to the first nested symbol (bounded).
            let first_child = sorted
                .iter()
                .filter(|c| c.start > s.start && c.end <= s.end)
                .map(|c| c.start)
                .min()
                .unwrap_or(s.end + 1);
            (s.start, (first_child - 1).min(s.start + 40).min(s.end))
        };
        if a > total || covered[a] && covered[b.min(total)] {
            continue;
        }
        let name = match &s.parent {
            Some(p) => format!("{}.{}", p, s.name),
            None => s.name.clone(),
        };
        let mut start = a;
        while start <= b {
            let end = (start + MAX_CHUNK - 1).min(b);
            out.push((Some(name.clone()), start, end));
            start = end + 1;
        }
        for c in covered.iter_mut().take(b.min(total) + 1).skip(a) {
            *c = true;
        }
    }
    // Uncovered regions (imports, top-level statements).
    let mut i = 1;
    while i <= total {
        if covered[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i <= total && !covered[i] && i - start < 60 {
            i += 1;
        }
        out.push((None, start, i - 1));
    }
    out.sort_by_key(|c| c.1);
    out
}

/// Chunks for prose/config: by markdown headings, else 60-line windows.
fn chunk_text(text: &str) -> Vec<(Option<String>, usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut start = 1;
    let mut title: Option<String> = None;
    for (i, l) in lines.iter().enumerate() {
        let n = i + 1;
        let heading = l.starts_with('#') && l.trim_start_matches('#').starts_with(' ');
        if (heading && n > start) || n - start >= 60 {
            out.push((title.clone(), start, n - 1));
            start = n;
        }
        if heading {
            title = Some(l.trim_start_matches('#').trim().to_string());
        }
    }
    if start <= lines.len() {
        out.push((title, start, lines.len()));
    }
    out
}

/// Resolve import edges to project files (import → definition across files).
pub fn resolve_imports(store: &Store, project: &Project) -> Result<usize> {
    let files = store.files()?;
    let by_path: HashMap<String, i64> = files.iter().map(|f| (f.path.clone(), f.id)).collect();
    let mut by_dir: HashMap<String, Vec<(String, i64)>> = HashMap::new();
    let mut by_stem: HashMap<String, Vec<(String, i64)>> = HashMap::new();
    for f in &files {
        let p = Path::new(&f.path);
        let dir = p
            .parent()
            .map(|d| d.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        by_dir.entry(dir).or_default().push((f.path.clone(), f.id));
        if let Some(stem) = p.file_stem() {
            by_stem
                .entry(stem.to_string_lossy().to_string())
                .or_default()
                .push((f.path.clone(), f.id));
        }
    }
    let go_module = std::fs::read_to_string(project.root.join("go.mod"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("module "))
                .map(|l| l[7..].trim().to_string())
        });

    let mut st = store.conn.prepare(
        "SELECT e.id, e.target, f.path, f.lang FROM edges e JOIN files f ON f.id = e.file_id WHERE e.kind = 'import'",
    )?;
    let rows: Vec<(i64, String, String, Option<String>)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .filter_map(|r| r.ok())
        .collect();
    drop(st);
    let mut resolved = 0;
    let tx = store.conn.unchecked_transaction()?;
    for (edge_id, target, from, lang) in rows {
        let from_dir = Path::new(&from)
            .parent()
            .map(|d| d.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let try_paths = |cands: Vec<String>| -> Option<i64> {
            for c in cands {
                let c = crate::paths::normalize(Path::new(&c))
                    .to_string_lossy()
                    .replace('\\', "/");
                if let Some(id) = by_path.get(&c) {
                    return Some(*id);
                }
            }
            None
        };
        let join = |dir: &str, rest: &str| {
            if dir.is_empty() {
                rest.to_string()
            } else {
                format!("{}/{}", dir, rest)
            }
        };
        let hit = match lang.as_deref() {
            Some("javascript" | "typescript" | "tsx") => {
                let base = if target.starts_with('.') {
                    Some(join(&from_dir, &target))
                } else {
                    target
                        .strip_prefix("@/")
                        .or_else(|| target.strip_prefix("~/"))
                        .map(|rest| format!("src/{}", rest))
                };
                base.and_then(|b| {
                    let mut c = vec![b.clone()];
                    for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "d.ts"] {
                        c.push(format!("{}.{}", b, ext));
                    }
                    for ext in ["ts", "tsx", "js", "jsx"] {
                        c.push(format!("{}/index.{}", b, ext));
                    }
                    // `./x.js` written for a `x.ts` source (ESM TypeScript).
                    if let Some(stripped) = b.strip_suffix(".js") {
                        c.push(format!("{}.ts", stripped));
                        c.push(format!("{}.tsx", stripped));
                    }
                    try_paths(c)
                })
            }
            Some("python") => {
                let dots = target.chars().take_while(|c| *c == '.').count();
                let rest = target[dots..].replace('.', "/");
                let mut bases = Vec::new();
                if dots > 0 {
                    let mut d = Path::new(&from_dir).to_path_buf();
                    for _ in 1..dots {
                        d.pop();
                    }
                    bases.push(join(&d.to_string_lossy(), &rest));
                } else {
                    bases.push(rest.clone());
                    bases.push(format!("src/{}", rest));
                    // Package roots are usually an ancestor of the importing file (e.g. `svc/`
                    // for `svc/app/service.py` importing `app.models`).
                    let mut anc = Some(Path::new(&from_dir));
                    while let Some(a) = anc {
                        if !a.as_os_str().is_empty() {
                            bases.push(join(&a.to_string_lossy().replace('\\', "/"), &rest));
                        }
                        anc = a.parent();
                    }
                }
                let mut c = Vec::new();
                for b in bases {
                    let b = b.trim_end_matches('/').to_string();
                    c.push(format!("{}.py", b));
                    c.push(format!("{}/__init__.py", b));
                }
                try_paths(c)
            }
            Some("rust") => {
                let t = target
                    .split('{')
                    .next()
                    .unwrap_or(&target)
                    .trim_end_matches("::");
                let segs: Vec<&str> = t.split("::").collect();
                let (base, rest): (String, &[&str]) = match segs.first().copied() {
                    Some("crate") => {
                        let root = if from.contains("src/") {
                            from[..from.find("src/").unwrap() + 3].to_string()
                        } else {
                            "src".into()
                        };
                        (root, &segs[1..])
                    }
                    Some("super") => {
                        // `a/b.rs` is module a::b, so `super` is the directory `a/`;
                        // `a/mod.rs` is module a, so `super` is one level higher.
                        let mut d = Path::new(&from)
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_default();
                        if from.ends_with("mod.rs") {
                            d.pop();
                        }
                        (d.to_string_lossy().replace('\\', "/"), &segs[1..])
                    }
                    Some("self") => {
                        let stem = Path::new(&from)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let d = if stem == "mod" || stem == "lib" || stem == "main" {
                            from_dir.clone()
                        } else {
                            join(&from_dir, &stem)
                        };
                        (d, &segs[1..])
                    }
                    _ => (String::new(), &segs[..0]),
                };
                let mut found = None;
                if !rest.is_empty() || !base.is_empty() {
                    for n in (1..=rest.len()).rev() {
                        let p = join(&base, &rest[..n].join("/"));
                        if let Some(id) =
                            try_paths(vec![format!("{}.rs", p), format!("{}/mod.rs", p)])
                        {
                            found = Some(id);
                            break;
                        }
                    }
                }
                found
            }
            Some("go") => {
                let rel = match &go_module {
                    Some(m) if target.starts_with(m.as_str()) => {
                        target[m.len()..].trim_start_matches('/').to_string()
                    }
                    _ => target.clone(),
                };
                by_dir
                    .iter()
                    .filter(|(d, _)| {
                        !d.is_empty() && (rel == **d || rel.ends_with(&format!("/{}", d)))
                    })
                    .flat_map(|(_, v)| v.iter())
                    .find(|(p, _)| p.ends_with(".go") && !p.ends_with("_test.go"))
                    .map(|(_, id)| *id)
            }
            Some("kotlin") | Some("swift") => {
                let last = target.rsplit('.').next().unwrap_or(&target);
                let pkg_dir = target
                    .rsplit_once('.')
                    .map(|(p, _)| p.replace('.', "/"))
                    .unwrap_or_default();
                by_stem
                    .get(last)
                    .and_then(|v| {
                        v.iter()
                            .find(|(p, _)| pkg_dir.is_empty() || p.contains(&pkg_dir))
                            .or(v.first())
                    })
                    .map(|(_, id)| *id)
            }
            _ => None,
        };
        if let Some(id) = hit {
            tx.execute(
                "UPDATE edges SET resolved_file_id = ? WHERE id = ?",
                rusqlite::params![id, edge_id],
            )?;
            resolved += 1;
        }
    }
    tx.commit()?;
    Ok(resolved)
}

/// Compute embeddings for all chunks that do not have one for the active embedder.
pub fn embed_pending(
    store: &Store,
    embedder: &Embedder,
    batch: usize,
    quiet: bool,
) -> Result<usize> {
    let model = embedder.id();
    let pending = store.count(&format!(
        "SELECT count(*) FROM chunks WHERE embedding IS NULL OR embed_model IS NOT '{}'",
        model.replace('\'', "''")
    )) as usize;
    if pending == 0 {
        store.set_meta("embed_model", &model)?;
        return Ok(0);
    }
    let mut prog = Progress::new("embeddings", pending, !quiet);
    let mut done = 0;
    loop {
        let chunks = store.chunks_needing_embedding(&model, batch.max(1) as i64)?;
        if chunks.is_empty() {
            break;
        }
        let texts: Vec<String> = chunks
            .iter()
            .map(|c| {
                let head = format!(
                    "{}{}\n",
                    c.path,
                    c.symbol
                        .as_ref()
                        .map(|s| format!(" :: {}", s))
                        .unwrap_or_default()
                );
                util::clip(&format!("{}{}", head, c.text), 2000)
            })
            .collect();
        let vecs = embedder.embed(&texts)?;
        let tx = store.conn.unchecked_transaction()?;
        for (c, v) in chunks.iter().zip(vecs) {
            store.set_chunk_embedding(c.id, &v, &model)?;
            prog.inc();
            done += 1;
        }
        tx.commit()?;
    }
    prog.finish();
    store.set_meta("embed_model", &model)?;
    Ok(done)
}

/// Full (incremental) learn of a project.
pub fn learn(project: &Project, cfg: &Config, opts: Options) -> Result<Report> {
    let store = Store::open(project)?;
    let mut rep = Report::default();
    let quiet = opts.quiet;
    store.set_meta("learn_started", &util::now().to_string())?;

    // 1. Scan.
    let entries = scan::scan(project, &cfg.index.exclude, cfg.learn.max_file_bytes);
    rep.scanned = entries.len();
    let seen: HashSet<String> = entries.iter().map(|e| e.rel.clone()).collect();
    for f in store.files()? {
        if !seen.contains(&f.path) {
            store.delete_file(&f.path)?;
            mark_summaries_stale(&store, &f.path)?;
            rep.removed += 1;
        }
    }

    let steps = if opts.ast_only { 2 } else { 4 };
    // 2. AST graph + chunks (deterministic, no LLM).
    set_step(1, steps);
    let mut prog = Progress::new("code graph", entries.len(), !quiet);
    for e in &entries {
        match index_file(&store, project, e) {
            Ok(true) => rep.indexed += 1,
            Ok(false) => {}
            Err(err) => rep.warnings.push(format!("{}: {}", e.rel, err)),
        }
        prog.inc();
    }
    prog.finish();
    resolve_imports(&store, project)?;

    // 3. Embeddings (hash fallback is instant; a model is used when reachable).
    let llm = Llm::new(cfg);
    let embedder = if opts.ast_only {
        Embedder::for_id(crate::embed::HASH_MODEL, &llm.rt)
    } else {
        Embedder::new(cfg, &llm.rt)
    };
    set_step(2, steps);
    let embed_result = embed_pending(&store, &embedder, cfg.learn.embed_batch, quiet);
    match embed_result {
        Ok(n) => rep.embedded = n,
        Err(err) => {
            rep.warnings.push(format!(
                "embedding model failed ({}); using built-in hash embeddings",
                err
            ));
            let fallback = Embedder::for_id(crate::embed::HASH_MODEL, &llm.rt);
            rep.embedded = embed_pending(&store, &fallback, 256, quiet)?;
        }
    }
    rep.embed_model = store.meta("embed_model")?.unwrap_or_default();

    if !opts.ast_only {
        // 4. Documents (images/PDFs → text, locally).
        set_step(3, steps);
        rep.docs = crate::docs::learn_docs(
            &store,
            project,
            cfg,
            &llm,
            opts.no_llm,
            quiet,
            &mut rep.warnings,
        )?;
        // Re-embed doc chunks that step 4 created.
        let _ = embed_pending(
            &store,
            &Embedder::for_id(&rep.embed_model, &llm.rt),
            cfg.learn.embed_batch,
            true,
        );

        // 5 + 6. Bottom-up summaries with symbol validation.
        set_step(4, steps);
        let s = crate::summarize::summarize_all(
            &store,
            project,
            cfg,
            &llm,
            opts.no_llm,
            quiet,
            opts.summary_budget,
        )?;
        rep.summaries = s.written;
        rep.llm_summaries = s.via_llm;
        rep.flagged = s.flagged;
        rep.summaries_pending = s.pending;
        store.set_meta("summaries_pending", &s.pending.to_string())?;

        // Archive memories whose files no longer exist.
        rep.archived_memories = crate::memory::archive_orphans(&store, project)?;
    }

    // A first full index rewrites most pages; compact once so the store stays small.
    if rep.indexed > 200 && rep.indexed * 2 > rep.scanned {
        let _ = store.compact();
    }
    rep.symbols = store.count("SELECT count(*) FROM symbols");
    rep.edges = store.count("SELECT count(*) FROM edges");
    rep.chunks = store.count("SELECT count(*) FROM chunks");
    if let Ok(mut s) = STEP.lock() {
        *s = None;
    }
    store.set_meta("last_learn", &util::now().to_string())?;
    if let Some(head) = git_head(&project.root) {
        store.set_meta("git_head", &head)?;
    }
    Ok(rep)
}

pub fn git_head(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Re-index a set of changed paths (used by the watcher). Returns number of files updated.
pub fn reindex_paths(
    store: &Store,
    project: &Project,
    cfg: &Config,
    paths: &[std::path::PathBuf],
) -> Result<usize> {
    let mut n = 0;
    for abs in paths {
        let rel = project.rel(abs);
        if !abs.exists() {
            if store.delete_file(&rel)? {
                mark_summaries_stale(store, &rel)?;
                n += 1;
            }
            continue;
        }
        if !abs.is_file() || !scan::should_index(project, abs, &cfg.index.exclude) {
            continue;
        }
        let Some(kind) = scan::classify(abs) else {
            continue;
        };
        let Ok(md) = std::fs::metadata(abs) else {
            continue;
        };
        if md.len() == 0
            || md.len() > cfg.learn.max_file_bytes * if kind == Kind::Doc { 50 } else { 1 }
        {
            continue;
        }
        let e = Entry {
            abs: abs.clone(),
            rel,
            kind,
            size: md.len(),
            mtime: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        };
        if index_file(store, project, &e)? {
            n += 1;
        }
    }
    if n > 0 {
        resolve_imports(store, project)?;
        // Hash embeddings are instant: keep lexical+vector search fresh right away.
        let model = store
            .meta("embed_model")?
            .unwrap_or_else(|| crate::embed::HASH_MODEL.into());
        if model == crate::embed::HASH_MODEL {
            let rt = crate::runtime::Runtime::new("none", "", "", 5, "");
            embed_pending(store, &Embedder::for_id(&model, &rt), 256, true)?;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cover_code() {
        let src = "use x;\n\nfn a() {\n  1\n}\n\nfn b() {\n  2\n}\n";
        let parsed = lang::parse(Lang::Rust, src);
        let chunks = chunk_code(src, &parsed.symbols);
        assert!(chunks
            .iter()
            .any(|c| c.0.as_deref() == Some("a") && c.1 == 3 && c.2 == 5));
        assert!(chunks.iter().any(|c| c.0.is_none() && c.1 == 1));
    }

    #[test]
    fn chunks_markdown_by_heading() {
        let t = "# A\ntext\n## B\nmore\n";
        let c = chunk_text(t);
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].0.as_deref(), Some("B"));
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(fmt_duration(0), "0s");
        assert_eq!(fmt_duration(42), "42s");
        assert_eq!(fmt_duration(65), "1m 05s");
        assert_eq!(fmt_duration(3780), "1h 03m");
    }

    #[test]
    fn eta_waits_for_data_then_extrapolates() {
        assert_eq!(eta_secs(0, 100, 10.0), None);
        assert_eq!(eta_secs(1, 100, 0.5), None);
        assert_eq!(eta_secs(100, 100, 50.0), None);
        // 10 done in 20 s → 2 s each → 90 left ≈ 180 s.
        assert_eq!(eta_secs(10, 100, 20.0), Some(180));
    }
}
