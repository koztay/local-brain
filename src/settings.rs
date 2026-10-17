//! `local-brain config`: list, explain, get, set and reset the global settings
//! (`~/.local-brain/config.toml`) without editing TOML by hand.
//!
//! Values are read and written through `toml::Value`, so every field of `Config` is reachable
//! with a `section.key` name; `SETTINGS` adds the explanation and, for settings that depend on
//! the machine, how to tune them. A test keeps `SETTINGS` and `Config` in sync.

use crate::config::Config;
use crate::paths;
use anyhow::{bail, Context, Result};
use toml::Value;

/// The RAM the defaults were chosen for (a tier-S machine).
const TUNED_FOR_GB: u64 = 16;

struct Setting {
    key: &'static str,
    help: &'static str,
    /// How to tune it for more or less RAM / a faster or slower machine; empty = not
    /// hardware-dependent.
    tune: &'static str,
}

const fn s(key: &'static str, help: &'static str, tune: &'static str) -> Setting {
    Setting { key, help, tune }
}

#[rustfmt::skip]
const SETTINGS: &[Setting] = &[
    s("runtime.provider", "Local model runtime: ollama, lmstudio, mlx, llamacpp, openai (any OpenAI-compatible server) or none.", ""),
    s("runtime.base_url", "Where the runtime listens.", ""),
    s("runtime.keep_alive", "How long the runtime keeps a model in memory after the last request (Ollama duration, e.g. \"10m\", \"30s\", \"0\").", "8-16 GB: \"5m\"-\"10m\". 32 GB+: \"30m\" avoids reload delays."),
    s("runtime.timeout_secs", "Per-request timeout for model calls (hooks use shorter limits).", "Raise on slow machines or large models."),
    s("runtime.api_key", "Bearer token for servers that require one.", ""),
    s("runtime.ollama_models_dir", "Where Ollama keeps its models when local-brain installed it (OLLAMA_MODELS). Empty = Ollama's default.", ""),
    s("models.tier", "Hardware tier S, M or L; picks the models from models.toml.", "S: 8-16 GB. M: 24-32 GB. L: 64 GB+ or a large GPU."),
    s("models.general", "Main model: summaries, compression, answers; also vision when general_has_vision.", "Bigger models write better summaries but need more RAM."),
    s("models.code", "Optional dedicated code model. Empty = general.", "Only worth it on 32 GB+."),
    s("models.vision", "Optional dedicated vision model. Empty = general if it has vision.", ""),
    s("models.embedding", "Embedding model. Empty = built-in hash embeddings (no model needed).", ""),
    s("models.general_has_vision", "Whether the general model accepts images.", ""),
    s("compress.threshold_tokens", "Command outputs at or under this size pass through untouched.", ""),
    s("compress.target_tokens", "Target size of compressed outputs.", ""),
    s("compress.llm_summary", "Use the local model when deterministic rules cannot shrink an output enough.", ""),
    s("compress.raw_ttl_days", "How long full outputs stay available for brain_raw.", ""),
    s("hooks.bash", "Compress long Bash output.", ""),
    s("hooks.read_docs", "Read images/PDFs locally instead of sending them to the cloud model.", ""),
    s("hooks.read_large", "Return an outline for large files.", ""),
    s("hooks.grep", "Rank and trim oversized Grep results.", ""),
    s("hooks.glob", "Trim oversized Glob results.", ""),
    s("hooks.session_brief", "Inject the project brief at session start.", ""),
    s("hooks.session_memory", "Summarize sessions into project memory.", ""),
    s("hooks.prompt_context", "Add where-the-code-lives context to task prompts.", ""),
    s("hooks.read_outside_docs", "Also read images/PDFs outside the project locally (e.g. ~/Desktop screenshots).", ""),
    s("hooks.large_file_lines", "Line count above which a file counts as large.", ""),
    s("hooks.large_file_tokens", "Token count above which a file counts as large.", ""),
    s("hooks.grep_max_results", "Grep results above this are trimmed.", ""),
    s("hooks.grep_show", "How many ranked Grep results are shown.", ""),
    s("hooks.glob_max_results", "Glob matches above this are trimmed.", ""),
    s("hooks.brief_tokens", "Budget of the session brief.", ""),
    s("hooks.prompt_context_tokens", "Budget of the per-prompt context note.", ""),
    s("learn.llm_summaries", "Use the local model for file/module/project summaries.", ""),
    s("learn.max_llm_files", "How many files get model-written summaries (most referenced first); the rest keep rule-based ones.", "Lower to finish background work sooner on slow machines."),
    s("learn.max_file_bytes", "Larger text files are skipped (documents may be 50x larger).", ""),
    s("learn.max_images", "Images described per learn run.", ""),
    s("learn.embed_batch", "Texts per embedding request.", "64-128 on 32 GB+ is faster; lower if the runtime runs out of memory."),
    s("index.exclude", "Extra gitignore-style patterns that are never indexed (comma-separated or a TOML array).", ""),
    s("daemon.debounce_ms", "Quiet time before a batch of file changes is indexed.", ""),
    s("daemon.idle_unload_minutes", "Unload the models after this long without any activity.", "8-16 GB: 5-10. 32 GB+: 30 or more."),
    s("daemon.min_free_memory_pct", "Background model work waits, and the model is unloaded, while free RAM (percent) is below this; on macOS also while the kernel reports memory pressure. 0 disables the check.", "8 GB: 20-25. 16 GB: 15. 32 GB+: 10. 0 disables."),
    s("daemon.bg_quiet_secs", "Background model work starts only after this long without file changes.", ""),
    s("daemon.bg_pass_secs", "Model time per background pass; file watching resumes between passes.", "Longer passes finish a queue sooner but keep the model busy longer."),
    s("daemon.bg_backoff_secs", "How long to wait (model unloaded) after finding free memory below the floor.", "Shorter recovers sooner but reloads the model more often."),
    s("daemon.bg_pace_secs", "Minimum gap between passes while free memory is under twice the floor (model unloaded in the gaps).", "16 GB: 90. 32 GB+: 0-30. Lower = faster summaries, more RAM held."),
    s("network.allow_downloads", "Allow init to install Ollama and download models.", ""),
    s("network.check_updates", "Ask GitHub once a day whether a newer release exists (no data is sent).", ""),
    s("telemetry.enabled", "Reserved; there is no telemetry.", ""),
];

