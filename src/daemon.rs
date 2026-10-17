//! The single global daemon: watches every registered project (debounced file events),
//! updates the AST graph immediately and catches up embeddings/summaries in the background.
//! It talks to nobody over the network; the CLI, hooks and MCP server share the SQLite stores.

use crate::config::Config;
use crate::paths::{self, Project};
use crate::registry;
use crate::store::Store;
use crate::util;
use anyhow::{bail, Context, Result};
use notify::{RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub fn read_pid() -> Option<u32> {
    std::fs::read_to_string(paths::daemon_pid())
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn is_running() -> Option<u32> {
    let pid = read_pid()?;
    let mut sys = sysinfo::System::new();
    let p = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[p]), true);
    let proc_ = sys.process(p)?;
    let name = proc_.name().to_string_lossy().to_lowercase();
    if name.contains("local-brain") || name.contains("local_brain") {
        Some(pid)
    } else {
        None
    }
}

pub fn memory_usage() -> Option<u64> {
    let pid = is_running()?;
    let mut sys = sysinfo::System::new();
    let p = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[p]), true);
    sys.process(p).map(|x| x.memory())
}

/// Start the daemon in the background (no-op if already running).
pub fn start() -> Result<u32> {
    if let Some(pid) = is_running() {
        return Ok(pid);
    }
    paths::ensure_home()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_file("daemon.log"))?;
    let mut cmd = std::process::Command::new(paths::exe());
    cmd.args(["daemon", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let child = cmd.spawn().context("starting daemon")?;
    std::thread::sleep(Duration::from_millis(300));
    Ok(child.id())
}

pub fn stop() -> Result<bool> {
    let Some(pid) = is_running() else {
        let _ = std::fs::remove_file(paths::daemon_pid());
        return Ok(false);
    };
    let mut sys = sysinfo::System::new();
    let p = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[p]), true);
    if let Some(proc_) = sys.process(p) {
        proc_.kill();
    }
    let _ = std::fs::remove_file(paths::daemon_pid());
    Ok(true)
}

fn write_status(watched: &[PathBuf], last_event: i64, started: i64) {
    let v = serde_json::json!({
        "pid": std::process::id(),
        "started": started,
        "watched": watched,
        "last_event": last_event,
        "heartbeat": util::now(),
    });
    let _ = std::fs::write(paths::daemon_status(), v.to_string());
}

fn is_noise(p: &Path) -> bool {
    p.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        matches!(
            s.as_ref(),
            ".local-brain"
                | "node_modules"
                | "target"
                | ".next"
                | "__pycache__"
                | ".venv"
                | "dist"
                | "build"
                | ".gradle"
        )
    }) || (p.components().any(|c| c.as_os_str() == ".git") && !p.ends_with(".git/HEAD"))
}

struct ProjState {
    pending: HashSet<PathBuf>,
    last_event: Instant,
    dirty_since: Option<Instant>,
    full_reindex: bool,
}

