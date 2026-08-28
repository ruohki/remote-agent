//! Wire protocol shared by the agent, the management console and the browser viewer.
//!
//! Three transports use these types:
//!
//! * **agent ⇄ console** over a WebSocket (`/ws/agent`): [`agent::AgentToConsole`] /
//!   [`agent::ConsoleToAgent`]. Carries enrollment state, heartbeats, configuration and
//!   WebRTC signaling relayed to/from a browser.
//! * **browser ⇄ console** over a WebSocket (`/ws/ui`): [`ui::UiToConsole`] /
//!   [`ui::ConsoleToUi`]. Carries live device state and WebRTC signaling.
//! * **browser ⇄ agent** over WebRTC data channels: [`channel::InputEvent`] on the
//!   `input` channel and [`channel::ControlMessage`] on the `control` channel.
//!
//! Every message is JSON with a `type` discriminator (`#[serde(tag = "type")]`).
//! TypeScript bindings are generated with `cargo test -p protocol` into `bindings/`.

pub mod agent;
pub mod channel;
pub mod common;
pub mod config;
pub mod files;
pub mod ui;

/// Bumped on incompatible wire changes. Sent in `hello` by both agent and console.
pub const PROTOCOL_VERSION: u32 = 1;

/// WebSocket path the agent connects to on the console.
pub const AGENT_WS_PATH: &str = "/ws/agent";
/// WebSocket path the browser UI connects to on the console.
pub const UI_WS_PATH: &str = "/ws/ui";
/// Label of the WebRTC data channel carrying [`channel::InputEvent`]s.
pub const INPUT_CHANNEL_LABEL: &str = "input";
/// Label of the WebRTC data channel carrying [`channel::ControlMessage`]s.
pub const CONTROL_CHANNEL_LABEL: &str = "control";
/// Label of the WebRTC data channel carrying [`files::FileMessage`]s and binary file chunks.
pub const FILES_CHANNEL_LABEL: &str = "files";
