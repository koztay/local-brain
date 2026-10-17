//! Project memory: decisions (what and why), changes, open tasks and session summaries.
//! Stored in SQLite and mirrored to a human-editable `.local-brain/memory.md`; edits made in
//! that file take precedence.

use crate::config::Config;
use crate::llm::Llm;
use crate::paths::Project;
use crate::store::Store;
use crate::util;
use anyhow::Result;
use rusqlite::params;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Mem {
    pub id: i64,
    pub kind: String,
    pub text: String,
    pub why: Option<String>,
    pub files: Option<String>,
    pub source: String,
    pub created: i64,
}

const SECTIONS: &[(&str, &str)] = &[
    ("decision", "Decisions"),
    ("todo", "Open tasks"),
    ("note", "Notes"),
    ("change", "Recent changes"),
    ("session", "Sessions"),
];

#[allow(clippy::too_many_arguments)]
pub fn add(
    store: &Store,
    kind: &str,
    text: &str,
    why: Option<&str>,
    files: Option<&str>,
    session: Option<&str>,
    source: &str,
) -> Result<i64> {
    let kind = if SECTIONS.iter().any(|(k, _)| *k == kind) {
        kind
    } else {
        "note"
    };
    store.conn.execute(
        "INSERT INTO memory(kind, text, why, files, session_id, source, created) VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![kind, text.trim(), why.map(str::trim).filter(|s| !s.is_empty()), files, session, source, util::now()],
    )?;
    Ok(store.conn.last_insert_rowid())
}

