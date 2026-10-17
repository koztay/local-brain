//! Runtime adapter: one interface — `generate`, `embed`, `unload`, `health` — over the user's
//! local model runtime. Providers:
//!
//! * `ollama`   — Ollama's native HTTP API (default `http://127.0.0.1:11434`)
//! * `lmstudio` — LM Studio's OpenAI-compatible server (`http://127.0.0.1:1234/v1`)
//! * `mlx`      — `mlx_lm.server` / `mlx_vlm.server` on Apple Silicon (`http://127.0.0.1:8080/v1`)
//! * `llamacpp` — `llama-server` from llama.cpp (`http://127.0.0.1:8080/v1`)
//! * `openai`   — any other OpenAI-compatible local server
//! * `none`     — no model: deterministic features only (hash embeddings, rule-based compression)

use crate::config::Config;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ollama,
    OpenAi,
    None,
}

#[derive(Clone)]
pub struct Runtime {
    pub provider: String,
    pub kind: Kind,
    pub base_url: String,
    keep_alive: String,
    api_key: String,
    agent: ureq::Agent,
    quick: ureq::Agent,
}

pub fn default_url(provider: &str) -> &'static str {
    match provider {
        "ollama" => "http://127.0.0.1:11434",
        "lmstudio" => "http://127.0.0.1:1234/v1",
        "mlx" | "llamacpp" => "http://127.0.0.1:8080/v1",
        "openai" => "http://127.0.0.1:8000/v1",
        _ => "",
    }
}

fn kind_of(provider: &str) -> Kind {
    match provider {
        "ollama" => Kind::Ollama,
        "lmstudio" | "mlx" | "llamacpp" | "openai" => Kind::OpenAi,
        _ => Kind::None,
    }
}

