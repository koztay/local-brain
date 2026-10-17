//! `brain_query`: answers questions about the codebase with a short answer, `file:line`
//! pointers and a confidence score, choosing the cheapest layer that can answer:
//!
//! * structural — definitions, callers, callees, implementations, importers (code graph)
//! * architectural — project overview and architecture map (summaries)
//! * semantic — hybrid lexical (FTS5/BM25) + vector search, optionally a local-LLM answer
//!
//! It never returns whole files; code is fetched with `brain_slice`.

use crate::config::Config;
use crate::embed::{self, Embedder};
use crate::llm::Llm;
use crate::paths::Project;
use crate::store::{ChunkRow, Store, SymbolRow};
use crate::util;
use anyhow::Result;
use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

pub struct Answer {
    /// The rendered answer (includes confidence, layer and staleness notes).
    pub text: String,
    /// Files the answer points to (used for savings accounting).
    pub files: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Intent {
    Definition,
    Callers,
    Callees,
    Implementations,
    Importers,
    Architecture,
    Semantic,
}

fn rx(cell: &'static OnceLock<Regex>, p: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(p).unwrap())
}

pub fn classify(q: &str, has_ident: bool) -> Intent {
    static CALLERS: OnceLock<Regex> = OnceLock::new();
    static CALLEES: OnceLock<Regex> = OnceLock::new();
    static IMPL: OnceLock<Regex> = OnceLock::new();
    static IMPORTERS: OnceLock<Regex> = OnceLock::new();
    static DEF: OnceLock<Regex> = OnceLock::new();
    static ARCH: OnceLock<Regex> = OnceLock::new();
    let q = q.to_lowercase();
    if rx(
        &CALLEES,
        r"\bwhat (functions |methods )?does .+ call\b|\bcallees\b|\bcalled by\b.*\bwhat\b",
    )
    .is_match(&q)
    {
        return Intent::Callees;
    }
    if rx(&CALLERS, r"\b(who|what|which|where)\b.*\b(calls?|uses?|invokes?|references?|is (called|used|invoked))\b|\bcallers?\b|\busages?\b|\breferences? to\b|\bwhere .* (used|called)\b").is_match(&q) && has_ident {
        return Intent::Callers;
    }
    if rx(&IMPL, r"\bimplement(s|ations?|ors?)?\b|\bsubclass(es)?\b|\binherit(s|ance)?\b|\bextends\b|\bconforms? to\b").is_match(&q) && has_ident {
        return Intent::Implementations;
    }
    if rx(
        &IMPORTERS,
        r"\b(who|what|which)\b.*\bimports?\b|\bimporters?\b|\bdepends? on\b|\bdependents\b",
    )
    .is_match(&q)
    {
        return Intent::Importers;
    }
    if rx(&ARCH, r"\barchitecture\b|\boverview\b|\bhigh[- ]level\b|\bentry ?points?\b|\bdata ?flow\b|\blayers?\b|\b(project|repo|codebase|code base) (structure|layout)\b|\bhow is (the|this) (project|repo|codebase|code) (organi[sz]ed|structured)\b|\bwhat does (this|the) (project|repo|codebase|app) do\b").is_match(&q) {
        return Intent::Architecture;
    }
    if has_ident && rx(&DEF, r"\bwhere\b|\bdefin(ed|ition)\b|\bdeclared?\b|\bfind\b|\blocate\b|\bsignature\b|^\s*`?[\w:.]+`?\s*\??\s*$").is_match(&q) {
        return Intent::Definition;
    }
    Intent::Semantic
}

/// Identifiers mentioned in the question that exist in the graph.
fn identifiers(store: &Store, q: &str) -> Vec<String> {
    let mut cands: Vec<String> = Vec::new();
    let mut rest = q;
    while let Some(a) = rest.find('`') {
        let after = &rest[a + 1..];
        let Some(b) = after.find('`') else { break };
        cands.push(after[..b].trim_end_matches("()").to_string());
        rest = &after[b + 1..];
    }
    for w in q.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '.')) {
        let w = w
            .trim_matches(|c| c == '.' || c == ':')
            .trim_end_matches("()");
        if w.len() >= 3 {
            cands.push(w.to_string());
        }
    }
    let mut out = Vec::new();
    for c in cands {
        let last = c.rsplit(['.', ':']).next().unwrap_or(&c).to_string();
        for name in [c.clone(), last] {
            if name.len() < 2 || out.contains(&name) {
                continue;
            }
            let looks_code = name.contains('_')
                || name.chars().skip(1).any(|ch| ch.is_uppercase())
                || q.contains(&format!("`{}", name));
            let common = name.chars().all(|ch| ch.is_lowercase()) && !looks_code;
            // Plain lowercase English words only count when they are exact symbol names.
            let hits = store.symbols_named(&name).unwrap_or_default();
            let exact = hits.iter().any(|s| s.name == name);
            if exact || (!common && !hits.is_empty()) {
                out.push(name);
            }
        }
    }
    out
}