pub fn list(store: &Store, kind: Option<&str>, limit: i64) -> Result<Vec<Mem>> {
    let mut st = store.conn.prepare(
        "SELECT id, kind, text, why, files, source, created FROM memory
         WHERE archived = 0 AND (?1 IS NULL OR kind = ?1) ORDER BY created DESC, id DESC LIMIT ?2",
    )?;
    let rows = st.query_map(params![kind, limit], |r| {
        Ok(Mem {
            id: r.get(0)?,
            kind: r.get(1)?,
            text: r.get(2)?,
            why: r.get(3)?,
            files: r.get(4)?,
            source: r.get(5)?,
            created: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn archive(store: &Store, id: i64) -> Result<bool> {
    Ok(store
        .conn
        .execute("UPDATE memory SET archived = 1 WHERE id = ?", [id])?
        > 0)
}

fn line_for(m: &Mem) -> String {
    let mut s = format!("- {}", m.text.replace('\n', " "));
    if let Some(w) = &m.why {
        s.push_str(&format!(" — why: {}", w.replace('\n', " ")));
    }
    if let Some(f) = &m.files {
        if !f.is_empty() {
            s.push_str(&format!(" (files: {})", f));
        }
    }
    s.push_str(&format!(" <!--id:{}-->", m.id));
    s
}

pub fn export_md(store: &Store, project: &Project) -> Result<()> {
    let mut out = String::from(
        "# Project memory\n\n<!-- Maintained by local-brain. Edit freely: your edits win. One entry per line, \
         \"- text — why: reason\". Delete a line to archive it; add a line (without an id) to create one. -->\n",
    );
    for (kind, title) in SECTIONS {
        let limit = if *kind == "session" || *kind == "change" {
            15
        } else {
            200
        };
        let items = list(store, Some(kind), limit)?;
        out.push_str(&format!("\n## {}\n", title));
        for m in items {
            out.push_str(&line_for(&m));
            out.push('\n');
        }
    }
    std::fs::write(project.memory_md(), out)?;
    let mtime = std::fs::metadata(project.memory_md())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    store.set_meta("memory_md_mtime", &mtime.to_string())?;
    Ok(())
}

/// Import edits the user made to memory.md (if it changed since the last export).
pub fn sync_from_md(store: &Store, project: &Project) -> Result<bool> {
    let p = project.memory_md();
    let Ok(md) = std::fs::metadata(&p) else {
        return Ok(false);
    };
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let last: i64 = store
        .meta("memory_md_mtime")?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if mtime <= last {
        return Ok(false);
    }
    let text = std::fs::read_to_string(&p)?;
    let mut kind = "note";
    let mut seen = std::collections::HashSet::new();
    let shown_kinds: std::collections::HashSet<&str> = SECTIONS.iter().map(|(k, _)| *k).collect();
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            kind = SECTIONS
                .iter()
                .find(|(_, t)| t.eq_ignore_ascii_case(h.trim()))
                .map(|(k, _)| *k)
                .unwrap_or("note");
            continue;
        }
        let Some(item) = line.strip_prefix("- ") else {
            continue;
        };
        let (body, id) = match item.rfind("<!--id:") {
            Some(i) => {
                let id: Option<i64> = item[i + 7..].trim_end_matches("-->").trim().parse().ok();
                (item[..i].trim(), id)
            }
            None => (item.trim(), None),
        };
        let (body, files) = match body.rfind(" (files: ") {
            Some(i) if body.ends_with(')') => (
                body[..i].trim(),
                Some(body[i + 9..body.len() - 1].to_string()),
            ),
            _ => (body, None),
        };
        let (what, why) = match body.split_once(" — why: ") {
            Some((a, b)) => (a.trim(), Some(b.trim())),
            None => (body, None),
        };
        if what.is_empty() {
            continue;
        }
        match id {
            Some(id) => {
                store.conn.execute(
                    "UPDATE memory SET text = ?, why = ?, kind = ?, files = coalesce(?, files), source = CASE WHEN text != ? OR coalesce(why,'') != coalesce(?,'') THEN 'user' ELSE source END WHERE id = ?",
                    params![what, why, kind, files, what, why, id],
                )?;
                seen.insert(id);
            }
            None => {
                let id = add(store, kind, what, why, files.as_deref(), None, "user")?;
                seen.insert(id);
            }
        }
    }
    // Entries removed from the file are archived. Only kinds that are fully shown count;
    // sessions/changes are truncated in the file, so only those within the shown window.
    for (k, _) in SECTIONS {
        if !shown_kinds.contains(k) {
            continue;
        }
        let limit = if *k == "session" || *k == "change" {
            15
        } else {
            200
        };
        for m in list(store, Some(k), limit)? {
            if !seen.contains(&m.id) && m.created < mtime {
                archive(store, m.id)?;
            }
        }
    }
    export_md(store, project)?;
    Ok(true)
}

pub fn archive_orphans(store: &Store, project: &Project) -> Result<usize> {
    let mut n = 0;
    let rows: Vec<(i64, String)> = {
        let mut st = store.conn.prepare(
            "SELECT id, files FROM memory WHERE archived = 0 AND files IS NOT NULL AND files != ''",
        )?;
        let r = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .flatten()
            .collect();
        r
    };
    for (id, files) in rows {
        let all_gone = files
            .split(',')
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .all(|f| !project.abs(f).exists());
        if all_gone {
            archive(store, id)?;
            n += 1;
        }
    }
    if n > 0 {
        export_md(store, project)?;
    }
    Ok(n)
}

/// The session-start briefing (bounded by `budget` tokens).
pub fn brief(store: &Store, project: &Project, budget: i64) -> Result<String> {
    let _ = sync_from_md(store, project);
    let mut out = String::from("[local-brain] Project brief\n");
    if let Some((s, stale, ..)) = store.summary("project", ".")? {
        out.push_str(&format!(
            "Overview{}: {}\n",
            if stale { " (may be stale)" } else { "" },
            util::clip(&s.replace('\n', " "), 500)
        ));
    }
    if let Some((a, ..)) = store.summary("architecture", ".")? {
        let lines: Vec<&str> = a.lines().filter(|l| l.starts_with("Entry")).collect();
        for l in lines {
            out.push_str(&util::clip(l, 300));
            out.push('\n');
        }
    }
    let decisions = list(store, Some("decision"), 6)?;
    if !decisions.is_empty() {
        out.push_str("Recent decisions:\n");
        for d in decisions {
            out.push_str(&format!(
                "- {}{}\n",
                util::clip(&d.text, 160),
                d.why
                    .map(|w| format!(" (why: {})", util::clip(&w, 100)))
                    .unwrap_or_default()
            ));
        }
    }
    let todos = list(store, Some("todo"), 6)?;
    if !todos.is_empty() {
        out.push_str("Open tasks:\n");
        for t in todos {
            out.push_str(&format!("- [{}] {}\n", t.id, util::clip(&t.text, 160)));
        }
    }
    if let Some(s) = list(store, Some("session"), 1)?.into_iter().next() {
        out.push_str(&format!(
            "Last session ({}): {}\n",
            util::fmt_ts(s.created),
            util::clip(&s.text, 300)
        ));
    }
    out.push_str("Use brain_query for questions about this codebase (answers with file:line pointers), brain_slice to fetch only the code you will edit, brain_doc for images/PDFs, brain_memory to record decisions.\n");
    Ok(util::clip_tokens(&out, budget).0)
}

// ---- session ingestion -------------------------------------------------------------------

struct Transcript {
    prompts: Vec<String>,
    replies: Vec<String>,
    edited: Vec<String>,
    commands: Vec<String>,
}

fn parse_transcript(text: &str) -> Transcript {
    let mut t = Transcript {
        prompts: vec![],
        replies: vec![],
        edited: vec![],
        commands: vec![],
    };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ty = v["type"].as_str().unwrap_or("");
        if v["isMeta"].as_bool() == Some(true) || v["isSidechain"].as_bool() == Some(true) {
            continue;
        }
        let content = &v["message"]["content"];
        let blocks: Vec<Value> = match content {
            Value::String(s) => vec![serde_json::json!({"type": "text", "text": s})],
            Value::Array(a) => a.clone(),
            _ => continue,
        };
        for b in blocks {
            match (ty, b["type"].as_str().unwrap_or("")) {
                ("user", "text") => {
                    let s = b["text"].as_str().unwrap_or("").trim().to_string();
                    if !s.is_empty()
                        && !s.starts_with('<')
                        && !s.starts_with("[Request interrupted")
                        && !s.starts_with("Caveat:")
                    {
                        t.prompts.push(s);
                    }
                }
                ("assistant", "text") => {
                    let s = b["text"].as_str().unwrap_or("").trim().to_string();
                    if !s.is_empty() {
                        t.replies.push(s);
                    }
                }
                ("assistant", "tool_use") => {
                    let name = b["name"].as_str().unwrap_or("");
                    let input = &b["input"];
                    match name {
                        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                            if let Some(p) = input["file_path"]
                                .as_str()
                                .or(input["notebook_path"].as_str())
                            {
                                if !t.edited.contains(&p.to_string()) {
                                    t.edited.push(p.to_string());
                                }
                            }
                        }
                        "Bash" => {
                            if let Some(c) = input["command"].as_str() {
                                t.commands.push(util::clip(c, 120));
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
    t
}

type Extracted = (String, Vec<(String, Option<String>)>, Vec<String>);

fn heuristic_extract(t: &Transcript) -> Extracted {
    let summary = match t.prompts.first() {
        Some(p) => format!("Asked: {}", util::clip(&p.replace('\n', " "), 200)),
        None => "Session".to_string(),
    };
    let dec_re = regex::Regex::new(r"(?i)\b(decided|we('ll| will) use|i('ll| will) use|chose|instead of|going with|switched to|opted for)\b").unwrap();
    let todo_re = regex::Regex::new(r"(?i)^\s*(?:[-*]\s*)?(?:todo|next steps?|follow[- ]?ups?|remaining|not yet done|still need)\b[:\s]*(.*)").unwrap();
    let mut decisions = Vec::new();
    let mut todos = Vec::new();
    for r in &t.replies {
        for sentence in r
            .split(['\n', '.'])
            .map(str::trim)
            .filter(|s| s.len() > 15 && s.len() < 300)
        {
            if dec_re.is_match(sentence) && decisions.len() < 5 {
                decisions.push((sentence.to_string(), None));
            }
        }
    }
    if let Some(last) = t.replies.last() {
        for l in last.lines() {
            if let Some(c) = todo_re.captures(l) {
                let item = c.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                if item.len() > 5 && todos.len() < 5 {
                    todos.push(item.to_string());
                }
            }
        }
    }
    (summary, decisions, todos)
}

/// Summarize a finished session transcript into memory. Replaces earlier entries of the
/// same session (Stop may fire several times before SessionEnd).
pub fn ingest_transcript(
    project: &Project,
    cfg: &Config,
    transcript_path: &str,
    session: &str,
) -> Result<usize> {
    let store = Store::open(project)?;
    let text = std::fs::read_to_string(transcript_path)?;
    let t = parse_transcript(&text);
    if t.prompts.is_empty() && t.edited.is_empty() {
        return Ok(0);
    }
    let edited: Vec<String> = t
        .edited
        .iter()
        .map(|p| project.rel(std::path::Path::new(p)))
        .collect();
    let llm = Llm::new(cfg);
    let mut summary = String::new();
    let mut decisions: Vec<(String, Option<String>)> = Vec::new();
    let mut todos: Vec<String> = Vec::new();
    if llm.has_text() {
        let mut convo = String::new();
        for p in t.prompts.iter().take(20) {
            convo.push_str(&format!("USER: {}\n", util::clip(p, 800)));
        }
        for r in t.replies.iter().rev().take(12).rev() {
            convo.push_str(&format!("ASSISTANT: {}\n", util::clip(r, 1200)));
        }
        let (convo, _) = util::clip_tokens(&convo, 5000);
        let prompt = format!(
            "From this coding session, extract JSON with keys: \"summary\" (one sentence: what was done), \
             \"decisions\" (array of {{\"what\": ..., \"why\": ...}} for technical decisions that were actually made), \
             \"todos\" (array of strings: work explicitly left open). Only include things stated in the session. \
             Output only JSON.\n\nFiles edited: {}\n\n{}",
            edited.join(", "),
            convo
        );
        if let Ok(out) = llm.text(&prompt, 600) {
            if let (Some(a), Some(b)) = (out.find('{'), out.rfind('}')) {
                if let Ok(v) = serde_json::from_str::<Value>(&out[a..=b]) {
                    summary = v["summary"].as_str().unwrap_or("").to_string();
                    for d in v["decisions"].as_array().cloned().unwrap_or_default() {
                        if let Some(w) = d["what"].as_str() {
                            decisions.push((w.to_string(), d["why"].as_str().map(String::from)));
                        }
                    }
                    for td in v["todos"].as_array().cloned().unwrap_or_default() {
                        if let Some(s) = td.as_str() {
                            todos.push(s.to_string());
                        }
                    }
                }
            }
        }
    }
    // Rule-based extraction fills whatever the model left empty.
    let (s, d, td) = heuristic_extract(&t);
    if summary.is_empty() {
        summary = s;
    }
    if decisions.is_empty() {
        decisions = d;
    }
    if todos.is_empty() {
        todos = td;
    }
    store.conn.execute(
        "DELETE FROM memory WHERE session_id = ? AND source = 'auto'",
        [session],
    )?;
    let files = if edited.is_empty() {
        None
    } else {
        Some(edited.join(", "))
    };
    let mut n = 0;
    add(
        &store,
        "session",
        &summary,
        None,
        files.as_deref(),
        Some(session),
        "auto",
    )?;
    n += 1;
    if let Some(f) = &files {
        add(
            &store,
            "change",
            &format!("Edited {}", util::clip(f, 400)),
            None,
            Some(f),
            Some(session),
            "auto",
        )?;
        n += 1;
    }
    for (what, why) in decisions.iter().take(8) {
        add(
            &store,
            "decision",
            what,
            why.as_deref(),
            files.as_deref(),
            Some(session),
            "auto",
        )?;
        n += 1;
    }
    for td in todos.iter().take(8) {
        add(&store, "todo", td, None, None, Some(session), "auto")?;
        n += 1;
    }
    export_md(&store, project)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transcript_lines() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":"Add a cache to the loader"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I decided to use an LRU cache instead of a HashMap because memory is bounded."},{"type":"tool_use","name":"Edit","input":{"file_path":"/p/src/loader.rs"}}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done.\nNext steps: add eviction metrics"}]}}"#;
        let t = parse_transcript(jsonl);
        assert_eq!(t.prompts.len(), 1);
        assert_eq!(t.edited, vec!["/p/src/loader.rs"]);
        let (s, d, td) = heuristic_extract(&t);
        assert!(s.contains("cache"));
        assert_eq!(d.len(), 1);
        assert_eq!(td, vec!["add eviction metrics"]);
    }
}
