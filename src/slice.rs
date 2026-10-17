//! Targeted code access: `brain_slice` (only the requested symbol or lines) and file
//! skeletons (signatures and structure) used when Claude tries to read a large file.

use crate::lang::{self, Lang};
use crate::paths::Project;
use crate::store::{Store, SymbolRow};
use crate::util;
use anyhow::{bail, Context, Result};

pub const MAX_SLICE_LINES: usize = 400;

pub struct Slice {
    pub text: String,
    pub full_tokens: i64,
}

fn symbols_for(store: &Store, project: &Project, rel: &str, src: &str) -> Result<Vec<SymbolRow>> {
    if let Some(f) = store.file(rel)? {
        let syms = store.symbols_in_file(f.id)?;
        let fresh = util::sha256_hex(src.as_bytes()) == f.hash;
        if fresh {
            return Ok(syms);
        }
    }
    // Not indexed yet or changed since: parse on the fly.
    let Some(l) = Lang::from_path(&project.abs(rel)) else {
        return Ok(vec![]);
    };
    Ok(lang::parse(l, src)
        .symbols
        .into_iter()
        .map(|s| SymbolRow {
            id: 0,
            file_id: 0,
            path: rel.to_string(),
            name: s.name,
            kind: s.kind,
            parent: s.parent,
            start_line: s.start as i64,
            end_line: s.end as i64,
            signature: s.signature,
            doc: s.doc,
        })
        .collect())
}

/// Extend a symbol's start upward over doc comments, attributes and decorators.
fn leading_start(lines: &[&str], start: usize) -> usize {
    let mut s = start;
    while s > 1 {
        let t = lines[s - 2].trim();
        let is_lead = t.starts_with("//")
            || t.starts_with("#[")
            || t.starts_with('@')
            || t.starts_with("/*")
            || t.starts_with('*')
            || (t.starts_with('#') && !t.starts_with("#include"));
        if !is_lead || start - s >= 30 {
            break;
        }
        s -= 1;
    }
    s
}

pub fn parse_lines(spec: &str) -> Option<(usize, usize)> {
    let spec = spec.trim();
    if let Some((a, b)) = spec.split_once(['-', ':', ',']) {
        let a: usize = a.trim().parse().ok()?;
        let b: usize = b.trim().parse().ok()?;
        Some((a.max(1), b.max(a)))
    } else {
        let a: usize = spec.parse().ok()?;
        Some((a.max(1), a + 40))
    }
}

