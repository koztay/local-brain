//! Local model runtime provisioning for `init`. When no runtime is available, local-brain offers
//! to install Ollama, lets the user put the (large) models on an external drive, starts Ollama as
//! a background service and points the configuration at it — so nobody has to install or
//! configure a model server by hand. Other runtimes (LM Studio, MLX, llama.cpp) are left alone.

use crate::catalog;
use crate::config::Config;
use crate::hardware;
use crate::paths;
use crate::runtime::{self, Runtime};
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// launchd label of the Ollama server that local-brain installs on macOS.
const MAC_AGENT: &str = "dev.local-brain.ollama";
/// Free space to leave on a drive besides the models themselves.
const HEADROOM_GB: f64 = 2.0;
/// Prefer the internal disk only when it keeps at least this much free after the download.
const INTERNAL_COMFORT_GB: f64 = 50.0;

fn ok(s: &str) {
    eprintln!("  ✓ {}", s);
}
fn warn(s: &str) {
    eprintln!("  ! {}", s);
}

fn ollama_url() -> &'static str {
    runtime::default_url("ollama")
}

fn ollama_up() -> bool {
    Runtime::new("ollama", ollama_url(), "10m", 2, "").is_up()
}

fn interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Make sure a usable runtime is running. Returns true when the user agreed to a full setup
/// (install + model download), so the caller can download models without asking again.
pub fn ensure_runtime(cfg: &mut Config, yes: bool, no_models: bool) -> Result<bool> {
    if no_models || !cfg.network.allow_downloads {
        return Ok(false);
    }
    match cfg.runtime.provider.as_str() {
        "none" => {}
        "ollama" if ollama_up() => return Ok(false),
        "ollama" => {
            if find_ollama().is_some() {
                eprintln!("Starting Ollama…");
                start_existing(&cfg.runtime.ollama_models_dir)?;
                wait_up(Duration::from_secs(60))?;
                ok("Ollama is running");
                return Ok(false);
            }
        }
        // LM Studio, MLX, llama.cpp or a custom server: the user chose it, do not interfere.
        _ => return Ok(false),
    }

    let models = tier_models(&cfg.models.tier);
    let need_gb: f64 = models.iter().map(|(_, gb)| gb).sum();
    let names: Vec<&str> = models.iter().map(|(n, _)| n.as_str()).collect();
    let q = format!(
        "No local model runtime found. Install Ollama and download {} (~{:.1} GB) now?",
        names.join(" + "),
        need_gb
    );
    if !yes {
        if !interactive() {
            warn("No local model runtime found; run `local-brain init --yes` to install Ollama and the models automatically.");
            return Ok(false);
        }
        if !util::confirm(&q, true) {
            warn("Skipped; run `local-brain init` again whenever you want the local model.");
            return Ok(false);
        }
    }

    let models_dir = choose_models_dir(need_gb, yes)?;

    if find_ollama().is_some() {
        ok("Ollama is already installed");
    } else {
        eprintln!("Installing Ollama…");
        install_ollama(models_dir.as_deref())?;
        ok("installed Ollama");
    }
    start_managed(models_dir.as_deref())?;
    wait_up(Duration::from_secs(90))?;
    ok("Ollama is running");

    let tier = cfg.models.tier.clone();
    cfg.runtime.provider = "ollama".into();
    cfg.runtime.base_url = ollama_url().into();
    cfg.runtime.ollama_models_dir = models_dir
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    crate::cli::apply_catalog(cfg, "ollama", &tier)?;
    cfg.save_global()?;
    ok(&format!(
        "configured provider ollama (tier {}) in {}",
        tier,
        paths::global_config().display()
    ));
    Ok(true)
}

/// Ollama model names and sizes for a tier, deduplicated.
fn tier_models(tier: &str) -> Vec<(String, f64)> {
    let Ok(cat) = catalog::load() else {
        return vec![];
    };
    let Some(t) = cat.tiers.get(tier).or_else(|| cat.tiers.get("S")) else {
        return vec![];
    };
    let mut out: Vec<(String, f64)> = Vec::new();
    for m in [&t.general, &t.code, &t.vision, &t.embedding]
        .into_iter()
        .flatten()
    {
        if let Some(n) = m.name_for("ollama") {
            if !out.iter().any(|(x, _)| *x == n) {
                out.push((n, m.size_gb));
            }
        }
    }
    out
}

