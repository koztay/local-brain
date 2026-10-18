//! Bottom-up summaries (file → module → project) plus a deterministic architecture map.
//! Every summary is bound to the hash of its sources and validated against the code graph:
//! summaries that mention names the graph does not know are regenerated or flagged.

use crate::config::Config;
use crate::learn::Progress;
use crate::llm::Llm;
use crate::paths::Project;
use crate::store::{FileRow, Store};
use crate::util;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Words that are fine in summaries even though they are not project symbols.
const ALLOWED: &[&str] = &[
    "String",
    "Vec",
    "HashMap",
    "HashSet",
    "BTreeMap",
    "Option",
    "Result",
    "Box",
    "Arc",
    "Rc",
    "Mutex",
    "JSON",
    "HTTP",
    "HTTPS",
    "API",
    "SQL",
    "SQLite",
    "CLI",
    "MCP",
    "LLM",
    "URL",
    "UI",
    "CSS",
    "HTML",
    "README",
    "TypeScript",
    "JavaScript",
    "Python",
    "Rust",
    "Go",
    "Swift",
    "Kotlin",
    "React",
    "Node",
    "async",
    "await",
    "GitHub",
    "OAuth",
    "REST",
    "gRPC",
    "YAML",
    "TOML",
    "CSV",
    "PDF",
    "AST",
    "ID",
    "UUID",
    "CRUD",
    "JWT",
    "SDK",
    "OpenAI",
    "Ollama",
    "macOS",
    "iOS",
    "Android",
    "Linux",
    "Windows",
    "stdin",
    "stdout",
    "stderr",
    "self",
    "this",
    "None",
    "null",
    "true",
    "false",
    "main",
    "init",
    "PostgreSQL",
    "MySQL",
    "Redis",
    "Docker",
    "WebSocket",
    "GraphQL",
    "npm",
    "pip",
    "cargo",
];

fn candidate_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    // `backticked` names.
    let mut rest = text;
    while let Some(a) = rest.find('`') {
        let after = &rest[a + 1..];
        let Some(b) = after.find('`') else { break };
        let tok = &after[..b];
        if !tok.is_empty() && tok.len() < 80 {
            out.push(tok.to_string());
        }
        rest = &after[b + 1..];
    }
    // camelCase / PascalCase-with-inner-capitals / snake_case words outside backticks.
    for w in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if w.len() < 3 || w.chars().next().map(|c| c.is_numeric()).unwrap_or(true) {
            continue;
        }
        let inner_upper =
            w.chars().skip(1).any(|c| c.is_uppercase()) && w.chars().any(|c| c.is_lowercase());
        let snake = w.contains('_') && w.chars().any(|c| c.is_alphabetic());
        if inner_upper || snake {
            out.push(w.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Names in a summary that do not exist in the graph (symbols, files, imported/called names).
pub fn unknown_names(text: &str, known: &HashSet<String>) -> (usize, Vec<String>) {
    let cands = candidate_names(text);
    let allowed: HashSet<&str> = ALLOWED.iter().copied().collect();
    let mut unknown = Vec::new();
    for c in &cands {
        // A path-like token: accept if any known file stem matches.
        let parts: Vec<&str> = c
            .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            continue;
        }
        let ok = parts.iter().any(|p| {
            known.contains(*p)
                || allowed.contains(*p)
                || p.chars().all(|ch| ch.is_numeric())
                || p.len() <= 2
        }) || known.contains(c.as_str());
        if !ok {
            unknown.push(c.clone());
        }
    }
    (cands.len(), unknown)
}

pub fn known_names(store: &Store) -> Result<HashSet<String>> {
    let mut known = store.symbol_names()?;
    let mut st = store.conn.prepare("SELECT DISTINCT target FROM edges")?;
    for t in st.query_map([], |r| r.get::<_, String>(0))?.flatten() {
        for p in t.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if !p.is_empty() {
                known.insert(p.to_string());
            }
        }
    }
    for f in store.files()? {
        for p in f.path.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if !p.is_empty() {
                known.insert(p.to_string());
            }
        }
    }
    Ok(known)
}

