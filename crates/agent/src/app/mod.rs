//! The branded agent application: a `tao` window hosting a `wry` webview (the UI in
//! `assets/`), plus a tray / menu-bar icon. It is the device-side face of the agent — status,
//! chat, "install as a service" and about — themed from the bakery trailer branding.
//!
//! Threading: the window and webview live on the process main thread inside [`run`]'s event
//! loop. Other threads (the session task, the hub) drive the UI by posting [`AppEvent`]s
//! through an [`tao::event_loop::EventLoopProxy`]; the loop applies them to the [`Controller`]
//! (which calls `evaluate_script`). JS → Rust messages arrive on the webview IPC handler
//! (also main thread) and are dispatched to the current session's callbacks.
//!
//! The chat presentation is wired through the [`crate::chat::ChatUi`] trait ([`AppChatUi`]),
//! so the session code is unchanged. Closing the window hides it (the session keeps running);
//! it is reopened from the tray. Every window we create is excluded from the screen capture
//! and ignores remote-injected input.

mod bar;
mod controller;

use crate::approval::{Indicator, IndicatorHandle};
use crate::chat::{ChatHandle, ChatLine, ChatUi};
use anyhow::Result;
use protocol::channel::ChatParty;
use protocol::common::OperatorInfo;
use protocol::config::LocalOverrides;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

pub use controller::run;

/// Options controlling how the app presents itself for a given launch.
#[derive(Debug, Clone, Copy)]
pub struct AppOptions {
    /// Show the window immediately (app / double-click mode) vs. stay in the tray (service mode).
    pub show_on_start: bool,
    /// Offer the "Install as a service" screen (only when not already installed).
    pub installable: bool,
}

/// Events posted to the UI thread from elsewhere.
#[derive(Debug, Clone)]
pub enum AppEvent {
    SessionStarted {
        operator: String,
    },
    SessionEnded,
    Chat {
        from: ChatParty,
        text: String,
        ts_ms: u64,
    },
    /// Bring the window forward (optionally to the chat screen).
    Show {
        chat: bool,
    },
    Hide,
    ConsoleStatus {
        connected: bool,
    },
    DeviceInfo {
        name: String,
        id: String,
    },
    InstallResult {
        ok: bool,
        message: String,
    },
    /// JSON policy blob for the Settings screen: { console, overrides, effective }.
    Policy(String),
    /// Branding changed (JSON for `setBranding`); the controller also refreshes title/icons.
    Branding(String),
    /// The console this agent is enrolled with (shown on the Status/About screens).
    ConsoleInfo {
        url: String,
    },
    /// OS permission state (macOS TCC); `supported = false` hides the section.
    Permissions {
        supported: bool,
        screen: bool,
        accessibility: bool,
    },
    /// A permission is required right now (session waiting): show the window on Status.
    PermissionNeeded,
    /// Result of "Move to Applications".
    MoveResult {
        ok: bool,
        message: String,
    },
    /// Messages from the session bar page.
    Bar(bar::BarIpc),
    Quit,
    /// Internal: the page finished loading and is ready to receive JS.
    #[doc(hidden)]
    __Ready,
}

type SendCb = Arc<dyn Fn(String) + Send + Sync>;
type DisconnectCb = Arc<dyn Fn() + Send + Sync>;

struct Callbacks {
    on_send: SendCb,
    on_disconnect: DisconnectCb,
}

/// Per-session callbacks (None between sessions).
static CALLBACKS: parking_lot::Mutex<Option<Callbacks>> = parking_lot::Mutex::new(None);
/// End the active session regardless of which one — set by the hub; used by the tray.
static GLOBAL_DISCONNECT: parking_lot::Mutex<Option<DisconnectCb>> = parking_lot::Mutex::new(None);
/// Applies device-side restriction changes (persist + recompute + apply live). Set by the hub.
type OverridesCb = Arc<dyn Fn(LocalOverrides) + Send + Sync>;
static OVERRIDES_CB: parking_lot::Mutex<Option<OverridesCb>> = parking_lot::Mutex::new(None);
static PROXY: OnceLock<parking_lot::Mutex<Option<Proxy>>> = OnceLock::new();
/// Config dir used to persist small UI state (session bar position).
static STATE_DIR: parking_lot::Mutex<Option<std::path::PathBuf>> = parking_lot::Mutex::new(None);

/// Where the app may persist UI state (the agent config dir).
pub fn set_state_dir(dir: std::path::PathBuf) {
    *STATE_DIR.lock() = Some(dir);
}

fn state_dir() -> Option<std::path::PathBuf> {
    STATE_DIR.lock().clone()
}

struct Proxy(tao::event_loop::EventLoopProxy<AppEvent>);
// SAFETY: EventLoopProxy is Send + Sync on the platforms we target; wrapper keeps it in a static.
unsafe impl Send for Proxy {}
unsafe impl Sync for Proxy {}

fn set_proxy(p: tao::event_loop::EventLoopProxy<AppEvent>) {
    *PROXY.get_or_init(|| parking_lot::Mutex::new(None)).lock() = Some(Proxy(p));
}

/// Post an event to the UI thread. No-op when the app loop is not running.
pub fn post(ev: AppEvent) {
    if let Some(cell) = PROXY.get() {
        if let Some(p) = cell.lock().as_ref() {
            let _ = p.0.send_event(ev);
        }
    }
}

/// Whether the app event loop is running (window/tray available).
pub fn is_running() -> bool {
    PROXY.get().map(|c| c.lock().is_some()).unwrap_or(false)
}

/// Set the process-wide "end the active session" action (used by the tray "End session").
pub fn set_global_disconnect(cb: DisconnectCb) {
    *GLOBAL_DISCONNECT.lock() = Some(cb);
}