/// Keys whose values are not printed.
const SECRET: &[&str] = &["runtime.api_key"];

fn setting(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}

fn table(cfg: &Config) -> Result<toml::Table> {
    Ok(toml::Table::try_from(cfg)?)
}

fn lookup<'a>(t: &'a toml::Table, key: &str) -> Option<&'a Value> {
    let (sec, k) = key.split_once('.')?;
    t.get(sec)?.as_table()?.get(k)
}

fn show(key: &str, v: &Value) -> String {
    if SECRET.contains(&key) {
        return match v.as_str() {
            Some("") => "\"\"".into(),
            _ => "(set)".into(),
        };
    }
    v.to_string()
}

fn unknown(key: &str) -> anyhow::Error {
    let sec = key.split_once('.').map_or(key, |(s, _)| s);
    let near: Vec<&str> = SETTINGS
        .iter()
        .map(|s| s.key)
        .filter(|k| k.starts_with(&format!("{}.", sec)))
        .collect();
    if near.is_empty() {
        anyhow::anyhow!("unknown setting `{}` (see `local-brain config list`)", key)
    } else {
        anyhow::anyhow!(
            "unknown setting `{}`; in [{}]: {}",
            key,
            sec,
            near.join(", ")
        )
    }
}

fn ram_gb() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    (sys.total_memory() as f64 / 1024f64.powi(3)).round() as u64
}

