//! Device-secret storage: OS keychain (macOS), DPAPI (Windows), else the 0600 config file.
//!
//! `agent.toml` keeps everything except the secret; the secret lives in the login keychain
//! (`service = "RemoteAgent"`, `account = <config dir>`) or as a DPAPI-protected blob inside
//! the file. When the keychain is unavailable (headless service context without a login
//! keychain, locked keychain, permission prompt denied) the plaintext file is used and
//! `status` reports it.

use crate::config::{LocalConfig, Paths};
use anyhow::{Context, Result};

pub const KEYCHAIN_SERVICE: &str = "RemoteAgent";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretBackend {
    /// macOS keychain item.
    Keychain,
    /// Windows DPAPI blob stored in `agent.toml`.
    Dpapi,
    /// Plaintext in `agent.toml` (owner-only permissions).
    File,
}

impl SecretBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretBackend::Keychain => "keychain",
            SecretBackend::Dpapi => "dpapi",
            SecretBackend::File => "file",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "keychain" => Some(SecretBackend::Keychain),
            "dpapi" => Some(SecretBackend::Dpapi),
            "file" => Some(SecretBackend::File),
            _ => None,
        }
    }
}

/// Account name under which the secret is filed (one per config directory).
fn account(paths: &Paths) -> String {
    paths.dir.to_string_lossy().into_owned()
}

/// Store `secret` for the config in the most protected backend available and update `cfg`
/// (`device_secret` / `device_secret_dpapi` / `secret_backend`). The caller saves `cfg`.
pub fn store(paths: &Paths, cfg: &mut LocalConfig, secret: &str) -> SecretBackend {
    #[cfg(target_os = "macos")]
    {
        match security_framework::passwords::set_generic_password(
            KEYCHAIN_SERVICE,
            &account(paths),
            secret.as_bytes(),
        ) {
            Ok(()) => {
                cfg.device_secret.clear();
                cfg.device_secret_dpapi = None;
                cfg.secret_backend = Some(SecretBackend::Keychain.as_str().into());
                return SecretBackend::Keychain;
            }
            Err(e) => {
                tracing::warn!(
                    "keychain unavailable ({e}); keeping the device secret in the config file"
                );
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        match dpapi::protect(secret.as_bytes()) {
            Ok(blob) => {
                cfg.device_secret.clear();
                cfg.device_secret_dpapi = Some(base64_encode(&blob));
                cfg.secret_backend = Some(SecretBackend::Dpapi.as_str().into());
                return SecretBackend::Dpapi;
            }
            Err(e) => {
                tracing::warn!(
                    "DPAPI unavailable ({e}); keeping the device secret in the config file"
                );
            }
        }
    }
    let _ = paths;
    cfg.device_secret = secret.to_string();
    cfg.device_secret_dpapi = None;
    cfg.secret_backend = Some(SecretBackend::File.as_str().into());
    SecretBackend::File
}

/// Resolve the device secret for `cfg`.
pub fn load(paths: &Paths, cfg: &LocalConfig) -> Result<(String, SecretBackend)> {
    match cfg.secret_backend.as_deref().and_then(SecretBackend::parse) {
        Some(SecretBackend::Keychain) => {
            #[cfg(target_os = "macos")]
            {
                let bytes = security_framework::passwords::get_generic_password(
                    KEYCHAIN_SERVICE,
                    &account(paths),
                )
                .context("reading the device secret from the keychain")?;
                Ok((
                    String::from_utf8(bytes).context("keychain secret is not UTF-8")?,
                    SecretBackend::Keychain,
                ))
            }
            #[cfg(not(target_os = "macos"))]
            {
                anyhow::bail!("config references the keychain but this platform has none")
            }
        }
        Some(SecretBackend::Dpapi) => {
            #[cfg(target_os = "windows")]
            {
                let blob = cfg
                    .device_secret_dpapi
                    .as_deref()
                    .context("config references DPAPI but holds no blob")?;
                let bytes = dpapi::unprotect(&base64_decode(blob)?)?;
                Ok((
                    String::from_utf8(bytes).context("DPAPI secret is not UTF-8")?,
                    SecretBackend::Dpapi,
                ))
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = paths;
                anyhow::bail!("config references DPAPI but this platform has none")
            }
        }
        _ => {
            if cfg.device_secret.is_empty() {
                anyhow::bail!("no device secret stored (not enrolled)");
            }
            Ok((cfg.device_secret.clone(), SecretBackend::File))
        }
    }
}

/// Move a plaintext secret into the platform store on first run. Returns the backend now in use.
pub fn migrate_if_needed(paths: &Paths, cfg: &mut LocalConfig) -> Result<SecretBackend> {
    if cfg.secret_backend.is_some() || cfg.device_secret.is_empty() {
        return load(paths, cfg).map(|(_, b)| b);
    }
    let secret = cfg.device_secret.clone();
    let backend = store(paths, cfg, &secret);
    cfg.save(paths)
        .context("saving config after secret migration")?;
    tracing::info!(
        backend = backend.as_str(),
        "device secret storage initialised"
    );
    Ok(backend)
}

/// Remove the secret from the platform store (used by `reset`).
pub fn forget(paths: &Paths) {
    #[cfg(target_os = "macos")]
    {
        let _ = security_framework::passwords::delete_generic_password(
            KEYCHAIN_SERVICE,
            &account(paths),
        );
    }
    let _ = paths;
}

#[cfg(target_os = "windows")]
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(target_os = "windows")]
fn base64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .context("DPAPI blob is not valid base64")
}

#[cfg(target_os = "windows")]
use crate::platform::dpapi;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_names_roundtrip() {
        for b in [
            SecretBackend::Keychain,
            SecretBackend::Dpapi,
            SecretBackend::File,
        ] {
            assert_eq!(SecretBackend::parse(b.as_str()), Some(b));
        }
        assert_eq!(SecretBackend::parse("nope"), None);
    }

    #[test]
    fn file_backend_when_config_has_plain_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            dir: tmp.path().to_path_buf(),
        };
        let cfg = LocalConfig {
            server_url: "https://c".into(),
            device_id: "d".into(),
            device_secret: "plain".into(),
            ..Default::default()
        };
        let (s, b) = load(&paths, &cfg).unwrap();
        assert_eq!(s, "plain");
        assert_eq!(b, SecretBackend::File);
        let empty = LocalConfig::default();
        assert!(load(&paths, &empty).is_err());
    }
}
