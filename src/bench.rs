//! `local-brain bench`: compare total session tokens and task success for
//! (a) vanilla Claude Code, (b) Claude Code + graphify, (c) Claude Code + local-brain.
//!
//! Tasks are derived from the project (seeded, reproducible) and can be edited by hand in
//! `.local-brain/bench/tasks.json`. Each configuration runs headless (`claude -p`) in its own
//! disposable git worktree. `--offline` gives a free, model-less estimate instead.

use crate::config::Config;
use crate::paths::{self, Project};
use crate::store::Store;
use crate::util;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Task {
    pub id: String,
    pub kind: String,
    pub prompt: String,
    #[serde(default)]
    pub verify: Verify,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Verify {
    /// At least `min_hits` of these strings must appear in the final answer.
    #[serde(default)]
    pub contains_any: Vec<String>,
    #[serde(default)]
    pub min_hits: usize,
    /// A shell command run in the worktree after the task; exit 0 = success.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub min_length: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TaskSet {
    pub version: u32,
    pub seed: u64,
    pub generated: i64,
    pub tasks: Vec<Task>,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = (self.next() as usize) % (i + 1);
            v.swap(i, j);
        }
    }
}

fn is_testy(p: &str) -> bool {
    let l = p.to_lowercase();
    l.contains("test") || l.contains("spec") || l.contains("fixture") || l.contains("example")
}

fn test_command(root: &Path) -> Option<String> {
    if root.join("Cargo.toml").exists() {
        return Some("cargo test".into());
    }
    if let Ok(pkg) = std::fs::read_to_string(root.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<Value>(&pkg) {
            if v["scripts"]["test"].is_string() {
                return Some("npm test".into());
            }
        }
    }
    if root.join("go.mod").exists() {
        return Some("go test ./...".into());
    }
    if root.join("pyproject.toml").exists()
        || root.join("pytest.ini").exists()
        || root.join("setup.py").exists()
    {
        return Some("pytest -q".into());
    }
    None
}

/// Derive a reproducible task set from the indexed project.
pub fn generate(project: &Project, store: &Store, n: usize, seed: u64) -> Result<TaskSet> {
    let mut rng = Rng(seed);
    let mut tasks = Vec::new();
    // Uniquely named, non-test definitions.
    let mut st = store.conn.prepare(
        "SELECT s.name, s.kind, f.path, s.start_line FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.kind IN ('function','method','class','struct','trait','interface','enum')
           AND length(s.name) >= 5
           AND s.name IN (SELECT name FROM symbols GROUP BY name HAVING count(*) = 1)
         ORDER BY f.path, s.start_line",
    )?;
    let mut defs: Vec<(String, String, String, i64)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .flatten()
        .filter(|d: &(String, String, String, i64)| !is_testy(&d.2) && !d.0.starts_with("test"))
        .collect();
    drop(st);
    rng.shuffle(&mut defs);

    // Callers: symbols called from at least two files.
    let mut callers: Vec<(String, Vec<String>)> = Vec::new();
    for d in defs.iter().take(400) {
        let edges = store.edges_to(&d.0, "call")?;
        let mut files: Vec<String> = edges
            .iter()
            .map(|e| e.path.clone())
            .filter(|p| p != &d.2)
            .collect();
        files.sort();
        files.dedup();
        if files.len() >= 2 && files.len() <= 15 {
            callers.push((d.0.clone(), files));
        }
    }

    let n_def = (n * 2 / 5).max(1);
    let n_call = (n * 3 / 10).max(1);
    for (i, d) in defs.iter().take(n_def).enumerate() {
        tasks.push(Task {
            id: format!("def-{}", i + 1),
            kind: "definition".into(),
            prompt: format!("Where is the {} `{}` defined in this repository? Reply with the file path and line number, and one sentence about what it does.", d.1, d.0),
            verify: Verify { contains_any: vec![d.2.clone()], min_hits: 1, ..Default::default() },
        });
    }
    for (i, (name, files)) in callers.iter().take(n_call).enumerate() {
        tasks.push(Task {
            id: format!("callers-{}", i + 1),
            kind: "callers".into(),
            prompt: format!(
                "Which files in this repository call `{}`? List the file paths.",
                name
            ),
            verify: Verify {
                contains_any: files.clone(),
                min_hits: files.len().min(2),
                ..Default::default()
            },
        });
    }
    // Architecture.
    let mut mods: Vec<String> = store
        .files()?
        .iter()
        .filter(|f| f.kind == "code")
        .filter_map(|f| f.path.split('/').next().map(String::from))
        .collect();
    mods.sort();
    mods.dedup();
    tasks.push(Task {
        id: "arch-1".into(),
        kind: "architecture".into(),
        prompt: "Explain the architecture of this repository in at most 8 sentences: its purpose, main modules and how data flows between them.".into(),
        verify: Verify { contains_any: mods.iter().take(8).cloned().collect(), min_hits: mods.len().clamp(1, 2), min_length: 300, ..Default::default() },
    });
    // Documents.
    let docs: Vec<String> = store
        .files()?
        .into_iter()
        .filter(|f| f.kind == "doc")
        .map(|f| f.path)
        .take(3)
        .collect();
    for (i, d) in docs.iter().enumerate() {
        tasks.push(Task {
            id: format!("doc-{}", i + 1),
            kind: "document".into(),
            prompt: format!("Summarize what `{}` contains and list any requirements or important values it states.", d),
            verify: Verify { min_length: 200, ..Default::default() },
        });
    }
    // Tests.
    if let Some(cmd) = test_command(&project.root) {
        tasks.push(Task {
            id: "tests-1".into(),
            kind: "tests".into(),
            prompt: format!("Run the test suite with `{}` and report how many tests passed and failed. If something fails, name the failing tests and the error. Do not change any files.", cmd),
            verify: Verify { contains_any: vec!["pass".into(), "fail".into(), "ok".into()], min_hits: 1, ..Default::default() },
        });
    }
    tasks.truncate(n.max(1));
    Ok(TaskSet {
        version: 1,
        seed,
        generated: util::now(),
        tasks,
    })
}

