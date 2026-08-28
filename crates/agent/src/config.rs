//! On-disk state of the agent.
//!
//! Layout inside the config directory (`Paths::dir`):
//! * `agent.toml` — [`LocalConfig`] (server URL, device id/secret, cached console config)
//! * `logs/`      — daily rolling log files
//!
//! The file is written with owner-only permissions because it holds the device secret.

use anyhow::{Context, Result};
use protocol::config::AgentConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "agent.toml";

#[derive(Debug, Clone)]
pub struct Paths {
    pub dir: PathBuf,
}

impl Paths {
    /// Resolve the configuration directory: explicit override, else a system-wide location
    /// (the agent runs as a service, so per-user dirs are not appropriate).
    pub fn resolve(override_dir: Option<PathBuf>) -> Result<Self> {
        let dir = match override_dir {
            Some(d) => d,
            None => default_dir(),
        };
        Ok(Self { dir })
    }

    pub fn config_file(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    pub fn log_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }
}

fn default_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/RemoteAgent")
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        base.join("RemoteAgent")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        PathBuf::from("/etc/remote-agent")
    }
}

/// Persistent local configuration. `cached` mirrors the last config received from the console.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalConfig {
    pub server_url: String,
    pub device_id: String,
    pub device_secret: String,
    #[serde(default)]
    pub cached: Option<AgentConfig>,
    /// ed25519 public key (base64) of the console this agent was enrolled with, from the bakery
    /// trailer. Pinned so a later re-baked binary presenting a different key is refused.
    #[serde(default)]
    pub console_public_key: Option<String>,
}

impl LocalConfig {
    pub fn load(paths: &Paths) -> Result<Option<Self>> {
        let file = paths.config_file();
        if !file.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let cfg: LocalConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        Ok(Some(cfg))
    }

    pub fn load_required(paths: &Paths) -> Result<Self> {
        Self::load(paths)?.context(
            "agent is not enrolled: run `remote-agent enroll --server <url> --token <token>`",
        )
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        std::fs::create_dir_all(&paths.dir)
            .with_context(|| format!("creating {}", paths.dir.display()))?;
        let text = toml::to_string_pretty(self)?;
        let file = paths.config_file();
        write_private(&file, text.as_bytes())?;
        Ok(())
    }

    pub fn is_enrolled(&self) -> bool {
        !self.server_url.is_empty() && !self.device_id.is_empty() && !self.device_secret.is_empty()
    }

    /// Effective agent config: the cached one from the console, else defaults.
    pub fn effective(&self) -> AgentConfig {
        let mut cfg = self.cached.clone().unwrap_or_default();
        if cfg.display_name.is_empty() {
            cfg.display_name = hostname::get()
                .map(|h| h.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".into());
        }
        cfg
    }
}

/// Write a file readable only by its owner (best effort on Windows via the ProgramData ACL).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(bytes)?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, bytes)?;
        }
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn print_status(paths: &Paths) -> Result<()> {
    println!("config dir : {}", paths.dir.display());
    match LocalConfig::load(paths)? {
        None => println!("enrolled   : no"),
        Some(cfg) => {
            println!(
                "enrolled   : {}",
                if cfg.is_enrolled() {
                    "yes"
                } else {
                    "incomplete"
                }
            );
            println!("server     : {}", cfg.server_url);
            println!("device id  : {}", cfg.device_id);
            let eff = cfg.effective();
            println!("name       : {}", eff.display_name);
            println!("mode       : {:?}", eff.mode);
        }
    }
    println!("version    : {}", crate::AGENT_VERSION);
    Ok(())
}

pub fn reset(paths: &Paths) -> Result<()> {
    let file = paths.config_file();
    if file.exists() {
        std::fs::remove_file(&file)?;
        println!("removed {}", file.display());
    } else {
        println!("nothing to remove");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            dir: tmp.path().join("cfg"),
        };
        let cfg = LocalConfig {
            server_url: "https://console.example".into(),
            device_id: "dev".into(),
            device_secret: "sec".into(),
            cached: Some(AgentConfig::default()),
            console_public_key: None,
        };
        cfg.save(&paths).unwrap();
        let back = LocalConfig::load(&paths).unwrap().unwrap();
        assert_eq!(back.server_url, cfg.server_url);
        assert_eq!(back.device_secret, "sec");
        assert!(back.is_enrolled());
        assert_eq!(back.effective().max_fps, 60);
    }
}
