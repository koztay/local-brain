//! Locations of the global home (`~/.local-brain`) and per-project stores (`<project>/.local-brain`).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub const PROJECT_DIR: &str = ".local-brain";

/// Global home. Override with `LOCAL_BRAIN_HOME` (useful for tests and sandboxes).
pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("LOCAL_BRAIN_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local-brain")
}

pub fn ensure_home() -> Result<PathBuf> {
    let h = home();
    std::fs::create_dir_all(h.join("logs")).with_context(|| format!("creating {}", h.display()))?;
    std::fs::create_dir_all(h.join("models"))?;
    Ok(h)
}

pub fn global_config() -> PathBuf {
    home().join("config.toml")
}

pub fn models_catalog() -> PathBuf {
    home().join("models.toml")
}

pub fn registry() -> PathBuf {
    home().join("projects.json")
}

pub fn log_file(name: &str) -> PathBuf {
    home().join("logs").join(name)
}

pub fn daemon_pid() -> PathBuf {
    home().join("daemon.pid")
}

pub fn daemon_status() -> PathBuf {
    home().join("daemon.json")
}

/// Per-project store paths.
#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
}

impl Project {
    pub fn new(root: PathBuf) -> Self {
        Project { root }
    }
    pub fn dir(&self) -> PathBuf {
        self.root.join(PROJECT_DIR)
    }
    pub fn db(&self) -> PathBuf {
        self.dir().join("brain.db")
    }
    pub fn raw_dir(&self) -> PathBuf {
        self.dir().join("raw")
    }
    pub fn config(&self) -> PathBuf {
        self.dir().join("config.toml")
    }
    pub fn memory_md(&self) -> PathBuf {
        self.dir().join("memory.md")
    }
    pub fn bench_dir(&self) -> PathBuf {
        self.dir().join("bench")
    }
    pub fn is_initialized(&self) -> bool {
        self.dir().is_dir()
    }
    /// Path relative to the project root, with forward slashes.
    pub fn rel(&self, p: &Path) -> String {
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        let p = normalize(&p);
        match p.strip_prefix(&self.root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => p.to_string_lossy().replace('\\', "/"),
        }
    }
    pub fn abs(&self, rel: &str) -> PathBuf {
        let p = Path::new(rel);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(rel)
        }
    }
}

/// Lexically normalize `.` and `..` components (does not touch the filesystem).
pub fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Walk up from `start` looking for an initialized project (a `.local-brain/` dir).
pub fn find_project(start: &Path) -> Option<Project> {
    let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let mut cur: Option<&Path> = Some(&start);
    while let Some(dir) = cur {
        let d = dir.join(PROJECT_DIR);
        if d.is_dir() && (d.join("brain.db").exists() || d.join("config.toml").exists()) {
            return Some(Project::new(dir.to_path_buf()));
        }
        cur = dir.parent();
    }
    None
}

/// Resolve the project for the current process: explicit path, `CLAUDE_PROJECT_DIR`, or cwd.
pub fn resolve_project(explicit: Option<&Path>) -> Result<Project> {
    if let Some(p) = explicit {
        if let Some(pr) = find_project(p) {
            return Ok(pr);
        }
        bail!(
            "{} is not a local-brain project (run `local-brain init` there first)",
            p.display()
        );
    }
    if let Ok(d) = std::env::var("CLAUDE_PROJECT_DIR") {
        if let Some(pr) = find_project(Path::new(&d)) {
            return Ok(pr);
        }
    }
    let cwd = std::env::current_dir()?;
    find_project(&cwd).ok_or_else(|| {
        anyhow::anyhow!("no local-brain project found here (run `local-brain init` first)")
    })
}

/// The git top-level for `dir`, if any.
pub fn git_root(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Absolute path of the running binary (used when writing hook/MCP configs).
pub fn exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "local-brain".into())
}
