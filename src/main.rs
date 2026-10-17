//! local-brain — a local helper brain for coding agents.

mod bench;
mod catalog;
mod cli;
mod compress;
mod config;
mod daemon;
mod docs;
mod embed;
mod gen;
mod hardware;
mod hooks;
mod integrate;
mod lang;
mod learn;
mod llm;
mod mcp;
mod memory;
mod paths;
mod query;
mod raw;
mod registry;
mod runtime;
mod scan;
mod secrets;
mod settings;
mod setup;
mod slice;
mod statusline;
mod store;
mod summarize;
mod update;
mod util;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "local-brain", version, about = "A local helper brain for coding agents: it learns your project so the cloud model spends tokens on writing code, not on exploring.", long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Set up local-brain for the current project (hardware profile, models, Claude Code integration)
    Init {
        /// Project directory (default: the git root of the current directory)
        path: Option<PathBuf>,
        /// Agent to integrate with
        #[arg(long, default_value = "claude-code")]
        agent: String,
        /// Do not touch agent configuration (MCP/hooks/CLAUDE.md)
        #[arg(long)]
        no_agent: bool,
        /// Re-run hardware profiling and model selection
        #[arg(long)]
        reconfigure: bool,
        /// Force a runtime provider: ollama, lmstudio, mlx, llamacpp, openai, none
        #[arg(long)]
        provider: Option<String>,
        /// Answer yes to prompts (e.g. model downloads)
        #[arg(short, long)]
        yes: bool,
        /// Never download models
        #[arg(long)]
        no_models: bool,
    },
    /// Build or update the index: code graph, embeddings, summaries, documents (resumable)
    Learn {
        #[arg(long)]
        project: Option<PathBuf>,
        /// Skip all local-LLM work (deterministic summaries only)
        #[arg(long)]
        no_llm: bool,
        /// Do not enable background watching afterwards
        #[arg(long)]
        no_watch: bool,
    },
    /// Turn background watching on or off for this project
    Watch {
        #[arg(value_parser = ["on", "off"], default_value = "on")]
        state: String,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Index state, model tier, daemon and token savings
    Status {
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Ask the brain a question from the terminal
    Ask {
        question: Vec<String>,
        /// Limit to a path prefix
        #[arg(long)]
        scope: Option<String>,
        /// Do not use the local LLM for the answer
        #[arg(long)]
        no_llm: bool,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Print only one symbol or a line range of a file
    Slice {
        path: String,
        #[arg(long, short)]
        symbol: Option<String>,
        #[arg(long, short)]
        lines: Option<String>,
    },
    /// Show (part of) a compressed output by raw_id
    Raw {
        raw_id: String,
        #[arg(long)]
        grep: Option<String>,
        #[arg(long)]
        range: Option<String>,
    },
    /// Read an image or PDF with the local model
    Doc {
        path: String,
        #[arg(long, short)]
        question: Option<String>,
    },
    /// Run a command and print its output compressed (errors are never dropped)
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Compare vanilla Claude Code, Claude Code + graphify and Claude Code + local-brain
    Bench {
        /// Free estimate without running Claude
        #[arg(long)]
        offline: bool,
        /// Number of tasks to generate
        #[arg(long, default_value_t = 20)]
        tasks: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        /// Regenerate .local-brain/bench/tasks.json
        #[arg(long)]
        regenerate: bool,
        /// Comma-separated: vanilla,graphify,local-brain
        #[arg(long, default_value = "vanilla,graphify,local-brain")]
        configs: String,
        /// Shell command that installs graphify into the benchmark worktree
        #[arg(long)]
        graphify_setup: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 30)]
        max_turns: u32,
        /// Per-task timeout in seconds
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Only run the first N tasks
        #[arg(long)]
        limit: Option<usize>,
        /// Re-render a saved report-*.json in the current format (no Claude calls)
        #[arg(long)]
        report: Option<PathBuf>,
        /// Run the task set this many times per configuration and show the spread
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        /// Load your own Claude Code settings and plugins in the sessions (default: left out)
        #[arg(long)]
        with_user_settings: bool,
    },
    /// Check runtime, models, hooks and MCP; optionally switch the runtime provider
    Doctor {
        #[arg(long)]
        project: Option<PathBuf>,
        /// Switch provider: ollama, lmstudio, mlx, llamacpp, openai, none
        #[arg(long)]
        set_provider: Option<String>,
        /// Base URL for the provider (default: its standard local address)
        #[arg(long)]
        url: Option<String>,
        /// Also run a generation test
        #[arg(long)]
        deep: bool,
    },
    /// Remove the integration and index (one project with --project, otherwise everything)
    Uninstall {
        #[arg(long = "project")]
        project_only: bool,
        #[arg(short, long)]
        yes: bool,
    },
    /// Generate a commit message for the staged changes with the local model
    Commit {
        /// Write the message into this file (used by the git prepare-commit-msg hook)
        #[arg(long)]
        write: Option<PathBuf>,
        /// Run `git commit` with the generated message (opens your editor to confirm)
        #[arg(long)]
        commit: bool,
        /// Install a git prepare-commit-msg hook that drafts messages automatically
        #[arg(long)]
        install_hook: bool,
    },
    /// Generate a pull request description for the current branch
    Pr {
        /// Base branch (default: origin/HEAD, main or master)
        #[arg(long)]
        base: Option<String>,
    },
    /// Pre-review staged changes (or a branch with --base): bugs, secrets, unused code, broken references
    Review {
        #[arg(long)]
        base: Option<String>,
        /// Install a git pre-commit hook that prints the review (never blocks the commit)
        #[arg(long)]
        install_hook: bool,
    },
    /// Suggest documentation (docstrings, CHANGELOG) for changed files; never writes files
    Docs { path: Option<String> },
    /// Project memory: decisions, open tasks, session summaries
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Show, explain and change settings (~/.local-brain/config.toml)
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCmd>,
    },
    /// Status-line segment for Claude Code: tokens saved this session and today (reads the
    /// status-line JSON on stdin). `--install` sets it as the status line if none is configured
    Statusline {
        #[arg(long)]
        install: bool,
    },
    /// Tune memory and background settings to this machine's RAM (shows the changes first)
    Optimize {
        /// Only show what would change
        #[arg(long)]
        dry_run: bool,
        /// Apply without asking
        #[arg(long, short)]
        yes: bool,
    },
    /// Run, start, stop or install the background daemon
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Start the MCP server on stdio (used by Claude Code)
    Mcp {
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Handle a Claude Code hook event (reads JSON on stdin)
    #[command(hide = true)]
    Hook { event: String },
    /// Update local-brain to the latest release (Homebrew, cargo or the installed binary)
    Update {
        /// Reinstall the latest release even if this version is current
        #[arg(long)]
        force: bool,
    },
    /// Refresh the cached latest-release check (run in the background)
    #[command(name = "__update-check", hide = true)]
    UpdateCheck,
    /// Re-point integrations and restart the daemon after an update
    #[command(name = "__post-update", hide = true)]
    PostUpdate,
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// Print the session brief and memory entries
    Show,
    /// Record a decision, open task or note
    Note {
        text: Vec<String>,
        #[arg(long)]
        why: Option<String>,
        #[arg(long, default_value = "decision", value_parser = ["decision", "todo", "note"])]
        kind: String,
    },
    /// Close/archive an entry by id
    Done { id: i64 },
    /// Path of the editable memory.md
    Path,
    /// Summarize a Claude Code transcript into memory (used by hooks)
    #[command(hide = true)]
    Ingest {
        #[arg(long)]
        transcript: String,
        #[arg(long)]
        session: String,
        #[arg(long)]
        project: PathBuf,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Every setting with its value, default, meaning and tuning notes (the default)
    List {
        /// Only settings that differ from the defaults
        #[arg(long)]
        changed: bool,
    },
    /// Show one setting, e.g. `daemon.min_free_memory_pct`
    Get { key: String },
    /// Change a setting, e.g. `local-brain config set daemon.bg_pace_secs 30`
    Set { key: String, value: String },
    /// Put a setting back to its default
    Reset { key: String },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run in the foreground
    Run,
    /// Start in the background
    Start,
    /// Stop the background daemon
    Stop,
    Status,
    /// Install as a login service (launchd, systemd --user, Task Scheduler)
    Install,
    Uninstall,
}

/// Write a git hook script that calls local-brain (refuses to overwrite foreign hooks).
fn install_git_hook(p: &paths::Project, name: &str, body: &str) -> Result<PathBuf> {
    let hooks_dir = gen::git(&p.root, &["rev-parse", "--git-path", "hooks"])?;
    let hooks_dir = p.root.join(hooks_dir.trim());
    std::fs::create_dir_all(&hooks_dir)?;
    let hook = hooks_dir.join(name);
    if hook.exists()
        && !std::fs::read_to_string(&hook)
            .unwrap_or_default()
            .contains("local-brain")
    {
        bail!(
            "{} already exists; add a call to `{}` to it yourself",
            hook.display(),
            paths::exe()
        );
    }
    std::fs::write(
        &hook,
        format!("#!/bin/sh\n# Installed by local-brain.\n{}\n", body),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(hook)
}

fn main() {
    // Behave like a normal Unix CLI when piped into `head` (exit quietly on a closed pipe).
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    // Commands a person runs by hand; hooks, MCP, the daemon and wrapped output stay silent.
    let interactive = matches!(
        cli.cmd,
        Cmd::Init { .. }
            | Cmd::Learn { .. }
            | Cmd::Watch { .. }
            | Cmd::Status { .. }
            | Cmd::Ask { .. }
            | Cmd::Bench { .. }
            | Cmd::Doctor { .. }
            | Cmd::Memory { .. }
            | Cmd::Config { .. }
            | Cmd::Optimize { .. }
    );
    if interactive {
        update::repair();
    }
    match real_main(cli) {
        Ok(code) => {
            if interactive {
                update::notify();
            }
            std::process::exit(code)
        }
        Err(e) => {
            eprintln!("error: {:#}", e);
            std::process::exit(1);
        }
    }
}

fn real_main(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Init {
            path,
            agent,
            no_agent,
            reconfigure,
            provider,
            yes,
            no_models,
        } => {
            cli::init(cli::InitOptions {
                path,
                agent,
                no_agent,
                reconfigure,
                yes,
                no_models,
                provider,
            })?;
        }
        Cmd::Learn {
            project,
            no_llm,
            no_watch,
        } => cli::learn(project.as_deref(), no_llm, no_watch)?,
        Cmd::Watch { state, project } => cli::watch(project.as_deref(), state == "on")?,
        Cmd::Status { project } => cli::status(project.as_deref())?,
        Cmd::Ask {
            question,
            scope,
            no_llm,
            project,
        } => {
            if question.is_empty() {
                bail!("usage: local-brain ask \"<question>\"");
            }
            cli::ask(
                project.as_deref(),
                &question.join(" "),
                scope.as_deref(),
                no_llm,
            )?
        }
        Cmd::Slice {
            path,
            symbol,
            lines,
        } => {
            let p = paths::resolve_project(None)?;
            let st = store::Store::open(&p)?;
            print!(
                "{}",
                slice::slice(&st, &p, &path, symbol.as_deref(), lines.as_deref())?.text
            );
        }
        Cmd::Raw {
            raw_id,
            grep,
            range,
        } => {
            let p = paths::resolve_project(None).ok();
            let t = raw::load(p.as_ref(), &raw_id)?;
            print!(
                "{}",
                raw::select(&t, grep.as_deref(), range.as_deref(), 2, 100_000)?
            );
        }
        Cmd::Doc { path, question } => {
            let p = paths::resolve_project(None)?;
            let cfg = config::Config::load(Some(&p));
            let st = store::Store::open(&p)?;
            let llm = llm::Llm::new(&cfg);
            println!(
                "{}",
                docs::read_doc(&st, &p, &cfg, &llm, &path, question.as_deref())?.text
            );
        }
        Cmd::Run { command } => return cli::run_wrapped(&command),
        Cmd::Bench {
            offline,
            tasks,
            seed,
            regenerate,
            configs,
            graphify_setup,
            model,
            max_turns,
            timeout,
            limit,
            report,
            repeat,
            with_user_settings,
        } => {
            if let Some(json) = report {
                let (path, md) = bench::rerender(&json)?;
                println!("{}", md);
                eprintln!("Report written to {}", path.display());
                return Ok(0);
            }
            let p = paths::resolve_project(None)?;
            let cfg = config::Config::load(Some(&p));
            let st = store::Store::open(&p)?;
            if st.count("SELECT count(*) FROM files") == 0 {
                bail!("the index is empty; run `local-brain learn` first");
            }
            let ts = bench::load_or_generate(&p, &st, tasks, seed, regenerate)?;
            eprintln!(
                "{} tasks ({}); edit them in {}",
                ts.tasks.len(),
                ts.seed,
                bench::tasks_path(&p).display()
            );
            let have_claude = std::process::Command::new("claude")
                .arg("--version")
                .output()
                .is_ok();
            if offline || !have_claude {
                if !offline {
                    eprintln!("`claude` CLI not found; showing the offline estimate.");
                }
                println!("{}", bench::offline(&p, &cfg, &st, &ts)?);
            } else {
                let opts = bench::RunOptions {
                    configs: configs
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                    graphify_setup,
                    model,
                    max_turns,
                    timeout_s: timeout,
                    limit,
                    repeat,
                    isolate: !with_user_settings,
                };
                eprintln!("Running real Claude Code sessions in disposable worktrees (this uses your Claude plan/API credits).");
                let (path, md) = bench::run(&p, &cfg, &ts, &opts)?;
                println!("{}", md);
                eprintln!("Report written to {}", path.display());
            }
        }
        Cmd::Doctor {
            project,
            set_provider,
            url,
            deep,
        } => {
            let healthy = cli::doctor(
                project.as_deref(),
                set_provider.as_deref(),
                url.as_deref(),
                deep,
            )?;
            return Ok(if healthy { 0 } else { 1 });
        }
        Cmd::Uninstall { project_only, yes } => cli::uninstall(None, project_only, yes)?,
        Cmd::Commit {
            write,
            commit,
            install_hook,
        } => {
            let p = paths::resolve_project(None).or_else(|_| {
                let cwd = std::env::current_dir()?;
                Ok::<_, anyhow::Error>(paths::Project::new(paths::git_root(&cwd).unwrap_or(cwd)))
            })?;
            if install_hook {
                let body = format!(
                    "case \"$2\" in message|merge|squash|commit) exit 0;; esac\n{} commit --write \"$1\" >/dev/null 2>&1 || true",
                    util::sh_quote(&paths::exe())
                );
                let hook = install_git_hook(&p, "prepare-commit-msg", &body)?;
                eprintln!("Installed {}", hook.display());
                return Ok(0);
            }
            let cfg = config::Config::load(Some(&p));
            let st = if p.is_initialized() {
                store::Store::open(&p).ok()
            } else {
                None
            };
            let msg = gen::commit_message(&p, &cfg, st.as_ref())?;
            if let Some(f) = write {
                let existing = std::fs::read_to_string(&f).unwrap_or_default();
                let has_content = existing
                    .lines()
                    .any(|l| !l.trim().is_empty() && !l.starts_with('#'));
                if !has_content {
                    std::fs::write(&f, format!("{}\n{}", msg, existing))?;
                }
            } else if commit {
                let tmp = std::env::temp_dir()
                    .join(format!("local-brain-commit-{}.txt", std::process::id()));
                std::fs::write(&tmp, &msg)?;
                let status = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&p.root)
                    .args(["commit", "-e", "-F"])
                    .arg(&tmp)
                    .status()?;
                let _ = std::fs::remove_file(&tmp);
                return Ok(status.code().unwrap_or(1));
            } else {
                println!("{}", msg);
            }
        }
        Cmd::Pr { base } => {
            let p = paths::resolve_project(None)?;
            let cfg = config::Config::load(Some(&p));
            let st = store::Store::open(&p).ok();
            println!(
                "{}",
                gen::pr_description(&p, &cfg, st.as_ref(), base.as_deref())?
            );
        }
        Cmd::Review { base, install_hook } => {
            let p = paths::resolve_project(None)?;
            if install_hook {
                let hook = install_git_hook(
                    &p,
                    "pre-commit",
                    &format!("{} review || true", util::sh_quote(&paths::exe())),
                )?;
                eprintln!("Installed {}", hook.display());
                return Ok(0);
            }
            let cfg = config::Config::load(Some(&p));
            let st = store::Store::open(&p).ok();
            print!("{}", gen::review(&p, &cfg, st.as_ref(), base.as_deref())?);
        }
        Cmd::Docs { path } => {
            let p = paths::resolve_project(None)?;
            let cfg = config::Config::load(Some(&p));
            let st = store::Store::open(&p)?;
            print!("{}", gen::docs_suggestions(&p, &cfg, &st, path.as_deref())?);
        }
        Cmd::Memory { cmd } => match cmd {
            MemoryCmd::Show => {
                let p = paths::resolve_project(None)?;
                let st = store::Store::open(&p)?;
                print!("{}", memory::brief(&st, &p, 2000)?);
                println!("\nAll entries ({}):", p.rel(&p.memory_md()));
                for m in memory::list(&st, None, 100)? {
                    println!(
                        "  #{:<4} {:<8} {}  [{}] {}",
                        m.id,
                        m.kind,
                        util::fmt_ts(m.created),
                        m.source,
                        util::clip(&m.text, 120)
                    );
                    if let Some(w) = m.why {
                        println!("        why: {}", util::clip(&w, 120));
                    }
                    if let Some(f) = m.files {
                        println!("        files: {}", util::clip(&f, 120));
                    }
                }
            }
            MemoryCmd::Note { text, why, kind } => {
                let p = paths::resolve_project(None)?;
                let st = store::Store::open(&p)?;
                let _ = memory::sync_from_md(&st, &p);
                let id = memory::add(
                    &st,
                    &kind,
                    &text.join(" "),
                    why.as_deref(),
                    None,
                    None,
                    "user",
                )?;
                memory::export_md(&st, &p)?;
                println!("Recorded {} #{}", kind, id);
            }
            MemoryCmd::Done { id } => {
                let p = paths::resolve_project(None)?;
                let st = store::Store::open(&p)?;
                let _ = memory::sync_from_md(&st, &p);
                if !memory::archive(&st, id)? {
                    bail!("no entry #{}", id);
                }
                memory::export_md(&st, &p)?;
                println!("Archived #{}", id);
            }
            MemoryCmd::Path => {
                let p = paths::resolve_project(None)?;
                println!("{}", p.memory_md().display());
            }
            MemoryCmd::Ingest {
                transcript,
                session,
                project,
            } => {
                let p = paths::resolve_project(Some(&project))?;
                let cfg = config::Config::load(Some(&p));
                match memory::ingest_transcript(&p, &cfg, &transcript, &session) {
                    Ok(n) => hooks::log(&format!("ingested session {} ({} entries)", session, n)),
                    Err(e) => hooks::log(&format!("ingest {} failed: {:#}", session, e)),
                }
            }
        },
        Cmd::Config { cmd } => match cmd.unwrap_or(ConfigCmd::List { changed: false }) {
            ConfigCmd::List { changed } => settings::list(changed)?,
            ConfigCmd::Get { key } => settings::get(&key)?,
            ConfigCmd::Set { key, value } => settings::set(&key, &value)?,
            ConfigCmd::Reset { key } => settings::reset(&key)?,
        },
        Cmd::Statusline { install } => {
            if install {
                statusline::install()?
            } else {
                statusline::print()
            }
        }
        Cmd::Optimize { dry_run, yes } => settings::optimize(dry_run, yes)?,
        Cmd::Daemon { cmd } => match cmd {
            DaemonCmd::Run => daemon::run()?,
            DaemonCmd::Start => println!("daemon running (pid {})", daemon::start()?),
            DaemonCmd::Stop => println!(
                "{}",
                if daemon::stop()? {
                    "daemon stopped"
                } else {
                    "daemon was not running"
                }
            ),
            DaemonCmd::Status => match daemon::is_running() {
                Some(pid) => {
                    println!("running (pid {})", pid);
                    if let Ok(s) = std::fs::read_to_string(paths::daemon_status()) {
                        println!("{}", s);
                    }
                }
                None => println!("not running"),
            },
            DaemonCmd::Install => println!("{}", daemon::install_service()?),
            DaemonCmd::Uninstall => println!("{}", daemon::uninstall_service()?),
        },
        Cmd::Mcp { project } => mcp::serve(project.as_deref())?,
        Cmd::Hook { event } => hooks::run(&event)?,
        Cmd::Update { force } => update::update(force)?,
        Cmd::UpdateCheck => update::refresh(),
        Cmd::PostUpdate => update::post_update(),
    }
    Ok(0)
}
