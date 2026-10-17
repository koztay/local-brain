//! The single global daemon: watches every registered project (debounced file events),
//! updates the AST graph immediately and catches up embeddings/summaries in the background.
//! It talks to nobody over the network; the CLI, hooks and MCP server share the SQLite stores.

use crate::config::{Config, DaemonCfg};
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
    /// Summaries the last background pass left for later.
    bg_pending: usize,
    /// The next background pass may not start before this (memory-pressure back-off).
    bg_not_before: Instant,
    /// When the last background pass finished.
    bg_last_pass: Option<Instant>,
    /// Background work is currently held back by memory pressure (logged once per episode).
    bg_waiting: bool,
}

impl ProjState {
    fn new() -> Self {
        ProjState {
            pending: HashSet::new(),
            last_event: Instant::now(),
            // Catch up once after start: model summaries `learn` left for the background.
            dirty_since: Some(Instant::now()),
            full_reindex: false,
            bg_pending: 0,
            bg_not_before: Instant::now(),
            bg_last_pass: None,
            bg_waiting: false,
        }
    }
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
                            // Create the state now, not on the first file event, so the
                            // start-up catch-up pass runs even when nothing is being edited.
                            states.entry(root.clone()).or_insert_with(ProjState::new);
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
                let st = states.entry(root).or_insert_with(ProjState::new);
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
                            summary_budget: None,
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
            let quiet = Duration::from_secs(global.daemon.bg_quiet_secs);
            if let Some(since) = st.dirty_since {
                if since.elapsed() >= quiet
                    && st.last_event.elapsed() >= quiet
                    && Instant::now() >= st.bg_not_before
                {
                    let mem = memory();
                    if let BgPlan::Wait(d) = next_background_pass(
                        &global.daemon,
                        mem.as_ref().map(Mem::effective_pct),
                        st.bg_pending,
                        st.bg_last_pass.map(|t| t.elapsed()),
                    ) {
                        // Release the model while waiting: a hot model under memory pressure is
                        // what pushes the OS into swapping to the system disk.
                        if !unloaded {
                            unload_models(&global);
                            unloaded = true;
                        }
                        if !st.bg_waiting {
                            eprintln!(
                                "[{}] {}: low memory ({}, floor {}%), background summaries wait; model unloaded",
                                util::fmt_ts(util::now()),
                                root.display(),
                                Mem::describe(mem.as_ref()),
                                global.daemon.min_free_memory_pct
                            );
                            st.bg_waiting = true;
                        }
                        st.bg_not_before = Instant::now() + d;
                        continue;
                    }
                    if st.bg_waiting {
                        eprintln!(
                            "[{}] {}: memory available again, background summaries resume",
                            util::fmt_ts(util::now()),
                            root.display()
                        );
                        st.bg_waiting = false;
                    }
                    st.dirty_since = None;
                    let cfg = Config::load(Some(&project));
                    match crate::learn::learn(
                        &project,
                        &cfg,
                        crate::learn::Options {
                            quiet: true,
                            // Short passes keep file watching responsive between them.
                            summary_budget: Some(Duration::from_secs(
                                global.daemon.bg_pass_secs.max(1),
                            )),
                            ..Default::default()
                        },
                    ) {
                        Ok(r) => {
                            if r.summaries + r.embedded > 0 || r.summaries_pending > 0 {
                                eprintln!(
                                    "[{}] {}: background refresh ({} summaries, {} by model, {} embeddings, {} pending)",
                                    util::fmt_ts(util::now()),
                                    root.display(),
                                    r.summaries,
                                    r.llm_summaries,
                                    r.embedded,
                                    r.summaries_pending
                                );
                            }
                            st.bg_pending = r.summaries_pending;
                            if r.summaries_pending > 0 {
                                // More to do: run the next pass as soon as things are quiet.
                                st.dirty_since = Instant::now().checked_sub(quiet);
                            }
                        }
                        Err(e) => eprintln!(
                            "[{}] {}: background refresh failed: {:#}",
                            util::fmt_ts(util::now()),
                            root.display(),
                            e
                        ),
                    }
                    last_activity = Instant::now();
                    st.bg_last_pass = Some(Instant::now());
                    unloaded = false;
                    // The pass itself can push the machine under pressure (the model is several
                    // GB); then do not let keep_alive hold it for minutes after the work is done.
                    let floor = global.daemon.min_free_memory_pct;
                    let mem = memory();
                    if mem.as_ref().is_some_and(|m| m.effective_pct() < floor) {
                        unload_models(&global);
                        unloaded = true;
                        eprintln!(
                            "[{}] {}: low memory after the pass ({}, floor {}%); model unloaded",
                            util::fmt_ts(util::now()),
                            root.display(),
                            Mem::describe(mem.as_ref()),
                            floor
                        );
                    }
                }
            }
        }

        // Free model memory after the idle period (runtimes also honor keep_alive).
        if !unloaded
            && last_activity.elapsed()
                >= Duration::from_secs(global.daemon.idle_unload_minutes * 60)
        {
            unload_models(&global);
            unloaded = true;
        }
    }
}