fn is_suspicious(total: usize, unknown: &[String]) -> bool {
    unknown.len() >= 2 && unknown.len() * 3 >= total.max(1)
}

/// Reject model output that is not prose: echoed code, code fences, empty or rambling text.
pub fn looks_like_summary(s: &str) -> bool {
    let t = s.trim();
    if t.len() < 20 || t.len() > 1500 || t.contains("```") {
        return false;
    }
    let lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
    let codey = lines
        .iter()
        .filter(|l| {
            let l = l.trim_end();
            l.ends_with(';')
                || l.ends_with('{')
                || l.ends_with('}')
                || l.trim_start().starts_with("fn ")
                || l.trim_start().starts_with("def ")
                || l.trim_start().starts_with("import ")
                || l.trim_start().starts_with("use ")
        })
        .count();
    codey * 4 <= lines.len()
}

/// Deterministic file summary from the graph.
pub fn heuristic_file_summary(store: &Store, f: &FileRow) -> Result<String> {
    let syms = store.symbols_in_file(f.id)?;
    let lang = f.lang.clone().unwrap_or_else(|| f.kind.clone());
    let mut s = format!("{} {} file, {} lines.", lang, f.kind, f.lines);
    if let Some(doc) = first_comment(store, f).or_else(|| syms.iter().find_map(|x| x.doc.clone())) {
        s.push(' ');
        s.push_str(&util::clip(&doc, 200));
    }
    let top: Vec<String> = syms
        .iter()
        .filter(|x| {
            x.parent.is_none()
                || matches!(
                    x.kind.as_str(),
                    "class" | "struct" | "interface" | "trait" | "enum"
                )
        })
        .take(12)
        .map(|x| format!("{} `{}`", x.kind, x.name))
        .collect();
    if !top.is_empty() {
        s.push_str(&format!(" Defines {}.", top.join(", ")));
        if syms.len() > top.len() {
            s.push_str(&format!(" ({} symbols total.)", syms.len()));
        }
    }
    let imports: Vec<String> = store
        .edges_of_file(f.id)?
        .into_iter()
        .filter(|e| e.kind == "import")
        .take(8)
        .map(|e| e.resolved_path.unwrap_or(e.target))
        .collect();
    if !imports.is_empty() {
        s.push_str(&format!(" Imports {}.", imports.join(", ")));
    }
    if f.kind == "text" {
        if let Some(c) = store.chunks_of_file(f.id)?.first() {
            let first: String = c
                .text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(3)
                .collect::<Vec<_>>()
                .join(" ");
            s.push_str(&format!(" Begins: {}", util::clip(&first, 200)));
        }
    }
    Ok(s)
}

fn first_comment(store: &Store, f: &FileRow) -> Option<String> {
    let c = store.chunks_of_file(f.id).ok()?.into_iter().next()?;
    let mut doc = Vec::new();
    for l in c.text.lines().take(15) {
        let t = l.trim();
        if t.starts_with("//!")
            || t.starts_with("///")
            || t.starts_with('#') && !t.starts_with("#[") && !t.starts_with("#!")
            || t.starts_with("\"\"\"")
            || t.starts_with("/**")
            || t.starts_with("* ")
        {
            let t = t.trim_start_matches(['/', '!', '#', '*', '"']).trim();
            if !t.is_empty() {
                doc.push(t.to_string());
            }
        } else if (!doc.is_empty()
            || (!t.is_empty()
                && !t.starts_with("//")
                && !t.starts_with("use ")
                && !t.starts_with("import")
                && !t.starts_with("package")))
            && !doc.is_empty()
        {
            break;
        }
    }
    if doc.is_empty() {
        None
    } else {
        Some(doc.join(" "))
    }
}

