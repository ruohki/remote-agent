//! Messages carried on the WebRTC data channels between the browser viewer and the agent.
//!
//! * `input` channel (browser → agent): [`InputEvent`], ordered + reliable, JSON.
//! * `control` channel (both directions): [`ControlMessage`], ordered + reliable, JSON.
//!
//! Tags are kept short because input events are sent at up to a few hundred Hz.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::{DisplayInfo, VideoCodec};

/// Who wrote a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ChatParty {
    Operator,
    Device,
}

/// What kind of rich content the device clipboard holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ClipboardKind {
    Image,
    Files,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "t", rename_all = "snake_case")]
#[ts(export)]
pub enum InputEvent {
    /// Absolute mouse position in *physical pixels* of the currently selected display.
    /// The agent adds the display origin to obtain global coordinates.
    #[serde(rename = "mm")]
    MouseMove { x: i32, y: i32 },
    #[serde(rename = "md")]
    MouseDown { button: MouseButton },
    #[serde(rename = "mu")]
    MouseUp { button: MouseButton },
    /// Scroll in lines (positive `dy` = wheel down / content moves up, like `WheelEvent.deltaY` sign).
    #[serde(rename = "mw")]
    MouseWheel { dx: f32, dy: f32 },
    /// W3C `KeyboardEvent.code` value, e.g. `"KeyA"`, `"Enter"`, `"MetaLeft"`.
    #[serde(rename = "kd")]
    KeyDown { code: String },
    #[serde(rename = "ku")]
    KeyUp { code: String },
    /// Type a unicode string (used for characters that have no physical key mapping).
    #[serde(rename = "tx")]
    Text { text: String },
    /// Release every key and button the agent believes is down (sent on blur / disconnect).
    #[serde(rename = "rel")]
    ReleaseAll,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "t", rename_all = "snake_case")]
#[ts(export)]
pub enum ControlMessage {
    // ── browser → agent ────────────────────────────────────────────────────────
    /// Switch the *primary* video track to another display (kept for single-tile viewers).
    SelectDisplay {
        index: u32,
    },
    /// Multi-display: which displays should stream. The browser adds one `recvonly` video
    /// transceiver per display (in `DisplayInfo` index order); the agent binds the i-th video
    /// m-line to display i and only encodes the displays listed here.
    SetActiveDisplays {
        indices: Vec<u32>,
    },
    /// Enable / disable the system-audio track (requires `AgentConfig.allow_audio`).
    SetAudio {
        enabled: bool,
    },
    /// Chat line; sent by either side, echoed to the console as a session event by the agent.
    Chat {
        from: ChatParty,
        text: String,
        /// Unix epoch milliseconds.
        ts_ms: u64,
    },
    /// Runtime quality knobs; `None` keeps the current value.
    SetQuality {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        max_fps: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        max_bitrate_kbps: Option<u32>,
    },
    /// Ask the encoder for an immediate keyframe (e.g. after packet loss / tab focus).
    RequestKeyframe,
    /// Send Ctrl+Alt+Del (Windows) / equivalent secure attention sequence.
    SecureAttention,
    /// Operator's clipboard text to place on the device clipboard.
    ClipboardSet {
        text: String,
    },

    // ── agent → browser ────────────────────────────────────────────────────────
    /// Sent once after the control channel opens and whenever displays change.
    DisplayInfo {
        displays: Vec<DisplayInfo>,
        /// Primary display (the one `select_display` targets).
        current: u32,
        /// Displays currently being streamed (see `set_active_displays`).
        #[serde(default)]
        active: Vec<u32>,
        /// Whether an audio track is available on this session.
        #[serde(default)]
        audio: bool,
    },
    /// The device clipboard holds an image or files the operator may pull with
    /// `FileMessage::RequestClipboard`.
    ClipboardAvailable {
        kind: ClipboardKind,
        /// File names (or a single generated name for images).
        names: Vec<String>,
        total_bytes: u64,
    },
    /// Device clipboard changed.
    ClipboardChanged {
        text: String,
    },
    /// Periodic encoder/capture statistics (one per active display).
    Stats {
        #[serde(default)]
        display: u32,
        codec: VideoCodec,
        fps: f32,
        bitrate_kbps: u32,
        width: u32,
        height: u32,
        /// Capture→encode→send latency estimate in milliseconds.
        pipeline_ms: f32,
        hardware: bool,
    },
    /// The person at the device ended the session.
    SessionEndedByUser,
    /// An admin started / stopped shadowing this session (only sent when the console asked
    /// the agent to notify the operator).
    ObserverJoined {
        name: String,
    },
    ObserverLeft {
        name: String,
    },
    /// The person at the device paused / resumed remote keyboard & mouse control (the
    /// session bar's emergency switch). While paused the agent drops every input event; the
    /// operator cannot lift it — only the device user can.
    ControlPaused {
        paused: bool,
    },
    // ── screen annotations (browser → agent; independent of input permission) ─────
    /// Start or continue a freehand stroke. `points` are physical pixels of `display`,
    /// appended in order; the first message for an `id` starts the stroke.
    AnnotateStroke {
        id: u32,
        display: u32,
        color: String,
        /// Width in physical pixels.
        width: f32,
        points: Vec<(f32, f32)>,
    },
    /// The stroke is complete (fade-out timer starts on the device).
    AnnotateEnd {
        id: u32,
    },
    /// Laser pointer position (physical pixels of `display`); `None` hides it.
    AnnotatePointer {
        display: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        point: Option<(f32, f32)>,
        color: String,
    },
    /// Remove all annotations (all displays).
    AnnotateClear,
    /// Agent → browser: annotations are not allowed on this device (policy or local override).
    AnnotationsDisabled,
}