pub fn tasks_path(project: &Project) -> PathBuf {
    project.bench_dir().join("tasks.json")
}

pub fn load_or_generate(
    project: &Project,
    store: &Store,
    n: usize,
    seed: u64,
    regenerate: bool,
) -> Result<TaskSet> {
    let p = tasks_path(project);
    if !regenerate {
        if let Ok(s) = std::fs::read_to_string(&p) {
            return serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()));
        }
    }
    let ts = generate(project, store, n, seed)?;
    std::fs::create_dir_all(project.bench_dir())?;
    std::fs::write(&p, serde_json::to_string_pretty(&ts)?)?;
    Ok(ts)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RunResult {
    pub task: String,
    pub config: String,
    pub success: bool,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub tool_calls: i64,
    pub turns: i64,
    pub duration_ms: i64,
    pub cost_usd: f64,
    pub error: Option<String>,
    pub answer_excerpt: String,
    /// Which repetition of the task set this run belongs to (`--repeat`).
    #[serde(default)]
    pub run: usize,
    /// The model the session actually ran on (Claude Code's default when `--model` is not given).
    #[serde(default)]
    pub model: String,
}

impl RunResult {
    pub fn total(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

fn verify(task: &Task, answer: &str, worktree: &Path) -> bool {
    let v = &task.verify;
    let lower = answer.to_lowercase();
    if !v.contains_any.is_empty() {
        let hits = v
            .contains_any
            .iter()
            .filter(|s| lower.contains(&s.to_lowercase()))
            .count();
        if hits < v.min_hits.max(1) {
            return false;
        }
    }
    if answer.len() < v.min_length {
        return false;
    }
    if let Some(cmd) = &v.command {
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(worktree)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return false;
        }
    }
    !answer.trim().is_empty()
}

fn strip_claude_md_section(worktree: &Path) {
    let p = worktree.join("CLAUDE.md");
    if let Ok(s) = std::fs::read_to_string(&p) {
        if let (Some(a), Some(b)) = (
            s.find("<!-- local-brain:start -->"),
            s.find("<!-- local-brain:end -->"),
        ) {
            let _ = std::fs::write(
                &p,
                format!("{}{}", &s[..a], &s[b + "<!-- local-brain:end -->".len()..]),
            );
        }
    }
}

/// Commit a configuration's files (graphify's index and CLAUDE.md section, local-brain's
/// section) in the detached worktree, so that resetting between tasks keeps them. 0.1.7 reset
/// with `git clean`, which deleted graphify's untracked files after the first task: 17 of 18
/// "graphify" sessions were really vanilla. Every configuration gets exactly one such commit, so
/// the git snapshot Claude Code shows is the same shape for all.
fn commit_setup(wt: &Path) {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(args)
            .output()
    };
    let _ = git(&[
        "add",
        "-A",
        "--",
        ".",
        &format!(":(exclude){}", paths::PROJECT_DIR),
    ]);
    let _ = git(&[
        "-c",
        "user.name=local-brain bench",
        "-c",
        "user.email=bench@local-brain",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "--no-verify",
        "--allow-empty",
        "-m",
        "local-brain bench: configuration setup",
    ]);
}

