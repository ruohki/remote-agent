//! remote-agent — screen sharing / remote control agent.
//!
//! The crate is a library plus a thin binary (`main.rs`) so that integration tests can drive
//! the session code with fake capture/encode/input implementations.
//!
//! Module map (see ARCHITECTURE.md in the repo root):
//!
//! * [`cli`]      — command line (`run`, `enroll`, `service install|uninstall|run`, `status`, `doctor`)
//! * [`config`]   — on-disk state: server URL, device credentials, cached [`protocol::config::AgentConfig`]
//! * [`enroll`]   — `POST /api/enroll` with an enrollment token
//! * [`hub`]      — persistent WebSocket to the console (`/ws/agent`): hello/heartbeat/config/signaling
//! * [`session`]  — one WebRTC peer connection per operator session: video track, input & control channels
//! * [`capture`]  — platform screen capture (ScreenCaptureKit / DXGI desktop duplication)
//! * [`encode`]   — H265/H264 encoders (VideoToolbox / Media Foundation / OpenH264 fallback)
//! * [`input`]    — mouse & keyboard injection
//! * [`approval`] — help-me mode prompt and on-screen session indicator
//! * [`chat`]     — session chat model + native chat window trait
//! * [`transfer`] — resumable file transfers, remote file browser, clipboard transports
//! * [`clipboard`]— clipboard change detection (text/image/files) and placement
//! * [`audio`]    — system audio capture + Opus encoding
//! * [`service`]  — launchd / Windows service integration
//! * [`platform`] — misc OS helpers (logged-in user, permissions, main-thread dispatch)

pub mod annotate;
pub mod app;
pub mod approval;
pub mod audio;
pub mod baked;
pub mod branding;
pub mod capture;
pub mod chat;
pub mod cli;
pub mod clipboard;
pub mod config;
pub mod congestion;
pub mod cursor;
pub mod encode;
pub mod enroll;
pub mod hub;
pub mod input;
pub mod platform;
pub mod secrets;
pub mod service;
pub mod session;
pub mod startup;
pub mod transfer;
pub mod transport;
pub mod updater;

/// Version baked in at build time (`CARGO_PKG_VERSION`).
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
