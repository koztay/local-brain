//! Global registry of initialized projects (`~/.local-brain/projects.json`), shared by the
//! CLI and the daemon.

use crate::paths;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub root: PathBuf,
    pub watch: bool,
    pub added: i64,
}

pub fn load() -> Vec<Entry> {
    std::fs::read_to_string(paths::registry())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(entries: &[Entry]) -> Result<()> {
    paths::ensure_home()?;
    let tmp = paths::registry().with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(entries)?)?;
    std::fs::rename(tmp, paths::registry())?;
    Ok(())
}

pub fn register(root: &Path) -> Result<()> {
    let mut e = load();
    if !e.iter().any(|x| x.root == root) {
        e.push(Entry {
            root: root.to_path_buf(),
            watch: false,
            added: crate::util::now(),
        });
        save(&e)?;
    }
    Ok(())
}

pub fn unregister(root: &Path) -> Result<()> {
    let mut e = load();
    e.retain(|x| x.root != root);
    save(&e)
}

pub fn set_watch(root: &Path, on: bool) -> Result<()> {
    let mut e = load();
    match e.iter_mut().find(|x| x.root == root) {
        Some(x) => x.watch = on,
        None => e.push(Entry {
            root: root.to_path_buf(),
            watch: on,
            added: crate::util::now(),
        }),
    }
    save(&e)
}

pub fn is_watched(root: &Path) -> bool {
    load().iter().any(|x| x.root == root && x.watch)
}