struct Worktree {
    root: PathBuf,
    path: PathBuf,
}

impl Worktree {
    fn create(root: &Path, dir: &Path) -> Result<Worktree> {
        if dir.exists() {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["worktree", "remove", "--force"])
                .arg(dir)
                .output();
            let _ = std::fs::remove_dir_all(dir);
        }
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["worktree", "add", "--detach"])
            .arg(dir)
            .arg("HEAD")
            .output()?;
        if !out.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(Worktree {
            root: root.to_path_buf(),
            path: dir.to_path_buf(),
        })
    }
    fn reset(&self) {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["checkout", "--", "."])
            .output();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["clean", "-fdq", "-e", ".local-brain"])
            .output();
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_claude(
    task: &Task,
    config: &str,
    wt: &Path,
    mcp_config: &Value,
    settings: Option<&Value>,
    model: Option<&str>,
    permission_mode: Option<&str>,
    max_turns: u32,
    timeout_s: u64,
    isolate: bool,
    transcript: Option<&Path>,
) -> RunResult {
    let mut r = RunResult {
        task: task.id.clone(),
        config: config.into(),
        ..Default::default()
    };
    let mut cmd = std::process::Command::new("claude");
    cmd.current_dir(wt)
        .arg("-p")
        .arg(&task.prompt)
        .args(["--output-format", "stream-json", "--verbose"])
        .args(["--max-turns", &max_turns.to_string()])
        .args([
            "--strict-mcp-config",
            "--mcp-config",
            &mcp_config.to_string(),
        ])
        .args(["--allowedTools", "Read,Grep,Glob,LS,Bash,mcp__local-brain"]);
    if isolate {
        // Leave out the user's own settings (plugins, hooks, status line): they add the same
        // large floor to every configuration and hide the differences being measured. Also leave
        // out `local`: Claude Code reads it from the *main* checkout for a git worktree, so every
        // configuration would inherit the hooks local-brain installed there. The worktree's
        // project settings (graphify's hooks) and --settings (local-brain's) still apply.
        cmd.args(["--setting-sources", "project"]);
    }
    if let Some(s) = settings {
        cmd.args(["--settings", &s.to_string()]);
    }
    if let Some(pm) = permission_mode {
        // e.g. bypassPermissions: Claude Code then steers Claude to Bash for reads and searches.
        cmd.args(["--permission-mode", pm]);
    }
    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let start = std::time::Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            r.error = Some(format!("could not start claude: {}", e));
            return r;
        }
    };
    let stdout = child.stdout.take().unwrap();
    let pid = child.id();
    let deadline = std::time::Duration::from_secs(timeout_s);
    let killer = std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let mut sys = sysinfo::System::new();
            let p = sysinfo::Pid::from_u32(pid);
            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[p]), true);
            match sys.process(p) {
                None => return,
                Some(pr) if t0.elapsed() > deadline => {
                    pr.kill();
                    return;
                }
                _ => {}
            }
        }
    });
    let mut answer = String::new();
    let mut keep = transcript.and_then(|p| std::fs::File::create(p).ok());
    for line in std::io::BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
    {
        if let Some(f) = keep.as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "{}", line);
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => {
                r.model = v["model"].as_str().unwrap_or("").to_string();
            }
            Some("assistant") => {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    r.tool_calls +=
                        blocks.iter().filter(|b| b["type"] == "tool_use").count() as i64;
                }
            }
            Some("result") => {
                let u = &v["usage"];
                r.input_tokens = u["input_tokens"].as_i64().unwrap_or(0);
                r.output_tokens = u["output_tokens"].as_i64().unwrap_or(0);
                r.cache_read_tokens = u["cache_read_input_tokens"].as_i64().unwrap_or(0);
                r.cache_write_tokens = u["cache_creation_input_tokens"].as_i64().unwrap_or(0);
                r.turns = v["num_turns"].as_i64().unwrap_or(0);
                r.duration_ms = v["duration_ms"].as_i64().unwrap_or(0);
                r.cost_usd = v["total_cost_usd"].as_f64().unwrap_or(0.0);
                answer = v["result"].as_str().unwrap_or("").to_string();
                if v["is_error"].as_bool() == Some(true) {
                    r.error = Some(format!(
                        "claude reported an error: {}",
                        v["subtype"].as_str().unwrap_or("")
                    ));
                }
            }
            _ => {}
        }
    }
    let status = child.wait();
    let _ = killer.join();
    if r.duration_ms == 0 {
        r.duration_ms = start.elapsed().as_millis() as i64;
    }
    if answer.is_empty() && r.error.is_none() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            use std::io::Read;
            let _ = e.read_to_string(&mut err);
        }
        r.error = Some(format!(
            "no result (exit {:?}): {}",
            status.ok().and_then(|s| s.code()),
            util::clip(err.trim(), 300)
        ));
    }
    r.success = r.error.is_none() && verify(task, &answer, wt);
    r.answer_excerpt = util::clip(&answer.replace('\n', " "), 300);
    r
}