fn pointer(s: &SymbolRow) -> String {
    let qual = match &s.parent {
        Some(p) => format!("{}.{}", p, s.name),
        None => s.name.clone(),
    };
    format!(
        "{}:{}-{} {} `{}` — {}",
        s.path,
        s.start_line,
        s.end_line,
        s.kind,
        qual,
        util::clip(&s.signature, 110)
    )
}

fn stale_files(store: &Store, project: &Project, files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for f in files {
        if let Ok(Some(row)) = store.file(f) {
            let disk = std::fs::metadata(project.abs(f))
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let indexed_mtime: i64 = store
                .conn
                .query_row("SELECT mtime FROM files WHERE id = ?", [row.id], |r| {
                    r.get(0)
                })
                .unwrap_or(0);
            let summary_stale = matches!(store.summary("file", f), Ok(Some((_, true, ..))));
            if disk.map(|d| d > indexed_mtime).unwrap_or(true) || summary_stale {
                out.push(f.clone());
            }
        }
    }
    out
}

fn in_scope(path: &str, scope: Option<&str>) -> bool {
    match scope {
        Some(s) if !s.is_empty() && s != "." => path.starts_with(s.trim_start_matches("./")),
        _ => true,
    }
}

fn definition(
    store: &Store,
    idents: &[String],
    scope: Option<&str>,
) -> Result<Option<(String, Vec<String>, f32)>> {
    let mut lines = Vec::new();
    let mut files = Vec::new();
    let mut total = 0;
    for id in idents.iter().take(3) {
        let name = id.rsplit(['.', ':']).next().unwrap_or(id);
        let hits: Vec<SymbolRow> = store
            .symbols_best(name)?
            .into_iter()
            .filter(|s| in_scope(&s.path, scope))
            .collect();
        if hits.is_empty() {
            continue;
        }
        total += hits.len();
        for s in hits.iter().take(8) {
            lines.push(pointer(s));
            if let Some(d) = &s.doc {
                lines.push(format!("    {}", util::clip(d, 160)));
            }
            files.push(s.path.clone());
        }
        if hits.len() > 8 {
            lines.push(format!(
                "  (+{} more definitions named `{}`)",
                hits.len() - 8,
                name
            ));
        }
    }
    if lines.is_empty() {
        return Ok(None);
    }
    let head = if total == 1 {
        "Defined at:".to_string()
    } else {
        format!("{} definitions:", total)
    };
    let conf = if total == 1 { 0.95 } else { 0.85 };
    Ok(Some((
        format!("{}\n{}", head, lines.join("\n")),
        files,
        conf,
    )))
}