/// Run the daemon in the foreground.
pub fn run() -> Result<()> {
    paths::ensure_home()?;
    if let Some(pid) = is_running() {
        if pid != std::process::id() {
            bail!("daemon already running (pid {})", pid);
        }
    }
    std::fs::write(paths::daemon_pid(), std::process::id().to_string())?;
    let started = util::now();
    eprintln!(
        "[{}] local-brain daemon started (pid {})",
        util::fmt_ts(started),
        std::process::id()
    );

    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    let mut watched: HashMap<PathBuf, ()> = HashMap::new();
    let mut states: HashMap<PathBuf, ProjState> = HashMap::new();
    let mut last_registry = Instant::now() - Duration::from_secs(60);
    let mut last_event_ts = 0;
    let mut last_activity = Instant::now();
    let mut unloaded = false;

    loop {
        // Refresh the set of watched projects from the registry.
        if last_registry.elapsed() >= Duration::from_secs(5) {
            last_registry = Instant::now();
            let want: HashSet<PathBuf> = registry::load()
                .into_iter()
                .filter(|e| e.watch && e.root.join(paths::PROJECT_DIR).is_dir())
                .map(|e| e.root)
                .collect();
            for root in want.iter() {
                if !watched.contains_key(root) {
                    match watcher.watch(root, RecursiveMode::Recursive) {
                        Ok(_) => {
                            eprintln!(
                                "[{}] watching {}",
                                util::fmt_ts(util::now()),
                                root.display()
                            );
                            watched.insert(root.clone(), ());
                        }
                        Err(e) => eprintln!("watch {}: {}", root.display(), e),
                    }
                }
            }
            let gone: Vec<PathBuf> = watched
                .keys()
                .filter(|r| !want.contains(*r))
                .cloned()
                .collect();
            for root in gone {
                let _ = watcher.unwatch(&root);
                watched.remove(&root);
                states.remove(&root);
                eprintln!(
                    "[{}] stopped watching {}",
                    util::fmt_ts(util::now()),
                    root.display()
                );
            }
            let roots: Vec<PathBuf> = watched.keys().cloned().collect();
            write_status(&roots, last_event_ts, started);
            if read_pid() != Some(std::process::id()) {
                eprintln!("pid file changed; exiting");
                return Ok(());
            }
        }

        // Collect events.
        while let Ok(res) = rx.recv_timeout(Duration::from_millis(200)) {
            let Ok(ev) = res else { continue };
            if matches!(ev.kind, notify::EventKind::Access(_)) {
                continue;
            }
            for p in ev.paths {
                if is_noise(&p) {
                    continue;
                }
                let Some(root) = watched
                    .keys()
                    .filter(|r| p.starts_with(r))
                    .max_by_key(|r| r.as_os_str().len())
                    .cloned()
                else {
                    continue;
                };
                let st = states.entry(root).or_insert_with(|| ProjState {
                    pending: HashSet::new(),
                    last_event: Instant::now(),
                    dirty_since: None,
                    full_reindex: false,
                });
                if p.ends_with(".git/HEAD") {
                    st.full_reindex = true;
                } else {
                    st.pending.insert(p);
                }
                st.last_event = Instant::now();
                last_event_ts = util::now();
                last_activity = Instant::now();
                unloaded = false;
            }
            if rx.try_recv().is_err() {
                break;
            }
        }

        // Process debounced batches.
        let global = Config::load_global();
        let debounce = Duration::from_millis(global.daemon.debounce_ms);
        for (root, st) in states.iter_mut() {
            let project = Project::new(root.clone());
            if (!st.pending.is_empty() || st.full_reindex) && st.last_event.elapsed() >= debounce {
                let cfg = Config::load(Some(&project));
                let paths_now: Vec<PathBuf> = st.pending.drain().collect();
                let bulk = st.full_reindex || paths_now.len() > 200;
                st.full_reindex = false;
                let res = if bulk {
                    // Branch switch or mass change: incremental full pass (hash-based, AST first).
                    crate::learn::learn(
                        &project,
                        &cfg,
                        crate::learn::Options {
                            ast_only: true,
                            quiet: true,
                            no_llm: true,
                        },
                    )
                    .map(|r| r.indexed)
                } else {
                    Store::open(&project)
                        .and_then(|s| crate::learn::reindex_paths(&s, &project, &cfg, &paths_now))
                };
                match res {
                    Ok(n) if n > 0 => {
                        eprintln!(
                            "[{}] {}: updated {} file(s){}",
                            util::fmt_ts(util::now()),
                            root.display(),
                            n,
                            if bulk { " (bulk)" } else { "" }
                        );
                        st.dirty_since = Some(Instant::now());
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!(
                        "[{}] {}: {:#}",
                        util::fmt_ts(util::now()),
                        root.display(),
                        e
                    ),
                }
            }
            // Background catch-up (model embeddings, stale summaries) once things are quiet.
            if let Some(since) = st.dirty_since {
                if since.elapsed() >= Duration::from_secs(45)
                    && st.last_event.elapsed() >= Duration::from_secs(30)
                {
                    st.dirty_since = None;
                    let cfg = Config::load(Some(&project));
                    match crate::learn::learn(
                        &project,
                        &cfg,
                        crate::learn::Options {
                            quiet: true,
                            ..Default::default()
                        },
                    ) {
                        Ok(r) => eprintln!(
                            "[{}] {}: background refresh ({} summaries, {} embeddings)",
                            util::fmt_ts(util::now()),
                            root.display(),
                            r.summaries,
                            r.embedded
                        ),
                        Err(e) => eprintln!(
                            "[{}] {}: background refresh failed: {:#}",
                            util::fmt_ts(util::now()),
                            root.display(),
                            e
                        ),
                    }
                    last_activity = Instant::now();
                }
            }
        }

        // Free model memory after the idle period (runtimes also honor keep_alive).
        if !unloaded
            && last_activity.elapsed()
                >= Duration::from_secs(global.daemon.idle_unload_minutes * 60)
        {
            let rt = crate::runtime::Runtime::from_config(&global);
            for m in [
                &global.models.general,
                &global.models.code,
                &global.models.vision,
                &global.models.embedding,
            ] {
                if !m.is_empty() {
                    let _ = rt.unload(m);
                }
            }
            unloaded = true;
        }
    }
}

// ---- OS service installation -----------------------------------------------------------------

pub fn install_service() -> Result<String> {
    let exe = paths::exe();
    if cfg!(target_os = "macos") {
        let home = dirs::home_dir().context("no home dir")?;
        let p = home.join("Library/LaunchAgents/dev.local-brain.daemon.plist");
        let log = paths::log_file("daemon.log");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.local-brain.daemon</string>
  <key>ProgramArguments</key><array><string>{}</string><string>daemon</string><string>run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
  <key>ProcessType</key><string>Background</string>
</dict>
</plist>
"#,
            exe,
            log.display(),
            log.display()
        );
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, plist)?;
        let _ = stop();
        let _ = std::process::Command::new("launchctl")
            .args(["unload", "-w"])
            .arg(&p)
            .output();
        let out = std::process::Command::new("launchctl")
            .args(["load", "-w"])
            .arg(&p)
            .output()?;
        if !out.status.success() {
            bail!(
                "launchctl load failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(format!("installed launchd agent {}", p.display()))
    } else if cfg!(target_os = "linux") {
        let base = dirs::config_dir()
            .context("no config dir")?
            .join("systemd/user");
        std::fs::create_dir_all(&base)?;
        let p = base.join("local-brain.service");
        let unit = format!(
            "[Unit]\nDescription=local-brain daemon (watches projects for coding agents)\n\n[Service]\nExecStart={} daemon run\nRestart=on-failure\nNice=10\n\n[Install]\nWantedBy=default.target\n",
            exe
        );
        std::fs::write(&p, unit)?;
        let _ = stop();
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        let out = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", "local-brain.service"])
            .output()?;
        if !out.status.success() {
            bail!("systemctl failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(format!("installed systemd user service {}", p.display()))
    } else if cfg!(windows) {
        let out = std::process::Command::new("schtasks")
            .args([
                "/Create",
                "/F",
                "/SC",
                "ONLOGON",
                "/TN",
                "local-brain",
                "/TR",
                &format!("\"{}\" daemon run", exe),
            ])
            .output()?;
        if !out.status.success() {
            bail!("schtasks failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok("installed scheduled task `local-brain` (runs at logon)".into())
    } else {
        bail!("unsupported OS for service installation; run `local-brain daemon start` instead")
    }
}

pub fn uninstall_service() -> Result<String> {
    let _ = stop();
    if cfg!(target_os = "macos") {
        let p = dirs::home_dir()
            .context("no home")?
            .join("Library/LaunchAgents/dev.local-brain.daemon.plist");
        if p.exists() {
            let _ = std::process::Command::new("launchctl")
                .args(["unload", "-w"])
                .arg(&p)
                .output();
            std::fs::remove_file(&p)?;
        }
        Ok("removed launchd agent".into())
    } else if cfg!(target_os = "linux") {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", "local-brain.service"])
            .output();
        if let Some(c) = dirs::config_dir() {
            let _ = std::fs::remove_file(c.join("systemd/user/local-brain.service"));
        }
        Ok("removed systemd user service".into())
    } else if cfg!(windows) {
        let _ = std::process::Command::new("schtasks")
            .args(["/Delete", "/F", "/TN", "local-brain"])
            .output();
        Ok("removed scheduled task".into())
    } else {
        Ok("nothing to remove".into())
    }
}
