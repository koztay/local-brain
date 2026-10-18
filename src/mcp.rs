//! MCP server over stdio (newline-delimited JSON-RPC 2.0). Five tools with short schemas,
//! because tool definitions cost tokens in every request.

use crate::config::Config;
use crate::llm::Llm;
use crate::paths::{self, Project};
use crate::store::Store;
use crate::util;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL: &str = "2025-06-18";

fn tools() -> Value {
    let mut tools = tool_defs();
    for t in tools.as_array_mut().into_iter().flatten() {
        // Claude Code defers MCP tools behind a ToolSearch turn unless told otherwise; five
        // small tools are cheaper in the cached prefix than a search before every use.
        t["_meta"] = json!({"anthropic/alwaysLoad": true});
    }
    tools
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "brain_query",
            "description": "Ask about this codebase (where is X, who calls X, how does Y work, architecture). Returns a short answer with file:line pointers, never whole files. Use before Grep/Read.",
            "inputSchema": {"type": "object", "properties": {
                "question": {"type": "string"},
                "scope": {"type": "string", "description": "optional path prefix"}
            }, "required": ["question"]}
        },
        {
            "name": "brain_slice",
            "description": "Fetch only the code you need: one symbol's source, or a line range. Use before editing instead of reading whole files.",
            "inputSchema": {"type": "object", "properties": {
                "path": {"type": "string"},
                "symbol": {"type": "string", "description": "e.g. parse or Parser.parse"},
                "lines": {"type": "string", "description": "e.g. 120-180"}
            }, "required": ["path"]}
        },
        {
            "name": "brain_doc",
            "description": "Read an image or PDF locally (no image tokens). Optionally ask a question about it.",
            "inputSchema": {"type": "object", "properties": {
                "path": {"type": "string"},
                "question": {"type": "string"}
            }, "required": ["path"]}
        },
        {
            "name": "brain_raw",
            "description": "Get part of a compressed output by its raw_id.",
            "inputSchema": {"type": "object", "properties": {
                "raw_id": {"type": "string"},
                "grep": {"type": "string", "description": "regex"},
                "range": {"type": "string", "description": "e.g. 100-200"}
            }, "required": ["raw_id"]}
        },
        {
            "name": "brain_memory",
            "description": "Project memory. read: brief + decisions + open tasks. note: record a decision/todo/note (text, why). done: close an item by id.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["read", "note", "done"]},
                "kind": {"type": "string", "enum": ["decision", "todo", "note"]},
                "text": {"type": "string"},
                "why": {"type": "string"},
                "id": {"type": "integer"}
            }, "required": ["action"]}
        }
    ])
}

pub struct Server {
    project: Option<Project>,
    cfg: Config,
    store: Option<Store>,
    llm: Llm,
    session: Option<String>,
}

fn s<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

impl Server {
    pub fn new(project: Option<Project>) -> Server {
        let cfg = Config::load(project.as_ref());
        let store = project.as_ref().and_then(|p| Store::open(p).ok());
        let llm = Llm::new(&cfg);
        Server {
            project,
            cfg,
            store,
            llm,
            session: std::env::var("CLAUDE_SESSION_ID").ok(),
        }
    }

    fn ctx(&self) -> Result<(&Project, &Store)> {
        match (&self.project, &self.store) {
            (Some(p), Some(s)) => Ok((p, s)),
            _ => bail!("local-brain is not initialized for this project. Run `local-brain init && local-brain learn` in the project root."),
        }
    }

    fn metric(&self, kind: &str, baseline: i64, actual: i64) {
        if let Some(st) = &self.store {
            let _ = st.record_metric("mcp", kind, baseline, actual, self.session.as_deref());
        }
    }