fn llm_file_summary(
    llm: &Llm,
    project: &Project,
    store: &Store,
    f: &FileRow,
    tier: &str,
    strict: Option<&HashSet<String>>,
) -> Result<String> {
    // Prompt and answer sizes drive the cost: on a small model each 100 prompt tokens cost
    // ~0.25 s and each answer token ~30 ms, so tier S keeps both tight.
    let small = tier == "S";
    let skeleton = crate::slice::skeleton(store, project, &f.path)?;
    let (skeleton, _) = util::clip_tokens(&skeleton, if small { 400 } else { 700 });
    let src = std::fs::read_to_string(project.abs(&f.path)).unwrap_or_default();
    let (src, _) = util::clip_tokens(&src, if small { 600 } else { 1500 });
    let length = if small {
        "1-2 sentences (at most 45 words)"
    } else {
        "2-3 sentences (at most 70 words)"
    };
    let mut prompt = format!(
        "Summarize this source file for a developer who has not seen it, in {length}. \
         State its responsibility and name its key symbols in backticks. \
         Use only names that literally appear in the file. No preamble, no markdown headings.\n\n\
         Path: {}\nOutline:\n{}\nSource (may be truncated):\n{}\n",
        f.path, skeleton, src
    );
    if let Some(names) = strict {
        let mut list: Vec<&String> = names.iter().collect();
        list.sort();
        list.truncate(80);
        prompt.push_str(&format!(
            "\nIMPORTANT: your previous answer mentioned names that do not exist. Only use these names: {}\n",
            list.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    let out = llm.code(&prompt, if small { 90 } else { 160 })?;
    Ok(out.trim().to_string())
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SummaryStats {
    pub written: usize,
    pub via_llm: usize,
    pub flagged: usize,
    /// Model summaries still to do because the time budget ran out.
    pub pending: usize,
}

/// How many files reference each file (resolved imports and calls): a cheap importance rank.
fn incoming_refs(store: &Store) -> HashMap<i64, i64> {
    let mut out = HashMap::new();
    if let Ok(mut st) = store.conn.prepare(
        "SELECT resolved_file_id, count(DISTINCT file_id) FROM edges \
         WHERE resolved_file_id IS NOT NULL AND resolved_file_id != file_id GROUP BY 1",
    ) {
        if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))) {
            out.extend(rows.flatten());
        }
    }
    out
}

/// Summarize everything that is missing or stale. Model summaries go to the `max_llm_files`
/// most referenced code files only; the rest keep instant rule-based summaries. With a
/// `budget`, model work stops when it runs out and the remainder is reported as `pending`
/// (rule-based summaries stand in until a later pass upgrades them).
pub fn summarize_all(
    store: &Store,
    project: &Project,
    cfg: &Config,
    llm: &Llm,
    no_llm: bool,
    quiet: bool,
    budget: Option<std::time::Duration>,
) -> Result<SummaryStats> {
    let use_llm = !no_llm && cfg.learn.llm_summaries && llm.has_text();
    let deadline = budget.map(|b| std::time::Instant::now() + b);
    let in_budget = || {
        deadline
            .map(|d| std::time::Instant::now() < d)
            .unwrap_or(true)
    };
    let tier = cfg.models.tier.clone();
    let known = known_names(store)?;
    let files = store.files()?;
    let mut written = 0;
    let mut via_llm = 0usize;
    let mut flagged = 0;
    let mut pending = 0usize;

    let refs = incoming_refs(store);
    let importance = |f: &FileRow| refs.get(&f.id).copied().unwrap_or(0);
    let mut ranked: Vec<&FileRow> = files
        .iter()
        .filter(|f| f.kind == "code" && f.lines <= 2000)
        .collect();
    ranked.sort_by_key(|f| (std::cmp::Reverse(importance(f)), f.path.clone()));
    let eligible: HashSet<i64> = ranked
        .iter()
        .take(if use_llm { cfg.learn.max_llm_files } else { 0 })
        .map(|f| f.id)
        .collect();

    let mut todo: Vec<&FileRow> = files
        .iter()
        .filter(|f| f.kind != "doc")
        .filter(|f| match store.summary("file", &f.path) {
            Ok(Some((_, stale, flagged, h))) => {
                stale || h != f.hash || f.stage < 3
                    // Upgrade rule-based summaries of important files once a model is available.
                    || (eligible.contains(&f.id) && !flagged && store.summary_model("file", &f.path).ok().flatten().as_deref() == Some("rules"))
            }
            _ => true,
        })
        .collect();
    // Most important first, so a limited budget is spent where it helps most.
    todo.sort_by_key(|f| {
        (
            !eligible.contains(&f.id),
            std::cmp::Reverse(importance(f)),
            f.path.clone(),
        )
    });
    let mut prog = Progress::new("summaries", todo.len(), !quiet);
    for f in todo {
        let mut text = None;
        let mut model = "rules".to_string();
        let mut flag = false;
        if eligible.contains(&f.id) && !in_budget() {
            pending += 1;
            // Keep a current rule-based summary as it is; write one if there is none yet.
            if let Ok(Some((_, false, _, h))) = store.summary("file", &f.path) {
                if h == f.hash && f.stage >= 3 {
                    prog.inc();
                    continue;
                }
            }
        } else if eligible.contains(&f.id) {
            match llm_file_summary(llm, project, store, f, &tier, None) {
                Ok(s) if looks_like_summary(&s) => {
                    let (total, unknown) = unknown_names(&s, &known);
                    if is_suspicious(total, &unknown) {
                        // Regenerate once with an explicit allow-list; flag if still dubious.
                        let mut local: HashSet<String> = store
                            .symbols_in_file(f.id)?
                            .into_iter()
                            .map(|s| s.name)
                            .collect();
                        local.extend(store.edges_of_file(f.id)?.into_iter().map(|e| e.target));
                        match llm_file_summary(llm, project, store, f, &tier, Some(&local)) {
                            Ok(s2) => {
                                let (t2, u2) = unknown_names(&s2, &known);
                                flag = is_suspicious(t2, &u2) || !looks_like_summary(&s2);
                                text = Some(s2);
                            }
                            Err(_) => {
                                flag = true;
                                text = Some(s);
                            }
                        }
                    } else {
                        text = Some(s);
                    }
                    model = llm.model_label();
                    via_llm += 1;
                }
                _ => {}
            }
        }
        let text = match text {
            Some(t) if !flag => t,
            // Flagged LLM output is replaced by the deterministic summary, keeping the flag.
            _ => heuristic_file_summary(store, f)?,
        };
        if flag {
            flagged += 1;
            model = "rules".into();
        }
        store.put_summary("file", &f.path, &text, &f.hash, flag, &model)?;
        store.set_stage(f.id, 3)?;
        written += 1;
        prog.inc();
    }
    prog.finish();

    // Modules (directories), deepest first.
    let mut dirs: BTreeMap<String, Vec<&FileRow>> = BTreeMap::new();
    for f in &files {
        let mut p = std::path::Path::new(&f.path).parent();
        while let Some(d) = p {
            let key = if d.as_os_str().is_empty() {
                ".".to_string()
            } else {
                d.to_string_lossy().replace('\\', "/")
            };
            dirs.entry(key.clone()).or_default().push(f);
            if key == "." {
                break;
            }
            p = d.parent();
        }
    }
    let mut module_llm = 0;
    let mut keys: Vec<&String> = dirs.keys().collect();
    keys.sort_by_key(|k| std::cmp::Reverse(k.matches('/').count()));
    for key in keys {
        let fs = &dirs[key];
        let mut hasher_input: Vec<String> = fs
            .iter()
            .map(|f| format!("{}:{}", f.path, f.hash))
            .collect();
        hasher_input.sort();
        let h = util::sha256_hex(hasher_input.join("\n").as_bytes());
        let module_eligible = use_llm && fs.len() >= 2;
        if let Ok(Some((_, stale, _, old))) = store.summary("module", key) {
            let rules =
                store.summary_model("module", key).ok().flatten().as_deref() == Some("rules");
            if !stale && old == h && !(module_eligible && rules) {
                continue;
            }
            if !stale && old == h && !in_budget() {
                pending += 1;
                continue;
            }
        }
        let direct: Vec<&&FileRow> = fs
            .iter()
            .filter(|f| {
                std::path::Path::new(&f.path)
                    .parent()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .map(|p| if p.is_empty() { ".".into() } else { p })
                    == Some(key.clone())
            })
            .collect();
        let mut lines = Vec::new();
        for f in direct.iter().take(25) {
            if let Some((t, ..)) = store.summary("file", &f.path)? {
                lines.push(format!("- {}: {}", f.path, util::clip(&t, 220)));
            }
        }
        let subdirs: Vec<&String> = dirs
            .keys()
            .filter(|d| {
                *d != key && d.rsplit_once('/').map(|(p, _)| p).unwrap_or(".") == key.as_str()
            })
            .collect();
        for sd in subdirs.iter().take(15) {
            if let Some((t, ..)) = store.summary("module", sd)? {
                lines.push(format!("- {}/: {}", sd, util::clip(&t, 220)));
            }
        }
        let langs = lang_stats(fs);
        let heuristic = format!(
            "{} — {} files ({}).\n{}",
            if key == "." {
                "project root".to_string()
            } else {
                format!("{}/", key)
            },
            fs.len(),
            langs,
            lines
                .iter()
                .take(12)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        let mut text = heuristic.clone();
        let mut model = "rules".to_string();
        if module_eligible && module_llm < 60 && !lines.is_empty() && !in_budget() {
            pending += 1;
        } else if module_eligible && module_llm < 60 && !lines.is_empty() {
            let prompt = format!(
                "Describe the purpose of {} in 2-3 sentences, based on these summaries of its files and subdirectories. Mention only names that appear below. No preamble.\n\n{}",
                if key == "." { "the project's root directory".to_string() } else { format!("the project directory `{}/`", key) },
                lines.join("\n")
            );
            let max = if tier == "S" { 120 } else { 200 };
            if let Ok(s) = llm.text(&prompt, max) {
                let (t, u) = unknown_names(&s, &known);
                if looks_like_summary(&s) && !is_suspicious(t, &u) {
                    text = format!("{}\n{}", s.trim(), heuristic);
                    model = llm.model_label();
                    module_llm += 1;
                }
            }
        }
        keep_or_put(store, "module", key, &text, &h, &model)?;
        written += 1;
    }

    // Architecture map (deterministic) and project overview.
    let arch_hash = util::sha256_hex(
        files
            .iter()
            .map(|f| f.hash.as_str())
            .collect::<Vec<_>>()
            .join("")
            .as_bytes(),
    );
    let arch_fresh = matches!(store.summary("architecture", "."), Ok(Some((_, false, _, ref h))) if *h == arch_hash);
    if !arch_fresh {
        let arch = architecture_map(store, project, &files)?;
        store.put_summary("architecture", ".", &arch, &arch_hash, false, "rules")?;
        written += 1;
    }
    let proj_rules = store
        .summary_model("project", ".")
        .ok()
        .flatten()
        .as_deref()
        == Some("rules");
    let proj_fresh = matches!(store.summary("project", "."), Ok(Some((_, false, _, ref h))) if *h == arch_hash)
        && !(use_llm && proj_rules && in_budget());
    if !proj_fresh {
        let readme = ["README.md", "readme.md", "README", "README.rst"]
            .iter()
            .find_map(|r| std::fs::read_to_string(project.root.join(r)).ok())
            .unwrap_or_default();
        let readme_intro = readme_intro(&readme);
        let root_mod = store
            .summary("module", ".")?
            .map(|s| s.0)
            .unwrap_or_default();
        let arch = store
            .summary("architecture", ".")?
            .map(|s| s.0)
            .unwrap_or_default();
        #[allow(unused_mut)]
        let mut text = heuristic_project_summary(store, &files, &readme_intro)?;
        let mut model = "rules".to_string();
        if use_llm && !in_budget() {
            pending += 1;
        } else if use_llm {
            let prompt = format!(
                "Write a 3-5 sentence overview of this software project for a new contributor: what it does, how it is organized, and where to start reading. Use only names that appear below. No preamble.\n\nREADME excerpt:\n{}\n\nTop-level structure:\n{}\n\nArchitecture:\n{}",
                util::clip(&readme, 2500),
                util::clip(&root_mod, 2000),
                util::clip(&arch, 1500)
            );
            if let Ok(s) = llm.text(&prompt, 300) {
                let (t, u) = unknown_names(&s, &known);
                if looks_like_summary(&s) && !is_suspicious(t, &u) {
                    text = s.trim().to_string();
                    model = llm.model_label();
                }
            }
        }
        keep_or_put(store, "project", ".", &text, &arch_hash, &model)?;
        written += 1;
    }
    Ok(SummaryStats {
        written,
        via_llm,
        flagged,
        pending,
    })
}

/// Deterministic overview: README intro, languages, and the most depended-upon files.
fn heuristic_project_summary(
    store: &Store,
    files: &[FileRow],
    readme_intro: &str,
) -> Result<String> {
    let refs: Vec<&FileRow> = files.iter().collect();
    let mut out = String::new();
    if !readme_intro.is_empty() {
        out.push_str(readme_intro);
        out.push('\n');
    }
    let code = files.iter().filter(|f| f.kind == "code").count();
    out.push_str(&format!(
        "{} files ({} code; {}).",
        files.len(),
        code,
        lang_stats(&refs)
    ));
    let mut st = store.conn.prepare(
        "SELECT f.path, count(*) c FROM edges e JOIN files f ON f.id = e.resolved_file_id
         WHERE e.kind = 'import' GROUP BY e.resolved_file_id ORDER BY c DESC LIMIT 6",
    )?;
    let core: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))?
        .flatten()
        .collect();
    drop(st);
    let mut key: Vec<String> = Vec::new();
    // Entry points first, then the most imported files.
    for f in files.iter().filter(|f| {
        let name = f.path.rsplit('/').next().unwrap_or(&f.path);
        let stem = name.split('.').next().unwrap_or(name);
        f.kind == "code"
            && matches!(
                stem,
                "main" | "index" | "app" | "server" | "cli" | "__main__" | "lib"
            )
            && f.path.matches('/').count() <= 2
    }) {
        key.push(f.path.clone());
    }
    for c in core {
        if !key.contains(&c) {
            key.push(c);
        }
    }
    if !key.is_empty() {
        out.push_str(" Key files:");
        for k in key.iter().take(7) {
            let s = store.summary("file", k)?.map(|s| s.0).unwrap_or_default();
            // Skip the "<lang> code file, N lines." prefix of deterministic summaries.
            let s = s
                .split_once(" lines. ")
                .map(|x| x.1)
                .unwrap_or(&s)
                .to_string();
            out.push_str(&format!("\n- {}: {}", k, util::clip(&s, 160)));
        }
    }
    Ok(out)
}

