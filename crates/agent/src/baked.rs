//! Reading the signed bakery trailer from the agent's own executable.
//!
//! A "baked" binary carries a [`protocol::bakery::BakedPayload`] appended as a trailer (see the
//! `protocol::bakery` module docs). At startup [`load`] reads the tail of the current executable,
//! verifies the ed25519 signature and, when valid, exposes the [`BakedConfig`] process-wide.
//!
//! Effects of a valid trailer:
//! * `server_url` becomes the default console URL for `enroll` / `run` / app mode;
//! * `enroll_token` lets the agent auto-enroll when it is not enrolled yet;
//! * `branding` themes the app window, banner, tray and approval dialog;
//! * the console public key is pinned into [`crate::config::LocalConfig`] at enrollment and a
//!   binary presenting a different key later is refused.

use protocol::bakery::{read_trailer, verify_payload, BakedConfig, Branding};
use std::io::{Read, Seek, SeekFrom};
use std::sync::OnceLock;

/// The verified baked configuration of this process, if any.
#[derive(Debug, Clone)]
pub struct Baked {
    pub config: BakedConfig,
    /// ed25519 public key (base64) the trailer was signed with; pinned at enrollment.
    pub public_key: String,
}

impl Baked {
    pub fn branding(&self) -> &Branding {
        &self.config.branding
    }
}

static BAKED: OnceLock<Option<Baked>> = OnceLock::new();

/// The verified trailer of the current executable (`None` when unbaked or invalid). Cached.
pub fn get() -> Option<&'static Baked> {
    BAKED
        .get_or_init(|| match load_from_current_exe() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("reading baked trailer: {e:#}");
                None
            }
        })
        .as_ref()
}

/// Product name for UI, falling back to a neutral default when unbranded.
pub fn product_name() -> String {
    get()
        .map(|b| b.config.branding.product_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Remote Support".to_string())
}

/// Only read the tail of the file: last 12 bytes give the payload length + magic, then the
/// payload itself. This avoids loading a multi-hundred-MB binary into memory.
fn load_from_current_exe() -> anyhow::Result<Option<Baked>> {
    let path = std::env::current_exe()?;
    let mut f = std::fs::File::open(&path)?;
    let len = f.metadata()?.len();
    if len < 12 {
        return Ok(None);
    }
    // Read the 12-byte footer (u32 payload length + 8-byte magic).
    f.seek(SeekFrom::End(-12))?;
    let mut footer = [0u8; 12];
    f.read_exact(&mut footer)?;
    if &footer[4..12] != protocol::bakery::TRAILER_MAGIC {
        return Ok(None);
    }
    let payload_len = u32::from_le_bytes(footer[0..4].try_into().unwrap()) as u64;
    if payload_len == 0
        || payload_len as usize > protocol::bakery::MAX_PAYLOAD_BYTES
        || payload_len + 12 > len
    {
        anyhow::bail!("corrupt trailer length");
    }
    // Read [payload][footer] and hand the whole tail to the shared parser.
    let tail_len = payload_len + 12;
    f.seek(SeekFrom::End(-(tail_len as i64)))?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail)?;
    let Some(payload) = read_trailer(&tail).map_err(|e| anyhow::anyhow!(e))? else {
        return Ok(None);
    };
    match verify_payload(&payload) {
        Ok(_) => Ok(Some(Baked {
            config: payload.config,
            public_key: payload.public_key,
        })),
        Err(e) => {
            tracing::error!("baked trailer signature invalid ({e}); ignoring branding/config");
            Ok(None)
        }
    }
}

/// Should the agent auto-enroll now? True when a token is baked in and we are not enrolled yet.
pub fn should_auto_enroll(local: Option<&crate::config::LocalConfig>) -> bool {
    let already = local.map(|c| c.is_enrolled()).unwrap_or(false);
    !already
        && get()
            .map(|b| b.config.enroll_token.is_some())
            .unwrap_or(false)
}

