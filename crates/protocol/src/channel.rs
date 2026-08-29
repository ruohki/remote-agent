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
    /// Engage / release the privacy screen: the device's own displays show a branded notice
    /// instead of the desktop while the operator works (requires
    /// `AgentConfig.allow_privacy_screen`, the operator's `manage` permission and device
    /// support). The person at the device can always lift it; once they do, it stays off for
    /// the rest of the session.
    SetPrivacyScreen {
        enabled: bool,
    },
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
        /// Encoded picture size (may be smaller than the display when viewport scaling is on).
        #[serde(default)]
        encoded_width: u32,
        #[serde(default)]
        encoded_height: u32,
        /// Capture → encoded latency and encode duration, milliseconds (averages over the window).
        #[serde(default)]
        capture_to_encoded_ms: f32,
        #[serde(default)]
        encode_ms: f32,
        /// Keyframes sent in the window and current GOP policy (0 = keyframes only on demand).
        #[serde(default)]
        keyframes: u32,
        #[serde(default)]
        frames_skipped_idle: u32,
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
    /// Agent → browser: privacy screen state changed. `locked` = the device user lifted it,
    /// so the operator cannot engage it again in this session.
    PrivacyScreen {
        active: bool,
        reason: crate::common::PrivacyScreenReason,
        #[serde(default)]
        locked: bool,
    },
    /// Agent → browser: a `set_privacy_screen` request was refused.
    PrivacyScreenDenied {
        reason: crate::common::PrivacyScreenReason,
    },

    // ── performance (browser → agent) ───────────────────────────────────────────
    /// Size at which the browser renders `display` (CSS px × devicePixelRatio). The agent
    /// encodes at `min(display size, viewport)` to save bandwidth; `None` = full resolution.
    SetViewport {
        display: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        height: Option<u32>,
    },

    // ── cursor (agent → browser; the capture omits the system cursor) ───────────
    /// New cursor image (PNG, base64) with its hotspot in physical pixels; sent when the
    /// shape changes. Browsers draw it locally so the cursor never lags the stream.
    CursorShape {
        id: u32,
        png_base64: String,
        hotspot_x: u32,
        hotspot_y: u32,
        width: u32,
        height: u32,
    },
    /// Cursor position in physical pixels of `display` (≤ 60 Hz, only on change);
    /// `visible = false` hides it (e.g. cursor hidden by the OS).
    CursorPosition {
        display: u32,
        x: i32,
        y: i32,
        shape_id: u32,
        visible: bool,
    },
}
