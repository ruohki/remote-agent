//! Enrollment: exchange a console-issued token for device credentials + config.

use crate::config::{LocalConfig, Paths};
use crate::secrets::SecretBackend;
use anyhow::{bail, Context, Result};
use protocol::config::{EnrollRequest, EnrollResponse};
use serde::Deserialize;
use std::time::Duration;

/// Error body returned by the console (`{ "error": { "code", "message" } }`).
#[derive(Debug, Deserialize)]
struct ApiError {
    error: ApiErrorBody,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    code: String,
    message: String,
}

/// The console answered `/api/enroll` with an error. `message` is the console's own wording
/// (e.g. "token expired"), suitable for showing verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "enrollment rejected ({}): {}", self.code, self.message)
    }
}

impl std::error::Error for Rejected {}

/// A completed enrollment: the saved configuration and where the secret went.
#[derive(Debug, Clone)]
pub struct Enrolled {
    pub config: LocalConfig,
    pub backend: SecretBackend,
}

impl Enrolled {
    /// CLI summary printed by `remote-agent enroll`.
    pub fn print_summary(&self) {
        let cfg = &self.config;
        println!("Enrolled successfully.");
        println!("  device id : {}", cfg.device_id);
        println!("  server    : {}", cfg.server_url);
        println!("  name      : {}", cfg.effective().display_name);
        println!("  mode      : {:?}", cfg.effective().mode);
        println!("  secret    : stored in {}", self.backend.as_str());
        if cfg.console_tls_spki_sha256.is_some() {
            println!("  tls pin   : active");
        }
        println!("\nStart the agent with `remote-agent service install` (or `remote-agent run`).");
    }
}

/// Auto-enroll from the bakery trailer when the agent is not enrolled yet and a token is baked
/// in. Returns `Ok(true)` when an enrollment happened. No-op (returns `Ok(false)`) for plain
/// binaries or an already-enrolled agent.
pub async fn auto_enroll_if_baked(paths: &Paths) -> Result<bool> {
    let already = LocalConfig::load(paths)?
        .map(|c| c.is_enrolled())
        .unwrap_or(false);
    if already {
        return Ok(false);
    }
    let Some(baked) = crate::baked::get() else {
        return Ok(false);
    };
    let Some(token) = baked.config.enroll_token.clone() else {
        return Ok(false);
    };
    crate::transport::check_console_url(&baked.config.server_url).context("baked console URL")?;
    tracing::info!(server = %baked.config.server_url, "auto-enrolling from baked configuration");
    enroll(paths, &baked.config.server_url, &token, None).await?;
    Ok(true)
}

/// `POST /api/enroll` only (nothing is saved). A console-side rejection is returned as a
/// downcastable [`Rejected`].
pub async fn request(server: &str, token: &str, name: Option<&str>) -> Result<EnrollResponse> {
    let server = server.trim_end_matches('/');
    let url = format!("{server}/api/enroll");
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".into());

    let body = EnrollRequest {
        token: token.to_string(),
        hostname: hostname.clone(),
        os: protocol::common::Os::current(),
        arch: protocol::common::Arch::current(),
        agent_version: crate::AGENT_VERSION.to_string(),
        display_name: name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
    };

    // A baked TLS pin applies from the very first request.
    if let Some(b) = crate::baked::get() {
        crate::transport::set_console_pin(b.config.console_tls_spki_sha256.as_deref(), server)
            .context("baked console TLS pin")?;
    }
    let client = crate::transport::http_client(server, Duration::from_secs(20))?;

    tracing::info!(%url, %hostname, "enrolling");
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("connecting to {url}"))?;

    let status = resp.status();
    let text = resp.text().await.context("reading enrollment response")?;
    if !status.is_success() {
        return Err(rejection(status.as_u16(), &text).into());
    }
    serde_json::from_str(&text).context("parsing enrollment response")
}

