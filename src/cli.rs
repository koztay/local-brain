//! Implementations of the user-facing commands.

use crate::catalog;
use crate::config::{self, Config};
use crate::daemon;
use crate::hardware;
use crate::integrate;
use crate::llm::Llm;
use crate::paths::{self, Project};
use crate::registry;
use crate::runtime::{self, Runtime};
use crate::store::Store;
use crate::util;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

fn ok(s: &str) {
    eprintln!("  ✓ {}", s);
}
fn warn(s: &str) {
    eprintln!("  ! {}", s);
}

pub struct InitOptions {
    pub path: Option<PathBuf>,
    pub agent: String,
    pub no_agent: bool,
    pub reconfigure: bool,
    pub yes: bool,
    pub no_models: bool,
    pub provider: Option<String>,
}

/// Fill model names for `provider` and `tier` from the catalog.
pub fn apply_catalog(cfg: &mut Config, provider: &str, tier: &str) -> Result<()> {
    let cat = catalog::load()?;
    let t = cat
        .tiers
        .get(tier)
        .or_else(|| cat.tiers.get("S"))
        .context("catalog has no tiers")?;
    let pick = |m: &Option<catalog::ModelRef>| {
        m.as_ref()
            .and_then(|m| m.name_for(provider))
            .unwrap_or_default()
    };
    cfg.models.tier = tier.to_string();
    cfg.models.general = pick(&t.general);
    cfg.models.code = pick(&t.code);
    cfg.models.vision = pick(&t.vision);
    cfg.models.embedding = pick(&t.embedding);
    cfg.models.general_has_vision = t.general.as_ref().map(|m| m.vision).unwrap_or(false);
    Ok(())
}

pub fn configure_global(
    reconfigure: bool,
    provider_override: Option<&str>,
    yes: bool,
    no_models: bool,
) -> Result<Config> {
    paths::ensure_home()?;
    catalog::install_default()?;
    let exists = paths::global_config().exists();
    let mut cfg = Config::load_global();
    if !exists || reconfigure || provider_override.is_some() {
        eprintln!("Profiling hardware…");
        let prof = hardware::profile();
        eprintln!("  {}", prof.describe());
        let cat = catalog::load()?;
        let tier = prof.choose_tier(&cat);
        let (provider, url) = match provider_override {
            Some(p) => (p.to_string(), runtime::default_url(p).to_string()),
            None => match prof.preferred_runtime() {
                Some(r) => (r.provider.clone(), r.base_url.clone()),
                None => ("none".into(), String::new()),
            },
        };
        for r in &prof.runtimes {
            eprintln!("  runtime {:<9} {}", r.provider, r.detail);
        }
        cfg.runtime.provider = provider.clone();
        cfg.runtime.base_url = url;
        if provider != "none" {
            apply_catalog(&mut cfg, &provider, &tier)?;
        } else {
            cfg.models.tier = tier.clone();
        }
        let desc = cat
            .tiers
            .get(&tier)
            .map(|t| t.description.clone())
            .unwrap_or_default();
        eprintln!("  tier {} ({}) · provider {}", tier, desc, provider);
        adopt_server_models(&mut cfg);
        cfg.save_global()?;
        ok(&format!("wrote {}", paths::global_config().display()));
    } else {
        ok(&format!(
            "using existing {} (provider {}, tier {})",
            paths::global_config().display(),
            cfg.runtime.provider,
            cfg.models.tier
        ));
    }
    // One "yes" to installing the runtime also covers downloading the models.
    let yes = crate::setup::ensure_runtime(&mut cfg, yes, no_models)? || yes;
    ensure_models(&cfg, yes, no_models);
    Ok(cfg)
}

