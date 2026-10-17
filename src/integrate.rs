//! Agent adapter layer. Phase 1 ships the Claude Code adapter (MCP registration, hooks and a
//! CLAUDE.md section); the trait keeps other agents (Codex, phase 2) pluggable.

use crate::paths::Project;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub struct Check {
    pub ok: bool,
    pub label: String,
    pub detail: String,
}

pub trait Agent {
    fn name(&self) -> &'static str;
    fn install(&self, project: &Project, exe: &str) -> Result<Vec<String>>;
    fn uninstall(&self, project: &Project) -> Result<Vec<String>>;
    fn check(&self, project: &Project, exe: &str) -> Vec<Check>;
}

pub fn agent(name: &str) -> Result<Box<dyn Agent>> {
    match name {
        "claude" | "claude-code" => Ok(Box::new(ClaudeCode)),
        "codex" => Ok(Box::new(Codex)),
        other => bail!(
            "unknown agent `{}` (supported: claude-code; codex is planned)",
            other
        ),
    }
}

pub const MCP_NAME: &str = "local-brain";
const MD_START: &str = "<!-- local-brain:start -->";
const MD_END: &str = "<!-- local-brain:end -->";

const CLAUDE_MD_SECTION: &str = "<!-- local-brain:start -->
## local-brain (local project brain)

This project is indexed by local-brain, which runs on this machine and costs no cloud tokens.
- For questions about the code (where is X, who calls X, how does Y work), use `brain_query` before Grep/Read.
- Before editing, fetch only the code you need with `brain_slice` (a symbol or a line range) instead of reading whole files.
- Read images and PDFs with `brain_doc`.
- Long command outputs are compressed; get the full text with `brain_raw` and the given `raw_id`.
- Record important technical decisions with `brain_memory` (action `note`).
<!-- local-brain:end -->
";

fn read_json(p: &Path) -> Result<Value> {
    match std::fs::read_to_string(p) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()))
        }
        _ => Ok(json!({})),
    }
}

fn write_json(p: &Path, v: &Value) -> Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, serde_json::to_string_pretty(v)? + "\n")?;
    Ok(())
}

fn is_ours(hook: &Value) -> bool {
    hook["command"]
        .as_str()
        .map(|c| c.contains("local-brain") && c.contains(" hook "))
        .unwrap_or(false)
}

/// The binary path at the start of one of our hook commands (`<exe> hook <event>`).
fn hook_binary(cmd: &str) -> Option<String> {
    let exe = &cmd[..cmd.rfind(" hook ")?];
    Some(
        match exe.strip_prefix('\'').and_then(|e| e.strip_suffix('\'')) {
            Some(q) => q.replace("'\\''", "'"),
            None => exe.to_string(),
        },
    )
}

/// Every binary path the Claude Code integration of `project` points at (hooks and MCP).
fn registered_binaries(project: &Project) -> Vec<String> {
    let mut out: Vec<String> = ClaudeCode::mcp_registered(project).into_iter().collect();
    if let Ok(s) = read_json(&ClaudeCode::settings_path(project)) {
        if let Some(hooks) = s["hooks"].as_object() {
            for groups in hooks.values() {
                for g in groups.as_array().into_iter().flatten() {
                    for h in g["hooks"].as_array().into_iter().flatten() {
                        if is_ours(h) {
                            if let Some(b) = h["command"].as_str().and_then(hook_binary) {
                                out.push(b);
                            }
                        }
                    }
                }
            }
        }
    }
    out.retain(|b| !b.is_empty());
    out.dedup();
    out
}

/// Re-install the Claude Code integration in registered projects whose hooks or MCP entry point
/// at a binary that no longer exists — e.g. the versioned Homebrew path removed by an upgrade.
/// Integrations that point at another existing binary (a dev build) are left alone.
pub fn repair_stale(exe: &str) -> Vec<String> {
    let mut msgs = Vec::new();
    for entry in crate::registry::load() {
        if !entry.root.is_dir() {
            continue;
        }
        let project = Project::new(entry.root.clone());
        let stale = registered_binaries(&project)
            .iter()
            .any(|b| b != exe && !Path::new(b).exists());
        if !stale {
            continue;
        }
        match ClaudeCode.install(&project, exe) {
            Ok(_) => msgs.push(format!(
                "updated the Claude Code integration in {} to {}",
                entry.root.display(),
                exe
            )),
            Err(e) => msgs.push(format!(
                "could not update the integration in {}: {:#} (run `local-brain init` there)",
                entry.root.display(),
                e
            )),
        }
    }
    msgs
}