/// Turn a non-2xx `/api/enroll` answer into a [`Rejected`], keeping the console's message when
/// the body is the API error shape and falling back to a short HTTP status otherwise.
fn rejection(status: u16, body: &str) -> Rejected {
    if let Ok(api) = serde_json::from_str::<ApiError>(body) {
        return Rejected {
            status,
            code: api.error.code,
            message: api.error.message,
        };
    }
    let text = body.trim();
    let message = if text.is_empty() || text.len() > 160 || text.contains('<') {
        format!("HTTP {status}")
    } else {
        text.to_string()
    };
    Rejected {
        status,
        code: format!("http_{status}"),
        message,
    }
}

/// Enroll and persist the result. Local restrictions from a previous enrollment are kept.
pub async fn enroll(
    paths: &Paths,
    server: &str,
    token: &str,
    name: Option<String>,
) -> Result<Enrolled> {
    let server = server.trim_end_matches('/');
    if token.trim().is_empty() {
        bail!("enrollment token is empty");
    }
    let enrolled = request(server, token.trim(), name.as_deref()).await?;
    let previous = LocalConfig::load(paths).ok().flatten();

    let mut cfg = LocalConfig {
        server_url: if enrolled.server_url.is_empty() {
            server.to_string()
        } else {
            enrolled.server_url.trim_end_matches('/').to_string()
        },
        device_id: enrolled.device_id.clone(),
        device_secret: String::new(),
        cached: Some(enrolled.config),
        overrides: previous.map(|p| p.overrides).unwrap_or_default(),
        // Pin the console key from the bakery trailer (if this is a baked binary).
        console_public_key: crate::baked::get().map(|b| b.public_key.clone()),
        console_tls_spki_sha256: crate::baked::get()
            .and_then(|b| b.config.console_tls_spki_sha256.clone()),
        secret_backend: None,
        device_secret_dpapi: None,
    };
    let backend = crate::secrets::store(paths, &mut cfg, &enrolled.device_secret);

    if let Some(name) = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
        if let Some(c) = cfg.cached.as_mut() {
            c.display_name = name;
        }
    }
    cfg.save(paths).context("saving agent configuration")?;
    Ok(Enrolled {
        config: cfg,
        backend,
    })
}

/// One-line, user-facing description of an enrollment failure: the console's own message for a
/// rejection, the root cause for a connection problem.
pub fn error_message(err: &anyhow::Error) -> String {
    if let Some(r) = err.downcast_ref::<Rejected>() {
        return r.message.clone();
    }
    let root = err.root_cause().to_string();
    let root = root.trim().trim_end_matches('.');
    if err.chain().any(|c| c.is::<reqwest::Error>()) {
        return format!("Cannot reach the console: {root}");
    }
    root.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejection_keeps_console_message() {
        let r = rejection(
            401,
            r#"{"error":{"code":"invalid_token","message":"token expired"}}"#,
        );
        assert_eq!(r.code, "invalid_token");
        assert_eq!(r.message, "token expired");
        assert_eq!(r.status, 401);
    }

    #[test]
    fn rejection_falls_back_to_status_for_html_or_long_bodies() {
        assert_eq!(
            rejection(502, "<html>bad gateway</html>").message,
            "HTTP 502"
        );
        assert_eq!(rejection(500, "").message, "HTTP 500");
        assert_eq!(rejection(429, &"x".repeat(400)).message, "HTTP 429");
        assert_eq!(rejection(404, "not found").message, "not found");
    }

    #[test]
    fn error_message_is_short() {
        let rejected: anyhow::Error = Rejected {
            status: 410,
            code: "token_exhausted".into(),
            message: "token already used".into(),
        }
        .into();
        assert_eq!(error_message(&rejected), "token already used");
        let wrapped = rejected.context("enrolling");
        assert_eq!(error_message(&wrapped), "token already used");

        let plain = anyhow::anyhow!("keychain unavailable.").context("saving");
        assert_eq!(error_message(&plain), "keychain unavailable");
    }
}