/// OpenAI-compatible servers (LM Studio, llama.cpp, MLX) serve whatever the user loaded; when
/// the catalog names are not there, use the loaded models instead.
fn adopt_server_models(cfg: &mut Config) {
    let rt = Runtime::from_config(cfg);
    if rt.kind != runtime::Kind::OpenAi {
        return;
    }
    let Ok(models) = rt.list_models() else { return };
    if models.is_empty() {
        return;
    }
    let is_embed = |m: &str| {
        m.to_lowercase().contains("embed")
            || m.to_lowercase().contains("bge")
            || m.to_lowercase().contains("e5-")
    };
    let present = |m: &str| models.iter().any(|x| x == m);
    if cfg.models.general.is_empty() || !present(&cfg.models.general) {
        if let Some(m) = models.iter().find(|m| !is_embed(m)) {
            ok(&format!("using loaded model {} as the general model", m));
            cfg.models.general = m.clone();
            let l = m.to_lowercase();
            cfg.models.general_has_vision = [
                "vl",
                "vision",
                "gemma-3",
                "gemma3",
                "llava",
                "pixtral",
                "smolvlm",
                "moondream",
                "minicpm-v",
            ]
            .iter()
            .any(|k| l.contains(k));
        }
    }
    if !cfg.models.code.is_empty() && !present(&cfg.models.code) {
        cfg.models.code.clear();
    }
    if !cfg.models.vision.is_empty() && !present(&cfg.models.vision) {
        cfg.models.vision.clear();
    }
    if cfg.models.embedding.is_empty() || !present(&cfg.models.embedding) {
        match models.iter().find(|m| is_embed(m)) {
            Some(m) => {
                ok(&format!("using loaded model {} for embeddings", m));
                cfg.models.embedding = m.clone();
            }
            None => cfg.models.embedding.clear(),
        }
    }
}

fn ensure_models(cfg: &Config, yes: bool, no_models: bool) {
    let rt = Runtime::from_config(cfg);
    if !rt.enabled() {
        warn("No local model: local-brain runs in deterministic mode (code graph, hash embeddings, rule-based compression).");
        return;
    }
    if !rt.is_up() {
        warn(&format!(
            "{} is configured but not reachable at {} — start it, then run `local-brain doctor`.",
            cfg.runtime.provider, rt.base_url
        ));
        return;
    }
    let mut wanted: Vec<String> = [
        &cfg.models.general,
        &cfg.models.code,
        &cfg.models.vision,
        &cfg.models.embedding,
    ]
    .iter()
    .filter(|m| !m.is_empty())
    .map(|m| m.to_string())
    .collect();
    wanted.dedup();
    let missing: Vec<String> = wanted.into_iter().filter(|m| !rt.has_model(m)).collect();
    if missing.is_empty() {
        ok("all configured models are available");
        return;
    }
    if no_models || !cfg.network.allow_downloads {
        warn(&format!(
            "missing models: {} (download skipped)",
            missing.join(", ")
        ));
        return;
    }
    if rt.kind != runtime::Kind::Ollama {
        warn(&format!("load these models in {}: {} (or edit ~/.local-brain/config.toml to match what you have)", cfg.runtime.provider, missing.join(", ")));
        return;
    }
    let cat = catalog::load().ok();
    let size: f64 = cat
        .as_ref()
        .and_then(|c| c.tiers.get(&cfg.models.tier))
        .map(|t| {
            [&t.general, &t.code, &t.vision, &t.embedding]
                .iter()
                .filter_map(|m| m.as_ref())
                .filter(|m| {
                    m.name_for("ollama")
                        .map(|n| missing.contains(&n))
                        .unwrap_or(false)
                })
                .map(|m| m.size_gb)
                .sum()
        })
        .unwrap_or(0.0);
    let q = format!(
        "Download {} (~{:.1} GB) with Ollama now?",
        missing.join(", "),
        size
    );
    if yes || util::confirm(&q, true) {
        for m in &missing {
            if let Err(e) = rt.pull(m) {
                warn(&format!("{}", e));
            } else {
                ok(&format!("downloaded {}", m));
            }
        }
    } else {
        warn(&format!(
            "skipped; later run: ollama pull {}",
            missing.join(" && ollama pull ")
        ));
    }
}