fn print_one(key: &str, cur: &Value, def: &Value, s: &Setting) {
    let changed = cur != def;
    println!(
        "  {} = {}{}",
        key.split_once('.').map_or(key, |(_, k)| k),
        show(key, cur),
        if changed {
            format!("   (default {})", show(key, def))
        } else {
            String::new()
        }
    );
    println!("      {}", s.help);
    if !s.tune.is_empty() {
        println!("      Tuning: {}", s.tune);
    }
}

/// `local-brain config [list]`: every setting with its value, default and meaning.
pub fn list(changed_only: bool) -> Result<()> {
    let cur = table(&Config::load_global())?;
    let def = table(&Config::default())?;
    let ram = ram_gb();
    println!(
        "Defaults are tuned for a {} GB machine; this one has {} GB of RAM.{}",
        TUNED_FOR_GB,
        ram,
        if ram >= TUNED_FOR_GB * 2 {
            " See the Tuning notes to use more of it."
        } else if ram < TUNED_FOR_GB {
            " See the Tuning notes to keep memory use down."
        } else {
            ""
        }
    );
    println!("File: {}", paths::global_config().display());
    println!(
        "Change: local-brain config set <key> <value>  ·  default: local-brain config reset <key>"
    );
    let mut section = "";
    let mut shown = 0;
    for s in SETTINGS {
        let (Some(c), Some(d)) = (lookup(&cur, s.key), lookup(&def, s.key)) else {
            continue;
        };
        if changed_only && c == d {
            continue;
        }
        let sec = s.key.split_once('.').map_or("", |(a, _)| a);
        if sec != section {
            println!("\n[{}]", sec);
            section = sec;
        }
        print_one(s.key, c, d, s);
        shown += 1;
    }
    if changed_only && shown == 0 {
        println!("\nAll settings are at their defaults.");
    }
    println!("\nProjects can override [index], [compress] and [hooks] in <project>/.local-brain/config.toml.");
    Ok(())
}

/// `local-brain config get <key>`.
pub fn get(key: &str) -> Result<()> {
    let s = setting(key).ok_or_else(|| unknown(key))?;
    let cur = table(&Config::load_global())?;
    let def = table(&Config::default())?;
    let (Some(c), Some(d)) = (lookup(&cur, key), lookup(&def, key)) else {
        return Err(unknown(key));
    };
    print_one(key, c, d, s);
    Ok(())
}

/// Parse `raw` as a value of the same type as `like`.
fn parse_like(key: &str, like: &Value, raw: &str) -> Result<Value> {
    let raw = raw.trim();
    Ok(match like {
        Value::String(_) => Value::String(raw.trim_matches('"').to_string()),
        Value::Boolean(_) => Value::Boolean(match raw.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" | "1" => true,
            "false" | "off" | "no" | "0" => false,
            _ => bail!("`{}` takes true or false, not `{}`", key, raw),
        }),
        Value::Integer(_) => Value::Integer(
            raw.replace('_', "")
                .parse()
                .with_context(|| format!("`{}` takes a whole number, not `{}`", key, raw))?,
        ),
        Value::Float(_) => Value::Float(
            raw.parse()
                .with_context(|| format!("`{}` takes a number, not `{}`", key, raw))?,
        ),
        Value::Array(_) => {
            if raw.starts_with('[') {
                toml::from_str::<toml::Table>(&format!("v = {}", raw))
                    .ok()
                    .and_then(|mut t| t.remove("v"))
                    .with_context(|| format!("`{}`: not a valid TOML array: {}", key, raw))?
            } else {
                Value::Array(
                    raw.split(',')
                        .map(str::trim)
                        .filter(|x| !x.is_empty())
                        .map(|x| Value::String(x.to_string()))
                        .collect(),
                )
            }
        }
        _ => bail!(
            "`{}` cannot be set from the command line; edit the file",
            key
        ),
    })
}