fn callers(
    store: &Store,
    idents: &[String],
    kind: &str,
    scope: Option<&str>,
) -> Result<Option<(String, Vec<String>, f32)>> {
    let Some(id) = idents.first() else {
        return Ok(None);
    };
    let name = id.rsplit(['.', ':']).next().unwrap_or(id);
    let edges: Vec<_> = store
        .edges_to(name, kind)?
        .into_iter()
        .filter(|e| in_scope(&e.path, scope))
        .collect();
    let defs = store.symbols_best(name)?;
    if edges.is_empty() {
        let verb = if kind == "call" {
            "calls to"
        } else {
            "types inheriting from / implementing"
        };
        let loc = defs
            .first()
            .map(|d| format!(" (defined at {}:{})", d.path, d.start_line))
            .unwrap_or_default();
        return Ok(Some((format!("No {} `{}` found in the code graph{}. Dynamic dispatch, reflection or string-based calls are not tracked.", verb, name, loc), vec![], 0.6)));
    }
    let mut grouped: BTreeMap<(String, String), Vec<i64>> = BTreeMap::new();
    for e in &edges {
        grouped
            .entry((
                e.path.clone(),
                e.source.clone().unwrap_or_else(|| "<file scope>".into()),
            ))
            .or_default()
            .push(e.line);
    }
    let mut lines = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for ((path, src), ls) in grouped.iter().take(30) {
        let ls_s: Vec<String> = ls.iter().take(5).map(|l| l.to_string()).collect();
        lines.push(format!("- {}:{} in `{}`", path, ls_s.join(","), src));
        if !files.contains(path) {
            files.push(path.clone());
        }
    }
    if grouped.len() > 30 {
        lines.push(format!("  (+{} more call sites)", grouped.len() - 30));
    }
    let what = if kind == "call" {
        "Call sites of"
    } else {
        "Implementations / subclasses of"
    };
    let mut text = format!(
        "{} `{}` ({} sites in {} files):\n{}",
        what,
        name,
        edges.len(),
        grouped.keys().map(|k| &k.0).collect::<HashSet<_>>().len(),
        lines.join("\n")
    );
    let conf = if defs.len() > 1 {
        text.push_str(&format!(
            "\nNote: {} symbols are named `{}`; matching is by name.",
            defs.len(),
            name
        ));
        0.75
    } else {
        0.9
    };
    Ok(Some((text, files, conf)))
}

fn callees(store: &Store, idents: &[String]) -> Result<Option<(String, Vec<String>, f32)>> {
    let Some(id) = idents.first() else {
        return Ok(None);
    };
    let name = id.rsplit(['.', ':']).next().unwrap_or(id);
    let defs = store.symbols_best(name)?;
    let Some(def) = defs.first() else {
        return Ok(None);
    };
    let edges = store.edges_from_symbol(def.file_id, name)?;
    let mut seen = Vec::new();
    let mut lines = Vec::new();
    let mut external: Vec<String> = Vec::new();
    for e in edges.iter().filter(|e| e.kind == "call") {
        if seen.contains(&e.target) {
            continue;
        }
        seen.push(e.target.clone());
        match store
            .symbols_named(&e.target)?
            .into_iter()
            .find(|s| s.name == e.target)
        {
            Some(s) => lines.push(format!(
                "- `{}` at line {} → {}:{}",
                e.target, e.line, s.path, s.start_line
            )),
            None => external.push(e.target.clone()),
        }
    }
    let mut text = format!(
        "`{}` ({}:{}-{}) calls these project symbols:\n{}",
        name,
        def.path,
        def.start_line,
        def.end_line,
        if lines.is_empty() {
            "(none)".into()
        } else {
            lines.join("\n")
        }
    );
    if !external.is_empty() {
        text.push_str(&format!(
            "\nExternal/library calls: {}",
            util::clip(&external.join(", "), 300)
        ));
    }
    Ok(Some((text, vec![def.path.clone()], 0.85)))
}