impl Runtime {
    pub fn new(
        provider: &str,
        base_url: &str,
        keep_alive: &str,
        timeout_secs: u64,
        api_key: &str,
    ) -> Runtime {
        let base = if base_url.is_empty() {
            default_url(provider).to_string()
        } else {
            base_url.trim_end_matches('/').to_string()
        };
        // Model requests only ever go to the local runtime; proxies must not intercept them.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .timeout(Duration::from_secs(timeout_secs.max(5)))
            .try_proxy_from_env(false)
            .build();
        let quick = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(600))
            .timeout(Duration::from_secs(3))
            .try_proxy_from_env(false)
            .build();
        Runtime {
            provider: provider.to_string(),
            kind: kind_of(provider),
            base_url: base,
            keep_alive: keep_alive.to_string(),
            api_key: api_key.to_string(),
            agent,
            quick,
        }
    }

    pub fn from_config(c: &Config) -> Runtime {
        Runtime::new(
            &c.runtime.provider,
            &c.runtime.base_url,
            &c.runtime.keep_alive,
            c.runtime.timeout_secs,
            &c.runtime.api_key,
        )
    }

    /// A runtime with a short overall timeout (used by hooks, which must stay fast).
    pub fn with_timeout(&self, secs: u64) -> Runtime {
        let mut r = self.clone();
        r.agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(2))
            .timeout(Duration::from_secs(secs))
            .try_proxy_from_env(false)
            .build();
        r
    }

    pub fn enabled(&self) -> bool {
        self.kind != Kind::None
    }

    fn req(&self, agent: &ureq::Agent, method: &str, path: &str) -> ureq::Request {
        let r = agent.request(method, &format!("{}{}", self.base_url, path));
        if self.api_key.is_empty() {
            r
        } else {
            r.set("Authorization", &format!("Bearer {}", self.api_key))
        }
    }

    fn call(
        &self,
        agent: &ureq::Agent,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let r = self.req(agent, method, path);
        let resp = match body {
            Some(b) => r.send_json(b),
            None => r.call(),
        };
        match resp {
            Ok(r) => Ok(r.into_json::<Value>()?),
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                bail!(
                    "{} {} -> HTTP {}: {}",
                    method,
                    path,
                    code,
                    crate::util::clip(&text, 300)
                )
            }
            Err(e) => Err(anyhow!("{} {}{}: {}", method, self.base_url, path, e)),
        }
    }

    /// Quick reachability check; returns a short description of the server.
    pub fn health(&self) -> Result<String> {
        match self.kind {
            Kind::None => Ok("no runtime (deterministic mode)".into()),
            Kind::Ollama => {
                let v = self.call(&self.quick, "GET", "/api/version", None)?;
                Ok(format!("Ollama {}", v["version"].as_str().unwrap_or("?")))
            }
            Kind::OpenAi => {
                let v = self.call(&self.quick, "GET", "/models", None)?;
                let n = v["data"].as_array().map(|a| a.len()).unwrap_or(0);
                Ok(format!("{} server ({} models listed)", self.provider, n))
            }
        }
    }

    pub fn is_up(&self) -> bool {
        self.enabled() && self.health().is_ok()
    }

    pub fn list_models(&self) -> Result<Vec<String>> {
        match self.kind {
            Kind::None => Ok(vec![]),
            Kind::Ollama => {
                let v = self.call(&self.quick, "GET", "/api/tags", None)?;
                Ok(v["models"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default())
            }
            Kind::OpenAi => {
                let v = self.call(&self.quick, "GET", "/models", None)?;
                Ok(v["data"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default())
            }
        }
    }

    pub fn has_model(&self, model: &str) -> bool {
        let Ok(list) = self.list_models() else {
            return false;
        };
        list.iter().any(|m| {
            m == model
                || m.trim_end_matches(":latest") == model
                || model.trim_end_matches(":latest") == m.trim_end_matches(":latest")
        })
    }

    /// Generate text. `images` are raw image bytes (PNG/JPEG…).
    pub fn generate(
        &self,
        model: &str,
        prompt: &str,
        images: &[Vec<u8>],
        max_tokens: u32,
    ) -> Result<String> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let text = match self.kind {
            Kind::None => bail!("no model runtime configured"),
            Kind::Ollama => {
                let mut msg = json!({"role": "user", "content": prompt});
                if !images.is_empty() {
                    msg["images"] = Value::Array(
                        images
                            .iter()
                            .map(|i| Value::String(b64.encode(i)))
                            .collect(),
                    );
                }
                let body = json!({
                    "model": model,
                    "messages": [msg],
                    "stream": false,
                    "keep_alive": self.keep_alive,
                    "options": {"temperature": 0.1, "num_predict": max_tokens, "num_ctx": 8192},
                });
                let v = self.call(&self.agent, "POST", "/api/chat", Some(body))?;
                v["message"]["content"].as_str().unwrap_or("").to_string()
            }
            Kind::OpenAi => {
                let content = if images.is_empty() {
                    Value::String(prompt.to_string())
                } else {
                    let mut parts = vec![json!({"type": "text", "text": prompt})];
                    for i in images {
                        let mime = sniff_mime(i);
                        parts.push(json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{}", mime, b64.encode(i))}}));
                    }
                    Value::Array(parts)
                };
                let body = json!({
                    "model": model,
                    "messages": [{"role": "user", "content": content}],
                    "temperature": 0.1,
                    "max_tokens": max_tokens,
                    "stream": false,
                });
                let v = self.call(&self.agent, "POST", "/chat/completions", Some(body))?;
                v["choices"][0]["message"]["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            }
        };
        Ok(strip_thinking(&text))
    }

    pub fn embed(&self, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let parse = |a: &Value| -> Vec<f32> {
            a.as_array()
                .map(|x| {
                    x.iter()
                        .filter_map(|f| f.as_f64())
                        .map(|f| f as f32)
                        .collect()
                })
                .unwrap_or_default()
        };
        let out: Vec<Vec<f32>> = match self.kind {
            Kind::None => bail!("no model runtime configured"),
            Kind::Ollama => {
                let body = json!({"model": model, "input": texts, "keep_alive": self.keep_alive, "truncate": true});
                let v = self.call(&self.agent, "POST", "/api/embed", Some(body))?;
                v["embeddings"]
                    .as_array()
                    .context("no embeddings in response")?
                    .iter()
                    .map(parse)
                    .collect()
            }
            Kind::OpenAi => {
                let body = json!({"model": model, "input": texts});
                let v = self.call(&self.agent, "POST", "/embeddings", Some(body))?;
                v["data"]
                    .as_array()
                    .context("no embeddings in response")?
                    .iter()
                    .map(|d| parse(&d["embedding"]))
                    .collect()
            }
        };
        if out.len() != texts.len() || out.iter().any(|v| v.is_empty()) {
            bail!(
                "embedding response had {} vectors for {} inputs",
                out.len(),
                texts.len()
            );
        }
        Ok(out)
    }

    /// Release a model from memory (Ollama). Other providers manage memory themselves.
    pub fn unload(&self, model: &str) -> Result<()> {
        if self.kind == Kind::Ollama {
            let body = json!({"model": model, "messages": [], "keep_alive": 0});
            let _ = self.call(&self.quick, "POST", "/api/chat", Some(body));
        }
        Ok(())
    }

    /// Download a model (Ollama only). Streams progress to stderr.
    pub fn pull(&self, model: &str) -> Result<()> {
        if self.kind != Kind::Ollama {
            bail!(
                "automatic downloads are supported for Ollama only; load `{}` in {} manually",
                model,
                self.provider
            );
        }
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .try_proxy_from_env(false)
            .build();
        let resp = agent
            .post(&format!("{}/api/pull", self.base_url))
            .send_json(json!({"model": model, "stream": true}))
            .map_err(|e| anyhow!("pull {}: {}", model, e))?;
        use std::io::{BufRead, Write};
        let reader = std::io::BufReader::new(resp.into_reader());
        let mut last = String::new();
        for line in reader.lines() {
            let line = line?;
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(err) = v["error"].as_str() {
                bail!("pull {}: {}", model, err);
            }
            let status = v["status"].as_str().unwrap_or("").to_string();
            let total = v["total"].as_f64().unwrap_or(0.0);
            let done = v["completed"].as_f64().unwrap_or(0.0);
            if total > 0.0 {
                eprint!("\r  {}: {} {:5.1}%   ", model, status, done / total * 100.0);
            } else if status != last {
                eprint!("\r  {}: {}                    ", model, status);
            }
            let _ = std::io::stderr().flush();
            last = status;
        }
        eprintln!();
        Ok(())
    }
}

fn sniff_mime(b: &[u8]) -> &'static str {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if b.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else if b.starts_with(b"GIF8") {
        "image/gif"
    } else if b.len() > 12 && &b[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/png"
    }
}

/// Remove `<think>…</think>` blocks some reasoning models emit.
pub fn strip_thinking(s: &str) -> String {
    let mut out = s.to_string();
    while let Some(start) = out.find("<think>") {
        match out[start..].find("</think>") {
            Some(end) => out.replace_range(start..start + end + "</think>".len(), ""),
            None => {
                out.truncate(start);
                break;
            }
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_thinking() {
        assert_eq!(
            super::strip_thinking("<think>hmm</think>\nAnswer"),
            "Answer"
        );
        assert_eq!(super::strip_thinking("plain"), "plain");
    }
}