pub fn init(opts: InitOptions) -> Result<()> {
    let start = match &opts.path {
        Some(p) => std::fs::canonicalize(p)?,
        None => std::env::current_dir()?,
    };
    let root = paths::git_root(&start).unwrap_or(start);
    let root = std::fs::canonicalize(&root)?;
    eprintln!("local-brain init — {}", root.display());
    if crate::update::offer_in_init(opts.yes) {
        // Continue with the new binary, same arguments; it must not offer the update again.
        eprintln!("Continuing with the new version…\n");
        let status = std::process::Command::new(paths::exe())
            .args(std::env::args_os().skip(1))
            .env("LOCAL_BRAIN_NO_UPDATE_CHECK", "1")
            .status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
    let _cfg = configure_global(
        opts.reconfigure,
        opts.provider.as_deref(),
        opts.yes,
        opts.no_models,
    )?;

    let project = Project::new(root.clone());
    std::fs::create_dir_all(project.dir())?;
    if !project.config().exists() {
        std::fs::write(project.config(), config::DEFAULT_PROJECT_CONFIG)?;
    }
    let store = Store::open(&project)?;
    store.set_meta("initialized", &util::now().to_string())?;
    if !project.memory_md().exists() {
        crate::memory::export_md(&store, &project)?;
    }
    ok(&format!("created {}/", project.rel(&project.dir())));
    if integrate::ensure_gitignore(&root, ".local-brain/")? {
        ok("added .local-brain/ to .gitignore");
    }
    registry::register(&root)?;
    ok("registered project with the daemon registry");

    if !opts.no_agent {
        let agent = integrate::agent(&opts.agent)?;
        eprintln!("Integrating with {}…", agent.name());
        for line in agent.install(&project, &paths::exe())? {
            ok(&line);
        }
    }
    eprintln!(
        "\nNext: `local-brain learn` to build the index (then restart Claude Code in this folder)."
    );
    Ok(())
}

/// Session briefings add context instead of replacing output, so their "saving" is negative;
/// say so instead of printing a minus sign.
fn saved_label(saved: i64) -> String {
    if saved >= 0 {
        format!("saved ~{}", util::human_tokens(saved))
    } else {
        format!("adds ~{} tokens of context", util::human_tokens(-saved))
    }
}

pub fn learn(path: Option<&Path>, no_llm: bool, no_watch: bool) -> Result<()> {
    let project = paths::resolve_project(path)?;
    let cfg = Config::load(Some(&project));
    eprintln!("local-brain learn — {}", project.root.display());
    let llm = Llm::new(&cfg);
    if !no_llm && cfg.runtime.provider != "none" && !llm.is_up() {
        warn(&format!(
            "model runtime ({}) not reachable; continuing with deterministic fallbacks",
            cfg.runtime.provider
        ));
    }
    let t0 = std::time::Instant::now();
    let rep = crate::learn::learn(
        &project,
        &cfg,
        crate::learn::Options {
            no_llm,
            // With watching on, the daemon writes model summaries in the background, so the
            // index is usable right away; without it they are written here.
            summary_budget: (!no_watch).then_some(std::time::Duration::ZERO),
            ..Default::default()
        },
    )?;
    eprintln!();
    ok(&format!(
        "{} files scanned, {} (re)indexed, {} removed",
        rep.scanned, rep.indexed, rep.removed
    ));
    ok(&format!(
        "{} symbols, {} edges, {} chunks",
        rep.symbols, rep.edges, rep.chunks
    ));
    ok(&format!(
        "{} embeddings computed ({})",
        rep.embedded, rep.embed_model
    ));
    ok(&format!(
        "{} summaries written ({} by the local model, {} flagged by validation)",
        rep.summaries, rep.llm_summaries, rep.flagged
    ));
    if rep.summaries_pending > 0 {
        ok(&format!(
            "{} model summaries will be written in the background (`local-brain status` shows progress)",
            rep.summaries_pending
        ));
    }
    if rep.docs > 0 {
        ok(&format!("{} documents read locally", rep.docs));
    }
    if rep.archived_memories > 0 {
        ok(&format!(
            "{} memory entries archived (their files are gone)",
            rep.archived_memories
        ));
    }
    for w in rep.warnings.iter().take(10) {
        warn(w);
    }
    if rep.warnings.len() > 10 {
        warn(&format!("… {} more warnings", rep.warnings.len() - 10));
    }
    eprintln!(
        "  done in {}",
        crate::learn::fmt_duration(t0.elapsed().as_secs_f64().round() as u64)
    );
    if !no_watch {
        registry::set_watch(&project.root, true)?;
        match daemon::start() {
            Ok(pid) => ok(&format!("watching for changes (daemon pid {})", pid)),
            Err(e) => warn(&format!("could not start the daemon: {}", e)),
        }
    }
    Ok(())
}

pub fn watch(path: Option<&Path>, on: bool) -> Result<()> {
    let project = paths::resolve_project(path)?;
    registry::set_watch(&project.root, on)?;
    if on {
        let pid = daemon::start()?;
        ok(&format!(
            "watching {} (daemon pid {})",
            project.root.display(),
            pid
        ));
    } else {
        ok(&format!("stopped watching {}", project.root.display()));
    }
    Ok(())
}

pub fn status(path: Option<&Path>) -> Result<()> {
    let cfg_global = Config::load_global();
    println!("local-brain {}", env!("CARGO_PKG_VERSION"));
    let rt = Runtime::from_config(&cfg_global);
    let health = rt
        .health()
        .map(|h| format!("up ({})", h))
        .unwrap_or_else(|e| format!("down ({})", util::clip(&e.to_string(), 80)));
    println!(
        "Runtime   {} · {}",
        cfg_global.runtime.provider,
        if rt.enabled() {
            health
        } else {
            "deterministic mode".into()
        }
    );
    println!(
        "Models    tier {} · general {} · code {} · vision {} · embedding {}",
        cfg_global.models.tier,
        or_dash(&cfg_global.models.general),
        or_dash(&cfg_global.models.code),
        or_dash(&cfg_global.vision_model().unwrap_or_default()),
        if cfg_global.models.embedding.is_empty() {
            crate::embed::HASH_MODEL.to_string()
        } else {
            cfg_global.models.embedding.clone()
        }
    );
    match daemon::is_running() {
        Some(pid) => println!(
            "Daemon    running (pid {}, {} RAM) · {} watched project(s)",
            pid,
            daemon::memory_usage()
                .map(util::human_bytes)
                .unwrap_or_else(|| "?".into()),
            registry::load().iter().filter(|e| e.watch).count()
        ),
        None => {
            println!("Daemon    not running (`local-brain watch on` or `local-brain daemon start`)")
        }
    }
    let Ok(project) = paths::resolve_project(path) else {
        println!("Project   (not in an initialized project)");
        return Ok(());
    };
    let store = Store::open(&project)?;
    println!("\nProject   {}", project.root.display());
    let last = store
        .meta("last_learn")?
        .and_then(|s| s.parse::<i64>().ok());
    println!(
        "Index     {} files · {} symbols · {} edges · {} chunks · {} docs · last learn {}",
        store.count("SELECT count(*) FROM files"),
        store.count("SELECT count(*) FROM symbols"),
        store.count("SELECT count(*) FROM edges"),
        store.count("SELECT count(*) FROM chunks"),
        store.count("SELECT count(*) FROM docs"),
        last.map(util::fmt_ts).unwrap_or_else(|| "never".into())
    );
    let pending: i64 = store
        .meta("summaries_pending")?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    println!(
        "Summaries {} ({} by model{}, {} stale, {} flagged) · embeddings: {}",
        store.count("SELECT count(*) FROM summaries"),
        store.count("SELECT count(*) FROM summaries WHERE model != 'rules'"),
        if pending > 0 {
            format!(", {} more being written in the background", pending)
        } else {
            String::new()
        },
        store.count("SELECT count(*) FROM summaries WHERE stale = 1"),
        store.count("SELECT count(*) FROM summaries WHERE flagged = 1"),
        store.meta("embed_model")?.unwrap_or_else(|| "-".into())
    );
    println!(
        "Watch     {}",
        if registry::is_watched(&project.root) {
            "on"
        } else {
            "off"
        }
    );
    println!(
        "Memory    {} active entries ({})",
        store.count("SELECT count(*) FROM memory WHERE archived = 0"),
        project.rel(&project.memory_md())
    );
    for (label, since) in [("today", util::today_start()), ("all time", 0)] {
        let (calls, base, act) = store.metric_totals(since)?;
        let saved = base - act;
        let pct = if base > 0 && saved > 0 {
            format!(" ({:.0}%)", 100.0 * saved as f64 / base as f64)
        } else {
            String::new()
        };
        println!(
            "Savings   {:<8} {} calls · Claude would have seen ~{} tokens, saw {} · {}{}",
            label,
            calls,
            util::human_tokens(base),
            util::human_tokens(act),
            saved_label(saved),
            pct
        );
    }
    let br = store.metric_breakdown(0)?;
    if !br.is_empty() {
        println!("By source:");
        for (kind, n, b, a) in br {
            println!("  {:<15} {:>5} calls · {}", kind, n, saved_label(b - a));
        }
    }
    Ok(())
}

fn or_dash(s: &str) -> String {
    if s.is_empty() {
        "-".into()
    } else {
        s.to_string()
    }
}

pub fn ask(path: Option<&Path>, question: &str, scope: Option<&str>, no_llm: bool) -> Result<()> {
    let project = paths::resolve_project(path)?;
    let cfg = Config::load(Some(&project));
    let store = Store::open(&project)?;
    let llm = Llm::new(&cfg);
    let a = crate::query::query(
        &store,
        &project,
        &cfg,
        if no_llm { None } else { Some(&llm) },
        question,
        scope,
    )?;
    println!("{}", a.text);
    Ok(())
}

pub fn run_wrapped(args: &[String]) -> Result<i32> {
    if args.is_empty() {
        bail!("usage: local-brain run -- <command> [args…]");
    }
    let shell_cmd = if args.len() == 1 {
        args[0].clone()
    } else {
        args.iter()
            .map(|a| util::sh_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let project = paths::resolve_project(None).ok();
    let cfg = Config::load(project.as_ref());
    let out = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", &format!("{} 2>&1", shell_cmd)])
            .output()?
    } else {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{{ {}\n}} 2>&1", shell_cmd))
            .output()?
    };
    let code = out.status.code().unwrap_or(1);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let llm = Llm::new(&cfg);
    let c = crate::compress::compress(
        project.as_ref(),
        &cfg,
        Some(&llm),
        &text,
        &shell_cmd,
        Some(code),
    );
    print!("{}", c.text);
    if !c.text.ends_with('\n') {
        println!();
    }
    if let Some(p) = &project {
        if let Ok(st) = Store::open(p) {
            let _ = st.record_metric("cli", "run", c.original_tokens, c.tokens, None);
        }
    }
    Ok(code)
}

// ---- doctor ----------------------------------------------------------------------------------

struct Doc {
    failures: usize,
}

impl Doc {
    fn line(&mut self, okay: bool, label: &str, detail: &str) {
        println!(
            "  {} {:<28} {}",
            if okay { "✓" } else { "✗" },
            label,
            detail
        );
        if !okay {
            self.failures += 1;
        }
    }
}

fn mcp_selftest(root: &Path) -> Result<usize> {
    use std::io::{BufRead, Write};
    let mut child = std::process::Command::new(paths::exe())
        .args(["mcp", "--project"])
        .arg(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"doctor","version":"1"}}}}}}"#
    )?;
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )?;
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#)?;
    drop(stdin);
    let reader = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut tools = 0;
    for line in reader.lines().map_while(Result::ok) {
        let v: serde_json::Value = serde_json::from_str(&line)?;
        if v["id"] == 2 {
            tools = v["result"]["tools"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
        }
    }
    let _ = child.wait();
    Ok(tools)
}