fn importers(
    store: &Store,
    q: &str,
    idents: &[String],
) -> Result<Option<(String, Vec<String>, f32)>> {
    // Find a file path or module name in the question.
    let files = store.files()?;
    let words: Vec<&str> = q
        .split(|c: char| c.is_whitespace() || c == '`' || c == '?' || c == '"' || c == '\'')
        .filter(|w| w.len() > 2)
        .collect();
    let target = files
        .iter()
        .find(|f| {
            words
                .iter()
                .any(|w| f.path == *w || f.path.ends_with(&format!("/{}", w)))
        })
        .or_else(|| {
            let stems: Vec<String> = idents
                .iter()
                .chain(
                    words
                        .iter()
                        .map(|w| w.to_string())
                        .collect::<Vec<_>>()
                        .iter(),
                )
                .map(|s| s.to_lowercase())
                .collect();
            files.iter().find(|f| {
                let stem = std::path::Path::new(&f.path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                stems.contains(&stem)
            })
        });
    let Some(t) = target else { return Ok(None) };
    let edges = store.importers_of(t.id)?;
    let mut uniq: Vec<String> = edges
        .iter()
        .map(|e| format!("{}:{}", e.path, e.line))
        .collect();
    uniq.dedup();
    let files_out: Vec<String> = edges
        .iter()
        .map(|e| e.path.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Some((
        format!(
            "{} is imported by {} files:\n{}",
            t.path,
            files_out.len(),
            uniq.iter()
                .take(40)
                .map(|s| format!("- {}", s))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        files_out,
        0.85,
    )))
}

fn architecture(store: &Store) -> Result<Option<(String, Vec<String>, f32)>> {
    let mut out = String::new();
    if let Some((p, stale, ..)) = store.summary("project", ".")? {
        out.push_str(&format!(
            "Overview{}:\n{}\n\n",
            if stale { " (stale)" } else { "" },
            p
        ));
    }
    if let Some((a, ..)) = store.summary("architecture", ".")? {
        out.push_str("Architecture map:\n");
        out.push_str(&a);
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some((out, vec![], 0.7)))
}

const STOP: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "what",
    "where",
    "which",
    "who",
    "how",
    "does",
    "this",
    "that",
    "from",
    "into",
    "are",
    "was",
    "were",
    "is",
    "it",
    "its",
    "of",
    "to",
    "in",
    "on",
    "a",
    "an",
    "be",
    "do",
    "can",
    "code",
    "file",
    "files",
    "function",
    "functions",
    "method",
    "class",
    "project",
    "use",
    "used",
    "using",
    "find",
    "show",
    "me",
    "there",
    "any",
    "about",
    "handled",
    "handle",
    "happens",
    "work",
    "works",
    "when",
];

fn fts_query(q: &str) -> String {
    let mut terms: Vec<String> = Vec::new();
    for w in q.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if w.len() < 3 {
            continue;
        }
        let lw = w.to_lowercase();
        if STOP.contains(&lw.as_str()) {
            continue;
        }
        terms.push(format!("\"{}\"", lw));
        for part in util::split_ident(w) {
            if part.len() >= 3 && part != lw && !STOP.contains(&part.as_str()) {
                terms.push(format!("\"{}\"", part));
            }
        }
    }
    terms.dedup();
    terms.join(" OR ")
}

fn query_terms(q: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for w in q.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if w.len() < 3 {
            continue;
        }
        let lw = w.to_lowercase();
        if STOP.contains(&lw.as_str()) {
            continue;
        }
        let st = embed::stem(&lw);
        if !terms.contains(&st) {
            terms.push(st);
        }
    }
    terms
}

/// Fraction of query terms present in a chunk (text, path and symbol), with stemming.
fn coverage(terms: &[String], c: &ChunkRow) -> f32 {
    if terms.is_empty() {
        return 0.0;
    }
    let mut words: HashSet<String> = HashSet::new();
    let hay = format!(
        "{} {} {}",
        c.text,
        c.path,
        c.symbol.clone().unwrap_or_default()
    );
    for w in hay.split(|ch: char| !(ch.is_alphanumeric() || ch == '_')) {
        if w.is_empty() || w.len() > 60 {
            continue;
        }
        words.insert(embed::stem(&w.to_lowercase()));
        for p in util::split_ident(w) {
            words.insert(embed::stem(&p));
        }
    }
    let hit = terms
        .iter()
        .filter(|t| {
            words.contains(*t)
                || words.iter().any(|w| {
                    w.len() > 4
                        && t.len() > 4
                        && (w.starts_with(t.as_str()) || t.starts_with(w.as_str()))
                })
        })
        .count();
    hit as f32 / terms.len() as f32
}

