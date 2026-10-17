//! The versioned model catalog (`models.toml`). The repository copy is the default; users edit
//! `~/.local-brain/models.toml`.

use crate::paths;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

pub const DEFAULT_CATALOG: &str = include_str!("../models.toml");

#[derive(Deserialize, Debug, Clone)]
pub struct Catalog {
    pub version: u32,
    pub tiers: BTreeMap<String, Tier>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Tier {
    #[serde(default)]
    pub description: String,
    pub general: Option<ModelRef>,
    pub code: Option<ModelRef>,
    pub vision: Option<ModelRef>,
    pub embedding: Option<ModelRef>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct ModelRef {
    pub ollama: Option<String>,
    pub lmstudio: Option<String>,
    pub mlx: Option<String>,
    pub llamacpp: Option<String>,
    pub openai: Option<String>,
    #[serde(default)]
    pub size_gb: f64,
    #[serde(default)]
    pub vision: bool,
}

impl ModelRef {
    pub fn name_for(&self, provider: &str) -> Option<String> {
        let v = match provider {
            "ollama" => self.ollama.as_ref(),
            "lmstudio" => self.lmstudio.as_ref(),
            "mlx" => self.mlx.as_ref(),
            "llamacpp" => self.llamacpp.as_ref(),
            "openai" => self.openai.as_ref().or(self.lmstudio.as_ref()),
            _ => None,
        };
        v.filter(|s| !s.is_empty()).cloned()
    }
}

impl Tier {
    /// Peak memory if models are loaded on demand: the largest generator plus the embedder.
    pub fn peak_gb(&self) -> f64 {
        let gen = [&self.general, &self.code, &self.vision]
            .iter()
            .filter_map(|m| m.as_ref().map(|m| m.size_gb))
            .fold(0.0, f64::max);
        gen + self.embedding.as_ref().map(|m| m.size_gb).unwrap_or(0.0)
    }
}

pub fn load() -> Result<Catalog> {
    let p = paths::models_catalog();
    let s = std::fs::read_to_string(&p).unwrap_or_else(|_| DEFAULT_CATALOG.to_string());
    toml::from_str(&s).with_context(|| format!("parsing model catalog {}", p.display()))
}

/// Write the default catalog to the global home unless the user already has one.
pub fn install_default() -> Result<()> {
    let p = paths::models_catalog();
    if !p.exists() {
        paths::ensure_home()?;
        std::fs::write(&p, DEFAULT_CATALOG)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_catalog_parses() {
        let c: super::Catalog = toml::from_str(super::DEFAULT_CATALOG).unwrap();
        assert!(
            c.tiers.contains_key("S") && c.tiers.contains_key("M") && c.tiers.contains_key("L")
        );
        assert!(c.tiers["S"].peak_gb() < 8.0);
        assert_eq!(
            c.tiers["M"]
                .general
                .as_ref()
                .unwrap()
                .name_for("ollama")
                .unwrap(),
            "gemma3:12b"
        );
    }
}