fn readme_intro(readme: &str) -> String {
    let mut out = Vec::new();
    for l in readme.lines() {
        let t = l.trim();
        if t.is_empty() {
            if !out.is_empty() {
                break;
            }
            continue;
        }
        if t.starts_with('#') || t.starts_with("[!") || t.starts_with("<") || t.starts_with("![") {
            continue;
        }
        out.push(t.to_string());
    }
    util::clip(&out.join(" "), 400)
}

fn lang_stats(fs: &[&FileRow]) -> String {
    let mut m: HashMap<String, usize> = HashMap::new();
    for f in fs {
        *m.entry(f.lang.clone().unwrap_or_else(|| f.kind.clone()))
            .or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = m.into_iter().collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    v.iter()
        .take(5)
        .map(|(l, n)| format!("{} {}", n, l))
        .collect::<Vec<_>>()
        .join(", ")
}

fn module_at(path: &str, depth: usize) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= depth {
        // A file directly inside the module level is its own module.
        return path.to_string();
    }
    parts[..depth].join("/")
}

/// Choose a module granularity that yields a useful number of modules (flat crates get
/// per-file modules, deep monorepos get directory modules).
fn module_depth(files: &[FileRow]) -> usize {
    for depth in 1..=4 {
        let mods: HashSet<String> = files
            .iter()
            .filter(|f| f.kind == "code")
            .map(|f| module_at(&f.path, depth))
            .collect();
        if mods.len() >= 4 {
            return depth;
        }
    }
    4
}

