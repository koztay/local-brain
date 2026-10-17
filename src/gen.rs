//! Low-risk generation delegated to the local model: commit messages, PR descriptions,
//! documentation suggestions and a pre-review. Every command has a deterministic fallback
//! and none of them writes to your files on its own.

use crate::config::Config;
use crate::llm::Llm;
use crate::paths::Project;
use crate::store::Store;
use crate::util;
use anyhow::{bail, Context, Result};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};

pub fn git(root: &std::path::Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Shrink a diff for a prompt: keep headers and changed lines, cap per file.
fn condense_diff(diff: &str, budget_tokens: i64) -> String {
    let mut out = String::new();
    let mut per_file = 0;
    for l in diff.lines() {
        if l.starts_with("diff --git") {
            per_file = 0;
            out.push_str(l);
            out.push('\n');
            continue;
        }
        if l.starts_with("index ") || l.starts_with("--- ") || l.starts_with("+++ ") {
            continue;
        }
        if l.starts_with("@@") || l.starts_with('+') || l.starts_with('-') {
            per_file += 1;
            if per_file <= 120 {
                out.push_str(&util::clip(l, 200));
                out.push('\n');
            } else if per_file == 121 {
                out.push_str("… (more changes in this file)\n");
            }
        }
    }
    util::clip_tokens(&crate::secrets::mask(&out), budget_tokens).0
}

fn changed_files(diff: &str) -> Vec<String> {
    diff.lines()
        .filter_map(|l| l.strip_prefix("diff --git a/"))
        .filter_map(|l| l.split(" b/").nth(1))
        .map(String::from)
        .collect()
}

fn file_summaries(store: Option<&Store>, files: &[String]) -> String {
    let Some(st) = store else {
        return String::new();
    };
    let mut out = String::new();
    for f in files.iter().take(12) {
        if let Ok(Some((s, ..))) = st.summary("file", f) {
            out.push_str(&format!(
                "- {}: {}\n",
                f,
                util::clip(&s.replace('\n', " "), 200)
            ));
        }
    }
    out
}

fn conventional_type(files: &[String], diff: &str) -> &'static str {
    let all = |pred: &dyn Fn(&str) -> bool| !files.is_empty() && files.iter().all(|f| pred(f));
    if all(&|f| {
        f.ends_with(".md") || f.starts_with("docs/") || f.ends_with(".txt") || f.ends_with(".rst")
    }) {
        "docs"
    } else if all(&|f| f.contains("test") || f.contains("spec")) {
        "test"
    } else if all(&|f| {
        f.starts_with(".github/")
            || f.contains("Dockerfile")
            || f.ends_with(".yml")
            || f.ends_with(".yaml")
    }) {
        "ci"
    } else if all(&|f| {
        f.ends_with(".toml")
            || f.ends_with(".json")
            || f.ends_with(".lock")
            || f.contains("package")
    }) {
        "chore"
    } else {
        let added = diff
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .count();
        let removed = diff
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .count();
        let lower = diff.to_lowercase();
        if lower.contains("fix") || lower.contains("bug") {
            "fix"
        } else if removed > added * 2 {
            "refactor"
        } else {
            "feat"
        }
    }
}

fn scope_of(files: &[String]) -> Option<String> {
    let tops: BTreeSet<String> = files
        .iter()
        .map(|f| {
            let parts: Vec<&str> = f.split('/').collect();
            if parts.len() >= 3
                && (parts[0] == "src"
                    || parts[0] == "lib"
                    || parts[0] == "packages"
                    || parts[0] == "apps")
            {
                parts[1].trim_end_matches(".rs").to_string()
            } else if parts.len() >= 2 {
                parts[0].to_string()
            } else {
                std::path::Path::new(parts[0])
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default()
            }
        })
        .collect();
    if tops.len() == 1 {
        tops.into_iter().next()
    } else {
        None
    }
}