// ---- where the models go ------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Volume {
    label: String,
    mount: PathBuf,
    free_gb: f64,
    internal: bool,
}

/// The disk holding the home directory first, then writable external drives.
fn list_volumes() -> Vec<Volume> {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    let home = dirs::home_dir().unwrap_or_default();
    let gb = |b: u64| b as f64 / 1024f64.powi(3);
    let mut out: Vec<Volume> = Vec::new();
    if let Some(d) = disks
        .list()
        .iter()
        .filter(|d| home.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
    {
        out.push(Volume {
            label: "Internal disk".into(),
            mount: d.mount_point().to_path_buf(),
            free_gb: gb(d.available_space()),
            internal: true,
        });
    }
    for d in disks.list() {
        let m = d.mount_point();
        if d.is_read_only() || !is_external_mount(m) || home.starts_with(m) {
            continue;
        }
        if out.iter().any(|v| v.mount == m) {
            continue;
        }
        let name = m
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| m.to_string_lossy().into_owned());
        out.push(Volume {
            label: name,
            mount: m.to_path_buf(),
            free_gb: gb(d.available_space()),
            internal: false,
        });
    }
    out
}

/// Mount points of extra drives: /Volumes/* on macOS, /media, /mnt and /run/media on Linux,
/// other drive letters on Windows. Hidden mounts (Time Machine snapshots and the like) are skipped.
fn is_external_mount(m: &Path) -> bool {
    let s = m.to_string_lossy();
    if s.split(['/', '\\']).any(|c| c.starts_with('.')) {
        return false;
    }
    if cfg!(target_os = "macos") {
        s.starts_with("/Volumes/")
    } else if cfg!(windows) {
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        s.len() <= 3 && !s.to_uppercase().starts_with(&sys.to_uppercase())
    } else {
        s.starts_with("/media/") || s.starts_with("/mnt/") || s.starts_with("/run/media/")
    }
}

/// Default storage: the internal disk while it keeps plenty of room, otherwise the external drive
/// with the most free space. Returns an index into `vols`.
fn default_volume(vols: &[Volume], need_gb: f64) -> usize {
    let fits = |v: &Volume| v.free_gb >= need_gb + HEADROOM_GB;
    if let Some(i) = vols
        .iter()
        .position(|v| v.internal && v.free_gb - need_gb >= INTERNAL_COMFORT_GB)
    {
        return i;
    }
    vols.iter()
        .enumerate()
        .filter(|(_, v)| !v.internal && fits(v))
        .max_by(|a, b| a.1.free_gb.total_cmp(&b.1.free_gb))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Ask where to keep the models when an external drive is available. `None` means Ollama's
/// default location on the internal disk.
fn choose_models_dir(need_gb: f64, yes: bool) -> Result<Option<PathBuf>> {
    let vols: Vec<Volume> = list_volumes();
    let usable: Vec<Volume> = vols
        .iter()
        .filter(|v| v.internal || v.free_gb >= need_gb + HEADROOM_GB)
        .cloned()
        .collect();
    let has_external = usable.iter().any(|v| !v.internal);
    if !has_external {
        if let Some(v) = usable.first() {
            if v.free_gb < need_gb + HEADROOM_GB {
                bail!(
                    "not enough disk space for the models: need ~{:.1} GB, {:.1} GB free; connect an external drive and run `local-brain init` again",
                    need_gb + HEADROOM_GB,
                    v.free_gb
                );
            }
        }
        return Ok(None);
    }
    let def = default_volume(&usable, need_gb);
    let pick = if yes || !interactive() {
        def
    } else {
        eprintln!("Where should the models be stored (~{:.1} GB)?", need_gb);
        for (i, v) in usable.iter().enumerate() {
            let tag = if v.internal {
                String::new()
            } else {
                format!(" ({})", v.mount.display())
            };
            eprintln!(
                "  {}) {}{} — {:.0} GB free{}",
                i + 1,
                v.label,
                tag,
                v.free_gb,
                if i == def { "  [default]" } else { "" }
            );
        }
        read_choice(usable.len(), def)
    };
    let v = &usable[pick];
    if v.internal {
        ok("models will be stored on the internal disk");
        return Ok(None);
    }
    let dir = v.mount.join("local-brain").join("ollama-models");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {} (is the drive writable?)", dir.display()))?;
    check_models_dir(&dir)?;
    ok(&format!("models will be stored in {}", dir.display()));
    warn("keep this drive connected while you use local-brain; without it the local model is unavailable and local-brain falls back to deterministic mode.");
    Ok(Some(dir))
}

/// The models path ends up in a launchd plist, a systemd unit written with sudo, or `setx`.
/// Accept only plain absolute paths without symlinks, so none of those can be subverted.
fn check_models_dir(dir: &Path) -> Result<()> {
    let s = dir.to_string_lossy();
    if !dir.is_absolute() {
        bail!("models directory must be an absolute path: {}", s);
    }
    // Backslash is the path separator on Windows, so it is only rejected elsewhere.
    let unsafe_char = |c: &char| {
        c.is_control()
            || matches!(c, '"' | '%' | '$' | '`' | '\'')
            || (*c == '\\' && !cfg!(windows))
    };
    if let Some(c) = s.chars().find(unsafe_char) {
        bail!(
            "the drive path {} contains {:?}, which cannot be used safely in a service definition; rename the drive or pick another one",
            s,
            c
        );
    }
    let real = std::fs::canonicalize(dir).with_context(|| format!("resolving {}", s))?;
    if real != dir {
        bail!(
            "{} resolves to {} through a symlink; pick the drive directly",
            s,
            real.display()
        );
    }
    Ok(())
}

fn read_choice(n: usize, def: usize) -> usize {
    use std::io::Write;
    eprint!("Choice [{}]: ", def + 1);
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return def;
    }
    match line.trim().parse::<usize>() {
        Ok(k) if (1..=n).contains(&k) => k - 1,
        _ => def,
    }
}