pub struct RunOptions {
    pub configs: Vec<String>,
    pub graphify_setup: Option<String>,
    pub model: Option<String>,
    pub max_turns: u32,
    pub timeout_s: u64,
    pub limit: Option<usize>,
    /// Run the task set this many times per configuration.
    pub repeat: usize,
    /// Exclude the user's settings and plugins from the sessions.
    pub isolate: bool,
    /// Save each session's stream-json under `.local-brain/bench/transcripts/<run>/`.
    pub keep_transcripts: bool,
    /// Claude Code permission mode for the sessions (`bypassPermissions`, `acceptEdits`, …).
    pub permission_mode: Option<String>,
}

/// Run the comparison with real Claude Code sessions.
pub fn run(
    project: &Project,
    cfg: &Config,
    ts: &TaskSet,
    opts: &RunOptions,
) -> Result<(PathBuf, String)> {
    let claude_version = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .context("the `claude` CLI is required for `bench` (use --offline for an estimate)")?;
    let claude_version = String::from_utf8_lossy(&claude_version.stdout)
        .trim()
        .to_string();
    let exe = paths::exe();
    let transcripts = opts.keep_transcripts.then(|| {
        project
            .bench_dir()
            .join("transcripts")
            .join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string())
    });
    if let Some(d) = &transcripts {
        std::fs::create_dir_all(d)?;
    }
    let scratch = std::env::temp_dir().join(format!("local-brain-bench-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let tasks: Vec<&Task> = ts
        .tasks
        .iter()
        .take(opts.limit.unwrap_or(usize::MAX))
        .collect();
    let mut results: Vec<RunResult> = Vec::new();
    let empty_mcp = json!({"mcpServers": {}});
    let _ = cfg;
    struct Setup {
        config: String,
        wt: Worktree,
        mcp: Value,
        settings: Option<Value>,
    }
    let mut setups: Vec<Setup> = Vec::new();
    for config in &opts.configs {
        let wt = Worktree::create(&project.root, &scratch.join(config))?;
        strip_claude_md_section(&wt.path);
        let (mcp, settings): (Value, Option<Value>) = match config.as_str() {
            "vanilla" => (empty_mcp.clone(), Some(json!({"hooks": {}}))),
            "graphify" => {
                let Some(setup) = &opts.graphify_setup else {
                    eprintln!("  skipping graphify: pass --graphify-setup \"<command that installs graphify into the current repo>\"");
                    continue;
                };
                eprintln!("  setting up graphify: {}", setup);
                let ok = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(setup)
                    .current_dir(&wt.path)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    eprintln!("  graphify setup failed; skipping");
                    continue;
                }
                // graphify's own setup writes its MCP/CLAUDE.md config into the worktree.
                let mcp = std::fs::read_to_string(wt.path.join(".mcp.json"))
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or(empty_mcp.clone());
                (mcp, None)
            }
            "local-brain" => {
                // Exactly what `local-brain init` installs: the hooks, the CLAUDE.md section and
                // the MCP server (always loaded, as the server itself asks).
                #[cfg(unix)]
                std::os::unix::fs::symlink(project.dir(), wt.path.join(paths::PROJECT_DIR))?;
                #[cfg(not(unix))]
                bail!("bench for local-brain currently needs a Unix-like OS");
                crate::integrate::ensure_claude_md_section(&wt.path)?;
                let mcp = json!({"mcpServers": {"local-brain": {
                    "command": exe, "args": ["mcp", "--project", wt.path], "alwaysLoad": true
                }}});
                // Without the memory hooks: they cost no tokens, and summaries of benchmark
                // sessions would otherwise land in the project's real memory.
                let mut hooks = crate::integrate::hooks_settings(&exe);
                for ev in ["Stop", "SessionEnd"] {
                    hooks["hooks"].as_object_mut().map(|o| o.remove(ev));
                }
                (mcp, Some(hooks))
            }
            other => {
                eprintln!(
                    "  unknown config `{}` (use vanilla, graphify, local-brain)",
                    other
                );
                continue;
            }
        };
        commit_setup(&wt.path);
        setups.push(Setup {
            config: config.clone(),
            wt,
            mcp,
            settings,
        });
    }
    // Interleaved: every task runs under each configuration back to back. Sessions of one
    // configuration are then spread over the whole run, so prompt-cache timing (the 1-hour
    // cache of the system prompt) favours none of them.
    let repeat = opts.repeat.max(1);
    let total = repeat * tasks.len() * setups.len();
    let mut n = 0;
    for run in 0..repeat {
        for t in tasks.iter() {
            for s in &setups {
                n += 1;
                eprint!(
                    "  [{}/{}] {}{} · {} … ",
                    n,
                    total,
                    if repeat > 1 {
                        format!("run {} · ", run + 1)
                    } else {
                        String::new()
                    },
                    t.id,
                    s.config
                );
                // Repeats carry a run marker: an identical prompt would replay the identical
                // first request, which the API serves from cache for free, so a repeat without
                // it measures caching, not the configuration.
                let marked;
                let t: &Task = if run == 0 {
                    t
                } else {
                    marked = Task {
                        prompt: format!("{}\n\n(run {} of {})", t.prompt, run + 1, repeat),
                        ..(*t).clone()
                    };
                    &marked
                };
                let mut r = run_claude(
                    t,
                    &s.config,
                    &s.wt.path,
                    &s.mcp,
                    s.settings.as_ref(),
                    opts.model.as_deref(),
                    opts.permission_mode.as_deref(),
                    opts.max_turns,
                    opts.timeout_s,
                    opts.isolate,
                    transcripts
                        .as_ref()
                        .map(|d| d.join(format!("{}-{}-r{}.jsonl", s.config, t.id, run + 1)))
                        .as_deref(),
                );
                r.run = run;
                eprintln!(
                    "{} · ${:.3} · {} new tokens · {} tool calls{}",
                    if r.success { "ok" } else { "FAIL" },
                    r.cost_usd,
                    util::human_tokens(new_tokens(r.input_tokens, r.cache_write_tokens)),
                    r.tool_calls,
                    r.error
                        .as_ref()
                        .map(|e| format!(" · {}", e))
                        .unwrap_or_default()
                );
                results.push(r);
                s.wt.reset();
            }
        }
    }
    drop(setups);
    let _ = std::fs::remove_dir_all(&scratch);
    write_report(
        project,
        ts,
        &results,
        &claude_version,
        opts.model.as_deref(),
        opts.isolate,
        opts.permission_mode.as_deref(),
    )
}

