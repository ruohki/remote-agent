//! Messages exchanged between the browser UI and the console over `/ws/ui`.
//!
//! The socket is authenticated by the regular login cookie. The browser creates the
//! WebRTC offer, the console relays signaling to the agent and pushes live state.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::{
    Arch, DeviceMode, DisplayInfo, EndReason, IceCandidate, IceServer, Os, SessionDescription,
    SessionState, VideoCodec,
};

/// Device row as shown in the console; also the payload of live updates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DeviceSummary {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub os: Os,
    pub arch: Arch,
    pub agent_version: String,
    pub mode: DeviceMode,
    pub tags: Vec<String>,
    pub online: bool,
    /// ISO-8601 timestamp of last heartbeat / disconnect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_seen_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub logged_in_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_ip: Option<String>,
    pub codecs: Vec<VideoCodec>,
    pub displays: Vec<DisplayInfo>,
    /// Id of the active session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub active_session_id: Option<String>,
}

/// Session row as shown in the console.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionSummary {
    pub id: String,
    pub device_id: String,
    pub device_name: String,
    pub operator_id: String,
    pub operator_name: String,
    pub state: SessionState,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub connected_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub end_reason: Option<EndReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub codec: Option<VideoCodec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum UiToConsole {
    /// Start a session: the browser has already created an offer.
    SessionOffer {
        device_id: String,
        offer: SessionDescription,
    },
    IceCandidate {
        session_id: String,
        candidate: IceCandidate,
    },
    SessionEnd {
        session_id: String,
    },
    /// Subscribe to live updates for all devices (sent once after connect).
    Subscribe,
    Ping {
        nonce: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ConsoleToUi {
    /// Sent right after [`UiToConsole::Subscribe`].
    Snapshot {
        devices: Vec<DeviceSummary>,
        sessions: Vec<SessionSummary>,
    },
    DeviceUpdate {
        device: DeviceSummary,
    },
    DeviceRemoved {
        device_id: String,
    },
    /// Console accepted the offer and forwarded it; carries the ICE servers the browser
    /// must add to its peer connection (TURN credentials are short-lived).
    SessionCreated {
        session_id: String,
        device_id: String,
        ice_servers: Vec<IceServer>,
    },
    SessionAnswer {
        session_id: String,
        answer: SessionDescription,
        codec: VideoCodec,
    },
    IceCandidate {
        session_id: String,
        candidate: IceCandidate,
    },
    SessionUpdate {
        session: SessionSummary,
    },
    /// Chat / transfer / display activity reported by the agent (see `SessionEvent`).
    SessionEvent {
        session_id: String,
        event: crate::agent::SessionEvent,
        /// ISO-8601 timestamp assigned by the console.
        ts: String,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        session_id: Option<String>,
        code: String,
        message: String,
    },
    Pong {
        nonce: u64,
    },
}
