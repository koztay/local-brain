//! Raw store for compressed outputs: the escape hatch behind every `raw_id`.

use crate::paths::{self, Project};
use crate::util;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn dir(project: Option<&Project>) -> PathBuf {
    match project {
        Some(p) => p.raw_dir(),
        None => paths::home().join("raw"),
    }
}

pub fn save(project: Option<&Project>, text: &str, ttl_days: i64) -> Result<String> {
    let d = dir(project);
    std::fs::create_dir_all(&d)?;
    let id = util::short_id();
    std::fs::write(d.join(format!("{}.log", id)), text)?;
    cleanup(&d, ttl_days);
    Ok(id)
}

fn cleanup(d: &std::path::Path, ttl_days: i64) {
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs((ttl_days.max(1) as u64) * 86_400);
    if let Ok(rd) = std::fs::read_dir(d) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                if m < cutoff {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

pub fn load(project: Option<&Project>, id: &str) -> Result<String> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric()) || id.is_empty() {
        bail!("invalid raw_id");
    }
    let mut candidates = vec![dir(project).join(format!("{}.log", id))];
    candidates.push(paths::home().join("raw").join(format!("{}.log", id)));
    for c in candidates {
        if c.exists() {
            return std::fs::read_to_string(&c).context("reading raw output");
        }
    }
    bail!(
        "raw_id {} not found (raw outputs expire after the configured TTL)",
        id
    )
}

/// Select part of a raw output: a regex `grep` (with context) and/or a line `range`.
pub fn select(
    text: &str,
    grep: Option<&str>,
    range: Option<&str>,
    context: usize,
    max_lines: usize,
) -> Result<String> {
    let lines: Vec<&str> = text.lines().collect();
    let (a, b) = match range {
        Some(r) => crate::slice::parse_lines(r).context("range must look like \"100-200\"")?,
        None => (1, lines.len()),
    };
    let b = b.min(lines.len());
    let mut out = String::new();
    let mut shown = 0;
    if let Some(g) = grep.filter(|g| !g.is_empty()) {
        let re = regex::RegexBuilder::new(g)
            .case_insensitive(true)
            .build()
            .context("invalid grep regex")?;
        let mut keep = vec![false; lines.len() + 1];
        let mut hits = 0;
        for i in a..=b {
            if re.is_match(lines[i - 1]) {
                hits += 1;
                let lo = i.saturating_sub(context).max(a);
                let hi = (i + context).min(b);
                keep[lo..=hi].iter_mut().for_each(|k| *k = true);
            }
        }
        let mut last = 0;
        for i in a..=b {
            if !keep[i] {
                continue;
            }
            if shown >= max_lines {
                out.push_str(&format!(
                    "… output capped at {} lines; narrow grep or range.\n",
                    max_lines
                ));
                break;
            }
            if last != 0 && i > last + 1 {
                out.push_str("  --\n");
            }
            out.push_str(&format!("{:>6}  {}\n", i, lines[i - 1]));
            last = i;
            shown += 1;
        }
        out.insert_str(
            0,
            &format!(
                "{} matching lines for /{}/ in {} total lines\n",
                hits,
                g,
                lines.len()
            ),
        );
    } else {
        for i in a..=b {
            if shown >= max_lines {
                out.push_str(&format!(
                    "… {} more lines; request range=\"{}-{}\".\n",
                    b - i + 1,
                    i,
                    b
                ));
                break;
            }
            out.push_str(&format!("{:>6}  {}\n", i, lines[i - 1]));
            shown += 1;
        }
    }
    Ok(out)
}