#[derive(Default, Serialize)]
struct Agg {
    tasks: usize,
    successes: usize,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    tool_calls: i64,
    duration_ms: i64,
    cost_usd: f64,
}

/// Report header facts.
struct Meta<'a> {
    date: String,
    version: &'a str,
    claude_version: &'a str,
    model: Option<&'a str>,
    tasks: usize,
    seed: u64,
    /// None for reports written before isolation existed.
    isolated: Option<bool>,
    /// Whether configurations were interleaved per task (None: older report, sequential).
    interleaved: Option<bool>,
    permission_mode: Option<&'a str>,
}

fn aggregate(results: &[RunResult]) -> BTreeMap<String, Agg> {
    let mut agg: BTreeMap<String, Agg> = BTreeMap::new();
    for r in results {
        let a = agg.entry(r.config.clone()).or_default();
        a.tasks += 1;
        a.successes += r.success as usize;
        a.input += r.input_tokens;
        a.output += r.output_tokens;
        a.cache_read += r.cache_read_tokens;
        a.cache_write += r.cache_write_tokens;
        a.tool_calls += r.tool_calls;
        a.duration_ms += r.duration_ms;
        a.cost_usd += r.cost_usd;
    }
    agg
}

/// Tokens a configuration put into the context for the first time: uncached input plus cache
/// writes. Billed at full (and cache-write) price, unlike cache reads, which cost a small
/// fraction — so this, not the raw total, is what a context tool has to keep down.
fn new_tokens(input: i64, cache_write: i64) -> i64 {
    input + cache_write
}