/// `remote-agent bake-info`: print the trailer (or say there is none).
pub fn print_info() -> anyhow::Result<()> {
    match get() {
        None => {
            println!("this binary has no valid bakery trailer (plain agent)");
        }
        Some(b) => {
            let c = &b.config;
            println!("baked trailer:");
            println!("  version      : {}", c.version);
            println!("  server url   : {}", c.server_url);
            println!(
                "  enroll token : {}",
                c.enroll_token
                    .as_deref()
                    .map(mask_token)
                    .unwrap_or_else(|| "(none)".into())
            );
            println!("  quick support: {}", c.quick_support);
            println!("  issued at    : {}", c.issued_at);
            println!("  public key   : {}", b.public_key);
            let br = &c.branding;
            println!("  product      : {}", br.product_name);
            println!("  organization : {}", br.organization);
            println!("  accent       : {}", br.accent);
            println!("  support text : {}", br.support_text);
            println!(
                "  logo         : {}",
                if br.logo_png_base64.is_some() {
                    "yes"
                } else {
                    "no"
                }
            );
        }
    }
    Ok(())
}

fn mask_token(t: &str) -> String {
    if t.len() <= 8 {
        "*".repeat(t.len())
    } else {
        format!("{}… ({} chars)", &t[..8], t.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::bakery::{append_trailer, sign_payload, BakedConfig, Branding};

    fn write_baked(dir: &std::path::Path, base: &[u8]) -> std::path::PathBuf {
        use ed25519_dalek::SigningKey;
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let cfg = BakedConfig {
            version: protocol::bakery::BAKED_VERSION,
            server_url: "https://c.example".into(),
            enroll_token: Some("abcdef123456".into()),
            quick_support: false,
            branding: Branding {
                product_name: "Acme".into(),
                accent: "#112233".into(),
                logo_png_base64: None,
                support_text: "help".into(),
                organization: "Acme Inc".into(),
            },
            issued_at: 42,
        };
        let baked = append_trailer(base, &sign_payload(cfg, &key));
        let p = dir.join("baked.bin");
        std::fs::write(&p, baked).unwrap();
        p
    }

    /// Reading the tail of a synthetic baked file yields the config; a plain file yields None.
    #[test]
    fn tail_read_of_synthetic_binary() {
        let dir = tempfile::tempdir().unwrap();
        // ~300 KB of "binary" so the tail-only read path is exercised.
        let base = vec![0xABu8; 300 * 1024];
        let p = write_baked(dir.path(), &base);

        let mut f = std::fs::File::open(&p).unwrap();
        f.seek(SeekFrom::End(-12)).unwrap();
        let mut footer = [0u8; 12];
        f.read_exact(&mut footer).unwrap();
        assert_eq!(&footer[4..12], protocol::bakery::TRAILER_MAGIC);
        let payload_len = u32::from_le_bytes(footer[0..4].try_into().unwrap()) as u64;
        let tail_len = payload_len + 12;
        f.seek(SeekFrom::End(-(tail_len as i64))).unwrap();
        let mut tail = vec![0u8; tail_len as usize];
        f.read_exact(&mut tail).unwrap();
        let payload = read_trailer(&tail).unwrap().unwrap();
        assert_eq!(payload.config.branding.product_name, "Acme");
        verify_payload(&payload).unwrap();

        // A plain file has no trailer.
        let plain = dir.path().join("plain.bin");
        std::fs::write(&plain, &base).unwrap();
        let mut pf = std::fs::File::open(&plain).unwrap();
        pf.seek(SeekFrom::End(-12)).unwrap();
        let mut footer = [0u8; 12];
        pf.read_exact(&mut footer).unwrap();
        assert_ne!(&footer[4..12], protocol::bakery::TRAILER_MAGIC);
    }

    #[test]
    fn auto_enroll_decision() {
        // No baked config anywhere in the test binary → never auto-enroll.
        assert!(!should_auto_enroll(None));
        let enrolled = crate::config::LocalConfig {
            server_url: "https://c".into(),
            device_id: "d".into(),
            device_secret: "s".into(),
            cached: None,
            console_public_key: None,
        };
        assert!(!should_auto_enroll(Some(&enrolled)));
    }

    #[test]
    fn token_is_masked() {
        assert_eq!(mask_token("short"), "*****");
        assert!(mask_token("abcdefghijkl").starts_with("abcdefgh… "));
    }
}
