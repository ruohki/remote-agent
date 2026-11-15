//! Local links between the agent's processes.
//!
//! An installed agent runs as several processes (see ARCHITECTURE.md, *Process model*):
//!
//! * the **service** (`service run`; LocalSystem / root, always running) owns the device
//!   identity, the console connection, configuration, updates and — on Windows — the secure
//!   attention sequence;
//! * a **session host** (`session-host`) works inside one desktop session: WebRTC, capture,
//!   encoding, input, audio, clipboard, files. On Windows it runs with the SYSTEM token in the
//!   console session so it reaches the sign-in screen, the lock screen and UAC prompts; on macOS
//!   it is a LaunchAgent in the user's GUI session or at the login window;
//! * on Windows a **UI process** (`ui`) runs as the signed-in user and shows the tray, the
//!   window, chat, the session bar, approval prompts and the privacy screen for the SYSTEM host.
//!
//! The service ⇄ host link carries [`HostToService`] / [`ServiceToHost`]; the host ⇄ UI link
//! carries [`crate::app::remote`] messages. Both are length-prefixed JSON frames
//! ([`frame`]) over a Unix socket or a named pipe ([`endpoint`]). Only our own executable may
//! connect: every accepted peer is checked against this process's image path.

pub mod endpoint;
pub mod frame;

use protocol::agent::{AgentCapabilities, AgentToConsole, ConsoleToAgent};
use protocol::common::{DesktopState, EndReason};
use protocol::config::{AgentConfig, LocalOverrides};
use serde::{Deserialize, Serialize};

/// Which desktop a session host serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    /// Windows: the SYSTEM host in the console session. It follows the input desktop, so one
    /// host covers the sign-in screen, the lock screen, UAC prompts and the user's desktop.
    Console,
    /// macOS: the LaunchAgent in a signed-in user's GUI session.
    UserSession,
    /// macOS: the LaunchAgent at the login window (nobody signed in).
    LoginWindow,
    /// A host started by hand (`session-host` from a terminal, tests).
    Manual,
}

/// What a session host can do and what its screen shows. Sent in the host's hello and
/// whenever it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostStatus {
    pub capabilities: AgentCapabilities,
    pub desktop: DesktopState,
    #[serde(default)]
    pub logged_in_user: Option<String>,
    /// Whether this host's session is the one on the physical console. On macOS several
    /// users can be signed in (fast user switching); only the one on console is offered to
    /// operators. Always true on Windows (the host follows the console session).
    pub on_console: bool,
}

/// Session host → service.
#[derive(Debug, Clone, Serialize, Deserialize)]
// `msg`, not `type`: the console messages wrapped by `Console(..)` use `type` themselves.
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum HostToService {
    /// First frame on a new link.
    Hello {
        /// `crate::AGENT_VERSION` of the host binary; a mismatch means a half-applied update.
        version: String,
        pid: u32,
        /// OS session id (Windows session id / macOS audit session id; 0 when unknown).
        os_session: u32,
        kind: HostKind,
        status: HostStatus,
    },
    /// Capabilities, desktop state or console ownership changed.
    Status(HostStatus),
    /// Session traffic bound for the console (answers, ICE, states, events, approvals).
    Console(AgentToConsole),
    /// The person at the device changed the local restrictions (app Settings screen).
    SetOverrides(LocalOverrides),
    /// The operator pressed Ctrl+Alt+Del. Windows only lets a service send it.
    SecureAttention,
}

/// Service → session host.
#[derive(Debug, Clone, Serialize, Deserialize)]
// `msg`, not `type`: the console messages wrapped by `Console(..)` use `type` themselves.
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ServiceToHost {
    /// First frame after the host's hello: who this device is and how it is configured.
    Welcome {
        device_id: String,
        server_url: String,
        console_connected: bool,
        settings: HostSettings,
    },
    /// Configuration or local restrictions changed.
    Settings(HostSettings),
    /// Console connection came up or went down.
    ConsoleStatus { connected: bool },
    /// The service cached new branding in the machine state directory: re-read it.
    BrandingChanged,
    /// Session signaling from the console: `session_request`, `ice_candidate`, `session_end`.
    Console(ConsoleToAgent),
    /// End every session on this host (another desktop became the active one, or the console
    /// connection dropped).
    EndSessions { reason: EndReason },
    /// Finish the active session, then exit (service stopping, host being replaced).
    Shutdown { reason: String },
}