fn pct_change(new: f64, base: f64) -> String {
    if base == 0.0 {
        return "n/a".into();
    }
    format!("{:+.1}%", 100.0 * (new - base) / base)
}

fn render_md(meta: &Meta, results: &[RunResult]) -> String {
    let agg = aggregate(results);
    let total = |a: &Agg| a.input + a.output + a.cache_read + a.cache_write;
    let mut md = format!(
        "# local-brain bench report\n\n- Date: {}\n- local-brain: {}\n- Claude Code: {}\n- Model: {}\n- Task set: {} tasks, seed {}, {} run(s) each\n- Sessions: {}{}\n- Order: {}\n\n",
        meta.date,
        meta.version,
        meta.claude_version,
        meta.model.map(String::from).unwrap_or_else(|| {
            let mut seen: Vec<&str> = results
                .iter()
                .map(|r| r.model.as_str())
                .filter(|m| !m.is_empty())
                .collect();
            seen.sort_unstable();
            seen.dedup();
            if seen.is_empty() {
                "default".to_string()
            } else {
                format!("{} (Claude Code's default; pass --model to pin it)", seen.join(", "))
            }
        }),
        meta.tasks,
        meta.seed,
        runs(results),
        match meta.isolated {
            Some(true) => "isolated (user settings and plugins left out)",
            Some(false) => "with the user's own settings and plugins (adds the same floor to every configuration)",
            None => "not recorded (before 0.1.7: the user's settings and plugins were loaded)",
        },
        meta.permission_mode
            .map(|m| format!(", permission mode {}", m))
            .unwrap_or_default(),
        match meta.interleaved {
            Some(true) => "configurations interleaved per task (prompt-cache timing affects all equally)",
            Some(false) | None => "one configuration after another (before 0.1.8; the later configurations ran with a warmer cache)",
        }
    );
    md.push_str(
        "Ranked by cost. **New tokens** = uncached input + cache writes: text a configuration adds \
         to the context, billed at full price. Cache reads re-read context the model already has \
         and cost a small fraction, so the raw total overstates savings that only cut re-reading.\n\n",
    );
    md.push_str("| Config | Success | Cost | Cost per run (min–max) | New tokens | Output | Cache read | Total tokens | Tool calls | Time |\n|---|---|---|---|---|---|---|---|---|---|\n");
    let mut ranked: Vec<(&String, &Agg)> = agg.iter().collect();
    ranked.sort_by(|a, b| a.1.cost_usd.total_cmp(&b.1.cost_usd));
    for (c, a) in &ranked {
        md.push_str(&format!(
            "| {} | {}/{} | ${:.2} | {} | {} | {} | {} | {} | {} | {:.0}s |\n",
            c,
            a.successes,
            a.tasks,
            a.cost_usd,
            run_spread(results, c),
            new_tokens(a.input, a.cache_write),
            a.output,
            a.cache_read,
            total(a),
            a.tool_calls,
            a.duration_ms as f64 / 1000.0,
        ));
    }
    md.push('\n');
    for (subject, base) in [
        ("local-brain", "vanilla"),
        ("local-brain", "graphify"),
        ("graphify", "vanilla"),
    ] {
        let (Some(s), Some(b)) = (agg.get(subject), agg.get(base)) else {
            continue;
        };
        md.push_str(&format!(
            "- {} vs {}: cost {}, new tokens {}, total tokens {}, success {}/{} vs {}/{}\n",
            subject,
            base,
            pct_change(s.cost_usd, b.cost_usd),
            pct_change(
                new_tokens(s.input, s.cache_write) as f64,
                new_tokens(b.input, b.cache_write) as f64
            ),
            pct_change(total(s) as f64, total(b) as f64),
            s.successes,
            s.tasks,
            b.successes,
            b.tasks
        ));
    }
    // The first session of a configuration also writes its prompt prefix (tools, system prompt)
    // to the cache — once per hour of inactivity in real use, but in a short run it is a large
    // share of the total and differs between configurations with different tool lists.
    let mut firsts: Vec<String> = Vec::new();
    for (c, _) in &ranked {
        if let Some(r) = results.iter().find(|r| &r.config == *c) {
            firsts.push(format!("{} ${:.3}", c, r.cost_usd));
        }
    }
    if !firsts.is_empty() {
        md.push_str(&format!(
            "- First session of each configuration, which also writes its prompt prefix to the \
             cache (once per hour of inactivity in real use): {}\n",
            firsts.join(" · ")
        ));
    }
    md.push_str("\n## Per task\n\n| Task | Config | OK | Cost | New tokens | Total tokens | Tool calls | Turns |\n|---|---|---|---|---|---|---|---|\n");
    for r in results {
        md.push_str(&format!(
            "| {} | {} | {} | ${:.3} | {} | {} | {} | {} |\n",
            r.task,
            r.config,
            if r.success { "✓" } else { "✗" },
            r.cost_usd,
            new_tokens(r.input_tokens, r.cache_write_tokens),
            r.total(),
            r.tool_calls,
            r.turns
        ));
    }
    md
}

