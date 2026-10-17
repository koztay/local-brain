//! Embeddings: a local embedding model through the runtime adapter, with a deterministic
//! feature-hashing fallback so semantic search works even with no model installed.

use crate::config::Config;
use crate::runtime::Runtime;
use crate::util;
use anyhow::Result;

pub const HASH_MODEL: &str = "hash-v1-512";
const DIM: usize = 512;

pub struct Embedder {
    rt: Runtime,
    model: Option<String>,
}

impl Embedder {
    /// Use the configured embedding model when the runtime is reachable, else hashing.
    pub fn new(cfg: &Config, rt: &Runtime) -> Embedder {
        let model = if !cfg.models.embedding.is_empty() && rt.is_up() {
            Some(cfg.models.embedding.clone())
        } else {
            None
        };
        Embedder {
            rt: rt.clone(),
            model,
        }
    }

    /// An embedder for a specific stored model id (used at query time).
    pub fn for_id(id: &str, rt: &Runtime) -> Embedder {
        Embedder {
            rt: rt.clone(),
            model: if id == HASH_MODEL || id.is_empty() {
                None
            } else {
                Some(id.to_string())
            },
        }
    }

    pub fn id(&self) -> String {
        self.model.clone().unwrap_or_else(|| HASH_MODEL.to_string())
    }

    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        match &self.model {
            Some(m) => {
                let mut v = self.rt.embed(m, texts)?;
                for e in v.iter_mut() {
                    normalize(e);
                }
                Ok(v)
            }
            None => Ok(texts.iter().map(|t| hash_embed(t)).collect()),
        }
    }
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Feature-hashed bag of words + identifier parts + word bigrams, L2-normalized.
pub fn hash_embed(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; DIM];
    let mut add = |feat: &str, w: f32| {
        let h = fnv(feat);
        let idx = (h % DIM as u64) as usize;
        let sign = if (h >> 63) & 1 == 1 { -1.0 } else { 1.0 };
        v[idx] += sign * w;
    };
    let mut prev: Option<String> = None;
    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.is_empty() || raw.len() > 60 {
            continue;
        }
        let lower = stem(&raw.to_lowercase());
        if STOP.contains(&lower.as_str()) {
            continue;
        }
        add(&lower, 1.0);
        let parts = util::split_ident(raw);
        if parts.len() > 1 {
            for p in &parts {
                if p.len() > 1 && !STOP.contains(&p.as_str()) {
                    add(&stem(p), 0.6);
                }
            }
        }
        if let Some(p) = &prev {
            add(&format!("{} {}", p, lower), 0.4);
        }
        prev = Some(lower);
    }
    normalize(&mut v);
    v
}

/// Very small suffix stripper so "masked", "masking" and "masks" share a feature.
pub fn stem(w: &str) -> String {
    for suf in [
        "ations", "ation", "ings", "ing", "ers", "er", "ies", "es", "ed", "s",
    ] {
        if w.len() > suf.len() + 3 && w.ends_with(suf) {
            let base = &w[..w.len() - suf.len()];
            return if suf == "ies" {
                format!("{}y", base)
            } else {
                base.to_string()
            };
        }
    }
    if w.len() > 4 && w.ends_with('e') {
        return w[..w.len() - 1].to_string();
    }
    w.to_string()
}

const STOP: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "is", "it", "for", "on", "with", "as", "be",
    "this", "that", "by", "at", "from", "are", "was", "if", "else", "return", "let", "const",
    "var", "fn", "def", "func", "function", "pub", "self", "new", "true", "false", "null", "none",
    "nil", "where", "what", "how", "which", "who", "does", "do", "i", "we", "you",
];

pub fn normalize(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_embeddings_are_similar_for_related_text() {
        let a = hash_embed("parse the http request headers");
        let b = hash_embed("fn parseHttpRequest(headers: &str)");
        let c = hash_embed("render a triangle with opengl shaders");
        assert!(cosine(&a, &b) > cosine(&a, &c));
    }
}
