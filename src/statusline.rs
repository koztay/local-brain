//! `local-brain statusline`: a Claude Code status-line segment with the tokens local-brain saved
//! in the current session and today. Claude Code runs the command on every status refresh with
//! the session as JSON on stdin, so it must be fast and silent: outside a local-brain project,
//! or on any error, it prints nothing.

use crate::paths;
use crate::store::Store;
use crate::util;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Net savings as a short phrase: "saved 12.3k" or "cost 1.2k" (context notes that outweigh
/// what was saved are a cost, and are shown as one).
fn net(base: i64, actual: i64) -> String {
    let saved = base - actual;
    if saved >= 0 {
        format!("saved {}", util::human_tokens(saved))
    } else {
        format!("cost {}", util::human_tokens(-saved))
    }
}

/// The segment for a project and session, or None when there is nothing to show yet.
fn segment(store: &Store, session: Option<&str>) -> Result<Option<String>> {
    let (today_calls, tb, ta) = store.metric_totals(util::today_start())?;
    if today_calls == 0 {
        return Ok(None);
    }
    let today = net(tb, ta);
    let session = match session {
        Some(s) => {
            let (calls, b, a) = store.metric_totals_for_session(s)?;
            (calls > 0).then(|| net(b, a))
        }
        None => None,
    };
    Ok(Some(match session {
        Some(s) => format!("brain: {} this session · {} today", s, today),
        None => format!("brain: {} today", today),
    }))
}

/// Print the segment for the session described by Claude Code's status-line JSON.
pub fn print() {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let dir = v["workspace"]["project_dir"]
        .as_str()
        .or_else(|| v["cwd"].as_str())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    let Some(project) = dir.as_deref().and_then(paths::find_project) else {
        return;
    };
    if !project.db().exists() {
        return;
    }
    let Ok(store) = Store::open(&project) else {
        return;
    };
    if let Ok(Some(s)) = segment(&store, v["session_id"].as_str()) {
        print!("{}", s);
    }
}

fn claude_settings() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// `local-brain statusline --install`: use it as the Claude Code status line, unless one is
/// already configured — then print how to add the segment to it instead of replacing it.
pub fn install() -> Result<()> {
    let path = claude_settings().context("no home directory")?;
    install_into(&path, &paths::exe())
}

fn install_into(path: &Path, exe: &str) -> Result<()> {
    let mut settings: Value = match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))?,
        Err(_) => json!({}),
    };
    let ours = format!("{} statusline", exe);
    if let Some(cmd) = settings["statusLine"]["command"].as_str() {
        if cmd.contains("local-brain") && cmd.contains("statusline") {
            println!("The Claude Code status line already shows local-brain savings.");
        } else {
            println!(
                "You already have a status line ({}); it was left unchanged.\n\
                 To add local-brain's segment, put this line in that script after it reads stdin \
                 into $input:\n\n  printf ' | %s' \"$(echo \"$input\" | {})\"",
                cmd, ours
            );
        }
        return Ok(());
    }
    settings["statusLine"] = json!({"type": "command", "command": ours});
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&settings)? + "\n")
        .with_context(|| format!("writing {}", path.display()))?;
    println!(
        "Claude Code's status line now shows local-brain savings ({}). It appears on the next \
         refresh.",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_reads_as_saving_or_cost() {
        assert_eq!(net(20_000, 5_000), "saved 15.0k");
        assert_eq!(net(0, 450), "cost 450");
    }

    #[test]
    fn install_never_replaces_a_foreign_status_line() {
        let dir = std::env::temp_dir().join(format!("lb-sl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        let mine = r#"{"statusLine":{"type":"command","command":"bash ~/my.sh"},"x":1}"#;
        std::fs::write(&p, mine).unwrap();
        install_into(&p, "/bin/local-brain").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), mine);

        std::fs::write(&p, r#"{"x":1}"#).unwrap();
        install_into(&p, "/bin/local-brain").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["statusLine"]["command"], "/bin/local-brain statusline");
        assert_eq!(v["x"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
