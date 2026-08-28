//! Messages exchanged between the agent and the console over `/ws/agent`.
//!
//! The first message on a fresh socket MUST be [`AgentToConsole::Hello`]; the console
//! replies with [`ConsoleToAgent::HelloAck`] (or closes the socket with a policy code).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::channel::ChatParty;
use crate::common::{
    Arch, DeviceMode, DisplayInfo, EndReason, IceCandidate, IceServer, OperatorInfo, Os,
    SessionDescription, VideoCodec,
};
use crate::config::{AgentConfig, LocalOverrides};
use crate::files::{TransferDirection, TransferKind};

/// Something noteworthy that happened inside a session; persisted by the console
/// (`GET /api/sessions/:id/events`) and pushed to UIs as `ConsoleToUi::SessionEvent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum SessionEvent {
    Chat {
        from: ChatParty,
        text: String,
    },
    TransferStarted {
        token: String,
        name: String,
        size: u64,
        kind: TransferKind,
        direction: TransferDirection,
        /// Resume offset (0 for a fresh transfer).
        offset: u64,
    },
    TransferCompleted {
        token: String,
        name: String,
        size: u64,
        direction: TransferDirection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        path: Option<String>,
    },
    TransferFailed {
        token: String,
        name: String,
        reason: String,
    },
    ClipboardSync {
        direction: TransferDirection,
        summary: String,
    },
    DisplaysChanged {
        active: Vec<u32>,
    },
    AudioChanged {
        enabled: bool,
    },
    ObserverJoined {
        observer: crate::common::OperatorInfo,
        session_id: String,
    },
    ObserverLeft {
        observer: crate::common::OperatorInfo,
        session_id: String,
    },
    /// Device user paused / resumed remote control during the session.
    ControlPaused {
        paused: bool,
    },
}

fn default_true() -> bool {
    true
}

/// What an agent can do, reported in `hello` and whenever it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AgentCapabilities {
    /// Codecs the agent can *encode*, best first (hardware H265 → hardware H264 → software H264).
    pub codecs: Vec<VideoCodec>,
    pub displays: Vec<DisplayInfo>,
    pub input: bool,
    pub clipboard: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum AgentToConsole {
    Hello {
        protocol_version: u32,
        device_id: String,
        device_secret: String,
        agent_version: String,
        hostname: String,
        os: Os,
        arch: Arch,
        /// Mode the agent is currently running with (from its cached config).
        mode: DeviceMode,
        capabilities: AgentCapabilities,
        /// User currently logged in at the console session, if known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        logged_in_user: Option<String>,
        /// Restrictions set locally by the person at the device.
        #[serde(default, skip_serializing_if = "LocalOverrides::is_empty")]
        local_overrides: LocalOverrides,
    },
    Heartbeat {
        uptime_s: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        logged_in_user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        cpu_percent: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        mem_percent: Option<f32>,
        /// Present only when displays changed since the last report.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        displays: Option<Vec<DisplayInfo>>,
        /// Present only when the local restrictions changed since the last report.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        local_overrides: Option<LocalOverrides>,
    },
    /// Result of the help-me approval prompt (only sent in help-me mode).
    ApprovalResult { session_id: String, approved: bool },
    /// SDP answer to the operator's offer.
    SessionAnswer {
        session_id: String,
        answer: SessionDescription,
        /// Codec the agent will encode with after negotiation.
        codec: VideoCodec,
    },
    IceCandidate {
        session_id: String,
        candidate: IceCandidate,
    },
    SessionState {
        session_id: String,
        state: crate::common::SessionState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        reason: Option<EndReason>,
    },
    /// In-session activity for the audit trail / session detail page.
    SessionEvent {
        session_id: String,
        event: SessionEvent,
        /// Unix epoch milliseconds (agent clock).
        ts_ms: u64,
    },
    /// Reply to [`ConsoleToAgent::Ping`].
    Pong { nonce: u64 },
    /// Free-form log line forwarded to the console (kept short; rate limited by the agent).
    Log { level: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ConsoleToAgent {
    HelloAck {
        protocol_version: u32,
        /// Unix epoch milliseconds on the console; used for clock-skew estimates only.
        server_time_ms: u64,
        config: AgentConfig,
    },
    ConfigUpdate {
        config: AgentConfig,
    },
    /// An operator wants to connect. The agent must:
    /// 1. in help-me mode: prompt the local user and send [`AgentToConsole::ApprovalResult`];
    /// 2. once allowed: create a peer connection, apply `offer`, send [`AgentToConsole::SessionAnswer`].
    SessionRequest {
        session_id: String,
        operator: OperatorInfo,
        offer: SessionDescription,
        /// ICE servers for this session (includes short-lived TURN credentials).
        ice_servers: Vec<IceServer>,
        /// `Observer` = an admin shadowing `shadow_of`; the agent fans out the existing
        /// encoded stream to this extra peer, ignores its `input` channel and reports
        /// `SessionEvent::ObserverJoined/Left`.
        #[serde(default)]
        role: crate::common::SessionRole,
        /// The operator session being shadowed (observers only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        shadow_of: Option<String>,
        /// Whether the operator's viewer / device banner should announce the observer.
        #[serde(default = "default_true")]
        notify_operator: bool,
    },
    IceCandidate {
        session_id: String,
        candidate: IceCandidate,
    },
    /// The console or operator ended the session; tear down the peer connection.
    SessionEnd {
        session_id: String,
        reason: EndReason,
    },
    Ping {
        nonce: u64,
    },
    /// Ask the agent to download and install the given version, then restart.
    Update {
        version: String,
        url: String,
        sha256: String,
    },
    /// Console is shutting down / rejecting; agent should reconnect with backoff.
    Goodbye {
        reason: String,
    },
}
