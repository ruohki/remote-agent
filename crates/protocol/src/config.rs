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
    /// Whether operators may send/receive files and browse the device file system.
    #[serde(default = "default_true")]
    pub allow_file_transfer: bool,
    /// Directory that receives uploads (default: `<home>/Downloads/RemoteAgent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub transfer_dir: Option<String>,
    /// Whether system audio may be streamed to the operator.
    #[serde(default = "default_true")]
    pub allow_audio: bool,
    /// Whether operators may draw guidance annotations on the device screen (independent of
    /// `allow_input`, so guidance works while control is disabled or paused).
    #[serde(default = "default_true")]
    pub allow_annotations: bool,
    /// Whether operators with `manage` permission may engage the privacy screen (the device's
    /// own displays show a branded notice while the operator works). Off by default: it hides
    /// the operator's actions from the person at the device.
    #[serde(default)]
    pub allow_privacy_screen: bool,
}

fn default_true() -> bool {
    true
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
            allow_file_transfer: true,
            transfer_dir: None,
            allow_audio: true,
            allow_annotations: true,
            allow_privacy_screen: false,
        }
    }
}

/// Restrictions the person at the device applies locally in the agent app. They can only
/// **tighten** the console's [`AgentConfig`] (require approval, block input/audio/clipboard/
/// files); `None` = follow the console. Reported to the console so admins see the effective
/// policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[ts(export)]
pub struct LocalOverrides {
    /// `Some(HelpMe)` forces approval even when the console says unattended (`Some(Unattended)`
    /// is ignored — it cannot loosen policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub mode: Option<DeviceMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub allow_input: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub allow_audio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub allow_clipboard: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub allow_file_transfer: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub allow_annotations: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub allow_privacy_screen: Option<bool>,
}

impl LocalOverrides {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Console config combined with the local restrictions (restrict-only).
    pub fn apply(&self, mut cfg: AgentConfig) -> AgentConfig {
        if self.mode == Some(DeviceMode::HelpMe) {
            cfg.mode = DeviceMode::HelpMe;
        }
        if self.allow_input == Some(false) {
            cfg.allow_input = false;
        }
        if self.allow_audio == Some(false) {
            cfg.allow_audio = false;
        }
        if self.allow_clipboard == Some(false) {
            cfg.allow_clipboard = false;
        }
        if self.allow_file_transfer == Some(false) {
            cfg.allow_file_transfer = false;
        }
        if self.allow_annotations == Some(false) {
            cfg.allow_annotations = false;
        }
        if self.allow_privacy_screen == Some(false) {
            cfg.allow_privacy_screen = false;
        }
        cfg
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
