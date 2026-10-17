//! Update notifications and `local-brain update`. Once a day a detached background process asks
//! GitHub for the latest release (never slowing a command down); interactive commands then print
//! a one-line notice, and `init` offers to update right away. `update` upgrades the way the binary
//! was installed: Homebrew, cargo, or an in-place swap of a checksum-verified release archive.

use crate::config::Config;
use crate::daemon;
use crate::paths;
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const REPO: &str = "koztay/local-brain";
const CHECK_EVERY_SECS: i64 = 24 * 3600;
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

fn ok(s: &str) {
    eprintln!("  ✓ {}", s);
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    checked_at: i64,
    latest: String,
}

fn cache_path() -> PathBuf {
    paths::home().join("update-check.json")
}

fn load_cache() -> Cache {
    std::fs::read_to_string(cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(latest: &str) {
    let c = Cache {
        checked_at: util::now(),
        latest: latest.to_string(),
    };
    if paths::ensure_home().is_ok() {
        let _ = std::fs::write(cache_path(), serde_json::to_string(&c).unwrap_or_default());
    }
}

/// Update checks are on unless turned off in the config, by `LOCAL_BRAIN_NO_UPDATE_CHECK`, or in CI.
fn enabled() -> bool {
    if std::env::var_os("LOCAL_BRAIN_NO_UPDATE_CHECK").is_some() || std::env::var_os("CI").is_some()
    {
        return false;
    }
    Config::load_global().network.check_updates
}

/// `major.minor.patch` with an optional `v` prefix; a pre-release sorts before its release.
fn parse(v: &str) -> Option<(u64, u64, u64, bool)> {
    let v = v.trim().trim_start_matches('v');
    let (core, pre) = match v.split_once('-') {
        Some((c, _)) => (c, true),
        None => (v, false),
    };
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let (a, b, c) = (it.next()??, it.next()??, it.next()??);
    Some((a, b, c, !pre))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse(candidate), parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Ask GitHub for the latest release tag (e.g. "0.1.2").
fn fetch_latest(timeout: Duration) -> Result<String> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let v: serde_json::Value = agent
        .get(&format!(
            "https://api.github.com/repos/{}/releases/latest",
            REPO
        ))
        .set("User-Agent", &format!("local-brain/{}", CURRENT))
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| anyhow!("checking for updates: {}", e))?
        .into_json()?;
    let tag = v["tag_name"]
        .as_str()
        .context("no tag_name in the latest release")?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// Hidden `__update-check` command: refresh the cache (runs detached from the user's command).
pub fn refresh() {
    if let Ok(latest) = fetch_latest(Duration::from_secs(10)) {
        save_cache(&latest);
    }
}

/// Start the refresher in the background if the cached answer is older than a day.
fn spawn_refresh_if_stale(cache: &Cache) {
    if util::now() - cache.checked_at < CHECK_EVERY_SECS {
        return;
    }
    // Record the attempt first so a failing network does not spawn a refresher on every command.
    save_cache(&cache.latest);
    let mut cmd = Command::new(paths::exe());
    cmd.arg("__update-check")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let _ = cmd.spawn();
}

fn on_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// Called after interactive commands: print a notice when a newer release is known.
/// Silent when stderr is not a terminal (hooks, MCP, Claude Code's Bash tool, scripts).
pub fn notify() {
    if !on_terminal() || !enabled() {
        return;
    }
    let cache = load_cache();
    if is_newer(&cache.latest, CURRENT) {
        eprintln!(
            "\nA new version of local-brain is available: {} → {}. Run `local-brain update` to install it.",
            CURRENT, cache.latest
        );
    }
    spawn_refresh_if_stale(&cache);
}

/// `init` checks synchronously (briefly) and offers to update before setting anything up.
/// Returns true when it updated, so the caller can stop and ask for a re-run.
pub fn offer_in_init(yes: bool) -> bool {
    use std::io::IsTerminal;
    if !enabled() || !std::io::stdin().is_terminal() {
        return false;
    }
    let mut cache = load_cache();
    if util::now() - cache.checked_at >= 3600 {
        if let Ok(latest) = fetch_latest(Duration::from_secs(3)) {
            save_cache(&latest);
            cache.latest = latest;
        }
    }
    if !is_newer(&cache.latest, CURRENT) {
        return false;
    }
    let q = format!(
        "local-brain {} is available (you have {}). Update now?",
        cache.latest, CURRENT
    );
    if !yes && !util::confirm(&q, true) {
        return false;
    }
    match update(false) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("  ! update failed: {:#}; continuing with {}", e, CURRENT);
            false
        }
    }
}

#[derive(Debug, PartialEq)]
enum Method {
    Homebrew,
    Cargo,
    Direct(PathBuf),
}

fn install_method(real_exe: &Path) -> Method {
    let s = real_exe.to_string_lossy();
    if s.contains("/Cellar/local-brain/") {
        return Method::Homebrew;
    }
    if let Some(home) = dirs::home_dir() {
        let cargo_bin = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cargo"))
            .join("bin");
        if real_exe.parent() == Some(cargo_bin.as_path()) {
            return Method::Cargo;
        }
    }
    Method::Direct(real_exe.to_path_buf())
}