/// Register the handler the Settings screen calls when the local restrictions change.
pub fn set_overrides_handler(cb: OverridesCb) {
    *OVERRIDES_CB.lock() = Some(cb);
}

/// Called from the webview IPC when the local user changes the restrictions.
fn dispatch_overrides(ov: LocalOverrides) {
    let cb = OVERRIDES_CB.lock().clone();
    if let Some(cb) = cb {
        cb(ov);
    }
}

/// Push the current policy (console vs. local restrictions vs. effective) to the Settings screen.
pub fn set_policy(policy_json: &str) {
    post(AppEvent::Policy(policy_json.to_string()));
}

/// Update the console connection status shown in the UI and tray tooltip.
pub fn set_console_status(connected: bool) {
    post(AppEvent::ConsoleStatus { connected });
}

/// A session is waiting for an OS permission: bring the window up on the Status screen.
pub fn permission_needed() {
    post(AppEvent::PermissionNeeded);
}

/// Tell the UI which console this agent talks to.
pub fn set_console_url(url: &str) {
    post(AppEvent::ConsoleInfo {
        url: url.to_string(),
    });
}

/// Push the current branding to the window (title, page, tray, dock).
pub fn refresh_branding() {
    post(AppEvent::Branding(crate::branding::page_json()));
}

/// Update the device name / id shown on the status screen.
pub fn set_device_info(name: &str, id: &str) {
    post(AppEvent::DeviceInfo {
        name: name.to_string(),
        id: id.to_string(),
    });
}

fn current_callbacks() -> (Option<SendCb>, Option<DisconnectCb>) {
    let g = CALLBACKS.lock();
    match g.as_ref() {
        Some(c) => (
            Some(Arc::clone(&c.on_send)),
            Some(Arc::clone(&c.on_disconnect)),
        ),
        None => (None, None),
    }
}

fn dispatch_send(text: String) {
    if let (Some(on_send), _) = current_callbacks() {
        on_send(text);
    }
}

fn dispatch_disconnect() {
    let (_, on_disc) = current_callbacks();
    if let Some(cb) = on_disc {
        cb();
    } else if let Some(cb) = GLOBAL_DISCONNECT.lock().as_ref() {
        cb();
    }
}

// ── Indicator implementation ──────────────────────────────────────────────────────────────

/// [`Indicator`] backed by the branded session bar window. The bar is shown/hidden by the
/// session lifecycle events the app already receives (`SessionStarted` / `SessionEnded`), so
/// the handle only marks the intent; the bar's own End button routes through the session
/// callbacks like the app window does.
#[derive(Debug, Default, Clone, Copy)]
pub struct AppIndicator;

struct AppIndicatorHandle;
impl IndicatorHandle for AppIndicatorHandle {}

impl Indicator for AppIndicator {
    fn show(
        &self,
        _operator: &OperatorInfo,
        _on_disconnect: DisconnectCb,
    ) -> Result<Box<dyn IndicatorHandle>> {
        Ok(Box::new(AppIndicatorHandle))
    }
}

// ── ChatUi implementation ─────────────────────────────────────────────────────────────────

/// [`ChatUi`] backed by the app window. Opening a session swaps in its callbacks and tells the
/// window; dropping the handle detaches the session (the window stays, showing "Session ended").
#[derive(Debug, Default, Clone, Copy)]
pub struct AppChatUi;

impl ChatUi for AppChatUi {
    fn open(
        &self,
        operator: &OperatorInfo,
        on_send: SendCb,
        on_disconnect: DisconnectCb,
    ) -> Result<Box<dyn ChatHandle>> {
        *CALLBACKS.lock() = Some(Callbacks {
            on_send,
            on_disconnect,
        });
        post(AppEvent::SessionStarted {
            operator: operator.name.clone(),
        });
        Ok(Box::new(AppChatHandle {
            unread: Arc::new(AtomicUsize::new(0)),
        }))
    }
}

struct AppChatHandle {
    unread: Arc<AtomicUsize>,
}

impl ChatHandle for AppChatHandle {
    fn push_line(&self, line: &ChatLine) {
        if matches!(line.from, ChatParty::Operator) {
            self.unread.fetch_add(1, Ordering::SeqCst);
        }
        post(AppEvent::Chat {
            from: line.from,
            text: line.text.clone(),
            ts_ms: line.ts_ms,
        });
    }

    fn set_visible(&self, visible: bool) {
        if visible {
            post(AppEvent::Show { chat: false });
        }
        // We never hide the window from under the local user mid-session.
    }
}

impl Drop for AppChatHandle {
    fn drop(&mut self) {
        *CALLBACKS.lock() = None;
        post(AppEvent::SessionEnded);
    }
}

/// One or two initials for the branding logo fallback / operator avatar.
fn key_fingerprint(pubkey_b64: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(pubkey_b64.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    hex.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Branding JSON handed to the page (`window.__app.setBranding`).
fn branding_json() -> String {
    crate::branding::page_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_fingerprint_is_grouped_hex() {
        // 8 bytes → 16 hex chars grouped in 4s → 4 groups.
        let fp = key_fingerprint("AAAA");
        assert_eq!(fp.split(' ').count(), 4);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit() || c == ' '));
    }

    #[test]
    fn branding_json_has_defaults_without_trailer() {
        let j: serde_json::Value = serde_json::from_str(&branding_json()).unwrap();
        assert!(j["product_name"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false));
        assert!(j["accent"]
            .as_str()
            .map(|s| s.starts_with('#'))
            .unwrap_or(false));
    }
}
