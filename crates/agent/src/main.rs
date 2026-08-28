//! remote-agent — screen sharing / remote control agent.
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
//! * [`service`]  — launchd / Windows service integration
//! * [`platform`] — misc OS helpers (logged-in user, permissions, session helper spawning)

mod approval;
mod capture;
mod cli;
mod config;
mod encode;
mod enroll;
mod hub;
mod input;
mod platform;
mod service;
mod session;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let code = match cli::run() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("error: {err:#}");
            tracing::error!("fatal: {err:#}");
            1
        }
    };
    std::process::exit(code);
}