pub fn doctor(
    path: Option<&Path>,
    set_provider: Option<&str>,
    url: Option<&str>,
    deep: bool,
) -> Result<bool> {
    if let Some(p) = set_provider {
        let valid = ["ollama", "lmstudio", "mlx", "llamacpp", "openai", "none"];
        if !valid.contains(&p) {
            bail!("provider must be one of {}", valid.join(", "));
        }
        let mut cfg = Config::load_global();
        cfg.runtime.provider = p.to_string();
        cfg.runtime.base_url = url
            .map(String::from)
            .unwrap_or_else(|| runtime::default_url(p).to_string());
        if p != "none" {
            let tier = cfg.models.tier.clone();
            apply_catalog(&mut cfg, p, &tier)?;
        }
        cfg.save_global()?;
        println!("Provider set to {} ({}).", p, cfg.runtime.base_url);
    }
    let mut d = Doc { failures: 0 };
    println!("local-brain doctor");
    println!("Installation");
    d.line(
        true,
        "binary",
        &format!("{} (v{})", paths::exe(), env!("CARGO_PKG_VERSION")),
    );
    let cfg_text = std::fs::read_to_string(paths::global_config());
    match &cfg_text {
        Ok(s) => match toml::from_str::<Config>(s) {
            Ok(_) => d.line(
                true,
                "global config",
                &paths::global_config().display().to_string(),
            ),
            Err(e) => d.line(false, "global config", &format!("parse error: {}", e)),
        },
        Err(_) => d.line(false, "global config", "missing — run `local-brain init`"),
    }
    match catalog::load() {
        Ok(c) => d.line(
            true,
            "model catalog",
            &format!(
                "v{} · tiers {}",
                c.version,
                c.tiers.keys().cloned().collect::<Vec<_>>().join("/")
            ),
        ),
        Err(e) => d.line(false, "model catalog", &e.to_string()),
    }
    let cfg = Config::load_global();

    println!("Runtime");
    let rt = Runtime::from_config(&cfg);
    if !rt.enabled() {
        d.line(
            true,
            "provider",
            "none (deterministic mode; set one with `local-brain doctor --set-provider ollama`)",
        );
    } else {
        match rt.health() {
            Ok(h) => {
                d.line(true, "provider", &format!("{} at {}", h, rt.base_url));
                let models = rt.list_models().unwrap_or_default();
                for (role, m) in [
                    ("general model", &cfg.models.general),
                    ("code model", &cfg.models.code),
                    ("vision model", &cfg.models.vision),
                    ("embedding model", &cfg.models.embedding),
                ] {
                    if m.is_empty() {
                        continue;
                    }
                    let present = rt.has_model(m);
                    d.line(
                        present,
                        role,
                        &if present {
                            m.clone()
                        } else {
                            format!(
                                "{} not found (available: {})",
                                m,
                                util::clip(&models.join(", "), 120)
                            )
                        },
                    );
                }
                if !cfg.models.embedding.is_empty() && rt.has_model(&cfg.models.embedding) {
                    match rt.embed(&cfg.models.embedding, &["hello world".to_string()]) {
                        Ok(v) => d.line(
                            true,
                            "embedding test",
                            &format!("{} dimensions", v[0].len()),
                        ),
                        Err(e) => d.line(false, "embedding test", &util::clip(&e.to_string(), 160)),
                    }
                }
                if deep {
                    if let Some(m) = cfg.text_model() {
                        let t0 = std::time::Instant::now();
                        match rt.generate(&m, "Reply with the single word: ready", &[], 10) {
                            Ok(r) => d.line(
                                true,
                                "generation test",
                                &format!(
                                    "{:?} in {:.1}s",
                                    util::clip(&r, 30),
                                    t0.elapsed().as_secs_f64()
                                ),
                            ),
                            Err(e) => {
                                d.line(false, "generation test", &util::clip(&e.to_string(), 160))
                            }
                        }
                    }
                }
            }
            Err(e) => d.line(
                false,
                "provider",
                &format!(
                    "{} not reachable at {}: {}",
                    cfg.runtime.provider,
                    rt.base_url,
                    util::clip(&e.to_string(), 100)
                ),
            ),
        }
    }

    println!("Daemon");
    match daemon::is_running() {
        Some(pid) => d.line(true, "daemon", &format!("running (pid {})", pid)),
        None => d.line(
            true,
            "daemon",
            "not running (optional; `local-brain watch on`)",
        ),
    }

    println!("Project");
    match paths::resolve_project(path) {
        Err(_) => d.line(
            false,
            "project",
            "not inside an initialized project (run `local-brain init`)",
        ),
        Ok(project) => {
            d.line(true, "project", &project.root.display().to_string());
            match Store::open(&project) {
                Ok(st) => {
                    let integrity: String = st
                        .conn
                        .query_row("PRAGMA quick_check", [], |r| r.get(0))
                        .unwrap_or_else(|_| "error".into());
                    d.line(
                        integrity == "ok",
                        "database",
                        &format!("{} ({})", project.rel(&project.db()), integrity),
                    );
                    let files = st.count("SELECT count(*) FROM files");
                    d.line(
                        files > 0,
                        "index",
                        &if files > 0 {
                            format!(
                                "{} files, last learn {}",
                                files,
                                st.meta("last_learn")?
                                    .and_then(|s| s.parse().ok())
                                    .map(util::fmt_ts)
                                    .unwrap_or_else(|| "?".into())
                            )
                        } else {
                            "empty — run `local-brain learn`".into()
                        },
                    );
                    let em = st.meta("embed_model")?.unwrap_or_default();
                    let want = if cfg.models.embedding.is_empty() {
                        crate::embed::HASH_MODEL.to_string()
                    } else {
                        cfg.models.embedding.clone()
                    };
                    if !em.is_empty() && em != want {
                        d.line(
                            false,
                            "embeddings",
                            &format!(
                                "index uses {} but config says {} — run `local-brain learn`",
                                em, want
                            ),
                        );
                    }
                    let v = serde_json::json!({"session_id": "doctor", "cwd": project.root, "hook_event_name": "SessionStart"});
                    match crate::hooks::handle("session-start", &v) {
                        Ok(Some(_)) => d.line(true, "hook handler", "SessionStart brief OK"),
                        Ok(None) => d.line(true, "hook handler", "OK (session brief disabled)"),
                        Err(e) => d.line(false, "hook handler", &e.to_string()),
                    }
                }
                Err(e) => d.line(false, "database", &e.to_string()),
            }
            match mcp_selftest(&project.root) {
                Ok(5) => d.line(true, "MCP server", "responds, 5 tools"),
                Ok(n) => d.line(
                    false,
                    "MCP server",
                    &format!("responds with {} tools (expected 5)", n),
                ),
                Err(e) => d.line(false, "MCP server", &e.to_string()),
            }
            let agent = integrate::ClaudeCode;
            use integrate::Agent;
            for c in agent.check(&project, &paths::exe()) {
                d.line(c.ok, &c.label, &c.detail);
            }
        }
    }
    println!();
    if d.failures == 0 {
        println!("All checks passed.");
    } else {
        println!("{} check(s) failed.", d.failures);
    }
    Ok(d.failures == 0)
}

