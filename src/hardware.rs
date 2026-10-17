//! Hardware profiling and tier selection (run during `init`, shown by `status`/`doctor`).

use crate::catalog::Catalog;
use crate::runtime::{self, Runtime};
use serde::Serialize;

#[derive(Serialize, Debug, Clone)]
pub struct Profile {
    pub os: String,
    pub arch: String,
    pub cpu: String,
    pub cores: usize,
    pub ram_gb: f64,
    pub apple_silicon: bool,
    pub nvidia_gpu: Option<String>,
    pub vram_gb: f64,
    pub disk_free_gb: f64,
    pub runtimes: Vec<DetectedRuntime>,
}

#[derive(Serialize, Debug, Clone)]
pub struct DetectedRuntime {
    pub provider: String,
    pub base_url: String,
    pub running: bool,
    pub installed: bool,
    pub detail: String,
}

fn on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| {
        let p = d.join(bin);
        p.is_file() || (cfg!(windows) && d.join(format!("{}.exe", bin)).is_file())
    })
}

fn nvidia() -> Option<(String, f64)> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next()?;
    let mut parts = line.split(',');
    let name = parts.next()?.trim().to_string();
    let mib: f64 = parts.next()?.trim().parse().ok()?;
    Some((name, mib / 1024.0))
}

pub fn detect_runtimes() -> Vec<DetectedRuntime> {
    let mut v = Vec::new();
    let probe = |provider: &str, installed: bool| -> DetectedRuntime {
        let url = runtime::default_url(provider).to_string();
        let rt = Runtime::new(provider, &url, "10m", 5, "");
        let (running, detail) = match rt.health() {
            Ok(d) => (true, d),
            Err(_) => (
                false,
                if installed {
                    "installed, not running".into()
                } else {
                    "not found".into()
                },
            ),
        };
        DetectedRuntime {
            provider: provider.into(),
            base_url: url,
            running,
            installed: installed || running,
            detail,
        }
    };
    v.push(probe("ollama", on_path("ollama")));
    v.push(probe("lmstudio", on_path("lms")));
    let mlx_installed =
        cfg!(target_os = "macos") && (on_path("mlx_lm.server") || on_path("mlx_vlm.server"));
    // Port 8080 is shared by mlx_lm.server and llama-server; label by what is installed.
    if mlx_installed {
        v.push(probe("mlx", true));
    } else {
        v.push(probe("llamacpp", on_path("llama-server")));
    }
    v
}

pub fn profile() -> Profile {
    use sysinfo::{Disks, System};
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu_all();
    let ram_gb = sys.total_memory() as f64 / 1024f64.powi(3);
    let cpu = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .unwrap_or_default();
    let cores = sys.cpus().len();
    let home = crate::paths::home();
    let disks = Disks::new_with_refreshed_list();
    let disk_free = disks
        .list()
        .iter()
        .filter(|d| home.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space() as f64 / 1e9)
        .unwrap_or(0.0);
    let gpu = nvidia();
    Profile {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        cpu,
        cores,
        ram_gb,
        apple_silicon: cfg!(target_os = "macos") && cfg!(target_arch = "aarch64"),
        nvidia_gpu: gpu.as_ref().map(|g| g.0.clone()),
        vram_gb: gpu.map(|g| g.1).unwrap_or(0.0),
        disk_free_gb: disk_free,
        runtimes: detect_runtimes(),
    }
}

impl Profile {
    pub fn accelerated(&self) -> bool {
        self.apple_silicon || self.vram_gb >= 6.0
    }

    /// Pick the largest tier whose peak model memory fits in ~50% of RAM (or the GPU).
    pub fn choose_tier(&self, catalog: &Catalog) -> String {
        let budget = if self.vram_gb > 0.0 && !self.apple_silicon {
            self.vram_gb * 0.9
        } else {
            self.ram_gb * 0.5
        };
        let wanted = if !self.accelerated() || self.ram_gb <= 20.0 {
            "S"
        } else if self.ram_gb >= 46.0 || self.vram_gb >= 24.0 {
            "L"
        } else {
            "M"
        };
        for t in ["L", "M", "S"].iter().skip_while(|t| **t != wanted) {
            if let Some(tier) = catalog.tiers.get(*t) {
                if tier.peak_gb() <= budget || *t == "S" {
                    return t.to_string();
                }
            }
        }
        "S".into()
    }

    /// Preferred provider: a running Ollama/LM Studio first, then MLX/llama.cpp servers.
    pub fn preferred_runtime(&self) -> Option<&DetectedRuntime> {
        let order = ["ollama", "lmstudio", "mlx", "llamacpp"];
        for p in order {
            if let Some(r) = self.runtimes.iter().find(|r| r.provider == p && r.running) {
                return Some(r);
            }
        }
        for p in order {
            if let Some(r) = self
                .runtimes
                .iter()
                .find(|r| r.provider == p && r.installed)
            {
                return Some(r);
            }
        }
        None
    }

    pub fn describe(&self) -> String {
        let accel = if self.apple_silicon {
            "Apple Silicon (unified memory)".to_string()
        } else if let Some(g) = &self.nvidia_gpu {
            format!("NVIDIA {} ({:.0} GB VRAM)", g, self.vram_gb)
        } else {
            "CPU only".to_string()
        };
        format!(
            "{} {} · {} ({} cores) · {:.0} GB RAM · {} · {:.0} GB free disk",
            self.os, self.arch, self.cpu, self.cores, self.ram_gb, accel, self.disk_free_gb
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(ram: f64, apple: bool, vram: f64) -> Profile {
        Profile {
            os: "x".into(),
            arch: "y".into(),
            cpu: "z".into(),
            cores: 8,
            ram_gb: ram,
            apple_silicon: apple,
            nvidia_gpu: if vram > 0.0 { Some("gpu".into()) } else { None },
            vram_gb: vram,
            disk_free_gb: 100.0,
            runtimes: vec![],
        }
    }

    #[test]
    fn tiers() {
        let c: Catalog = toml::from_str(crate::catalog::DEFAULT_CATALOG).unwrap();
        assert_eq!(p(16.0, true, 0.0).choose_tier(&c), "S");
        assert_eq!(p(32.0, true, 0.0).choose_tier(&c), "M");
        assert_eq!(p(64.0, true, 0.0).choose_tier(&c), "L");
        assert_eq!(p(64.0, false, 0.0).choose_tier(&c), "S");
        assert_eq!(p(32.0, false, 24.0).choose_tier(&c), "L");
    }
}