/// Apply `key = raw` to `cfg`, validating type and range. Returns the new value.
fn apply(cfg: &Config, key: &str, raw: &str) -> Result<(Config, Value)> {
    setting(key).ok_or_else(|| unknown(key))?;
    let mut t = table(cfg)?;
    let (sec, k) = key.split_once('.').ok_or_else(|| unknown(key))?;
    let like = lookup(&t, key).ok_or_else(|| unknown(key))?.clone();
    let v = parse_like(key, &like, raw)?;
    if key.ends_with("_pct") && !matches!(v.as_integer(), Some(0..=100)) {
        bail!("`{}` is a percentage: 0-100", key);
    }
    t.get_mut(sec)
        .and_then(Value::as_table_mut)
        .ok_or_else(|| unknown(key))?
        .insert(k.to_string(), v.clone());
    let new: Config = Value::Table(t)
        .try_into()
        .with_context(|| format!("`{}`: invalid value `{}`", key, raw))?;
    Ok((new, v))
}

/// `local-brain config set <key> <value>`.
pub fn set(key: &str, raw: &str) -> Result<()> {
    if key == "runtime.provider" {
        bail!(
            "use `local-brain doctor --set-provider {}` — it also sets the URL and matching models",
            raw
        );
    }
    let (cfg, v) = apply(&Config::load_global(), key, raw)?;
    cfg.save_global()?;
    println!("{} = {}", key, show(key, &v));
    if key.starts_with("daemon.") || key.starts_with("runtime.") {
        println!("The daemon picks this up on its next pass; no restart needed.");
    }
    Ok(())
}

