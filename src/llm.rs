//! Thin role-aware wrapper over the runtime: text / code / vision generation with graceful
//! degradation when no model is available.

use crate::config::Config;
use crate::runtime::Runtime;
use anyhow::{bail, Result};
use std::cell::OnceCell;

pub struct Llm {
    pub rt: Runtime,
    pub text_model: Option<String>,
    pub code_model: Option<String>,
    pub vision_model: Option<String>,
    up: OnceCell<bool>,
}

impl Llm {
    pub fn new(cfg: &Config) -> Llm {
        Llm::with_runtime(cfg, Runtime::from_config(cfg))
    }

    pub fn with_runtime(cfg: &Config, rt: Runtime) -> Llm {
        Llm {
            text_model: cfg.text_model(),
            code_model: cfg.code_model(),
            vision_model: cfg.vision_model(),
            rt,
            up: OnceCell::new(),
        }
    }

    /// Whether the runtime answers (checked once per process).
    pub fn is_up(&self) -> bool {
        *self.up.get_or_init(|| self.rt.is_up())
    }

    pub fn has_text(&self) -> bool {
        self.text_model.is_some() && self.is_up()
    }

    pub fn has_vision(&self) -> bool {
        self.vision_model.is_some() && self.is_up()
    }

    pub fn text(&self, prompt: &str, max_tokens: u32) -> Result<String> {
        let Some(m) = &self.text_model else {
            bail!("no text model configured")
        };
        if !self.is_up() {
            bail!("model runtime is not reachable");
        }
        self.rt.generate(m, prompt, &[], max_tokens)
    }

    pub fn code(&self, prompt: &str, max_tokens: u32) -> Result<String> {
        let Some(m) = &self.code_model else {
            bail!("no code model configured")
        };
        if !self.is_up() {
            bail!("model runtime is not reachable");
        }
        self.rt.generate(m, prompt, &[], max_tokens)
    }

    pub fn vision(&self, prompt: &str, image: &[u8], max_tokens: u32) -> Result<String> {
        let Some(m) = &self.vision_model else {
            bail!("no vision-capable model configured")
        };
        if !self.is_up() {
            bail!("model runtime is not reachable");
        }
        self.rt.generate(m, prompt, &[image.to_vec()], max_tokens)
    }

    pub fn model_label(&self) -> String {
        self.text_model.clone().unwrap_or_else(|| "rules".into())
    }
}