pub fn uninstall(path: Option<&Path>, project_only: bool, yes: bool) -> Result<()> {
    let remove_project = |root: &Path| -> Result<()> {
        let project = Project::new(root.to_path_buf());
        use integrate::Agent;
        for l in integrate::ClaudeCode.uninstall(&project)? {
            ok(&format!("{}: {}", root.display(), l));
        }
        if project.dir().exists() {
            std::fs::remove_dir_all(project.dir())?;
            ok(&format!("{}: removed .local-brain/", root.display()));
        }
        registry::unregister(root)?;
        Ok(())
    };
    if project_only {
        let project = paths::resolve_project(path)?;
        if !yes
            && !util::confirm(
                &format!(
                    "Remove local-brain from {} (index, memory, hooks, MCP registration)?",
                    project.root.display()
                ),
                false,
            )
        {
            bail!("aborted");
        }
        return remove_project(&project.root);
    }
    let projects = registry::load();
    if !yes
        && !util::confirm(
            &format!(
                "Remove local-brain from {} project(s), stop the daemon and delete {}?",
                projects.len(),
                paths::home().display()
            ),
            false,
        )
    {
        bail!("aborted");
    }
    for p in projects {
        if let Err(e) = remove_project(&p.root) {
            warn(&format!("{}: {}", p.root.display(), e));
        }
    }
    let _ = daemon::uninstall_service();
    let _ = daemon::stop();
    if let Some(m) = crate::setup::remove_service(&Config::load_global().runtime.ollama_models_dir)
    {
        ok(&m);
    }
    if paths::home().exists() {
        std::fs::remove_dir_all(paths::home())?;
    }
    ok("removed global data. Delete the binary itself to finish (e.g. `brew uninstall local-brain` or remove it from your PATH).");
    Ok(())
}
