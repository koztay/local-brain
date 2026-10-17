//! Global configuration (`~/.local-brain/config.toml`) and per-project overrides.

use crate::paths::{self, Project};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub runtime: RuntimeCfg,
    pub models: ModelsCfg,
    pub compress: CompressCfg,
    pub hooks: HooksCfg,
    pub learn: LearnCfg,
    pub index: IndexCfg,
    pub daemon: DaemonCfg,
    pub network: NetworkCfg,
    pub telemetry: TelemetryCfg,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct RuntimeCfg {
    /// `ollama`, `lmstudio`, `mlx`, `llamacpp`, `openai` (any OpenAI-compatible server) or `none`.
    pub provider: String,
    pub base_url: String,
    /// How long the runtime keeps a model in memory after the last request.
    pub keep_alive: String,
    pub timeout_secs: u64,
    pub api_key: String,
    /// Where Ollama keeps its models when local-brain installed it on another drive.
    /// Empty = Ollama's default location.
    pub ollama_models_dir: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ModelsCfg {
    /// Hardware tier: S, M or L. Chosen by `init`; edit to override.
    pub tier: String,
    /// General multimodal model (summaries, compression, vision on S/M tiers).
    pub general: String,
    /// Optional dedicated code model (L tier). Empty = use `general`.
    pub code: String,
    /// Optional dedicated vision model (L tier). Empty = use `general`.
    pub vision: String,
    /// Embedding model. Empty = built-in hashing embeddings (no model needed).
    pub embedding: String,
    /// Whether `general` accepts images.
    pub general_has_vision: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct CompressCfg {
    /// Outputs at or under this many tokens pass through untouched.
    pub threshold_tokens: i64,
    /// Target size of a compressed output.
    pub target_tokens: i64,
    /// Use the local LLM to summarize what deterministic rules could not shrink.
    pub llm_summary: bool,
    pub raw_ttl_days: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct HooksCfg {
    pub bash: bool,
    pub read_docs: bool,
    pub read_large: bool,
    pub grep: bool,
    pub glob: bool,
    pub session_brief: bool,
    pub session_memory: bool,
    /// Add the most relevant files to each prompt (UserPromptSubmit).
    pub prompt_context: bool,
    /// Read images/PDFs outside the project (e.g. ~/Desktop screenshots) locally too.
    pub read_outside_docs: bool,
    pub large_file_lines: usize,
    pub large_file_tokens: i64,
    pub grep_max_results: usize,
    pub grep_show: usize,
    pub glob_max_results: usize,
    pub brief_tokens: i64,
    pub prompt_context_tokens: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct LearnCfg {
    pub llm_summaries: bool,
    pub max_llm_files: usize,
    pub max_file_bytes: u64,
    pub max_images: usize,
    pub embed_batch: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct IndexCfg {
    /// Extra gitignore-style patterns that are never indexed.
    pub exclude: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct DaemonCfg {
    pub debounce_ms: u64,
    pub idle_unload_minutes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct NetworkCfg {
    /// Allow `init` to install Ollama and download models.
    pub allow_downloads: bool,
    /// Ask GitHub once a day whether a newer local-brain release exists (no data is sent).
    pub check_updates: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct TelemetryCfg {
    /// Always off. There is no telemetry endpoint; the flag is reserved for an opt-in future.
    pub enabled: bool,
}

impl Default for RuntimeCfg {
    fn default() -> Self {
        RuntimeCfg {
            provider: "none".into(),
            base_url: String::new(),
            keep_alive: "10m".into(),
            timeout_secs: 180,
            api_key: String::new(),
            ollama_models_dir: String::new(),
        }
    }
}

impl Default for ModelsCfg {
    fn default() -> Self {
        ModelsCfg {
            tier: "S".into(),
            general: String::new(),
            code: String::new(),
            vision: String::new(),
            embedding: String::new(),
            general_has_vision: true,
        }
    }
}

impl Default for CompressCfg {
    fn default() -> Self {
        CompressCfg {
            threshold_tokens: 2000,
            target_tokens: 1200,
            llm_summary: true,
            raw_ttl_days: 7,
        }
    }
}

impl Default for HooksCfg {
    fn default() -> Self {
        HooksCfg {
            bash: true,
            read_docs: true,
            read_large: true,
            grep: true,
            glob: true,
            session_brief: true,
            session_memory: true,
            prompt_context: true,
            read_outside_docs: true,
            large_file_lines: 600,
            large_file_tokens: 8000,
            grep_max_results: 80,
            grep_show: 30,
            glob_max_results: 200,
            brief_tokens: 500,
            prompt_context_tokens: 600,
        }
    }
}

impl Default for LearnCfg {
    fn default() -> Self {
        LearnCfg {
            llm_summaries: true,
            max_llm_files: 300,
            max_file_bytes: 1_000_000,
            max_images: 50,
            embed_batch: 32,
        }
    }
}

impl Default for DaemonCfg {
    fn default() -> Self {
        DaemonCfg {
            debounce_ms: 500,
            idle_unload_minutes: 10,
        }
    }
}

impl Default for NetworkCfg {
    fn default() -> Self {
        NetworkCfg {
            allow_downloads: true,
            check_updates: true,
        }
    }
}

/// Per-project overrides in `<project>/.local-brain/config.toml`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ProjectCfg {
    pub index: IndexCfg,
    pub compress: Option<CompressCfg>,
    pub hooks: Option<HooksCfg>,
}

impl Config {
    pub fn load_global() -> Config {
        let p = paths::global_config();
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                eprintln!("warning: could not parse {}: {}", p.display(), e);
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    /// Global config merged with project overrides.
    pub fn load(project: Option<&Project>) -> Config {
        let mut c = Config::load_global();
        if let Some(pr) = project {
            if let Ok(s) = std::fs::read_to_string(pr.config()) {
                if let Ok(pc) = toml::from_str::<ProjectCfg>(&s) {
                    c.index.exclude.extend(pc.index.exclude);
                    if let Some(cc) = pc.compress {
                        c.compress = cc;
                    }
                    if let Some(h) = pc.hooks {
                        c.hooks = h;
                    }
                }
            }
        }
        c
    }

    pub fn save_global(&self) -> Result<()> {
        paths::ensure_home()?;
        let s = toml::to_string_pretty(self)?;
        let header = "# local-brain global configuration. Edit freely; `local-brain doctor` validates it.\n\n";
        std::fs::write(paths::global_config(), format!("{}{}", header, s))
            .context("writing global config")
    }

    pub fn vision_model(&self) -> Option<String> {
        if !self.models.vision.is_empty() {
            return Some(self.models.vision.clone());
        }
        if self.models.general_has_vision && !self.models.general.is_empty() {
            return Some(self.models.general.clone());
        }
        None
    }

    pub fn text_model(&self) -> Option<String> {
        if !self.models.general.is_empty() {
            return Some(self.models.general.clone());
        }
        if !self.models.code.is_empty() {
            return Some(self.models.code.clone());
        }
        None
    }

    pub fn code_model(&self) -> Option<String> {
        if !self.models.code.is_empty() {
            return Some(self.models.code.clone());
        }
        self.text_model()
    }
}

pub const DEFAULT_PROJECT_CONFIG: &str = r#"# local-brain project configuration.
# Paths in .gitignore, .env*, key files and these patterns are never indexed.
[index]
exclude = []

# Uncomment to override global compression settings for this project.
# [compress]
# threshold_tokens = 2000
# target_tokens = 1200
# llm_summary = true
# raw_ttl_days = 7
"#;