fn unload_models(global: &Config) {
    let rt = crate::runtime::Runtime::from_config(global);
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
}

/// A reading of system memory.
struct Mem {
    /// Free memory in percent of RAM.
    free_pct: u64,
    /// The OS reports memory pressure (macOS: warn or critical) — it is already compressing
    /// and swapping, whatever `free_pct` says.
    pressure: bool,
}

impl Mem {
    /// The value the back-off policy compares with the floor: 0 under pressure.
    fn effective_pct(&self) -> u64 {
        if self.pressure {
            0
        } else {
            self.free_pct
        }
    }

    fn describe(m: Option<&Mem>) -> String {
        match m {
            None => "free memory unknown".into(),
            Some(m) if m.pressure => format!("{}% free, OS reports memory pressure", m.free_pct),
            Some(m) => format!("{}% free", m.free_pct),
        }
    }
}

/// Read system memory, or None when it cannot be read.
#[cfg(target_os = "macos")]
fn memory() -> Option<Mem> {
    // sysinfo's available memory reads ~0 on macOS (it counts only truly free pages, and the
    // kernel keeps those near zero). Ask the kernel instead: `kern.memorystatus_level` is the
    // free percentage `memory_pressure` prints; pressure level 2 is warn, 4 critical.
    fn sysctl_u32(name: &std::ffi::CStr) -> Option<u32> {
        let mut v: u32 = 0;
        let mut len = std::mem::size_of::<u32>();
        // SAFETY: `v` and `len` describe a valid u32 buffer; no new value is written.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut v as *mut u32 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(v)
    }
    Some(Mem {
        free_pct: u64::from(sysctl_u32(c"kern.memorystatus_level")?),
        pressure: sysctl_u32(c"kern.memorystatus_vm_pressure_level").is_some_and(|l| l >= 2),
    })
}

/// Read system memory, or None when it cannot be read.
#[cfg(not(target_os = "macos"))]
fn memory() -> Option<Mem> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let total = sys.total_memory();
    (total > 0).then(|| Mem {
        free_pct: sys.available_memory() * 100 / total,
        pressure: false,
    })
}

/// What the daemon does with a due background pass (model summaries/embeddings).
#[derive(Debug, PartialEq)]
enum BgPlan {
    /// Run the pass now; the model stays loaded.
    Run,
    /// Skip it: unload the model and ask again after this long.
    Wait(Duration),
}