/// Configuration a host needs, computed by the service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostSettings {
    /// Effective configuration: console policy tightened by the local restrictions.
    pub config: AgentConfig,
    /// The console's policy before local restrictions (Settings screen).
    pub console_config: AgentConfig,
    pub overrides: LocalOverrides,
}

impl HostSettings {
    /// JSON blob for the app Settings screen: `{ console, overrides, effective }`.
    pub fn policy_json(&self) -> String {
        let flags = |c: &AgentConfig| {
            serde_json::json!({
                "mode": c.mode,
                "allow_input": c.allow_input,
                "allow_audio": c.allow_audio,
                "allow_clipboard": c.allow_clipboard,
                "allow_file_transfer": c.allow_file_transfer,
                "allow_annotations": c.allow_annotations,
                "allow_privacy_screen": c.allow_privacy_screen,
            })
        };
        serde_json::json!({
            "console": flags(&self.console_config),
            "overrides": self.overrides,
            "effective": flags(&self.config),
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_roundtrip_as_tagged_json() {
        let hello = HostToService::Hello {
            version: "1.2.3".into(),
            pid: 42,
            os_session: 1,
            kind: HostKind::Console,
            status: HostStatus {
                capabilities: AgentCapabilities {
                    codecs: vec![],
                    displays: vec![],
                    input: true,
                    clipboard: true,
                    privacy_screen: Default::default(),
                },
                desktop: DesktopState::SignIn,
                logged_in_user: None,
                on_console: true,
            },
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert!(json.starts_with(r#"{"msg":"hello""#), "{json}");
        assert!(json.contains(r#""desktop":"sign_in""#));
        let back: HostToService = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, HostToService::Hello { pid: 42, .. }));

        let end = ServiceToHost::EndSessions {
            reason: EndReason::AgentOffline,
        };
        let json = serde_json::to_string(&end).unwrap();
        let back: ServiceToHost = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            ServiceToHost::EndSessions {
                reason: EndReason::AgentOffline
            }
        ));
    }

    #[test]
    fn policy_json_has_three_views() {
        let s = HostSettings {
            config: AgentConfig::default(),
            console_config: AgentConfig::default(),
            overrides: LocalOverrides::default(),
        };
        let v: serde_json::Value = serde_json::from_str(&s.policy_json()).unwrap();
        assert!(v["console"]["mode"].is_string());
        assert!(v["effective"]["allow_input"].is_boolean());
        assert!(v["overrides"].is_object());
    }

    /// Console messages carry their own `type` tag; wrapping them must not collide.
    #[test]
    fn wrapped_console_messages_roundtrip() {
        let req = ServiceToHost::Console(protocol::agent::ConsoleToAgent::SessionEnd {
            session_id: "s".into(),
            reason: EndReason::OperatorClosed,
        });
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            json.contains(r#""msg":"console""#) && json.contains(r#""type":"session_end""#),
            "{json}"
        );
        assert!(matches!(
            serde_json::from_str::<ServiceToHost>(&json).unwrap(),
            ServiceToHost::Console(protocol::agent::ConsoleToAgent::SessionEnd { .. })
        ));
        let out = HostToService::Console(protocol::agent::AgentToConsole::IceCandidate {
            session_id: "s".into(),
            candidate: protocol::common::IceCandidate {
                candidate: "candidate:1 1 udp 1 1.2.3.4 5 typ host".into(),
                sdp_mid: Some("0".into()),
                sdp_mline_index: Some(0),
                username_fragment: None,
            },
        });
        let json = serde_json::to_string(&out).unwrap();
        assert!(matches!(
            serde_json::from_str::<HostToService>(&json).unwrap(),
            HostToService::Console(protocol::agent::AgentToConsole::IceCandidate { .. })
        ));
        let ov = HostToService::SetOverrides(LocalOverrides::default());
        let json = serde_json::to_string(&ov).unwrap();
        assert!(matches!(
            serde_json::from_str::<HostToService>(&json).unwrap(),
            HostToService::SetOverrides(_)
        ));
    }
}
