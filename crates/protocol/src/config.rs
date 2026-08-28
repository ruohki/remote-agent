use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::{DeviceMode, IceServer, VideoCodec};

/// Runtime configuration of an agent. Owned by the console, pushed to the agent in
/// `hello_ack` and `config_update`, and cached on disk by the agent so it can start
/// while the console is unreachable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AgentConfig {
    /// Human readable name shown in the console (defaults to hostname).
    pub display_name: String,
    pub mode: DeviceMode,
    /// Seconds between heartbeats.
    pub heartbeat_interval_s: u32,
    /// STUN/TURN servers to use for sessions (may be overridden per session).
    pub ice_servers: Vec<IceServer>,
    /// Upper bound for capture/encode frame rate.
    pub max_fps: u32,
    /// Encoder target bitrate in kbit/s.
    pub max_bitrate_kbps: u32,
    /// Codec to prefer when both sides support it.
    pub preferred_codec: VideoCodec,
    /// Whether operators are allowed to inject mouse/keyboard input.
    pub allow_input: bool,
    /// Whether clipboard sync is allowed.
    pub allow_clipboard: bool,
    /// Help-me mode: seconds the approval prompt stays open before auto-deny.
    pub approval_timeout_s: u32,
    /// Show an on-screen indicator while a session is active.
    pub show_session_indicator: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            display_name: String::new(),
            mode: DeviceMode::Unattended,
            heartbeat_interval_s: 15,
            ice_servers: vec![IceServer {
                urls: vec!["stun:stun.l.google.com:19302".into()],
                username: None,
                credential: None,
            }],
            max_fps: 60,
            max_bitrate_kbps: 8000,
            preferred_codec: VideoCodec::H265,
            allow_input: true,
            allow_clipboard: true,
            approval_timeout_s: 60,
            show_session_indicator: true,
        }
    }
}

/// Body of `POST /api/enroll` sent by a freshly installed agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EnrollRequest {
    /// One-time / limited-use enrollment token issued by the console.
    pub token: String,
    pub hostname: String,
    pub os: crate::common::Os,
    pub arch: crate::common::Arch,
    pub agent_version: String,
    /// Optional operator-facing name (`remote-agent enroll --name`); defaults to the hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub display_name: Option<String>,
}

/// Response of `POST /api/enroll`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EnrollResponse {
    pub device_id: String,
    /// Long-lived secret the agent authenticates with on `/ws/agent`. Shown once.
    pub device_secret: String,
    /// Canonical console base URL the agent should use from now on.
    pub server_url: String,
    pub config: AgentConfig,
}