/// `local-brain config reset <key>`.
pub fn reset(key: &str) -> Result<()> {
    setting(key).ok_or_else(|| unknown(key))?;
    let def = table(&Config::default())?;
    let d = lookup(&def, key).ok_or_else(|| unknown(key))?;
    let raw = match d {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let (cfg, v) = apply(&Config::load_global(), key, &raw)?;
    cfg.save_global()?;
    println!("{} = {} (default)", key, show(key, &v));
    Ok(())
}

/// The memory/background settings `optimize` tunes for a machine with `ram_gb` of RAM.
/// 16 GB gets the defaults; less RAM holds the model for shorter and backs off earlier,
/// more RAM keeps it loaded longer and summarizes faster.
fn recommended(ram_gb: u64) -> Vec<(&'static str, &'static str)> {
    // keep_alive, idle_unload, min_free, backoff, pace, pass, embed_batch, max_llm_files
    let v: [&str; 8] = match ram_gb {
        0..=11 => ["5m", "5", "25", "300", "180", "20", "16", "150"],
        12..=23 => ["10m", "10", "15", "180", "90", "30", "32", "300"],
        24..=47 => ["20m", "20", "10", "120", "30", "60", "64", "500"],
        _ => ["30m", "30", "8", "60", "0", "120", "128", "800"],
    };
    [
        "runtime.keep_alive",
        "daemon.idle_unload_minutes",
        "daemon.min_free_memory_pct",
        "daemon.bg_backoff_secs",
        "daemon.bg_pace_secs",
        "daemon.bg_pass_secs",
        "learn.embed_batch",
        "learn.max_llm_files",
    ]
    .into_iter()
    .zip(v)
    .collect()
}

/// `local-brain optimize`: tune the memory and background settings to this machine's RAM.
/// Models are not changed (that needs downloads); a tier mismatch is only pointed out.
pub fn optimize(dry_run: bool, yes: bool) -> Result<()> {
    use std::io::IsTerminal;
    let ram = ram_gb();
    let cfg = Config::load_global();
    let cur = table(&cfg)?;
    let mut new = cfg.clone();
    let mut changes = Vec::new();
    for (key, raw) in recommended(ram) {
        let (next, v) = apply(&new, key, raw)?;
        let old = lookup(&cur, key)
            .cloned()
            .unwrap_or(Value::String(String::new()));
        if old != v {
            changes.push((key, old, v));
        }
        new = next;
    }
    println!("This machine has {} GB of RAM.", ram);
    if let Ok(cat) = crate::catalog::load() {
        let tier = crate::hardware::profile().choose_tier(&cat);
        if !tier.eq_ignore_ascii_case(&cfg.models.tier) {
            println!(
                "Model tier is {} but this machine fits tier {}; switching models needs downloads, so run \
                 `local-brain init --reconfigure` if you want that.",
                cfg.models.tier, tier
            );
        }
    }
    if changes.is_empty() {
        println!("Settings are already tuned for it; nothing to change.");
        return Ok(());
    }
    println!("Recommended changes:");
    for (key, old, v) in &changes {
        println!("  {:<30} {} -> {}", key, show(key, old), show(key, v));
        if let Some(s) = setting(key) {
            println!("      {}", s.help);
        }
    }
    if dry_run {
        return Ok(());
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            println!("Not applied: rerun with --yes to apply without a prompt.");
            return Ok(());
        }
        if !crate::util::confirm("Apply these changes?", true) {
            println!("Nothing changed.");
            return Ok(());
        }
    }
    new.save_global()?;
    println!(
        "Applied {} change(s). Fine-tune any of them with `local-brain config set <key> <value>`.",
        changes.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(t: &toml::Table) -> Vec<String> {
        let mut v = Vec::new();
        for (sec, inner) in t {
            if let Some(inner) = inner.as_table() {
                for k in inner.keys() {
                    v.push(format!("{}.{}", sec, k));
                }
            }
        }
        v
    }

    #[test]
    fn every_setting_is_explained_and_exists() {
        let t = table(&Config::default()).unwrap();
        for k in keys(&t) {
            assert!(
                setting(&k).is_some(),
                "add `{}` to SETTINGS in settings.rs",
                k
            );
        }
        for s in SETTINGS {
            assert!(
                lookup(&t, s.key).is_some(),
                "`{}` is not a config field",
                s.key
            );
        }
    }

    #[test]
    fn set_parses_by_type() {
        let c = Config::default();
        let (c2, _) = apply(&c, "daemon.bg_pace_secs", "30").unwrap();
        assert_eq!(c2.daemon.bg_pace_secs, 30);
        let (c2, _) = apply(&c, "hooks.grep", "off").unwrap();
        assert!(!c2.hooks.grep);
        let (c2, _) = apply(&c, "runtime.keep_alive", "30m").unwrap();
        assert_eq!(c2.runtime.keep_alive, "30m");
        let (c2, _) = apply(&c, "index.exclude", "fixtures/**, *.snap").unwrap();
        assert_eq!(c2.index.exclude, vec!["fixtures/**", "*.snap"]);
    }

    #[test]
    fn set_rejects_bad_values() {
        let c = Config::default();
        assert!(apply(&c, "daemon.bg_pace_secs", "fast").is_err());
        assert!(apply(&c, "daemon.bg_pace_secs", "-5").is_err());
        assert!(apply(&c, "daemon.min_free_memory_pct", "150").is_err());
        assert!(apply(&c, "daemon.nope", "1").is_err());
        assert!(apply(&c, "hooks.grep", "maybe").is_err());
    }

    #[test]
    fn recommendations_are_valid_and_16gb_is_the_default() {
        let def = Config::default();
        for ram in [4, 8, 16, 32, 64, 128] {
            let mut c = def.clone();
            for (k, raw) in recommended(ram) {
                c = apply(&c, k, raw).unwrap().0;
            }
            if ram == TUNED_FOR_GB {
                assert_eq!(table(&c).unwrap(), table(&def).unwrap());
            }
        }
        // Less RAM backs off earlier, more RAM keeps the model longer.
        let pct = |ram| recommended(ram)[2].1.parse::<u64>().unwrap();
        assert!(pct(8) > pct(16) && pct(16) > pct(64));
    }

    #[test]
    fn secrets_are_not_printed() {
        assert_eq!(
            show("runtime.api_key", &Value::String("sk-1".into())),
            "(set)"
        );
        assert_eq!(
            show("runtime.api_key", &Value::String(String::new())),
            "\"\""
        );
    }
}