/// Hybrid retrieval: BM25 and vector candidates, fused by reciprocal rank, then reranked by
/// query-term coverage (so tiny chunks and long functions are judged on content).
pub fn search(
    store: &Store,
    cfg: &Config,
    q: &str,
    scope: Option<&str>,
    idents: &[String],
    k: usize,
) -> Result<Vec<(ChunkRow, f32)>> {
    let mut rrf: HashMap<i64, f32> = HashMap::new();
    let fts = store.fts(&fts_query(q), 200)?;
    for (rank, (id, _)) in fts.iter().enumerate() {
        *rrf.entry(*id).or_default() += 1.0 / (30.0 + rank as f32);
    }
    let model = store
        .meta("embed_model")?
        .unwrap_or_else(|| embed::HASH_MODEL.into());
    let rt = crate::runtime::Runtime::from_config(cfg).with_timeout(20);
    let embedder = Embedder::for_id(&model, &rt);
    let mut vec_sim: HashMap<i64, f32> = HashMap::new();
    if let Ok(qv) = embedder.embed(&[q.to_string()]) {
        let qv = &qv[0];
        let mut sims: Vec<(i64, f32)> = store
            .embeddings(&model)?
            .into_iter()
            .map(|(id, _, v)| (id, embed::cosine(qv, &v)))
            .collect();
        sims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (rank, (id, sim)) in sims.iter().take(100).enumerate() {
            *rrf.entry(*id).or_default() += 1.0 / (30.0 + rank as f32);
            vec_sim.insert(*id, *sim);
        }
    }
    let max_rrf = rrf.values().cloned().fold(0.0f32, f32::max).max(1e-6);
    let terms = query_terms(q);
    let mut out = Vec::new();
    for (id, r) in rrf {
        let Some(c) = store.chunk(id)? else { continue };
        if !in_scope(&c.path, scope) {
            continue;
        }
        let cov = coverage(&terms, &c);
        let mut score = cov + 0.5 * (r / max_rrf) + 0.3 * vec_sim.get(&id).copied().unwrap_or(0.0);
        let meaningful = c.text.lines().filter(|l| l.trim().len() > 3).count();
        if meaningful < 3 {
            score *= 0.6;
        }
        if let Some(sym) = &c.symbol {
            if idents.iter().any(|i| sym.ends_with(i.as_str())) {
                score += 0.3;
            }
            if sym.starts_with("tests.") || sym.starts_with("test") {
                score *= 0.8;
            }
        }
        let lower = c.path.to_lowercase();
        if lower.contains("test") || lower.contains("spec") {
            score *= 0.85;
        }
        out.push((c, score));
    }
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    // Prefer diversity: at most 3 chunks per file in the top results.
    let mut per_file: HashMap<String, usize> = HashMap::new();
    out.retain(|(c, _)| {
        let n = per_file.entry(c.path.clone()).or_default();
        *n += 1;
        *n <= 3
    });
    out.truncate(k);
    Ok(out)
}

fn semantic(
    store: &Store,
    cfg: &Config,
    llm: Option<&Llm>,
    q: &str,
    scope: Option<&str>,
    idents: &[String],
) -> Result<(String, Vec<String>, f32)> {
    let hits = search(store, cfg, q, scope, idents, 8)?;
    if hits.is_empty() {
        return Ok(("No relevant code found in the index. Try different words, a symbol name in backticks, or run `local-brain learn`.".into(), vec![], 0.1));
    }
    let mut files: Vec<String> = Vec::new();
    let mut pointers = Vec::new();
    for (c, _) in &hits {
        let first = c
            .text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        pointers.push(format!(
            "- {}:{}-{}{} — {}",
            c.path,
            c.start_line,
            c.end_line,
            c.symbol
                .as_ref()
                .map(|s| format!(" `{}`", s))
                .unwrap_or_default(),
            util::clip(first, 100)
        ));
        if !files.contains(&c.path) {
            files.push(c.path.clone());
        }
    }
    let mut answer = String::new();
    if let Some(llm) = llm.filter(|l| l.has_text()) {
        let mut ctx = String::new();
        for (c, _) in hits.iter().take(6) {
            ctx.push_str(&format!(
                "--- {}:{}-{}\n{}\n",
                c.path,
                c.start_line,
                c.end_line,
                util::clip_tokens(&c.text, 450).0
            ));
        }
        let prompt = format!(
            "Answer the developer's question in at most 4 sentences using only these code excerpts. Cite locations as path:line. If the excerpts are insufficient, say what is missing.\n\nQuestion: {}\n\n{}",
            q, ctx
        );
        if let Ok(a) = llm.text(&prompt, 260) {
            // Hard cap: small models do not always respect the sentence limit.
            answer = util::clip_tokens(a.trim(), 160).0.trim().to_string();
            if !answer.is_empty() {
                answer = format!("{} (local model)", answer);
            }
        }
    }
    if answer.is_empty() {
        let mut lines = Vec::new();
        for f in files.iter().take(3) {
            if let Some((s, ..)) = store.summary("file", f)? {
                lines.push(format!("{}: {}", f, util::clip(&s.replace('\n', " "), 220)));
            }
        }
        answer = format!(
            "Most relevant locations (ranked by lexical + vector search):\n{}",
            lines.join("\n")
        );
    }
    // The best hit's score is mostly query-term coverage (0..1) plus fused-rank strength.
    let strength = hits[0].1;
    let conf = (0.2 + strength * 0.4).clamp(0.25, 0.8);
    Ok((
        format!("{}\n\nPointers:\n{}", answer, pointers.join("\n")),
        files,
        conf,
    ))
}