fn runs(results: &[RunResult]) -> usize {
    results.iter().map(|r| r.run + 1).max().unwrap_or(1)
}

/// Cost of each full pass over the task set, as "$min–$max" (or "–" with a single run): when
/// the ranges of two configurations overlap, their difference is within run-to-run noise.
fn run_spread(results: &[RunResult], config: &str) -> String {
    let mut per_run: BTreeMap<usize, f64> = BTreeMap::new();
    for r in results.iter().filter(|r| r.config == config) {
        *per_run.entry(r.run).or_default() += r.cost_usd;
    }
    if per_run.len() < 2 {
        return "–".into();
    }
    let lo = per_run.values().cloned().fold(f64::INFINITY, f64::min);
    let hi = per_run.values().cloned().fold(f64::NEG_INFINITY, f64::max);
    format!("${:.2}–${:.2}", lo, hi)
}

/// `bench --report <json>`: render a saved run again (e.g. after the report format changed)
/// without running Claude. Writes the Markdown next to the JSON.
pub fn rerender(json_path: &Path) -> Result<(PathBuf, String)> {
    let v: Value = serde_json::from_str(
        &std::fs::read_to_string(json_path)
            .with_context(|| format!("reading {}", json_path.display()))?,
    )?;
    let results: Vec<RunResult> = serde_json::from_value(v["results"].clone())
        .context("the report has no readable `results`")?;
    let date = std::fs::metadata(json_path)
        .and_then(|m| m.modified())
        .map(|t| {
            chrono::DateTime::<chrono::Local>::from(t)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default();
    let meta = Meta {
        date,
        version: v["version"].as_str().unwrap_or("?"),
        claude_version: v["claude_version"].as_str().unwrap_or("?"),
        model: v["model"].as_str(),
        tasks: v["tasks"].as_array().map_or(0, |t| t.len()),
        seed: v["seed"].as_u64().unwrap_or(0),
        isolated: v["isolated"].as_bool(),
        interleaved: v["interleaved"].as_bool(),
        permission_mode: v["permission_mode"].as_str(),
    };
    let md = render_md(&meta, &results);
    let md_path = json_path.with_extension("md");
    std::fs::write(&md_path, &md)?;
    Ok((md_path, md))
}

fn write_report(
    project: &Project,
    ts: &TaskSet,
    results: &[RunResult],
    claude_version: &str,
    model: Option<&str>,
    isolated: bool,
    permission_mode: Option<&str>,
) -> Result<(PathBuf, String)> {
    let agg = aggregate(results);
    let md = render_md(
        &Meta {
            date: util::fmt_ts(util::now()),
            version: env!("CARGO_PKG_VERSION"),
            claude_version,
            model,
            tasks: ts.tasks.len(),
            seed: ts.seed,
            isolated: Some(isolated),
            interleaved: Some(true),
            permission_mode,
        },
        results,
    );
    let ts_now = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    std::fs::create_dir_all(project.bench_dir())?;
    let md_path = project.bench_dir().join(format!("report-{}.md", ts_now));
    let json_path = project.bench_dir().join(format!("report-{}.json", ts_now));
    std::fs::write(&md_path, &md)?;
    std::fs::write(
        &json_path,
        serde_json::to_string_pretty(&json!({
            "version": env!("CARGO_PKG_VERSION"),
            "claude_version": claude_version,
            "model": model,
            "seed": ts.seed,
            "isolated": isolated,
            "interleaved": true,
            "permission_mode": permission_mode,
            "tasks": ts.tasks,
            "aggregates": agg,
            "results": results,
        }))?,
    )?;
    Ok((md_path, md))
}

/// Free estimate without running Claude: what exploring with Grep + Read would put into the
/// context versus what brain_query + brain_slice return.
pub fn offline(project: &Project, cfg: &Config, store: &Store, ts: &TaskSet) -> Result<String> {
    let mut out = String::from("Offline estimate (no Claude calls; vanilla = grep the name + read the files it is found in, capped like the Read tool):\n\n| Task | Vanilla est. | local-brain | Saved |\n|---|---|---|---|\n");
    let (mut tv, mut tl) = (0i64, 0i64);
    for t in &ts.tasks {
        if !matches!(t.kind.as_str(), "definition" | "callers" | "architecture") {
            continue;
        }
        let name = t.prompt.split('`').nth(1).unwrap_or("").to_string();
        let mut vanilla = 300;
        if !name.is_empty() {
            let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(&name)))?;
            let mut grep_out = 0;
            let mut files_read = 0;
            for f in store.files()? {
                if f.kind == "doc" {
                    continue;
                }
                if let Some(text) = util::read_text(&project.abs(&f.path)) {
                    let n = text.lines().filter(|l| re.is_match(l)).count();
                    if n > 0 {
                        grep_out += n as i64 * 30;
                        if files_read < 4 {
                            vanilla += util::tokens(&text).min(25_000);
                            files_read += 1;
                        }
                    }
                }
            }
            vanilla += grep_out.min(20_000);
        } else {
            // Architecture questions: a vanilla agent typically reads the README and several entry files.
            let mut files: Vec<_> = store
                .files()?
                .into_iter()
                .filter(|f| f.kind != "doc")
                .collect();
            files.sort_by_key(|f| f.path.matches('/').count());
            for f in files.iter().take(8) {
                vanilla += util::tokens_len(f.size as u64).min(25_000);
            }
        }
        let q = t.prompt.clone();
        let a = crate::query::query(store, project, cfg, None, &q, None)?;
        let mut lb = util::tokens(&a.text) + 150;
        if t.kind == "definition" {
            if let Some(f) = a.files.first() {
                if let Ok(sl) = crate::slice::slice(store, project, f, Some(&name), None) {
                    lb += util::tokens(&sl.text);
                }
            }
        }
        tv += vanilla;
        tl += lb;
        out.push_str(&format!(
            "| {} | {} | {} | {:.0}% |\n",
            t.id,
            vanilla,
            lb,
            100.0 * (vanilla - lb) as f64 / vanilla.max(1) as f64
        ));
    }
    out.push_str(&format!(
        "\nTotal: vanilla ≈ {} tokens, local-brain ≈ {} tokens ({:.0}% less). Run `local-brain bench` with the `claude` CLI installed for real session measurements.\n",
        util::human_tokens(tv),
        util::human_tokens(tl),
        100.0 * (tv - tl) as f64 / tv.max(1) as f64
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rr(config: &str, run: usize, cost: f64, cache_write: i64) -> RunResult {
        RunResult {
            task: "t".into(),
            config: config.into(),
            success: true,
            cache_write_tokens: cache_write,
            cache_read_tokens: 100_000,
            cost_usd: cost,
            run,
            ..Default::default()
        }
    }

    #[test]
    fn report_ranks_by_cost_and_shows_run_spread() {
        let results = vec![
            rr("vanilla", 0, 0.10, 1000),
            rr("vanilla", 1, 0.12, 1000),
            rr("local-brain", 0, 0.08, 800),
            rr("local-brain", 1, 0.09, 800),
        ];
        assert_eq!(run_spread(&results, "vanilla"), "$0.10–$0.12");
        assert_eq!(run_spread(&results[..1], "vanilla"), "–");
        let md = render_md(
            &Meta {
                date: "d".into(),
                version: "v",
                claude_version: "c",
                model: None,
                tasks: 1,
                seed: 1,
                isolated: Some(true),
                interleaved: Some(true),
                permission_mode: None,
            },
            &results,
        );
        let lb = md.find("| local-brain |").unwrap();
        let va = md.find("| vanilla |").unwrap();
        assert!(lb < va, "cheapest configuration first");
        assert!(md.contains("2 run(s) each"));
        assert!(md.contains("local-brain vs vanilla: cost -22.7%, new tokens -20.0%"));
    }
}
