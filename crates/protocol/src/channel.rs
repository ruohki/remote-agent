//! Messages carried on the WebRTC data channels between the browser viewer and the agent.
//!
//! * `input` channel (browser → agent): [`InputEvent`], ordered + reliable, JSON.
//! * `control` channel (both directions): [`ControlMessage`], ordered + reliable, JSON.
//!
//! Tags are kept short because input events are sent at up to a few hundred Hz.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::{DisplayInfo, VideoCodec};

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
    /// Switch the video track to another display.
    SelectDisplay { index: u32 },
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
    ClipboardSet { text: String },

    // ── agent → browser ────────────────────────────────────────────────────────
    /// Sent once after the control channel opens and whenever displays change.
    DisplayInfo {
        displays: Vec<DisplayInfo>,
        current: u32,
    },
    /// Device clipboard changed.
    ClipboardChanged { text: String },
    /// Periodic encoder/capture statistics.
    Stats {
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
}
