//! Enrollment: exchange a console-issued token for device credentials + config.

use crate::config::{LocalConfig, Paths};
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

pub async fn enroll(paths: &Paths, server: &str, token: &str, name: Option<String>) -> Result<()> {
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
        display_name: name
            .clone()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty()),
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(format!("remote-agent/{}", crate::AGENT_VERSION))
        .build()
        .context("building HTTP client")?;

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
        if let Ok(api) = serde_json::from_str::<ApiError>(&text) {
            bail!(
                "enrollment rejected ({}): {}",
                api.error.code,
                api.error.message
            );
        }
        bail!("enrollment failed with HTTP {status}: {}", text.trim());
    }

    let enrolled: EnrollResponse =
        serde_json::from_str(&text).context("parsing enrollment response")?;

    let mut cfg = LocalConfig {
        server_url: if enrolled.server_url.is_empty() {
            server.to_string()
        } else {
            enrolled.server_url.trim_end_matches('/').to_string()
        },
        device_id: enrolled.device_id.clone(),
        device_secret: enrolled.device_secret,
        cached: Some(enrolled.config),
    };
    if let Some(name) = name {
        if let Some(c) = cfg.cached.as_mut() {
            c.display_name = name;
        }
    }
    cfg.save(paths).context("saving agent configuration")?;

    println!("Enrolled successfully.");
    println!("  device id : {}", enrolled.device_id);
    println!("  server    : {}", cfg.server_url);
    println!("  name      : {}", cfg.effective().display_name);
    println!("  mode      : {:?}", cfg.effective().mode);
    println!("\nStart the agent with `remote-agent service install` (or `remote-agent run`).");
    Ok(())
}
