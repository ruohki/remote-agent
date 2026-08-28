use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Os {
    Windows,
    Macos,
    Linux,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            Os::Linux
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn current() -> Self {
        if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            Arch::X86_64
        }
    }
}

/// Video codec used on the WebRTC video track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum VideoCodec {
    H265,
    H264,
}

impl VideoCodec {
    /// MIME type as it appears in SDP / `RTCRtpCodecCapability.mimeType`.
    pub fn mime_type(self) -> &'static str {
        match self {
            VideoCodec::H265 => "video/H265",
            VideoCodec::H264 => "video/H264",
        }
    }
}

/// How the agent authorizes incoming operator sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum DeviceMode {
    /// Operators may connect at any time without anyone at the device.
    #[default]
    Unattended,
    /// The person at the device must approve every session (“help me” mode).
    HelpMe,
}

/// A physical display attached to the device, in the device's global coordinate space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DisplayInfo {
    /// Stable index used by [`crate::channel::ControlMessage::SelectDisplay`].
    pub index: u32,
    pub name: String,
    /// Origin in global (virtual desktop) logical coordinates.
    pub x: i32,
    pub y: i32,
    /// Size in physical pixels (the size of the captured/encoded video).
    pub width: u32,
    pub height: u32,
    /// Backing scale factor (2.0 on Retina). Logical size = physical / scale.
    pub scale: f32,
    pub primary: bool,
}

/// ICE server entry as passed to `RTCPeerConnection` / webrtc-rs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub credential: Option<String>,
}

/// Role of a participant in a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SessionRole {
    /// Controls the device.
    #[default]
    Operator,
    /// Admin shadowing the session: receives the same video/audio, no input.
    Observer,
}

/// Minimal description of the operator shown to the person at the device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OperatorInfo {
    pub id: String,
    pub name: String,
}

/// State machine of a remote session as tracked by the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SessionState {
    /// Created by the operator; offer forwarded to the agent.
    Requested,
    /// Agent is waiting for the person at the device to approve (help-me mode).
    AwaitingApproval,
    /// Approved / unattended; ICE is being negotiated.
    Connecting,
    /// Media is flowing.
    Connected,
    /// Terminal state.
    Ended,
}

/// Why a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum EndReason {
    OperatorClosed,
    DeviceUserClosed,
    Denied,
    ApprovalTimeout,
    AgentOffline,
    ConnectionFailed,
    Error,
}

/// Session-level SDP description (mirrors `RTCSessionDescriptionInit`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionDescription {
    /// `"offer"` or `"answer"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub sdp: String,
}

/// Trickle ICE candidate (mirrors `RTCIceCandidateInit`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(default, rename = "sdpMid", skip_serializing_if = "Option::is_none")]
    #[ts(optional, rename = "sdpMid")]
    pub sdp_mid: Option<String>,
    #[serde(
        default,
        rename = "sdpMLineIndex",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional, rename = "sdpMLineIndex")]
    pub sdp_mline_index: Option<u16>,
    #[serde(
        default,
        rename = "usernameFragment",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional, rename = "usernameFragment")]
    pub username_fragment: Option<String>,
}