pub fn slice(
    store: &Store,
    project: &Project,
    path: &str,
    symbol: Option<&str>,
    lines: Option<&str>,
) -> Result<Slice> {
    let rel = project.rel(std::path::Path::new(path));
    let abs = project.abs(&rel);
    if crate::scan::Filter::new(&[]).is_secret(&rel) {
        bail!(
            "{} looks like a secret file; local-brain will not read it",
            rel
        );
    }
    let src = std::fs::read_to_string(&abs).with_context(|| format!("reading {}", rel))?;
    let all: Vec<&str> = src.lines().collect();
    let full_tokens = util::tokens(&src);
    let mut out = String::new();

    if let Some(sym) = symbol.filter(|s| !s.trim().is_empty()) {
        let syms = symbols_for(store, project, &rel, &src)?;
        let (parent, name) = match sym.rsplit_once(['.', ':']) {
            Some((p, n)) => (Some(p.trim_end_matches(':')), n),
            None => (None, sym),
        };
        let mut hits: Vec<&SymbolRow> = syms
            .iter()
            .filter(|s| s.name == name && (parent.is_none() || s.parent.as_deref() == parent))
            .collect();
        if hits.is_empty() {
            hits = syms
                .iter()
                .filter(|s| s.name.eq_ignore_ascii_case(name))
                .collect();
        }
        if hits.is_empty() {
            let names: Vec<String> = syms.iter().take(40).map(|s| s.name.clone()).collect();
            bail!(
                "symbol `{}` not found in {}. Symbols here: {}",
                sym,
                rel,
                names.join(", ")
            );
        }
        for s in hits.iter().take(3) {
            let a = leading_start(&all, s.start_line as usize);
            let b = (s.end_line as usize).min(all.len());
            let (b, cut) = if b - a + 1 > MAX_SLICE_LINES {
                (a + MAX_SLICE_LINES - 1, true)
            } else {
                (b, false)
            };
            out.push_str(&format!("{}:{}-{} ({} {})\n", rel, a, b, s.kind, s.name));
            out.push_str(&util::numbered(&all[a - 1..b].join("\n"), a));
            if cut {
                out.push_str(&format!(
                    "… truncated at {} lines; request lines=\"{}-{}\" for the rest.\n",
                    MAX_SLICE_LINES,
                    b + 1,
                    s.end_line
                ));
            }
        }
        if hits.len() > 3 {
            out.push_str(&format!(
                "({} more matches for `{}` in this file)\n",
                hits.len() - 3,
                sym
            ));
        }
    } else {
        let (a, b) = match lines {
            Some(l) => parse_lines(l).context("lines must look like \"120-180\"")?,
            None => (1, all.len().min(MAX_SLICE_LINES)),
        };
        if a > all.len() {
            bail!("{} has only {} lines", rel, all.len());
        }
        let b = b.min(all.len()).min(a + MAX_SLICE_LINES - 1);
        out.push_str(&format!("{}:{}-{} (of {})\n", rel, a, b, all.len()));
        out.push_str(&util::numbered(&all[a - 1..b].join("\n"), a));
        if lines.is_none() && all.len() > b {
            out.push_str(&format!(
                "… {} more lines. Ask for a symbol or a line range.\n",
                all.len() - b
            ));
        }
    }
    Ok(Slice {
        text: crate::secrets::mask(&out),
        full_tokens,
    })
}

/// File outline: imports and symbol signatures with line ranges.
pub fn skeleton(store: &Store, project: &Project, rel: &str) -> Result<String> {
    let abs = project.abs(rel);
    let src = util::read_text(&abs).with_context(|| format!("reading {}", rel))?;
    let lines: Vec<&str> = src.lines().collect();
    let syms = symbols_for(store, project, rel, &src)?;
    let mut out = format!(
        "{} — {} lines, ~{} tokens\n",
        rel,
        lines.len(),
        util::tokens(&src)
    );
    if syms.is_empty() {
        // Prose/config: headings, else the first lines.
        let heads: Vec<String> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.starts_with('#') || l.starts_with('[') && l.trim_end().ends_with(']')
            })
            .take(60)
            .map(|(i, l)| format!("  L{} {}", i + 1, util::clip(l.trim(), 120)))
            .collect();
        if heads.is_empty() {
            for (i, l) in lines.iter().take(25).enumerate() {
                out.push_str(&format!("  L{} {}\n", i + 1, util::clip(l, 160)));
            }
        } else {
            out.push_str(&heads.join("\n"));
            out.push('\n');
        }
        return Ok(out);
    }
    if let Some(f) = store.file(rel)? {
        let imports: Vec<String> = store
            .edges_of_file(f.id)?
            .into_iter()
            .filter(|e| e.kind == "import")
            .map(|e| e.target)
            .collect();
        if !imports.is_empty() {
            out.push_str(&format!(
                "imports: {}\n",
                util::clip(&imports.join(", "), 400)
            ));
        }
    }
    for s in syms.iter().take(200) {
        let indent = if s.parent.is_some() && matches!(s.kind.as_str(), "method" | "function") {
            "    "
        } else {
            "  "
        };
        out.push_str(&format!(
            "{}L{}-{} {} {}{}\n",
            indent,
            s.start_line,
            s.end_line,
            s.kind,
            util::clip(&s.signature, 140),
            s.doc
                .as_ref()
                .map(|d| format!("  // {}", util::clip(d, 80)))
                .unwrap_or_default()
        ));
    }
    if syms.len() > 200 {
        out.push_str(&format!("  … {} more symbols\n", syms.len() - 200));
    }
    Ok(out)
}