// ---- install ------------------------------------------------------------------------------------

/// The `ollama` binary, on PATH or in the usual install locations.
fn find_ollama() -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        "ollama.exe"
    } else {
        "ollama"
    };
    if let Some(path) = std::env::var_os("PATH") {
        if let Some(p) = std::env::split_paths(&path)
            .map(|d| d.join(exe))
            .find(|p| p.is_file())
        {
            return Some(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        candidates.push("/opt/homebrew/bin/ollama".into());
        candidates.push("/usr/local/bin/ollama".into());
        for app in macos_app_candidates() {
            candidates.push(app.join("Contents/Resources/ollama"));
        }
    } else if cfg!(windows) {
        if let Some(d) = dirs::data_local_dir() {
            candidates.push(d.join("Programs").join("Ollama").join("ollama.exe"));
        }
    } else {
        candidates.push("/usr/local/bin/ollama".into());
        candidates.push("/usr/bin/ollama".into());
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn macos_app_candidates() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/Applications/Ollama.app")];
    if let Some(h) = dirs::home_dir() {
        v.push(h.join("Applications").join("Ollama.app"));
    }
    v
}

fn has(bin: &str) -> bool {
    hardware::on_path(bin)
}

/// Run a command with the terminal attached, so the user sees progress and password prompts.
fn run(program: &str, args: &[&str]) -> Result<()> {
    eprintln!("  $ {} {}", program, args.join(" "));
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("running {}", program))?;
    if !status.success() {
        bail!("`{} {}` failed ({})", program, args.join(" "), status);
    }
    Ok(())
}

fn quiet_ok(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn install_ollama(models_dir: Option<&Path>) -> Result<()> {
    if cfg!(target_os = "macos") {
        if has("brew") {
            return run("brew", &["install", "ollama"]);
        }
        return install_macos_app();
    }
    if cfg!(windows) {
        if let Some(d) = models_dir {
            // Persist for the Ollama app, which the installer starts right away.
            run("setx", &["OLLAMA_MODELS", &d.to_string_lossy()])?;
        }
        if has("winget") {
            return run(
                "winget",
                &[
                    "install",
                    "--exact",
                    "--id",
                    "Ollama.Ollama",
                    "--accept-source-agreements",
                    "--accept-package-agreements",
                ],
            );
        }
        bail!("winget is not available; install Ollama from https://ollama.com/download and run `local-brain init` again");
    }
    if !has("curl") {
        bail!("curl is required to install Ollama; install curl and run `local-brain init` again");
    }
    // The official Linux installer sets up a systemd service; it asks for sudo when needed.
    run(
        "sh",
        &["-c", "curl -fsSL https://ollama.com/install.sh | sh"],
    )
}

/// macOS without Homebrew: download the signed Ollama.app and unpack it into Applications.
fn install_macos_app() -> Result<()> {
    let dest_dir = if is_writable("/Applications") {
        PathBuf::from("/Applications")
    } else {
        let d = dirs::home_dir()
            .context("no home directory")?
            .join("Applications");
        std::fs::create_dir_all(&d)?;
        d
    };
    let work = private_temp_dir()?;
    let result = (|| -> Result<()> {
        let zip = work.join("Ollama-darwin.zip");
        download(
            "https://ollama.com/download/Ollama-darwin.zip",
            &zip,
            "Ollama",
        )?;
        // ditto keeps the code signature and extended attributes intact.
        let unpacked = work.join("unpacked");
        run(
            "ditto",
            &[
                "-x",
                "-k",
                &zip.to_string_lossy(),
                &unpacked.to_string_lossy(),
            ],
        )?;
        let app = unpacked.join("Ollama.app");
        verify_macos_app(&app)?;
        run(
            "ditto",
            &[
                &app.to_string_lossy(),
                &dest_dir.join("Ollama.app").to_string_lossy(),
            ],
        )
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// A fresh directory only this user can read, so nothing else can swap the download.
pub(crate) fn private_temp_dir() -> Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("local-brain-{}-{}", std::process::id(), nanos));
    // create_dir (not create_dir_all) fails if the path already exists.
    std::fs::create_dir(&d).with_context(|| format!("creating {}", d.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(d)
}

/// Refuse an Ollama.app whose signature is broken or that Gatekeeper would not run.
fn verify_macos_app(app: &Path) -> Result<()> {
    let a = app.to_string_lossy();
    if !quiet_ok("codesign", &["--verify", "--deep", "--strict", &a]) {
        bail!("the downloaded Ollama.app has an invalid code signature; not installing it");
    }
    if !quiet_ok("spctl", &["--assess", "--type", "execute", &a]) {
        bail!("Gatekeeper rejected the downloaded Ollama.app; not installing it");
    }
    ok("verified the Ollama.app signature and notarization");
    Ok(())
}

fn is_writable(dir: &str) -> bool {
    let p = PathBuf::from(dir).join(".local-brain-write-test");
    let okay = std::fs::write(&p, b"").is_ok();
    let _ = std::fs::remove_file(&p);
    okay
}

pub(crate) fn download(url: &str, to: &Path, label: &str) -> Result<()> {
    use std::io::{Read, Write};
    let resp = ureq::get(url)
        .call()
        .map_err(|e| anyhow!("downloading {}: {}", url, e))?;
    let total: f64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let mut reader = resp.into_reader();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .with_context(|| format!("creating {}", to.display()))?;
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0f64;
    let mut shown = -1i64;
    let tty = {
        use std::io::IsTerminal;
        std::io::stderr().is_terminal()
    };
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as f64;
        let pct = if total > 0.0 {
            (done / total * 100.0) as i64
        } else {
            -1
        };
        if tty && pct >= 0 && pct != shown {
            shown = pct;
            eprint!("\r  {}: downloading {:3}%   ", label, pct);
            let _ = std::io::stderr().flush();
        }
    }
    if shown >= 0 {
        eprintln!();
    }
    Ok(())
}

// ---- run ----------------------------------------------------------------------------------------

/// Start an Ollama that local-brain just installed (or adopted), as a service that survives
/// reboots and carries the chosen models directory.
fn start_managed(models_dir: Option<&Path>) -> Result<()> {
    let bin = find_ollama().context("ollama binary not found after installation")?;
    if cfg!(target_os = "macos") {
        return install_mac_agent(&bin, models_dir);
    }
    if cfg!(target_os = "linux") && quiet_ok("systemctl", &["cat", "ollama"]) {
        if let Some(d) = models_dir {
            linux_models_override(d)?;
        }
        return run("sudo", &["systemctl", "enable", "--now", "ollama"]);
    }
    if ollama_up() {
        // Windows: the installer already started the app (with OLLAMA_MODELS set via setx).
        return Ok(());
    }
    spawn_serve(&bin, models_dir)
}

/// Start an Ollama the user installed earlier, the way it was installed.
fn start_existing(models_dir: &str) -> Result<()> {
    let dir = (!models_dir.is_empty()).then(|| PathBuf::from(models_dir));
    if cfg!(target_os = "macos") {
        let agent = mac_agent_path()?;
        if agent.exists() {
            return run("launchctl", &["load", "-w", &agent.to_string_lossy()]);
        }
        if let Some(app) = macos_app_candidates().into_iter().find(|p| p.exists()) {
            return run("open", &["-g", "-a", &app.to_string_lossy()]);
        }
        if has("brew") && quiet_ok("brew", &["list", "--formula", "ollama"]) {
            return run("brew", &["services", "start", "ollama"]);
        }
    } else if cfg!(target_os = "linux") && quiet_ok("systemctl", &["cat", "ollama"]) {
        return run("sudo", &["systemctl", "enable", "--now", "ollama"]);
    }
    let bin = find_ollama().context("ollama binary not found")?;
    spawn_serve(&bin, dir.as_deref())
}

fn mac_agent_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home directory")?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", MAC_AGENT)))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn mac_agent_plist(bin: &Path, models_dir: Option<&Path>, log: &Path) -> String {
    let env = match models_dir {
        Some(d) => format!(
            "\n  <key>EnvironmentVariables</key><dict><key>OLLAMA_MODELS</key><string>{}</string></dict>",
            xml_escape(&d.to_string_lossy())
        ),
        None => String::new(),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>{bin}</string><string>serve</string></array>{env}
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>ProcessType</key><string>Background</string>
</dict>
</plist>
"#,
        label = MAC_AGENT,
        bin = xml_escape(&bin.to_string_lossy()),
        env = env,
        log = xml_escape(&log.to_string_lossy()),
    )
}

/// A launchd agent running `ollama serve`: starts now and at every login.
fn install_mac_agent(bin: &Path, models_dir: Option<&Path>) -> Result<()> {
    paths::ensure_home()?;
    let p = mac_agent_path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(
        &p,
        mac_agent_plist(bin, models_dir, &paths::log_file("ollama.log")),
    )?;
    let _ = Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&p)
        .output();
    let out = Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&p)
        .output()?;
    if !out.status.success() {
        bail!(
            "launchctl load failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    ok(&format!("installed launchd agent {}", p.display()));
    Ok(())
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Point the systemd `ollama` service at the chosen directory (the service runs as user ollama).
fn linux_models_override(dir: &Path) -> Result<()> {
    check_models_dir(dir)?;
    let d = sh_quote(&dir.to_string_lossy());
    let script = format!(
        "sudo mkdir -p {d} && sudo chown -R ollama:ollama {d} && \
         sudo mkdir -p /etc/systemd/system/ollama.service.d && \
         printf '[Service]\\nEnvironment=\"OLLAMA_MODELS=%s\"\\n' {d} | sudo tee /etc/systemd/system/ollama.service.d/local-brain.conf >/dev/null && \
         sudo systemctl daemon-reload && sudo systemctl restart ollama",
        d = d
    );
    run("sh", &["-c", &script])
}

/// Fallback: run `ollama serve` detached from this terminal, logging to ~/.local-brain/logs.
fn spawn_serve(bin: &Path, models_dir: Option<&Path>) -> Result<()> {
    paths::ensure_home()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_file("ollama.log"))?;
    let mut cmd = Command::new(bin);
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if let Some(d) = models_dir {
        cmd.env("OLLAMA_MODELS", d);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().context("starting `ollama serve`")?;
    Ok(())
}

fn wait_up(timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if ollama_up() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!(
        "Ollama did not start within {}s; check {} and run `local-brain doctor`",
        timeout.as_secs(),
        paths::log_file("ollama.log").display()
    )
}

/// Used by `uninstall`: stop the Ollama service local-brain installed. Ollama itself and the
/// downloaded models stay; the returned message says where.
pub fn remove_service(models_dir: &str) -> Option<String> {
    let mut msg = None;
    if cfg!(target_os = "macos") {
        if let Ok(p) = mac_agent_path() {
            if p.exists() {
                let _ = Command::new("launchctl")
                    .args(["unload", "-w"])
                    .arg(&p)
                    .output();
                let _ = std::fs::remove_file(&p);
                msg = Some("stopped the Ollama service installed by local-brain".to_string());
            }
        }
    }
    if !models_dir.is_empty() {
        let m = format!(
            "Ollama and its models were kept; models are in {} (delete it to free the space)",
            models_dir
        );
        msg = Some(match msg {
            Some(s) => format!("{}. {}", s, m),
            None => m,
        });
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(label: &str, free: f64, internal: bool) -> Volume {
        Volume {
            label: label.into(),
            mount: PathBuf::from(format!("/Volumes/{}", label)),
            free_gb: free,
            internal,
        }
    }

    #[test]
    fn tier_models_are_deduplicated_and_sized() {
        let m = tier_models("S");
        assert!(!m.is_empty());
        let mut names: Vec<_> = m.iter().map(|(n, _)| n.clone()).collect();
        names.dedup();
        assert_eq!(names.len(), m.len());
        assert!(m.iter().all(|(_, gb)| *gb > 0.0));
    }

    #[test]
    fn skips_when_downloads_are_disabled() {
        let mut cfg = Config::default();
        cfg.runtime.provider = "none".into();
        cfg.network.allow_downloads = false;
        assert!(!ensure_runtime(&mut cfg, true, false).unwrap());
        assert_eq!(cfg.runtime.provider, "none");
        assert!(!ensure_runtime(&mut cfg, true, true).unwrap());
    }

    #[test]
    fn leaves_other_providers_alone() {
        let mut cfg = Config::default();
        cfg.runtime.provider = "lmstudio".into();
        assert!(!ensure_runtime(&mut cfg, true, false).unwrap());
        assert_eq!(cfg.runtime.provider, "lmstudio");
    }

    #[test]
    fn roomy_internal_disk_is_the_default() {
        let v = [vol("Internal", 200.0, true), vol("Ext", 900.0, false)];
        assert_eq!(default_volume(&v, 3.6), 0);
    }

    #[test]
    fn tight_internal_disk_defaults_to_largest_external() {
        let v = [
            vol("Internal", 20.0, true),
            vol("Small", 30.0, false),
            vol("Big", 800.0, false),
        ];
        assert_eq!(default_volume(&v, 3.6), 2);
        // An external drive that cannot hold the models is never the default.
        let v = [vol("Internal", 20.0, true), vol("Tiny", 4.0, false)];
        assert_eq!(default_volume(&v, 3.6), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn external_mounts_on_macos() {
        assert!(is_external_mount(Path::new("/Volumes/NVMe-SSD")));
        assert!(!is_external_mount(Path::new("/")));
        assert!(!is_external_mount(Path::new("/System/Volumes/Data")));
        assert!(!is_external_mount(Path::new(
            "/Volumes/.timemachine/abc/2026-10-17.backup"
        )));
    }

    #[test]
    fn agent_plist_carries_models_dir() {
        let p = mac_agent_plist(
            Path::new("/opt/homebrew/bin/ollama"),
            Some(Path::new("/Volumes/A & B/local-brain/ollama-models")),
            Path::new("/tmp/ollama.log"),
        );
        assert!(p.contains(
            "<key>OLLAMA_MODELS</key><string>/Volumes/A &amp; B/local-brain/ollama-models</string>"
        ));
        assert!(p.contains("<string>/opt/homebrew/bin/ollama</string><string>serve</string>"));
        let p = mac_agent_plist(Path::new("/x/ollama"), None, Path::new("/tmp/l"));
        assert!(!p.contains("OLLAMA_MODELS"));
    }

    #[test]
    fn models_dir_validation() {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let ok_dir = tmp.join(format!("lb-ok-{}", std::process::id()));
        std::fs::create_dir_all(&ok_dir).unwrap();
        assert!(check_models_dir(&ok_dir).is_ok());
        assert!(check_models_dir(Path::new("relative/dir")).is_err());
        for bad in ["a\"b", "a$b", "a%b", "a\nb", "a'b"] {
            let d = tmp.join(format!("lb-{}", bad));
            assert!(check_models_dir(&d).is_err(), "{:?}", d);
        }
        #[cfg(unix)]
        {
            let link = tmp.join(format!("lb-link-{}", std::process::id()));
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&ok_dir, &link).unwrap();
            assert!(check_models_dir(&link).is_err());
            std::fs::remove_file(&link).unwrap();
        }
        std::fs::remove_dir_all(&ok_dir).unwrap();
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote("/mnt/my drive"), "'/mnt/my drive'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }
}