fn hook_entry(exe: &str, event: &str, timeout: u64, matcher: Option<&str>) -> Value {
    let cmd = format!("{} hook {}", crate::util::sh_quote(exe), event);
    let mut v = json!({"hooks": [{"type": "command", "command": cmd, "timeout": timeout}]});
    if let Some(m) = matcher {
        v["matcher"] = json!(m);
    }
    v
}

pub fn ensure_gitignore(root: &Path, line: &str) -> Result<bool> {
    let p = root.join(".gitignore");
    let cur = std::fs::read_to_string(&p).unwrap_or_default();
    if cur
        .lines()
        .any(|l| l.trim() == line || l.trim() == line.trim_end_matches('/'))
    {
        return Ok(false);
    }
    let mut s = cur;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(line);
    s.push('\n');
    std::fs::write(&p, s)?;
    Ok(true)
}

pub struct ClaudeCode;

impl ClaudeCode {
    fn settings_path(project: &Project) -> PathBuf {
        project.root.join(".claude").join("settings.local.json")
    }

    fn remove_our_hooks(settings: &mut Value) {
        if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
            for (_, groups) in hooks.iter_mut() {
                if let Some(arr) = groups.as_array_mut() {
                    for g in arr.iter_mut() {
                        if let Some(hs) = g.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                            hs.retain(|h| !is_ours(h));
                        }
                    }
                    arr.retain(|g| g["hooks"].as_array().map(|a| !a.is_empty()).unwrap_or(true));
                }
            }
            hooks.retain(|_, g| g.as_array().map(|a| !a.is_empty()).unwrap_or(true));
        }
        if settings
            .get("hooks")
            .and_then(|h| h.as_object())
            .map(|o| o.is_empty())
            .unwrap_or(false)
        {
            settings.as_object_mut().map(|o| o.remove("hooks"));
        }
    }

    /// Register the MCP server with `claude mcp add --scope local` (kept out of the repo).
    /// Falls back to a project `.mcp.json` when the `claude` CLI is unavailable.
    fn register_mcp(project: &Project, exe: &str) -> Result<String> {
        let run = |args: &[&str]| {
            std::process::Command::new("claude")
                .args(args)
                .current_dir(&project.root)
                .stdin(std::process::Stdio::null())
                .output()
        };
        if std::env::var("LOCAL_BRAIN_NO_CLAUDE_CLI").is_err() {
            if let Ok(out) = run(&["mcp", "remove", "--scope", "local", MCP_NAME]) {
                let _ = out;
                if let Ok(o) = run(&["mcp", "add", "--scope", "local", MCP_NAME, "--", exe, "mcp"])
                {
                    if o.status.success() {
                        return Ok(
                            "registered MCP server `local-brain` (claude mcp add --scope local)"
                                .into(),
                        );
                    }
                }
            }
        }
        let p = project.root.join(".mcp.json");
        let mut v = read_json(&p)?;
        v["mcpServers"][MCP_NAME] = json!({"type": "stdio", "command": exe, "args": ["mcp"]});
        write_json(&p, &v)?;
        // Pre-approve the project server so Claude Code does not prompt.
        let sp = Self::settings_path(project);
        let mut s = read_json(&sp)?;
        let list = s["enabledMcpjsonServers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !list.iter().any(|x| x == MCP_NAME) {
            let mut l = list;
            l.push(json!(MCP_NAME));
            s["enabledMcpjsonServers"] = Value::Array(l);
        }
        write_json(&sp, &s)?;
        Ok("registered MCP server in .mcp.json (claude CLI not found)".into())
    }

    fn mcp_registered(project: &Project) -> Option<String> {
        if let Some(home) = dirs::home_dir() {
            if let Ok(v) = read_json(&home.join(".claude.json")) {
                let root = project.root.to_string_lossy().to_string();
                if !v["projects"][&root]["mcpServers"][MCP_NAME].is_null() {
                    return Some(
                        v["projects"][&root]["mcpServers"][MCP_NAME]["command"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                    );
                }
                if !v["mcpServers"][MCP_NAME].is_null() {
                    return Some(
                        v["mcpServers"][MCP_NAME]["command"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                    );
                }
            }
        }
        let v = read_json(&project.root.join(".mcp.json")).ok()?;
        v["mcpServers"][MCP_NAME]["command"]
            .as_str()
            .map(String::from)
    }
}

impl Agent for ClaudeCode {
    fn name(&self) -> &'static str {
        "Claude Code"
    }

    fn install(&self, project: &Project, exe: &str) -> Result<Vec<String>> {
        let mut done = Vec::new();
        done.push(Self::register_mcp(project, exe)?);

        let sp = Self::settings_path(project);
        let mut s = read_json(&sp)?;
        Self::remove_our_hooks(&mut s);
        let add = |s: &mut Value, event: &str, entry: Value| {
            if !s["hooks"].is_object() {
                s["hooks"] = json!({});
            }
            let arr = s["hooks"][event].as_array().cloned().unwrap_or_default();
            let mut arr = arr;
            arr.push(entry);
            s["hooks"][event] = Value::Array(arr);
        };
        add(
            &mut s,
            "PreToolUse",
            hook_entry(exe, "pre-tool-use", 120, Some("Read|Grep|Glob")),
        );
        add(
            &mut s,
            "PostToolUse",
            hook_entry(exe, "post-tool-use", 60, Some("Bash")),
        );
        add(
            &mut s,
            "SessionStart",
            hook_entry(exe, "session-start", 20, None),
        );
        add(&mut s, "Stop", hook_entry(exe, "stop", 10, None));
        add(
            &mut s,
            "SessionEnd",
            hook_entry(exe, "session-end", 15, None),
        );
        write_json(&sp, &s)?;
        done.push(format!("installed hooks in {}", project.rel(&sp)));
        if ensure_gitignore(&project.root, ".claude/settings.local.json")? {
            done.push("added .claude/settings.local.json to .gitignore".into());
        }

        let md = project.root.join("CLAUDE.md");
        let cur = std::fs::read_to_string(&md).unwrap_or_default();
        let new = match (cur.find(MD_START), cur.find(MD_END)) {
            (Some(a), Some(b)) if b > a => format!(
                "{}{}{}",
                &cur[..a],
                CLAUDE_MD_SECTION.trim_end(),
                &cur[b + MD_END.len()..]
            ),
            _ => {
                let mut c = cur.clone();
                if !c.is_empty() && !c.ends_with("\n\n") {
                    c.push_str(if c.ends_with('\n') { "\n" } else { "\n\n" });
                }
                c.push_str(CLAUDE_MD_SECTION);
                c
            }
        };
        if new != cur {
            std::fs::write(&md, new)?;
            done.push("added the local-brain section to CLAUDE.md".into());
        }
        Ok(done)
    }

    fn uninstall(&self, project: &Project) -> Result<Vec<String>> {
        let mut done = Vec::new();
        let _ = std::process::Command::new("claude")
            .args(["mcp", "remove", "--scope", "local", MCP_NAME])
            .current_dir(&project.root)
            .stdin(std::process::Stdio::null())
            .output();
        let mp = project.root.join(".mcp.json");
        if mp.exists() {
            let mut v = read_json(&mp)?;
            if v["mcpServers"].get(MCP_NAME).is_some() {
                v["mcpServers"].as_object_mut().map(|o| o.remove(MCP_NAME));
                if v["mcpServers"]
                    .as_object()
                    .map(|o| o.is_empty())
                    .unwrap_or(false)
                    && v.as_object().map(|o| o.len() == 1).unwrap_or(false)
                {
                    std::fs::remove_file(&mp)?;
                } else {
                    write_json(&mp, &v)?;
                }
            }
        }
        done.push("removed MCP registration".into());
        let sp = Self::settings_path(project);
        if sp.exists() {
            let mut s = read_json(&sp)?;
            Self::remove_our_hooks(&mut s);
            if let Some(l) = s
                .get_mut("enabledMcpjsonServers")
                .and_then(|l| l.as_array_mut())
            {
                l.retain(|x| x != MCP_NAME);
            }
            if s.get("enabledMcpjsonServers")
                .and_then(|l| l.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(false)
            {
                s.as_object_mut().map(|o| o.remove("enabledMcpjsonServers"));
            }
            write_json(&sp, &s)?;
            done.push("removed hooks".into());
        }
        let md = project.root.join("CLAUDE.md");
        if let Ok(cur) = std::fs::read_to_string(&md) {
            if let (Some(a), Some(b)) = (cur.find(MD_START), cur.find(MD_END)) {
                let new = format!(
                    "{}{}",
                    cur[..a].trim_end_matches('\n'),
                    &cur[b + MD_END.len()..]
                );
                let new = if new.trim().is_empty() {
                    String::new()
                } else {
                    format!("{}\n", new.trim_end())
                };
                if new.is_empty() {
                    std::fs::remove_file(&md)?;
                } else {
                    std::fs::write(&md, new)?;
                }
                done.push("removed the CLAUDE.md section".into());
            }
        }
        Ok(done)
    }

    fn check(&self, project: &Project, exe: &str) -> Vec<Check> {
        let mut out = Vec::new();
        match Self::mcp_registered(project) {
            Some(cmd) => out.push(Check {
                ok: Path::new(&cmd).exists() || cmd == "local-brain",
                label: "MCP server registered".into(),
                detail: if Path::new(&cmd).exists() {
                    cmd
                } else {
                    format!("{} (binary missing — re-run `local-brain init`)", cmd)
                },
            }),
            None => out.push(Check {
                ok: false,
                label: "MCP server registered".into(),
                detail: "not found — run `local-brain init`".into(),
            }),
        }
        let s = read_json(&Self::settings_path(project)).unwrap_or(json!({}));
        let mut events = Vec::new();
        let mut stale_exe = false;
        if let Some(h) = s["hooks"].as_object() {
            for (ev, groups) in h {
                for g in groups.as_array().cloned().unwrap_or_default() {
                    for hk in g["hooks"].as_array().cloned().unwrap_or_default() {
                        if is_ours(&hk) {
                            events.push(ev.clone());
                            if !hk["command"].as_str().unwrap_or("").contains(exe) {
                                stale_exe = true;
                            }
                        }
                    }
                }
            }
        }
        events.sort();
        events.dedup();
        out.push(Check {
            ok: events.len() >= 5 && !stale_exe,
            label: "Claude Code hooks".into(),
            detail: if events.is_empty() {
                "none installed — run `local-brain init`".into()
            } else if stale_exe {
                format!(
                    "{} (point to another binary — re-run `local-brain init`)",
                    events.join(", ")
                )
            } else {
                events.join(", ")
            },
        });
        let md = std::fs::read_to_string(project.root.join("CLAUDE.md")).unwrap_or_default();
        out.push(Check {
            ok: md.contains(MD_START),
            label: "CLAUDE.md section".into(),
            detail: if md.contains(MD_START) {
                "present".into()
            } else {
                "missing".into()
            },
        });
        out
    }
}

/// Codex adapter: planned for phase 2. The trait is in place so it can be added without
/// touching the core.
pub struct Codex;

impl Agent for Codex {
    fn name(&self) -> &'static str {
        "Codex"
    }
    fn install(&self, _: &Project, _: &str) -> Result<Vec<String>> {
        bail!("the Codex adapter is planned for phase 2; for now use `local-brain mcp` as a generic MCP server")
    }
    fn uninstall(&self, _: &Project) -> Result<Vec<String>> {
        Ok(vec![])
    }
    fn check(&self, _: &Project, _: &str) -> Vec<Check> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_binary_parsing() {
        assert_eq!(
            hook_binary("/opt/homebrew/bin/local-brain hook stop").as_deref(),
            Some("/opt/homebrew/bin/local-brain")
        );
        assert_eq!(
            hook_binary("'/Users/a b/bin/local-brain' hook pre-tool-use").as_deref(),
            Some("/Users/a b/bin/local-brain")
        );
        assert_eq!(hook_binary("echo user"), None);
    }

    #[test]
    fn install_and_uninstall_roundtrip() {
        std::env::set_var("LOCAL_BRAIN_NO_CLAUDE_CLI", "1");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("CLAUDE.md"), "# Mine\n\nKeep me.\n").unwrap();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(root.join(".claude/settings.local.json"), r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo user"}]}]}}"#).unwrap();
        let p = Project::new(root.clone());
        let cc = ClaudeCode;
        cc.install(&p, "/usr/local/bin/local-brain").unwrap();
        cc.install(&p, "/usr/local/bin/local-brain").unwrap(); // idempotent
        let s: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(s["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        let md = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert_eq!(md.matches(MD_START).count(), 1);
        assert!(md.contains("Keep me."));
        cc.uninstall(&p).unwrap();
        let s: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert!(s["hooks"].get("SessionStart").is_none());
        let md = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(!md.contains(MD_START) && md.contains("Keep me."));
        assert!(!root.join(".mcp.json").exists());
    }
}