fn clean_message(s: &str) -> String {
    let s = s
        .trim()
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let s = s.strip_prefix("text").unwrap_or(s).trim();
    s.lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Generate a Conventional Commits message for the staged changes.
pub fn commit_message(project: &Project, cfg: &Config, store: Option<&Store>) -> Result<String> {
    let diff = git(
        &project.root,
        &["diff", "--cached", "--no-color", "--no-ext-diff"],
    )?;
    if diff.trim().is_empty() {
        bail!("nothing is staged (git add your changes first)");
    }
    let files = changed_files(&diff);
    let llm = Llm::new(cfg);
    if llm.has_text() {
        let prompt = format!(
            "Write a git commit message in Conventional Commits format for this staged diff.\n\
             Rules: first line `type(scope): summary` (types: feat, fix, refactor, perf, docs, test, build, ci, chore), \
             imperative mood, at most 72 characters. Then a blank line and 1-4 short bullet points on what changed and why. \
             Output only the message.\n\nContext (file summaries):\n{}\nDiff:\n{}",
            file_summaries(store, &files),
            condense_diff(&diff, 5000)
        );
        if let Ok(m) = llm.code(&prompt, 300) {
            let m = clean_message(&m);
            // Git convention: a blank line between subject and body.
            let m = match m.split_once('\n') {
                Some((subject, body)) if !body.starts_with('\n') => {
                    format!("{}\n\n{}", subject, body.trim_start())
                }
                _ => m,
            };
            let first = m.lines().next().unwrap_or("");
            let re = Regex::new(r"^(feat|fix|refactor|perf|docs|test|build|ci|chore|style|revert)(\([^)]+\))?!?: .+").unwrap();
            if re.is_match(first) {
                return Ok(m);
            }
        }
    }
    let ty = conventional_type(&files, &diff);
    let scope = scope_of(&files)
        .map(|s| format!("({})", s))
        .unwrap_or_default();
    let names: Vec<String> = files
        .iter()
        .map(|f| f.rsplit('/').next().unwrap_or(f).to_string())
        .collect();
    let subject = if names.len() <= 3 {
        format!("update {}", names.join(", "))
    } else {
        format!("update {} files", names.len())
    };
    let stat =
        git(&project.root, &["diff", "--cached", "--stat", "--no-color"]).unwrap_or_default();
    Ok(format!(
        "{}{}: {}\n\n{}",
        ty,
        scope,
        util::clip(&subject, 60),
        stat.lines()
            .map(|l| format!("- {}", l.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

pub fn default_base(root: &std::path::Path) -> String {
    if let Ok(s) = git(
        root,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    for b in ["main", "master", "origin/main", "origin/master"] {
        if git(root, &["rev-parse", "--verify", "--quiet", b]).is_ok() {
            return b.to_string();
        }
    }
    "HEAD~1".into()
}

/// Title + body for a pull request from the branch diff and the decision log.
pub fn pr_description(
    project: &Project,
    cfg: &Config,
    store: Option<&Store>,
    base: Option<&str>,
) -> Result<String> {
    let base = base
        .map(String::from)
        .unwrap_or_else(|| default_base(&project.root));
    let range = format!("{}...HEAD", base);
    let log = git(
        &project.root,
        &[
            "log",
            "--no-color",
            "--format=- %s",
            &format!("{}..HEAD", base),
        ],
    )?;
    let diff = git(
        &project.root,
        &["diff", "--no-color", "--no-ext-diff", &range],
    )?;
    if diff.trim().is_empty() {
        bail!("no changes between {} and HEAD", base);
    }
    let stat = git(&project.root, &["diff", "--stat", "--no-color", &range])?;
    let files = changed_files(&diff);
    let decisions = match store {
        Some(st) => crate::memory::list(st, Some("decision"), 10)?
            .into_iter()
            .map(|d| {
                format!(
                    "- {}{}",
                    d.text,
                    d.why.map(|w| format!(" (why: {})", w)).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    };
    let llm = Llm::new(cfg);
    if llm.has_text() {
        let prompt = format!(
            "Write a pull request description in Markdown. Start with a line `Title: <concise title>`, then sections \
             `## Summary` (2-4 sentences), `## Changes` (bullets), `## Decisions` (only if relevant decisions are listed below), \
             `## Testing` (how to verify). Be factual; do not invent tests that are not in the diff.\n\n\
             Commits:\n{}\nDiffstat:\n{}\nRecent decisions:\n{}\nFile summaries:\n{}\nDiff:\n{}",
            log,
            stat,
            decisions,
            file_summaries(store, &files),
            condense_diff(&diff, 5000)
        );
        if let Ok(m) = llm.code(&prompt, 700) {
            let m = clean_message(&m);
            if !m.is_empty() {
                return Ok(m);
            }
        }
    }
    let ty = conventional_type(&files, &diff);
    let first_commit = log
        .lines()
        .next()
        .map(|l| l.trim_start_matches("- ").to_string());
    let title = first_commit.unwrap_or_else(|| format!("{}: update {} files", ty, files.len()));
    Ok(format!(
        "Title: {}\n\n## Summary\nChanges on this branch compared to `{}`.\n\n## Commits\n{}\n## Changes\n```\n{}```\n{}## Testing\n- [ ] Describe how this was verified\n",
        title,
        base,
        log,
        stat,
        if decisions.is_empty() { String::new() } else { format!("\n## Decisions\n{}\n\n", decisions) }
    ))
}

pub struct Finding {
    pub severity: &'static str,
    pub location: String,
    pub message: String,
}

/// Deterministic pre-review checks plus an optional local-LLM pass.
pub fn review(
    project: &Project,
    cfg: &Config,
    store: Option<&Store>,
    base: Option<&str>,
) -> Result<String> {
    let (diff, label) = match base {
        Some(b) => (
            git(
                &project.root,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "-U0",
                    &format!("{}...HEAD", b),
                ],
            )?,
            format!("{}...HEAD", b),
        ),
        None => {
            let staged = git(
                &project.root,
                &["diff", "--cached", "--no-color", "--no-ext-diff", "-U0"],
            )?;
            if staged.trim().is_empty() {
                (
                    git(
                        &project.root,
                        &["diff", "--no-color", "--no-ext-diff", "-U0"],
                    )?,
                    "working tree".to_string(),
                )
            } else {
                (staged, "staged changes".to_string())
            }
        }
    };
    if diff.trim().is_empty() {
        return Ok("Nothing to review (no changes).".into());
    }
    let mut findings: Vec<Finding> = Vec::new();
    let mut file = String::new();
    let mut line_no = 0usize;
    let mut added_names: BTreeMap<String, String> = BTreeMap::new();
    let mut removed_names: BTreeSet<String> = BTreeSet::new();
    let debug_re = Regex::new(r"\b(console\.log|debugger;|dbg!\(|println!\(\s*\x22DEBUG|breakpoint\(\)|pdb\.set_trace|binding\.pry|var_dump\(|print\(\s*f?\x22DEBUG)").unwrap();
    let todo_re = Regex::new(r"\b(TODO|FIXME|XXX|HACK)\b").unwrap();
    let def_re = Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:export\s+)?(?:async\s+)?(?:fn|def|func|function|class|struct|enum|trait|interface)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    for l in diff.lines() {
        if let Some(rest) = l.strip_prefix("+++ b/") {
            file = rest.to_string();
            continue;
        }
        if l.starts_with("+++") || l.starts_with("---") {
            continue;
        }
        if let Some(h) = l.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@
            if let Some(plus) = h.split_whitespace().find(|p| p.starts_with('+')) {
                line_no = plus
                    .trim_start_matches('+')
                    .split(',')
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            }
            continue;
        }
        if let Some(added) = l.strip_prefix('+') {
            let loc = format!("{}:{}", file, line_no);
            if crate::secrets::mask(added) != added {
                findings.push(Finding {
                    severity: "high",
                    location: loc.clone(),
                    message: "possible secret added (API key, token or password)".into(),
                });
            }
            if added.starts_with("<<<<<<<") || added.starts_with(">>>>>>>") || added == "=======" {
                findings.push(Finding {
                    severity: "high",
                    location: loc.clone(),
                    message: "merge conflict marker".into(),
                });
            }
            if debug_re.is_match(added) && !file.contains("test") {
                findings.push(Finding {
                    severity: "medium",
                    location: loc.clone(),
                    message: format!("debug statement: `{}`", util::clip(added.trim(), 80)),
                });
            }
            if todo_re.is_match(added) {
                findings.push(Finding {
                    severity: "low",
                    location: loc.clone(),
                    message: format!("new {}", util::clip(added.trim(), 100)),
                });
            }
            if let Some(c) = def_re.captures(added) {
                added_names.insert(c[1].to_string(), loc.clone());
            }
            line_no += 1;
        } else if let Some(removed) = l.strip_prefix('-') {
            if let Some(c) = def_re.captures(removed) {
                removed_names.insert(c[1].to_string());
            }
        }
    }
    // Graph consistency: symbols deleted by this change that are still referenced elsewhere.
    if let Some(st) = store {
        for name in removed_names
            .iter()
            .filter(|n| !added_names.contains_key(*n))
        {
            let still_defined = std::process::Command::new("git")
                .arg("-C")
                .arg(&project.root)
                .args([
                    "grep",
                    "-n",
                    "-w",
                    "-E",
                    &format!(
                        r"(fn|def|func|function|class|struct|enum|trait|interface)\s+{}\b",
                        regex::escape(name)
                    ),
                ])
                .output()
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(false);
            if still_defined {
                continue;
            }
            let refs = st.edges_to(name, "call")?;
            let live: Vec<_> = refs
                .into_iter()
                .filter(|e| project.abs(&e.path).exists())
                .collect();
            if !live.is_empty() {
                let locs: Vec<String> = live
                    .iter()
                    .take(5)
                    .map(|e| format!("{}:{}", e.path, e.line))
                    .collect();
                findings.push(Finding {
                    severity: "high",
                    location: locs[0].clone(),
                    message: format!(
                        "`{}` was removed but is still called at {}",
                        name,
                        locs.join(", ")
                    ),
                });
            }
        }
        // Possibly unused new symbols.
        for (name, loc) in &added_names {
            if name.len() < 4 || name == "main" || name.starts_with("test") {
                continue;
            }
            let calls = st.edges_to(name, "call")?.len() + st.edges_to(name, "inherit")?.len();
            let mentioned = std::process::Command::new("git")
                .arg("-C")
                .arg(&project.root)
                .args(["grep", "-c", "-w", name])
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter_map(|l| l.rsplit(':').next()?.parse::<usize>().ok())
                        .sum::<usize>()
                })
                .unwrap_or(2);
            if calls == 0 && mentioned <= 1 {
                findings.push(Finding {
                    severity: "low",
                    location: loc.clone(),
                    message: format!("`{}` is new and not referenced anywhere (unused?)", name),
                });
            }
        }
    }

    let mut out = format!(
        "local-brain pre-review of {} ({} files)\n",
        label,
        changed_files(&diff)
            .len()
            .max(diff.matches("+++ b/").count())
    );
    findings.sort_by_key(|f| match f.severity {
        "high" => 0,
        "medium" => 1,
        _ => 2,
    });
    if findings.is_empty() {
        out.push_str("Rule checks: no issues found.\n");
    } else {
        out.push_str("Rule checks:\n");
        for f in findings.iter().take(60) {
            out.push_str(&format!(
                "  [{}] {} — {}\n",
                f.severity, f.location, f.message
            ));
        }
    }
    let llm = Llm::new(cfg);
    if llm.has_text() {
        let prompt = format!(
            "You are reviewing a code change before a human does. List only concrete, likely bugs or risky changes \
             (wrong logic, missing error handling, off-by-one, null/None handling, resource leaks, security). \
             Use the format `- path:line — issue`. At most 8 items. If nothing stands out, answer `No obvious issues.`\n\nDiff:\n{}",
            condense_diff(&diff, 5000)
        );
        if let Ok(r) = llm.code(&prompt, 500) {
            out.push_str("\nLocal model review (verify before acting):\n");
            out.push_str(&clean_message(&r));
            out.push('\n');
        }
    }
    Ok(out)
}

/// Documentation suggestions: missing docstrings for public symbols in changed files and a
/// CHANGELOG entry. Never writes files.
pub fn docs_suggestions(
    project: &Project,
    cfg: &Config,
    store: &Store,
    path: Option<&str>,
) -> Result<String> {
    let files: Vec<String> = match path {
        Some(p) => vec![project.rel(std::path::Path::new(p))],
        None => {
            let d = git(&project.root, &["diff", "--name-only", "HEAD"]).unwrap_or_default();
            let mut v: Vec<String> = d.lines().map(String::from).collect();
            if v.is_empty() {
                v = git(&project.root, &["diff", "--name-only", "HEAD~1"])
                    .unwrap_or_default()
                    .lines()
                    .map(String::from)
                    .collect();
            }
            v
        }
    };
    let llm = Llm::new(cfg);
    let mut out =
        String::from("Documentation suggestions (nothing was written; copy what you want):\n");
    let mut count = 0;
    for f in files.iter().take(20) {
        let Some(row) = store.file(f)? else { continue };
        let syms = store.symbols_in_file(row.id)?;
        let src = std::fs::read_to_string(project.abs(f)).unwrap_or_default();
        let lines: Vec<&str> = src.lines().collect();
        for s in syms.iter().filter(|s| s.doc.is_none()) {
            let line = lines.get(s.start_line as usize - 1).copied().unwrap_or("");
            let public = line.contains("pub ")
                || line.contains("export ")
                || (row.lang.as_deref() == Some("python") && !s.name.starts_with('_'))
                || (row.lang.as_deref() == Some("go")
                    && s.name
                        .chars()
                        .next()
                        .map(|c| c.is_uppercase())
                        .unwrap_or(false));
            if !public || matches!(s.kind.as_str(), "const" | "static") {
                continue;
            }
            count += 1;
            if count > 25 {
                break;
            }
            let body = crate::learn::slice_lines(
                &src,
                s.start_line as usize,
                (s.end_line as usize).min(s.start_line as usize + 60),
            );
            let suggestion = if llm.has_text() {
                llm.code(&format!("Write a concise doc comment (1-3 lines) for this {} in the idiomatic style of its language. Output only the comment.\n\n{}", row.lang.clone().unwrap_or_default(), body), 120)
                    .map(|c| clean_message(&c))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            out.push_str(&format!(
                "\n{}:{} {} `{}` has no doc comment.\n",
                f, s.start_line, s.kind, s.name
            ));
            if !suggestion.is_empty() {
                out.push_str(&format!(
                    "  suggested:\n{}\n",
                    suggestion
                        .lines()
                        .map(|l| format!("    {}", l))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
        }
    }
    if count == 0 {
        out.push_str("\nAll public symbols in the changed files have doc comments.\n");
    }
    // CHANGELOG suggestion from commits since the last tag.
    let since = git(&project.root, &["describe", "--tags", "--abbrev=0"])
        .ok()
        .map(|s| s.trim().to_string());
    let range = since
        .as_ref()
        .map(|t| format!("{}..HEAD", t))
        .unwrap_or_else(|| "HEAD~20..HEAD".into());
    if let Ok(log) = git(&project.root, &["log", "--no-color", "--format=%s", &range]) {
        if !log.trim().is_empty() {
            let mut groups: BTreeMap<&str, Vec<String>> = BTreeMap::new();
            for l in log.lines() {
                let key = if l.starts_with("feat") {
                    "Added"
                } else if l.starts_with("fix") {
                    "Fixed"
                } else if l.starts_with("docs") {
                    "Documentation"
                } else {
                    "Changed"
                };
                let msg = l.split_once(": ").map(|x| x.1).unwrap_or(l);
                groups.entry(key).or_default().push(msg.to_string());
            }
            out.push_str(&format!(
                "\nCHANGELOG entry (commits {}):\n## [Unreleased]\n",
                range
            ));
            for (k, v) in groups {
                out.push_str(&format!("### {}\n", k));
                for m in v {
                    out.push_str(&format!("- {}\n", m));
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_types() {
        assert_eq!(conventional_type(&["README.md".into()], ""), "docs");
        assert_eq!(
            conventional_type(&["src/a.rs".into()], "+fn x() {}\n"),
            "feat"
        );
        assert_eq!(
            scope_of(&["src/store/a.rs".into(), "src/store/b.rs".into()]).as_deref(),
            Some("store")
        );
    }
}