/// `local-brain update`: install the latest release (or reinstall it with `force`).
pub fn update(force: bool) -> Result<()> {
    let latest = fetch_latest(Duration::from_secs(15))?;
    save_cache(&latest);
    if !force && !is_newer(&latest, CURRENT) {
        ok(&format!("local-brain {} is the latest version", CURRENT));
        return Ok(());
    }
    let real = std::env::current_exe()?.canonicalize()?;
    if is_newer(&latest, CURRENT) {
        eprintln!("Updating local-brain {} → {}…", CURRENT, latest);
    } else if latest == CURRENT {
        eprintln!("Reinstalling local-brain {}…", latest);
    } else {
        eprintln!("Installing local-brain {} (you have {})…", latest, CURRENT);
    }
    match install_method(&real) {
        Method::Homebrew => {
            let formula = format!("{}/local-brain", REPO);
            if force {
                run("brew", &["reinstall", &formula])?;
            } else {
                run("brew", &["upgrade", &formula])?;
            }
        }
        Method::Cargo => run(
            "cargo",
            &[
                "install",
                "--git",
                &format!("https://github.com/{}", REPO),
                "--tag",
                &format!("v{}", latest),
                "--locked",
                "--force",
            ],
        )?,
        Method::Direct(path) => replace_binary(&path, &latest)?,
    }
    ok(&format!("updated to local-brain {}", latest));
    restart_daemon();
    Ok(())
}

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

/// The running daemon is still the old binary; restart it so it picks up the new one.
fn restart_daemon() {
    if daemon::is_running().is_some() {
        let _ = daemon::stop();
        if let Ok(pid) = daemon::start() {
            ok(&format!("restarted the daemon (pid {})", pid));
        }
    }
}

fn release_target() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        (os, arch) => bail!("no prebuilt release for {} {}; build from source", os, arch),
    })
}

fn sha256_file(p: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut f = std::fs::File::open(p)?;
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// Download the release archive, check it against SHA256SUMS and swap the binary in place.
fn replace_binary(exe: &Path, version: &str) -> Result<()> {
    let target = release_target()?;
    let archive = if cfg!(windows) {
        format!("local-brain-{}.zip", target)
    } else {
        format!("local-brain-{}.tar.gz", target)
    };
    let base = format!("https://github.com/{}/releases/download/v{}", REPO, version);
    let work = crate::setup::private_temp_dir()?;
    let result = (|| -> Result<()> {
        let sums = work.join("SHA256SUMS");
        let arc = work.join(&archive);
        crate::setup::download(&format!("{}/SHA256SUMS", base), &sums, "SHA256SUMS")?;
        crate::setup::download(&format!("{}/{}", base, archive), &arc, &archive)?;
        let expected = std::fs::read_to_string(&sums)?
            .lines()
            .find_map(|l| {
                let (sum, name) = l.split_once(char::is_whitespace)?;
                (name.trim().trim_start_matches('*') == archive).then(|| sum.to_lowercase())
            })
            .with_context(|| format!("{} is not listed in SHA256SUMS", archive))?;
        if sha256_file(&arc)? != expected {
            bail!("checksum mismatch for {}; not installing it", archive);
        }
        ok("checksum verified");
        let out = work.join("out");
        std::fs::create_dir(&out)?;
        // bsdtar ships with macOS, Linux distributions and Windows 10+, and reads .zip too.
        let unpacked = Command::new("tar")
            .args(["-xf", &arc.to_string_lossy(), "-C", &out.to_string_lossy()])
            .status()
            .context("running tar")?;
        if !unpacked.success() {
            bail!("could not unpack {}", archive);
        }
        let name = if cfg!(windows) {
            "local-brain.exe"
        } else {
            "local-brain"
        };
        let new_bin = out.join(name);
        if !new_bin.is_file() {
            bail!("{} does not contain {}", archive, name);
        }
        if cfg!(target_os = "macos")
            && !Command::new("codesign")
                .args(["--verify", "--strict"])
                .arg(&new_bin)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        {
            bail!("the downloaded binary has an invalid code signature; not installing it");
        }
        swap_in(&new_bin, exe)
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// Copy next to the target first (same filesystem), then rename over it. On Unix the running
/// process keeps its old inode; Windows cannot overwrite a running .exe but can rename it away.
fn swap_in(new_bin: &Path, exe: &Path) -> Result<()> {
    let dir = exe.parent().context("binary has no parent directory")?;
    let staged = dir.join(".local-brain.new");
    std::fs::copy(new_bin, &staged)
        .with_context(|| format!("writing to {} (permission?)", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    if cfg!(windows) {
        let old = dir.join("local-brain.old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old)?;
    }
    std::fs::rename(&staged, exe)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.2.0", "0.2.0-rc.1"));
        assert!(!is_newer("0.2.0-rc.1", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
    }

    #[test]
    fn detects_install_method() {
        assert_eq!(
            install_method(Path::new(
                "/opt/homebrew/Cellar/local-brain/0.1.1/bin/local-brain"
            )),
            Method::Homebrew
        );
        assert_eq!(
            install_method(Path::new("/home/u/.local/bin/local-brain")),
            Method::Direct(PathBuf::from("/home/u/.local/bin/local-brain"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn swap_in_replaces_the_file() {
        let d = std::env::temp_dir().join(format!("lb-swap-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let exe = d.join("local-brain");
        let new = d.join("new-bin");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        swap_in(&new, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!d.join(".local-brain.new").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
