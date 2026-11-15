//! Sessions run inside this process: the [`SessionManager`] plus the device-side UI it talks to.
//!
//! Used in two places:
//! * the standalone agent (`remote-agent run` / the double-clicked app), where the hub feeds it
//!   console signaling directly ([`crate::hub::Backend`]);
//! * a session host behind the service, where the same signaling arrives over the local link
//!   ([`crate::session_host`]).

use crate::approval::{
    ApprovalOutcome, Approver, AutoApprover, Indicator, NativeApprover, NativeIndicator,
    NoIndicator,
};
use crate::chat::{ChatUi, NativeChatUi, NoChatUi};
use crate::hub::{Backend, HubSink};
use crate::input::{Injector, InputHandler};
use crate::link::{HostSettings, HostStatus};
use crate::session::media::{MediaFactory, SystemMedia};
use crate::session::{SessionDeps, SessionManager, SessionRequest};
use futures_util::future::BoxFuture;
use parking_lot::RwLock;
use protocol::agent::{AgentCapabilities, ConsoleToAgent};
use protocol::common::EndReason;
use protocol::config::AgentConfig;
use std::sync::Arc;
use std::time::Duration;

/// The device-side presentation a [`LocalSessions`] uses.
pub struct UiDeps {
    pub approver: Arc<dyn Approver>,
    pub indicator: Arc<dyn Indicator>,
    pub chat: Arc<dyn ChatUi>,
    pub annotations: Arc<dyn crate::annotate::AnnotationSink>,
}

impl UiDeps {
    /// Pick by what this process has: the branded app loop, a bare native main loop, or nothing.
    pub fn detect() -> Self {
        let interactive = crate::platform::main_loop_running();
        let app = crate::app::is_running();
        Self {
            approver: if interactive {
                Arc::new(NativeApprover)
            } else {
                // Without a GUI loop nobody can answer: deny help-me sessions rather than allow.
                tracing::warn!("no UI loop: help-me approval prompts will be denied");
                Arc::new(AutoApprover(ApprovalOutcome::Denied))
            },
            indicator: if app {
                Arc::new(crate::app::AppIndicator)
            } else if interactive {
                Arc::new(NativeIndicator)
            } else {
                Arc::new(NoIndicator)
            },
            chat: if app {
                Arc::new(crate::app::AppChatUi)
            } else if interactive {
                Arc::new(NativeChatUi)
            } else {
                Arc::new(NoChatUi)
            },
            annotations: if app {
                Arc::new(crate::app::AppAnnotations)
            } else {
                Arc::new(crate::annotate::NoAnnotations)
            },
        }
    }

    /// Everything through the app module, which forwards to a UI process over its link
    /// (Windows SYSTEM session host). Approval prompts go to the signed-in user's UI and are
    /// denied while none is connected.
    pub fn remote_app() -> Self {
        Self {
            approver: Arc::new(crate::app::remote::RemoteApprover),
            indicator: Arc::new(crate::app::AppIndicator),
            chat: Arc::new(crate::app::AppChatUi),
            annotations: Arc::new(crate::app::AppAnnotations),
        }
    }
}

/// In-process sessions.
pub struct LocalSessions {
    pub sessions: Arc<SessionManager>,
    media: Arc<SystemMedia>,
    config: Arc<RwLock<AgentConfig>>,
}

impl LocalSessions {
    pub fn new(sink: HubSink, initial: AgentConfig, ui: UiDeps) -> Arc<Self> {
        let config = Arc::new(RwLock::new(initial));
        let media = Arc::new(SystemMedia);
        let input: crate::session::InputFactory =
            Arc::new(|| Ok(Box::new(Injector::new()?) as Box<dyn InputHandler>));
        let sessions = SessionManager::new(SessionDeps {
            media: media.clone(),
            input,
            approver: ui.approver,
            indicator: ui.indicator,
            chat: ui.chat,
            clipboard: Arc::new(crate::clipboard::SystemClipboard),
            hub: sink,
            config: Arc::clone(&config),
            annotations: ui.annotations,
        });
        Arc::new(Self {
            sessions,
            media,
            config,
        })
    }

    /// Hook the app window / tray to these sessions: tray *End session*, the bar's pause
    /// switch and the privacy screen's *Show screen*.
    pub fn wire_app(self: &Arc<Self>) {
        if !crate::app::is_running() && !crate::app::remote::is_host() {
            return;
        }
        let s = Arc::clone(&self.sessions);
        crate::app::set_global_disconnect(Arc::new(move || s.end_all(EndReason::DeviceUserClosed)));
        let s = Arc::clone(&self.sessions);
        crate::app::set_pause_handler(Arc::new(move |paused| s.set_control_paused(paused)));
        let s = Arc::clone(&self.sessions);
        crate::app::set_privacy_reveal_handler(Arc::new(move || s.lift_privacy_screen()));
    }

    pub fn current_config(&self) -> AgentConfig {
        self.config.read().clone()
    }

    /// Capabilities and screen state of this process's desktop.
    pub fn host_status(&self) -> HostStatus {
        HostStatus {
            capabilities: AgentCapabilities {
                codecs: self.media.available_codecs(),
                displays: self.media.list_displays().unwrap_or_default(),
                input: true,
                clipboard: true,
                privacy_screen: crate::privacy::support(),
            },
            desktop: crate::platform::desktop_state(),
            logged_in_user: crate::platform::logged_in_user(),
            on_console: crate::platform::session_on_console(),
        }
    }

    /// Route one piece of console session signaling.
    pub fn deliver(&self, msg: ConsoleToAgent) {
        match msg {
            ConsoleToAgent::SessionRequest {
                session_id,
                operator,
                offer,
                ice_servers,
                role,
                shadow_of,
                notify_operator,
                privacy_screen_allowed,
            } => self.sessions.start(SessionRequest {
                session_id,
                operator,
                offer,
                ice_servers,
                role,
                shadow_of,
                notify_operator,
                privacy_screen_allowed,
            }),
            ConsoleToAgent::IceCandidate {
                session_id,
                candidate,
            } => self.sessions.add_ice_candidate(&session_id, candidate),
            ConsoleToAgent::SessionEnd { session_id, reason } => {
                self.sessions.end(&session_id, reason)
            }
            other => tracing::debug!(?other, "not session signaling; ignored"),
        }
    }

    pub fn apply_settings(&self, settings: &HostSettings) {
        *self.config.write() = settings.config.clone();
        self.sessions.apply_overrides(settings.config.clone());
        crate::app::set_policy(&settings.policy_json());
    }
}

impl Backend for LocalSessions {
    fn status(&self) -> HostStatus {
        self.host_status()
    }

    fn deliver(&self, msg: ConsoleToAgent) {
        LocalSessions::deliver(self, msg)
    }

    fn settings_changed(&self, settings: &HostSettings) {
        self.apply_settings(settings)
    }

    fn console_status(&self, connected: bool) {
        crate::app::set_console_status(connected);
    }

    fn end_all(&self, reason: EndReason) {
        self.sessions.end_all(reason)
    }

    fn has_session(&self) -> bool {
        self.sessions.active_session_id().is_some()
    }

    fn wait_idle(&self, timeout: Duration) -> BoxFuture<'_, ()> {
        Box::pin(self.sessions.wait_idle(timeout))
    }
}