/// Decide whether a due background pass runs now.
///
/// `free_pct` is free RAM in percent (None if unknown), `pending` the summaries the previous
/// pass left over (0 before the first pass), `since_last` the time since the previous pass
/// finished. `Wait` unloads the model (several GB) for its duration, so RAM is really returned
/// to the system. All thresholds come from `[daemon]` (`local-brain config list`):
///
/// * Below `min_free_memory_pct`: wait `bg_backoff_secs`.
/// * Under twice that with a queue: at least `bg_pace_secs` between passes.
/// * Otherwise (or when memory cannot be read): run; `keep_alive` and the idle unload apply.
fn next_background_pass(
    d: &DaemonCfg,
    free_pct: Option<u64>,
    pending: usize,
    since_last: Option<Duration>,
) -> BgPlan {
    let Some(free) = free_pct else {
        return BgPlan::Run;
    };
    if free < d.min_free_memory_pct {
        return BgPlan::Wait(Duration::from_secs(d.bg_backoff_secs));
    }
    let gap = Duration::from_secs(d.bg_pace_secs);
    if free < d.min_free_memory_pct.saturating_mul(2) && pending > 0 {
        if let Some(since) = since_last.filter(|s| *s < gap) {
            return BgPlan::Wait(gap - since);
        }
    }
    BgPlan::Run
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

/// The binary an installed daemon service starts, if a service is installed.
fn service_binary() -> Option<String> {
    if cfg!(target_os = "macos") {
        let p = dirs::home_dir()?.join("Library/LaunchAgents/dev.local-brain.daemon.plist");
        let s = std::fs::read_to_string(p).ok()?;
        let start = s.find("<key>ProgramArguments</key><array><string>")?
            + "<key>ProgramArguments</key><array><string>".len();
        let end = start + s[start..].find("</string>")?;
        Some(s[start..end].to_string())
    } else if cfg!(target_os = "linux") {
        let p = dirs::config_dir()?.join("systemd/user/local-brain.service");
        let s = std::fs::read_to_string(p).ok()?;
        let line = s.lines().find(|l| l.starts_with("ExecStart="))?;
        Some(
            line["ExecStart=".len()..]
                .trim_end_matches(" daemon run")
                .to_string(),
        )
    } else {
        None
    }
}

/// Re-install the daemon service when it points at a binary that no longer exists.
pub fn repair_stale_service(exe: &str) -> Option<String> {
    let b = service_binary()?;
    if b == exe || Path::new(&b).exists() {
        return None;
    }
    Some(match install_service() {
        Ok(_) => format!("updated the daemon service to {}", exe),
        Err(e) => format!("could not update the daemon service: {:#}", e),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    fn plan(free: Option<u64>, pending: usize, since: Option<u64>) -> BgPlan {
        next_background_pass(&DaemonCfg::default(), free, pending, since.map(S))
    }

    #[test]
    fn waits_below_the_memory_floor() {
        assert_eq!(plan(Some(10), 50, None), BgPlan::Wait(S(180)));
        assert_eq!(plan(Some(14), 0, Some(600)), BgPlan::Wait(S(180)));
    }

    #[test]
    fn paces_a_queue_when_memory_is_tight() {
        assert_eq!(plan(Some(20), 30, Some(10)), BgPlan::Wait(S(80)));
        assert_eq!(plan(Some(20), 30, Some(90)), BgPlan::Run);
        assert_eq!(plan(Some(20), 30, None), BgPlan::Run);
        // A one-off pass (nothing queued) is not delayed.
        assert_eq!(plan(Some(20), 0, Some(10)), BgPlan::Run);
    }

    #[test]
    fn runs_with_plenty_of_memory_or_when_unknown() {
        assert_eq!(plan(Some(60), 300, Some(1)), BgPlan::Run);
        assert_eq!(plan(None, 300, Some(1)), BgPlan::Run);
    }

    #[test]
    fn pressure_counts_as_no_free_memory_but_is_reported_honestly() {
        let m = Mem {
            free_pct: 42,
            pressure: true,
        };
        assert_eq!(m.effective_pct(), 0);
        assert_eq!(
            Mem::describe(Some(&m)),
            "42% free, OS reports memory pressure"
        );
        assert_eq!(Mem::describe(None), "free memory unknown");
    }

    #[test]
    fn follows_the_configured_thresholds() {
        let d = DaemonCfg {
            min_free_memory_pct: 0,
            ..DaemonCfg::default()
        };
        // A floor of 0 disables the back-off.
        assert_eq!(
            next_background_pass(&d, Some(0), 300, Some(S(1))),
            BgPlan::Run
        );
        let d = DaemonCfg {
            min_free_memory_pct: 30,
            bg_backoff_secs: 60,
            bg_pace_secs: 20,
            ..DaemonCfg::default()
        };
        assert_eq!(
            next_background_pass(&d, Some(25), 1, None),
            BgPlan::Wait(S(60))
        );
        assert_eq!(
            next_background_pass(&d, Some(45), 1, Some(S(5))),
            BgPlan::Wait(S(15))
        );
    }
}
