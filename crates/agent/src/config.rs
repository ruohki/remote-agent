//! On-disk state of the agent.
//!
//! Layout inside the config directory (`Paths::dir`):
//! * `agent.toml` — [`LocalConfig`] (server URL, device id/secret, cached console config)
//! * `logs/`      — daily rolling log files
//!
//! The file is written with owner-only permissions because it holds the device secret.

use anyhow::{Context, Result};
use protocol::config::{AgentConfig, LocalOverrides};
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
            None => {
                // Services run as root/SYSTEM and use the machine-wide directory; an
                // interactive launch by a normal user (double-clicked app bundle) must
                // never fail on permissions, so fall back to the per-user directory.
                let system = default_dir();
                if dir_is_writable(&system) {
                    system
                } else {
                    let user = user_dir().unwrap_or_else(|| system.clone());
                    tracing::debug!(
                        "{} is not writable; using {}",
                        system.display(),
                        user.display()
                    );
                    user
                }
            }
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

/// Whether `dir` exists (or can be created) and files can be written into it.
fn dir_is_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".write-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Per-user state directory used when the machine-wide one is not writable.
fn user_dir() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    #[cfg(target_os = "macos")]
    {
        Some(
            base.home_dir()
                .join("Library/Application Support/RemoteAgent"),
        )
    }
    #[cfg(target_os = "windows")]
    {
        Some(base.data_local_dir().join("RemoteAgent"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Some(base.config_dir().join("remote-agent"))
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
    /// Restrictions applied locally by the person at the device (restrict-only over `cached`).
    #[serde(default)]
    pub overrides: LocalOverrides,
    /// ed25519 public key (base64) of the console this agent was enrolled with, from the bakery
    /// trailer. Pinned so a later re-baked binary presenting a different key is refused.
    #[serde(default)]
    pub console_public_key: Option<String>,
    /// SHA-256 (base64) of the console TLS certificate's SubjectPublicKeyInfo, from the bakery
    /// trailer; every TLS connection to the console must present this key.
    #[serde(default)]
    pub console_tls_spki_sha256: Option<String>,
    /// Where the device secret lives: `keychain`, `dpapi` or `file` (see `secrets`).
    #[serde(default)]
    pub secret_backend: Option<String>,
    /// DPAPI-protected device secret (base64), Windows only.
    #[serde(default)]
    pub device_secret_dpapi: Option<String>,
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
        let has_secret = !self.device_secret.is_empty()
            || self.device_secret_dpapi.is_some()
            || matches!(self.secret_backend.as_deref(), Some("keychain"));
        !self.server_url.is_empty() && !self.device_id.is_empty() && has_secret
    }

    /// The console's config (cached), with a hostname display-name default. This is the policy
    /// the console set, *before* any local restrictions.
    pub fn console_config(&self) -> AgentConfig {
        let mut cfg = self.cached.clone().unwrap_or_default();
        if cfg.display_name.is_empty() {
            cfg.display_name = hostname::get()
                .map(|h| h.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".into());
        }
        cfg
    }

    /// Backwards-compatible alias for [`console_config`].
    pub fn effective(&self) -> AgentConfig {
        self.console_config()
    }

    /// The config sessions actually run with: the console config tightened by the local
    /// restrictions the person at the device set.
    pub fn effective_with_overrides(&self) -> AgentConfig {
        self.overrides.apply(self.console_config())
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
            println!(
                "secret     : {}",
                cfg.secret_backend
                    .as_deref()
                    .unwrap_or("file (not yet migrated)")
            );
            println!(
                "tls pin    : {}",
                cfg.console_tls_spki_sha256.as_deref().unwrap_or("none")
            );
        }
    }
    println!("version    : {}", crate::AGENT_VERSION);
    Ok(())
}

pub fn reset(paths: &Paths) -> Result<()> {
    crate::secrets::forget(paths);
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
            overrides: LocalOverrides::default(),
            console_public_key: None,
            console_tls_spki_sha256: None,
            secret_backend: None,
            device_secret_dpapi: None,
        };
        cfg.save(&paths).unwrap();
        let back = LocalConfig::load(&paths).unwrap().unwrap();
        assert_eq!(back.server_url, cfg.server_url);
        assert_eq!(back.device_secret, "sec");
        assert!(back.is_enrolled());
        assert_eq!(back.effective().max_fps, 60);
    }

    #[test]
    fn overrides_persist_and_tighten_only() {
        use protocol::common::DeviceMode;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            dir: tmp.path().join("cfg"),
        };
        // Console allows everything, unattended.
        let console = AgentConfig {
            mode: DeviceMode::Unattended,
            allow_input: true,
            allow_audio: true,
            ..AgentConfig::default()
        };
        let cfg = LocalConfig {
            server_url: "https://c".into(),
            device_id: "d".into(),
            device_secret: "s".into(),
            cached: Some(console),
            overrides: LocalOverrides {
                mode: Some(DeviceMode::HelpMe),
                allow_input: Some(false),
                ..Default::default()
            },
            console_public_key: None,
            console_tls_spki_sha256: None,
            secret_backend: None,
            device_secret_dpapi: None,
        };
        cfg.save(&paths).unwrap();
        let back = LocalConfig::load(&paths).unwrap().unwrap();
        // Console config is unchanged; the effective one is tightened.
        assert_eq!(back.console_config().mode, DeviceMode::Unattended);
        assert!(back.console_config().allow_input);
        let eff = back.effective_with_overrides();
        assert_eq!(eff.mode, DeviceMode::HelpMe);
        assert!(!eff.allow_input);
        // Overrides cannot loosen: an "allow" override on a console-forbidden capability stays off.
        let mut c2 = back.clone();
        c2.cached.as_mut().unwrap().allow_audio = false;
        c2.overrides.allow_audio = None; // "follow console"
        assert!(!c2.effective_with_overrides().allow_audio);
    }
}