const GENERIC: &[&str] = &[
    "new",
    "default",
    "from",
    "into",
    "clone",
    "fmt",
    "eq",
    "hash",
    "drop",
    "next",
    "iter",
    "len",
    "get",
    "set",
    "ok",
    "err",
    "map",
    "as_str",
    "to_string",
    "unwrap",
    "push",
    "insert",
    "remove",
    "contains",
    "is_empty",
    "line",
    "text",
    "name",
    "id",
    "value",
    "init",
    "main",
    "run",
    "call",
    "apply",
    "build",
    "update",
    "close",
    "write",
    "read",
    "toString",
    "constructor",
    "__init__",
    "String",
    "Error",
];

/// Layers, entry points and data flow, computed from the graph.
pub fn architecture_map(store: &Store, project: &Project, files: &[FileRow]) -> Result<String> {
    let mut out = String::new();
    let refs: Vec<&FileRow> = files.iter().collect();
    out.push_str(&format!("Languages: {}\n", lang_stats(&refs)));

    // Entry points.
    let mut entries: Vec<String> = Vec::new();
    for f in files {
        let name = f.path.rsplit('/').next().unwrap_or(&f.path);
        let stem = name.split('.').next().unwrap_or(name);
        if f.kind == "code"
            && matches!(
                stem,
                "main" | "index" | "app" | "server" | "cli" | "__main__" | "manage" | "lib" | "mod"
            )
            && f.path.matches('/').count() <= 2
            && stem != "mod"
        {
            entries.push(f.path.clone());
        }
    }
    let mut st = store.conn.prepare("SELECT f.path, s.start_line FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.name = 'main' AND s.kind = 'function' LIMIT 10")?;
    for (p, l) in st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .flatten()
    {
        let e = format!("{}:{} main()", p, l);
        entries.retain(|x| x != &p);
        entries.push(e);
    }
    drop(st);
    if let Ok(pkg) = std::fs::read_to_string(project.root.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
            if let Some(m) = v["main"].as_str() {
                entries.push(format!("package.json main → {}", m));
            }
            if let Some(b) = v["bin"].as_object() {
                for (k, p) in b {
                    entries.push(format!(
                        "package.json bin {} → {}",
                        k,
                        p.as_str().unwrap_or("")
                    ));
                }
            }
        }
    }
    entries.sort();
    entries.dedup();
    if !entries.is_empty() {
        out.push_str(&format!(
            "Entry points: {}\n",
            entries
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // Layers: modules with size and fan-in/fan-out over resolved imports.
    let id_to_path: HashMap<i64, &str> = files.iter().map(|f| (f.id, f.path.as_str())).collect();
    let depth = module_depth(files);
    let module_of = |p: &str| module_at(p, depth);
    let mut mod_files: BTreeMap<String, usize> = BTreeMap::new();
    for f in files.iter().filter(|f| f.kind == "code") {
        *mod_files.entry(module_of(&f.path)).or_default() += 1;
    }
    let mut flows: HashMap<(String, String), usize> = HashMap::new();
    let mut st = store.conn.prepare("SELECT file_id, resolved_file_id FROM edges WHERE kind = 'import' AND resolved_file_id IS NOT NULL")?;
    for (a, b) in st
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .flatten()
    {
        if let (Some(pa), Some(pb)) = (id_to_path.get(&a), id_to_path.get(&b)) {
            let (ma, mb) = (module_of(pa), module_of(pb));
            if ma != mb {
                *flows.entry((ma, mb)).or_default() += 1;
            }
        }
    }
    drop(st);
    let mut fan_in: HashMap<String, usize> = HashMap::new();
    let mut fan_out: HashMap<String, usize> = HashMap::new();
    for ((a, b), n) in &flows {
        *fan_out.entry(a.clone()).or_default() += n;
        *fan_in.entry(b.clone()).or_default() += n;
    }
    let mut mods: Vec<(&String, &usize)> = mod_files.iter().collect();
    mods.sort_by(|a, b| b.1.cmp(a.1));
    out.push_str("Modules (files · imported-by · imports):\n");
    for (m, n) in mods.iter().take(15) {
        let role = match (
            fan_in.get(*m).copied().unwrap_or(0),
            fan_out.get(*m).copied().unwrap_or(0),
        ) {
            (i, o) if i > 0 && o == 0 => " — foundation layer",
            (0, o) if o > 0 => " — top layer / entry",
            _ => "",
        };
        let slash = if m.contains('.') { "" } else { "/" };
        out.push_str(&format!(
            "  {}{} ({} · {} · {}){}\n",
            m,
            slash,
            n,
            fan_in.get(*m).copied().unwrap_or(0),
            fan_out.get(*m).copied().unwrap_or(0),
            role
        ));
    }
    let mut fl: Vec<(&(String, String), &usize)> = flows.iter().collect();
    fl.sort_by(|a, b| b.1.cmp(a.1));
    if !fl.is_empty() {
        out.push_str("Main dependencies: ");
        out.push_str(
            &fl.iter()
                .take(10)
                .map(|((a, b), n)| format!("{} → {} ({})", a, b, n))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push('\n');
    }

    // Most referenced project symbols.
    let mut st = store.conn.prepare(
        "SELECT e.target, count(*) c FROM edges e WHERE e.kind IN ('call', 'inherit')
           AND e.target IN (SELECT name FROM symbols GROUP BY name HAVING count(*) <= 2)
         GROUP BY e.target ORDER BY c DESC LIMIT 40",
    )?;
    let hot: Vec<String> = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .flatten()
        .filter(|(n, _)| !GENERIC.contains(&n.as_str()) && n.len() > 2)
        .take(12)
        .map(|(n, c)| format!("{} ({})", n, c))
        .collect();
    if !hot.is_empty() {
        out.push_str(&format!("Most referenced symbols: {}\n", hot.join(", ")));
    }
    Ok(out)
}

/// Store a refreshed summary — unless it is only the rule-based fallback (the model was busy,
/// out of budget, or unloaded under memory pressure) and a model-written one exists: then the
/// model's text is kept, marked stale under its old hash so the next pass retries. A downgrade
/// would put a file list into every session brief until the model gets to it.
fn keep_or_put(
    store: &Store,
    scope: &str,
    key: &str,
    text: &str,
    hash: &str,
    model: &str,
) -> Result<()> {
    if model == "rules" {
        let old_model = store.summary_model(scope, key).ok().flatten();
        if old_model.is_some_and(|m| m != "rules") {
            // The old hash stays, so the next pass sees the change and retries with the model.
            return store.mark_stale(scope, key);
        }
    }
    store.put_summary(scope, key, text, hash, false, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_summary_is_not_downgraded_to_rules() {
        let d = tempfile::tempdir().unwrap();
        let project = Project::new(d.path().to_path_buf());
        let store = Store::open(&project).unwrap();
        store
            .put_summary(
                "project",
                ".",
                "Takvim is an event calendar.",
                "h1",
                false,
                "gemma3:4b",
            )
            .unwrap();
        keep_or_put(
            &store,
            "project",
            ".",
            "1048 files (935 code)",
            "h2",
            "rules",
        )
        .unwrap();
        let (text, stale, _, hash) = store.summary("project", ".").unwrap().unwrap();
        assert_eq!(text, "Takvim is an event calendar.");
        assert!(stale, "marked stale so the brief says so");
        assert_eq!(hash, "h1", "old hash kept, so the next pass retries");
        // A model result replaces it as usual.
        keep_or_put(&store, "project", ".", "New overview.", "h2", "gemma3:4b").unwrap();
        assert_eq!(
            store.summary("project", ".").unwrap().unwrap().0,
            "New overview."
        );
    }

    #[test]
    fn rejects_non_prose() {
        assert!(!looks_like_summary("```rust\nfn x() {}\n```"));
        assert!(!looks_like_summary("use a::b;\nfn main() {\n}"));
        assert!(looks_like_summary(
            "Loads the model catalog from `models.toml` and exposes tiers."
        ));
    }

    #[test]
    fn detects_hallucinated_names() {
        let known: HashSet<String> = ["Store", "open", "insert_symbol", "store"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (_, u) = unknown_names("Opens the `Store` and calls `insert_symbol`.", &known);
        assert!(u.is_empty(), "{:?}", u);
        let (t, u) = unknown_names("Uses `FancyCache` and `magic_loader` with `Store`.", &known);
        assert_eq!(u.len(), 2);
        assert!(is_suspicious(t, &u));
    }
}