    pub fn call_tool(&self, name: &str, args: &Value) -> Result<String> {
        match name {
            "brain_query" => {
                let (p, st) = self.ctx()?;
                let q = s(args, "question").ok_or_else(|| anyhow!("`question` is required"))?;
                let a =
                    crate::query::query(st, p, &self.cfg, Some(&self.llm), q, s(args, "scope"))?;
                // Baseline: what exploring without the brain would cost — reading the pointed
                // files (capped like the Read tool) plus a search round.
                let mut baseline = 400;
                for f in a.files.iter().take(6) {
                    if let Ok(Some(row)) = st.file(f) {
                        baseline += util::tokens_len(row.size as u64).min(25_000);
                    }
                }
                self.metric("brain_query", baseline, util::tokens(&a.text));
                Ok(a.text)
            }
            "brain_slice" => {
                let (p, st) = self.ctx()?;
                let path = s(args, "path").ok_or_else(|| anyhow!("`path` is required"))?;
                let sl = crate::slice::slice(st, p, path, s(args, "symbol"), s(args, "lines"))?;
                self.metric(
                    "brain_slice",
                    sl.full_tokens.min(25_000),
                    util::tokens(&sl.text),
                );
                Ok(sl.text)
            }
            "brain_doc" => {
                let (p, st) = self.ctx()?;
                let path = s(args, "path").ok_or_else(|| anyhow!("`path` is required"))?;
                let d =
                    crate::docs::read_doc(st, p, &self.cfg, &self.llm, path, s(args, "question"))?;
                self.metric("brain_doc", d.baseline_tokens, util::tokens(&d.text));
                Ok(d.text)
            }
            "brain_raw" => {
                let id = s(args, "raw_id").ok_or_else(|| anyhow!("`raw_id` is required"))?;
                let text = crate::raw::load(self.project.as_ref(), id)?;
                let out = crate::raw::select(&text, s(args, "grep"), s(args, "range"), 2, 300)?;
                let out = util::clip_tokens(&out, 8000).0;
                self.metric("brain_raw", util::tokens(&text), util::tokens(&out));
                Ok(out)
            }
            "brain_memory" => {
                let (p, st) = self.ctx()?;
                let _ = crate::memory::sync_from_md(st, p);
                match s(args, "action").unwrap_or("read") {
                    "read" => {
                        let mut out = crate::memory::brief(st, p, 700)?;
                        let notes = crate::memory::list(st, Some("note"), 8)?;
                        if !notes.is_empty() {
                            out.push_str("Notes:\n");
                            for n in notes {
                                out.push_str(&format!(
                                    "- [{}] {}\n",
                                    n.id,
                                    util::clip(&n.text, 200)
                                ));
                            }
                        }
                        Ok(out)
                    }
                    "note" => {
                        let text = s(args, "text")
                            .ok_or_else(|| anyhow!("`text` is required for note"))?;
                        let kind = s(args, "kind").unwrap_or("decision");
                        let id = crate::memory::add(
                            st,
                            kind,
                            text,
                            s(args, "why"),
                            None,
                            self.session.as_deref(),
                            "agent",
                        )?;
                        crate::memory::export_md(st, p)?;
                        Ok(format!("Recorded {} #{}.", kind, id))
                    }
                    "done" => {
                        let id = args
                            .get("id")
                            .and_then(|v| v.as_i64())
                            .ok_or_else(|| anyhow!("`id` is required for done"))?;
                        let ok = crate::memory::archive(st, id)?;
                        crate::memory::export_md(st, p)?;
                        Ok(if ok {
                            format!("Closed #{}.", id)
                        } else {
                            format!("No memory item #{}.", id)
                        })
                    }
                    other => bail!("unknown action `{}` (use read, note or done)", other),
                }
            }
            other => bail!("unknown tool `{}`", other),
        }
    }

    fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let id = id?; // notifications get no response
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                let v = params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or(PROTOCOL);
                Ok(json!({
                    "protocolVersion": v,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "local-brain", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Local project brain. Prefer brain_query over Grep/Read for questions about the code, brain_slice to fetch code before editing, brain_doc for images/PDFs."
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "resources/list" => Ok(json!({"resources": []})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "prompts/list" => Ok(json!({"prompts": []})),
            "tools/call" => {
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match self.call_tool(name, &args) {
                    Ok(text) => {
                        Ok(json!({"content": [{"type": "text", "text": text}], "isError": false}))
                    }
                    Err(e) => Ok(
                        json!({"content": [{"type": "text", "text": format!("local-brain: {}", e)}], "isError": true}),
                    ),
                }
            }
            _ => Err((-32601, format!("method not found: {}", method))),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, m)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": m}})
            }
        })
    }
}

pub fn serve(explicit: Option<&std::path::Path>) -> Result<()> {
    let project = paths::resolve_project(explicit).ok();
    let mut server = Server::new(project);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {}", e)}});
                writeln!(stdout, "{}", err)?;
                stdout.flush()?;
                continue;
            }
        };
        let responses: Vec<Value> = match &msg {
            Value::Array(batch) => batch.iter().filter_map(|m| server.handle(m)).collect(),
            m => server.handle(m).into_iter().collect(),
        };
        for r in responses {
            writeln!(stdout, "{}", r)?;
            stdout.flush()?;
        }
    }
    Ok(())
}
