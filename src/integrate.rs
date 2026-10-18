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

This project is indexed by local-brain (a local code index, no cloud tokens). Use it for every task:
- Task prompts come with a `local-brain` note: the named symbols' definitions and call sites
  (file:lines, often the code itself) or the files where the work belongs. Trust it; go there
  directly instead of searching.
- Read code in ranges (Read `offset`/`limit`, or `brain_slice` for one symbol), whole files only
  to rewrite them. Ask `brain_query` for what the note does not answer (how does X work).
- Images/PDFs: use Read; local-brain returns a local description. Long command outputs are
  compressed (`brain_raw` + `raw_id` for the full text). Record decisions with `brain_memory`.
<!-- local-brain:end -->
";

/// Add or refresh the local-brain section in `<root>/CLAUDE.md`. Returns true when the file
/// changed.
pub fn ensure_claude_md_section(root: &Path) -> Result<bool> {
    let md = root.join("CLAUDE.md");
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
    if new == cur {
        return Ok(false);
    }
    std::fs::write(&md, new).with_context(|| format!("writing {}", md.display()))?;
    Ok(true)
}

/// The hooks this version installs, as a Claude Code settings object (`{"hooks": {...}}`) —
/// what `install` writes, and what `bench` gives its local-brain sessions.
pub fn hooks_settings(exe: &str) -> Value {
    let mut hooks = serde_json::Map::new();
    for (event, entry) in our_hooks(exe) {
        if let Some(a) = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(vec![]))
            .as_array_mut()
        {
            a.push(entry);
        }
    }
    json!({ "hooks": hooks })
}

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
        let bins = registered_binaries(&project);
        if bins.is_empty() {
            continue; // set up with --no-agent
        }
        if !needs_refresh(
            &bins,
            exe,
            |b| Path::new(b).exists(),
            || ClaudeCode::is_current(&project),
        ) {
            continue;
        }
        match ClaudeCode.install(&project, exe) {
            Ok(_) => msgs.push(format!(
                "updated the Claude Code integration in {}",
                entry.root.display()
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

/// Whether `exe` should re-install a project's integration: when a binary it points at is gone
/// (stale), or when it points at `exe` itself and is outdated. A newer local-brain may add hooks
/// or change the CLAUDE.md section — but only the project's own binary refreshes it, otherwise
/// running a dev build (or a second install) anywhere would silently re-point the user's
/// sessions at it.
fn needs_refresh(
    bins: &[String],
    exe: &str,
    exists: impl Fn(&str) -> bool,
    is_current: impl FnOnce() -> bool,
) -> bool {
    let stale = bins.iter().any(|b| b != exe && !exists(b));
    let points_here = bins.iter().all(|b| b == exe || b == "local-brain");
    stale || (points_here && !is_current())
}

/// Every hook this version installs, as (event, settings entry).
fn our_hooks(exe: &str) -> Vec<(&'static str, Value)> {
    vec![
        (
            "PreToolUse",
            hook_entry(exe, "pre-tool-use", 60, Some("Grep|Glob")),
        ),
        // Read is handled after the permission check (images/PDFs, large files).
        (
            "PostToolUse",
            hook_entry(exe, "post-tool-use", 120, Some("Bash|Read")),
        ),
        ("SessionStart", hook_entry(exe, "session-start", 20, None)),
        (
            "UserPromptSubmit",
            hook_entry(exe, "user-prompt-submit", 15, None),
        ),
        ("Stop", hook_entry(exe, "stop", 10, None)),
        ("SessionEnd", hook_entry(exe, "session-end", 15, None)),
    ]
}

/// A hook entry with the binary path stripped from its command, for comparisons.
fn normalize_entry(entry: &Value) -> Value {
    let mut e = entry.clone();
    if let Some(hs) = e["hooks"].as_array_mut() {
        for h in hs {
            if let Some(cmd) = h["command"].as_str() {
                if let Some(i) = cmd.rfind(" hook ") {
                    h["command"] = json!(cmd[i..].to_string());
                }
            }
        }
    }
    e
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
        v["mcpServers"][MCP_NAME] =
            json!({"type": "stdio", "command": exe, "args": ["mcp"], "alwaysLoad": true});
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

    /// True when every hook this version installs is present with the same matcher, timeout
    /// and handler (whichever binary it calls), and CLAUDE.md has the current section.
    fn is_current(project: &Project) -> bool {
        let s = read_json(&Self::settings_path(project)).unwrap_or(json!({}));
        let hooks_ok = our_hooks("local-brain").into_iter().all(|(event, want)| {
            let want = normalize_entry(&want);
            s["hooks"][event]
                .as_array()
                .map(|gs| gs.iter().any(|g| normalize_entry(g) == want))
                .unwrap_or(false)
        });
        let md = std::fs::read_to_string(project.root.join("CLAUDE.md")).unwrap_or_default();
        hooks_ok && md.contains(CLAUDE_MD_SECTION.trim_end())
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
        if !s["hooks"].is_object() {
            s["hooks"] = json!({});
        }
        for (event, entry) in our_hooks(exe) {
            let mut arr = s["hooks"][event].as_array().cloned().unwrap_or_default();
            arr.push(entry);
            s["hooks"][event] = Value::Array(arr);
        }
        write_json(&sp, &s)?;
        done.push(format!("installed hooks in {}", project.rel(&sp)));
        if ensure_gitignore(&project.root, ".claude/settings.local.json")? {
            done.push("added .claude/settings.local.json to .gitignore".into());
        }

        if ensure_claude_md_section(&project.root)? {
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
            Some(cmd) => {
                let exists = Path::new(&cmd).exists() || cmd == "local-brain";
                let here = cmd == exe || cmd == "local-brain";
                out.push(Check {
                    ok: exists && here,
                    label: "MCP server registered".into(),
                    detail: if !exists {
                        format!("{} (binary missing — re-run `local-brain init`)", cmd)
                    } else if !here {
                        format!(
                            "{} (another binary than {} — re-run `local-brain init`)",
                            cmd, exe
                        )
                    } else {
                        cmd
                    },
                })
            }
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
            ok: events.len() >= our_hooks(exe).len() && !stale_exe,
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
    fn only_the_registered_binary_refreshes_an_integration() {
        let brew = "/opt/homebrew/bin/local-brain";
        let bins = vec![brew.to_string()];
        let dev = "/src/local-brain/target/debug/local-brain";
        let exists = |_: &str| true;
        // A dev build must not take over an outdated integration that belongs to Homebrew.
        assert!(!needs_refresh(&bins, dev, exists, || false));
        // Homebrew's own binary refreshes it after an upgrade.
        assert!(needs_refresh(&bins, brew, exists, || false));
        assert!(!needs_refresh(&bins, brew, exists, || true));
        // A binary that disappeared is repaired by whoever runs next.
        assert!(needs_refresh(&bins, dev, |_| false, || true));
    }

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
    fn hook_comparison_ignores_the_binary() {
        let a = normalize_entry(&hook_entry(
            "/opt/homebrew/bin/local-brain",
            "stop",
            10,
            None,
        ));
        let b = normalize_entry(&hook_entry("/x/y/local-brain", "stop", 10, None));
        assert_eq!(a, b);
        let c = normalize_entry(&hook_entry(
            "/x/y/local-brain",
            "post-tool-use",
            60,
            Some("Bash"),
        ));
        let d = normalize_entry(&hook_entry(
            "/x/y/local-brain",
            "post-tool-use",
            120,
            Some("Bash|Read"),
        ));
        assert_ne!(c, d);
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
