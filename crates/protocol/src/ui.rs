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

/// A device group the current user can see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct GroupRef {
    pub id: String,
    pub name: String,
}

/// The calling user's effective permission on a device (admins always `manage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum DevicePermission {
    /// Status and history only.
    #[default]
    View,
    /// May open sessions, rename and tag.
    Connect,
    /// May change config, groups and delete (admins).
    Manage,
}

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
    /// Groups the device belongs to.
    #[serde(default)]
    pub groups: Vec<GroupRef>,
    /// What the *requesting* user may do with this device (per-user; not broadcast state).
    #[serde(default)]
    pub permission: DevicePermission,
    /// Restrictions the person at the device applied locally (tighten-only).
    #[serde(default)]
    pub local_overrides: crate::config::LocalOverrides,
    /// Whether the agent can hide the device's displays (from its `hello` capabilities).
    #[serde(default)]
    pub privacy_screen: crate::common::PrivacyScreenSupport,
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
    /// `observer` when this row is a shadow of `shadow_of`.
    #[serde(default)]
    pub role: crate::common::SessionRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub shadow_of: Option<String>,
    /// Admins currently shadowing this (operator) session.
    #[serde(default)]
    pub observers: Vec<crate::common::OperatorInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum UiToConsole {
    /// Start a session: the browser has already created an offer.
    SessionOffer {
        device_id: String,
        offer: SessionDescription,
        /// Admins only: shadow this running session instead of starting a new one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        shadow_of: Option<String>,
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