pub fn query(
    store: &Store,
    project: &Project,
    cfg: &Config,
    llm: Option<&Llm>,
    question: &str,
    scope: Option<&str>,
) -> Result<Answer> {
    let all_idents = identifiers(store, question);
    // Code-looking names (snake_case, camelCase, `backticked`) always count; plain words that
    // happen to be symbol names only count in short questions like "where is parse".
    let strong: Vec<String> = all_idents
        .iter()
        .filter(|i| {
            i.contains('_')
                || i.chars().skip(1).any(|c| c.is_uppercase())
                || i.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
                || question.contains(&format!("`{}", i))
        })
        .cloned()
        .collect();
    let short = question.split_whitespace().count() <= 5;
    let idents = if !strong.is_empty() {
        strong.clone()
    } else if short {
        all_idents.clone()
    } else {
        vec![]
    };
    let intent = classify(question, !idents.is_empty());
    let (mut text, files, conf, layer) = {
        let structural = match intent {
            Intent::Definition => definition(store, &idents, scope)?,
            Intent::Callers => callers(store, &idents, "call", scope)?,
            Intent::Implementations => callers(store, &idents, "inherit", scope)?,
            Intent::Callees => callees(store, &idents)?,
            Intent::Importers => importers(store, question, &idents)?,
            Intent::Architecture => architecture(store)?,
            Intent::Semantic => None,
        };
        match structural {
            Some((t, f, c)) => (
                t,
                f,
                c,
                if intent == Intent::Architecture {
                    "architectural"
                } else {
                    "structural"
                },
            ),
            None => {
                let (t, f, c) = semantic(store, cfg, llm, question, scope, &all_idents)?;
                // A semantic question that names a known symbol also gets its definition.
                let mut t = t;
                if intent == Intent::Semantic && !strong.is_empty() {
                    if let Some((d, _, _)) = definition(store, &strong, scope)? {
                        t = format!("{}\n\n{}", d, t);
                    }
                }
                (t, f, c, "semantic")
            }
        }
    };
    let stale = stale_files(store, project, &files);
    if !stale.is_empty() {
        text.push_str(&format!("\nstale: true — changed since indexing: {}. Fetch current code with brain_slice before relying on details.", stale.iter().take(5).cloned().collect::<Vec<_>>().join(", ")));
    }
    text.push_str(&format!("\n[confidence {:.2} · {} layer]", conf, layer));
    Ok(Answer {
        text: crate::secrets::mask(&text),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_questions() {
        assert_eq!(classify("who calls `open_db`?", true), Intent::Callers);
        assert_eq!(classify("where is Store defined", true), Intent::Definition);
        assert_eq!(
            classify("give me an overview of the architecture", false),
            Intent::Architecture
        );
        assert_eq!(
            classify("how are errors retried when the network fails", false),
            Intent::Semantic
        );
        assert_eq!(
            classify("what implements Display", true),
            Intent::Implementations
        );
        assert_eq!(classify("what does run_hook call", true), Intent::Callees);
    }

    #[test]
    fn builds_fts_query() {
        assert_eq!(
            fts_query("where is parseConfig handled"),
            "\"parseconfig\" OR \"parse\" OR \"config\""
        );
    }
}
