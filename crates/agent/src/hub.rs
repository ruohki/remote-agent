//! Persistent connection to the management console (`/ws/agent`).
//!
//! TODO(builder-core): implement
//! * connect with exponential backoff (1s → 60s, jitter), TLS via rustls;
//! * send `Hello` first, wait for `HelloAck`, persist `config` into `LocalConfig.cached`;
//! * heartbeat every `config.heartbeat_interval_s`, reply to `Ping` with `Pong`;
//! * on `ConfigUpdate` re-apply mode/limits (running sessions keep their settings);
//! * on `SessionRequest` → `session::SessionManager::start` (help-me prompt first when
//!   `mode == HelpMe`); relay `IceCandidate`s both ways; `SessionEnd` → stop;
//! * report `SessionState` transitions and display changes (`Heartbeat.displays`).

use crate::config::Paths;
use anyhow::Result;

pub async fn run_agent(paths: Paths) -> Result<()> {
    let cfg = crate::config::LocalConfig::load_required(&paths)?;
    tracing::info!(
        server = %cfg.server_url,
        device = %cfg.device_id,
        version = crate::AGENT_VERSION,
        "starting agent"
    );
    anyhow::bail!("hub client not implemented yet")
}
