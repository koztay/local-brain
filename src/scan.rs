//! Project file discovery: respects .gitignore, built-in exclusions, secret files and
//! user-defined exclude patterns.

use crate::paths::Project;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Code,
    Text,
    Doc,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Code => "code",
            Kind::Text => "text",
            Kind::Doc => "doc",
        }
    }
}

/// Directories that are never indexed even when not gitignored.
const DEFAULT_EXCLUDES: &[&str] = &[
    "**/.git/**",
    "**/.local-brain/**",
    "**/node_modules/**",
    "**/target/**",
    "**/dist/**",
    "**/build/**",
    "**/.next/**",
    "**/.venv/**",
    "**/venv/**",
    "**/__pycache__/**",
    "**/vendor/**",
    "**/.gradle/**",
    "**/.idea/**",
    "**/.build/**",
    "**/Pods/**",
    "**/DerivedData/**",
    "**/coverage/**",
    "**/*.min.js",
    "**/*.min.css",
    "**/*.map",
    "**/*.lock",
    "**/package-lock.json",
    "**/pnpm-lock.yaml",
    "**/yarn.lock",
    "**/go.sum",
];

/// Secret-bearing files: never indexed, never read by hooks, never summarized.
const SECRET_PATTERNS: &[&str] = &[
    "**/.env",
    "**/.env.*",
    "**/*.env",
    "**/*.pem",
    "**/*.key",
    "**/*.p12",
    "**/*.pfx",
    "**/*.jks",
    "**/*.keystore",
    "**/*.kdbx",
    "**/id_rsa*",
    "**/id_ed25519*",
    "**/id_ecdsa*",
    "**/.netrc",
    "**/.npmrc",
    "**/.pypirc",
    "**/credentials",
    "**/credentials.{json,yaml,yml,xml,csv,ini,toml,txt}",
    "**/secrets.{json,yaml,yml,toml,ini,txt,env}",
    "**/*secret*.json",
    "**/service-account*.json",
    "**/*.tfstate",
    "**/*.tfvars",
];

pub fn secret_globs() -> GlobSet {
    build(SECRET_PATTERNS.iter().map(|s| s.to_string()))
}

fn build<I: IntoIterator<Item = String>>(pats: I) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in pats {
        let p = if p.starts_with("**/") || p.starts_with('/') {
            p
        } else {
            format!("**/{}", p.trim_start_matches("./"))
        };
        if let Ok(g) = Glob::new(&p) {
            b.add(g);
        }
        // `dir/` style patterns should match the directory's contents.
        if !p.ends_with("/**") && !p.contains('.') {
            if let Ok(g) = Glob::new(&format!("{}/**", p.trim_end_matches('/'))) {
                b.add(g);
            }
        }
    }
    b.build().unwrap_or_else(|_| GlobSet::empty())
}

pub struct Filter {
    excludes: GlobSet,
    secrets: GlobSet,
}

impl Filter {
    pub fn new(user_excludes: &[String]) -> Filter {
        let all = DEFAULT_EXCLUDES
            .iter()
            .map(|s| s.to_string())
            .chain(user_excludes.iter().cloned());
        Filter {
            excludes: build(all),
            secrets: secret_globs(),
        }
    }

    pub fn is_secret(&self, rel: &str) -> bool {
        self.secrets.is_match(rel) || self.secrets.is_match(format!("/{}", rel))
    }

    pub fn is_excluded(&self, rel: &str) -> bool {
        let r = format!("/{}", rel);
        self.excludes.is_match(rel) || self.excludes.is_match(&r) || self.is_secret(rel)
    }
}

pub fn classify(path: &Path) -> Option<Kind> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if crate::lang::Lang::from_path(path).is_some() {
        return Some(Kind::Code);
    }
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "pdf" => Some(Kind::Doc),
        // Other source languages: embedded and summarized, but no AST graph.
        "c" | "h" | "cc" | "cpp" | "hpp" | "cxx" | "cs" | "java" | "rb" | "php" | "scala"
        | "dart" | "lua" | "ex" | "exs" | "erl" | "hs" | "ml" | "clj" | "r" | "jl" | "zig"
        | "nim" | "vue" | "svelte" | "sql" | "sh" | "bash" | "zsh" | "fish" | "ps1" | "m"
        | "mm" | "gradle" | "groovy" | "proto" | "graphql" | "gql" | "tf" | "hcl" | "css"
        | "scss" | "less" | "html" | "htm" | "xml" | "astro" => Some(Kind::Code),
        "md" | "mdx" | "txt" | "rst" | "adoc" | "toml" | "yaml" | "yml" | "json" | "ini"
        | "cfg" | "conf" | "csv" => Some(Kind::Text),
        _ => match name.as_str() {
            "makefile" | "dockerfile" | "justfile" | "rakefile" | "gemfile" | "procfile"
            | "readme" | "license" | "changelog" => Some(Kind::Text),
            _ => None,
        },
    }
}

pub struct Entry {
    pub abs: PathBuf,
    pub rel: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime: i64,
}

/// List indexable files of a project.
pub fn scan(project: &Project, user_excludes: &[String], max_bytes: u64) -> Vec<Entry> {
    let filter = Filter::new(user_excludes);
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(&project.root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            n != ".git" && n != ".local-brain"
        })
        .build();
    for ent in walker.flatten() {
        if !ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let abs = ent.path().to_path_buf();
        let rel = project.rel(&abs);
        if filter.is_excluded(&rel) {
            continue;
        }
        let Some(kind) = classify(&abs) else { continue };
        let Ok(md) = ent.metadata() else { continue };
        let limit = if kind == Kind::Doc {
            max_bytes * 50
        } else {
            max_bytes
        };
        if md.len() > limit || md.len() == 0 {
            continue;
        }
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        out.push(Entry {
            abs,
            rel,
            kind,
            size: md.len(),
            mtime,
        });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

/// Whether a single path (e.g. from a watcher event) should be indexed.
pub fn should_index(project: &Project, abs: &Path, user_excludes: &[String]) -> bool {
    let rel = project.rel(abs);
    if rel.starts_with("..") || Filter::new(user_excludes).is_excluded(&rel) {
        return false;
    }
    if classify(abs).is_none() {
        return false;
    }
    // Respect .gitignore via `git check-ignore` when available.
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&project.root)
        .args(["check-ignore", "-q"])
        .arg(abs)
        .status();
    !matches!(out, Ok(s) if s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_secrets_and_vendor() {
        let f = Filter::new(&["generated/".to_string()]);
        assert!(f.is_excluded(".env"));
        assert!(f.is_excluded("config/.env.local"));
        assert!(f.is_excluded("keys/server.pem"));
        assert!(f.is_excluded("node_modules/a/index.js"));
        assert!(f.is_excluded("generated/x.rs"));
        assert!(!f.is_excluded("src/main.rs"));
        assert!(!f.is_excluded("src/environment.ts"));
        assert!(!f.is_excluded("src/secrets.rs"));
        assert!(f.is_excluded("config/secrets.yaml"));
        assert!(f.is_excluded("gcp/credentials.json"));
    }
}
