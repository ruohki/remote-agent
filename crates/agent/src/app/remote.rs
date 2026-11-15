//! The app UI in another process.
//!
//! On Windows the session host runs as SYSTEM so it can reach the sign-in screen, the lock
//! screen and UAC prompts. A SYSTEM process must not show a WebView to the user (it would be a
//! privileged surface the user can drive — links, dialogs), so the tray, window, chat, session
//! bar, approval prompt and privacy screen live in a `ui` process started with the signed-in
//! user's token. This module bridges the two over the UI link ([`crate::link`]):
//!
//! * **host side** ([`serve_host`]): [`crate::app::post`] and friends forward [`UiCommand`]s to
//!   the connected UI process; its [`UiEvent`]s are dispatched to the same callbacks the
//!   in-process app would have called. Nothing connected → approval prompts are denied and the
//!   privacy screen is unavailable, exactly like a headless agent.
//! * **UI side** ([`run_ui_client`]): runs inside the normal app loop; replays commands as
//!   [`AppEvent`]s and sends the user's actions back. When the host goes away (logoff, update,
//!   crash) the UI removes any privacy screen at once and reconnects.

use super::AppEvent;
use crate::annotate::AnnotateEvent;
use crate::approval::{ApprovalOutcome, Approver};
use crate::link::endpoint::{self, Access};
use crate::link::frame::{read_message, spawn_writer};
use crate::privacy::PrivacyScreenInfo;
use anyhow::Result;
use parking_lot::Mutex;
use protocol::channel::ChatParty;
use protocol::common::{DisplayInfo, OperatorInfo};
use protocol::config::LocalOverrides;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Host → UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiCommand {
    SessionStarted {
        operator: String,
    },
    SessionEnded,
    Chat {
        from: ChatParty,
        text: String,
        ts_ms: u64,
    },
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
    ConsoleInfo {
        url: String,
    },
    Policy {
        json: String,
    },
    /// Branding changed: re-read it from the machine state directory.
    Branding,
    PermissionNeeded,
    ControlPaused {
        paused: bool,
    },
    Annotate {
        event: AnnotateEvent,
    },
    AnnotationsEnded,
    PrivacyEngage {
        id: u64,
        info: PrivacyScreenInfo,
        displays: Vec<DisplayInfo>,
    },
    PrivacyDisengage {
        id: u64,
    },
    PrivacyHeartbeat,
    Approve {
        id: u64,
        operator: String,
        timeout_ms: u64,
    },
    /// The user signed out or the agent stops: close the UI.
    Quit,
}

/// UI → host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiEvent {
    ChatSend { text: String },
    Disconnect,
    Pause { paused: bool },
    PrivacyReveal,
    SetOverrides { overrides: LocalOverrides },
    PrivacyEngaged { id: u64, result: Result<(), String> },
    PrivacyDisengaged { id: u64 },
    Approval { id: u64, outcome: Outcome },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Approved,
    Denied,
    TimedOut,
}

impl From<ApprovalOutcome> for Outcome {
    fn from(o: ApprovalOutcome) -> Self {
        match o {
            ApprovalOutcome::Approved => Outcome::Approved,
            ApprovalOutcome::Denied => Outcome::Denied,
            ApprovalOutcome::TimedOut => Outcome::TimedOut,
        }
    }
}

impl From<Outcome> for ApprovalOutcome {
    fn from(o: Outcome) -> Self {
        match o {
            Outcome::Approved => ApprovalOutcome::Approved,
            Outcome::Denied => ApprovalOutcome::Denied,
            Outcome::TimedOut => ApprovalOutcome::TimedOut,
        }
    }
}

// ─── host side ───────────────────────────────────────────────────────────────────────────

/// This process is a session host whose app UI lives in a `ui` process.
static HOST_MODE: AtomicBool = AtomicBool::new(false);
/// Sender to the connected UI process, if any.
static UI: Mutex<Option<mpsc::UnboundedSender<UiCommand>>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
type EngageConfirm = crate::privacy::Confirm<Result<(), String>>;
/// Privacy confirmations waiting for the UI process.
static ENGAGES: Mutex<Option<HashMap<u64, EngageConfirm>>> = Mutex::new(None);
static DISENGAGES: Mutex<Option<HashMap<u64, crate::privacy::Confirm<()>>>> = Mutex::new(None);
static APPROVALS: Mutex<Option<HashMap<u64, oneshot::Sender<ApprovalOutcome>>>> = Mutex::new(None);
/// Overrides changed in the UI; set by the session host (sent on to the service).
type OverridesSink = std::sync::Arc<dyn Fn(LocalOverrides) + Send + Sync>;
static OVERRIDES_SINK: Mutex<Option<OverridesSink>> = Mutex::new(None);
/// The last state a fresh UI process needs (it may start mid-session or after a restart).
static REPLAY: Mutex<Vec<UiCommand>> = Mutex::new(Vec::new());

/// Whether this process forwards its app UI to a `ui` process.
pub fn is_host() -> bool {
    HOST_MODE.load(Ordering::SeqCst)
}

/// Whether a UI process is connected right now.
pub fn ui_connected() -> bool {
    UI.lock().is_some()
}

pub fn set_overrides_sink(sink: OverridesSink) {
    *OVERRIDES_SINK.lock() = Some(sink);
}

fn send(cmd: UiCommand) -> bool {
    match UI.lock().as_ref() {
        Some(tx) => tx.send(cmd).is_ok(),
        None => false,
    }
}

/// Remember the latest value of state-like commands so a UI that connects later shows it.
fn remember(cmd: &UiCommand) {
    let key = |c: &UiCommand| std::mem::discriminant(c);
    let keep = matches!(
        cmd,
        UiCommand::ConsoleStatus { .. }
            | UiCommand::DeviceInfo { .. }
            | UiCommand::ConsoleInfo { .. }
            | UiCommand::Policy { .. }
            | UiCommand::ControlPaused { .. }
            | UiCommand::SessionStarted { .. }
    );
    let mut replay = REPLAY.lock();
    if matches!(cmd, UiCommand::SessionEnded) {
        replay.retain(|c| {
            !matches!(
                c,
                UiCommand::SessionStarted { .. } | UiCommand::ControlPaused { .. }
            )
        });
        return;
    }
    if keep {
        replay.retain(|c| key(c) != key(cmd));
        replay.push(cmd.clone());
    }
}

/// Forward an [`AppEvent`] posted in the host to the UI process (called by `app::post`).
pub(super) fn forward(ev: AppEvent) {
    let cmd = match ev {
        AppEvent::SessionStarted { operator } => UiCommand::SessionStarted { operator },
        AppEvent::SessionEnded => UiCommand::SessionEnded,
        AppEvent::Chat { from, text, ts_ms } => UiCommand::Chat { from, text, ts_ms },
        AppEvent::Show { chat } => UiCommand::Show { chat },
        AppEvent::Hide => UiCommand::Hide,
        AppEvent::ConsoleStatus { connected } => UiCommand::ConsoleStatus { connected },
        AppEvent::DeviceInfo { name, id } => UiCommand::DeviceInfo { name, id },
        AppEvent::ConsoleInfo { url } => UiCommand::ConsoleInfo { url },
        AppEvent::Policy(json) => UiCommand::Policy { json },
        AppEvent::Branding(_) => UiCommand::Branding,
        AppEvent::PermissionNeeded => UiCommand::PermissionNeeded,
        AppEvent::ControlPaused { paused } => UiCommand::ControlPaused { paused },
        AppEvent::Annotate(event) => UiCommand::Annotate { event },
        AppEvent::AnnotationsEnded => UiCommand::AnnotationsEnded,
        AppEvent::PrivacyHeartbeat => UiCommand::PrivacyHeartbeat,
        AppEvent::PrivacyEngage {
            info,
            displays,
            confirm,
        } => {
            let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
            ENGAGES
                .lock()
                .get_or_insert_with(HashMap::new)
                .insert(id, confirm.clone());
            if !send(UiCommand::PrivacyEngage { id, info, displays }) {
                ENGAGES.lock().as_mut().map(|m| m.remove(&id));
                crate::privacy::confirm(&confirm, Err("no UI process".into()));
            }
            return;
        }
        AppEvent::PrivacyDisengage { confirm } => {
            let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
            DISENGAGES
                .lock()
                .get_or_insert_with(HashMap::new)
                .insert(id, confirm.clone());
            if !send(UiCommand::PrivacyDisengage { id }) {
                // No UI process means no privacy windows to remove.
                DISENGAGES.lock().as_mut().map(|m| m.remove(&id));
                crate::privacy::confirm(&confirm, ());
            }
            return;
        }
        AppEvent::Quit => UiCommand::Quit,
        // UI-internal events never originate in the host.
        other => {
            tracing::trace!(?other, "app event not forwarded to the UI process");
            return;
        }
    };
    remember(&cmd);
    send(cmd);
}

/// Approval prompts answered by the signed-in user's UI process.
#[derive(Debug, Default, Clone, Copy)]
pub struct RemoteApprover;

#[async_trait::async_trait]
impl Approver for RemoteApprover {
    async fn ask(&self, operator: &OperatorInfo, timeout: Duration) -> Result<ApprovalOutcome> {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        APPROVALS
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert(id, tx);
        let asked = send(UiCommand::Approve {
            id,
            operator: operator.name.clone(),
            timeout_ms: timeout.as_millis() as u64,
        });
        if !asked {
            APPROVALS.lock().as_mut().map(|m| m.remove(&id));
            tracing::info!("help-me request with nobody signed in to approve it; denied");
            return Ok(ApprovalOutcome::Denied);
        }
        // The UI enforces the timeout; allow it a moment beyond that to report it.
        match tokio::time::timeout(timeout + Duration::from_secs(5), rx).await {
            Ok(Ok(outcome)) => Ok(outcome),
            _ => {
                APPROVALS.lock().as_mut().map(|m| m.remove(&id));
                Ok(ApprovalOutcome::TimedOut)
            }
        }
    }
}

fn on_ui_event(ev: UiEvent) {
    match ev {
        UiEvent::ChatSend { text } => super::dispatch_send(text),
        UiEvent::Disconnect => super::dispatch_disconnect(),
        UiEvent::Pause { paused } => super::dispatch_pause(paused),
        UiEvent::PrivacyReveal => super::dispatch_privacy_reveal(),
        UiEvent::SetOverrides { overrides } => {
            let sink = OVERRIDES_SINK.lock().clone();
            match sink {
                Some(s) => s(overrides),
                None => tracing::warn!("local restrictions changed but nobody applies them"),
            }
        }
        UiEvent::PrivacyEngaged { id, result } => {
            if let Some(c) = ENGAGES.lock().as_mut().and_then(|m| m.remove(&id)) {
                crate::privacy::confirm(&c, result);
            }
        }
        UiEvent::PrivacyDisengaged { id } => {
            if let Some(c) = DISENGAGES.lock().as_mut().and_then(|m| m.remove(&id)) {
                crate::privacy::confirm(&c, ());
            }
        }
        UiEvent::Approval { id, outcome } => {
            if let Some(tx) = APPROVALS.lock().as_mut().and_then(|m| m.remove(&id)) {
                let _ = tx.send(outcome.into());
            }
        }
    }
}

/// The UI process went away: fail what was waiting on it.
fn ui_gone() {
    *UI.lock() = None;
    for (_, c) in ENGAGES.lock().take().unwrap_or_default() {
        crate::privacy::confirm(&c, Err("UI process went away".into()));
    }
    // Its windows went with it.
    for (_, c) in DISENGAGES.lock().take().unwrap_or_default() {
        crate::privacy::confirm(&c, ());
    }
    for (_, tx) in APPROVALS.lock().take().unwrap_or_default() {
        let _ = tx.send(ApprovalOutcome::Denied);
    }
}

/// Serve the UI link for Windows session `session` until the process exits. One UI process
/// at a time; a new connection replaces the previous one.
pub async fn serve_host(session: u32) -> Result<()> {
    HOST_MODE.store(true, Ordering::SeqCst);
    let name = endpoint::ui_endpoint(session);
    let mut listener = endpoint::Listener::bind(&name, Access::InteractiveUsers).await?;
    tracing::info!(%name, "UI link listening");
    loop {
        let (stream, peer) = listener.accept().await?;
        if let Err(e) = endpoint::check_peer(&peer) {
            tracing::warn!("{e:#}");
            continue;
        }
        if cfg!(windows) && peer.session.is_some_and(|s| s != session) {
            tracing::warn!(peer = ?peer.session, "UI process from another session refused");
            continue;
        }
        tracing::info!(pid = ?peer.pid, "UI process connected");
        let (read, write) = tokio::io::split(stream);
        let (tx, writer) = spawn_writer::<_, UiCommand>(write);
        // Bring the newcomer up to date before anything else.
        for cmd in REPLAY.lock().iter() {
            let _ = tx.send(cmd.clone());
        }
        let _ = tx.send(UiCommand::Branding);
        if let Some(old) = UI.lock().replace(tx) {
            let _ = old.send(UiCommand::Quit);
        }
        tokio::spawn(async move {
            let mut read = read;
            loop {
                match read_message::<_, UiEvent>(&mut read).await {
                    Ok(Some(ev)) => on_ui_event(ev),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("UI link: {e:#}");
                        break;
                    }
                }
            }
            tracing::info!("UI process disconnected");
            ui_gone();
            writer.abort();
        });
    }
}

// ─── UI side ─────────────────────────────────────────────────────────────────────────────

/// Last privacy heartbeat seen by the UI process (ms since start), 0 = none engaged.
static UI_PRIVACY_BEAT: AtomicU64 = AtomicU64::new(0);

/// Run the UI end of the link inside the app loop's worker thread: connect to the session
/// host of this Windows session, replay its commands, send the user's actions back. Returns
/// when the host tells the UI to quit or the process shuts down.
pub fn run_ui_client(session: u32) -> i32 {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("UI runtime: {e}");
            return 1;
        }
    };
    rt.block_on(async move {
        crate::shutdown::install_handlers();
        let started = std::time::Instant::now();
        let now_ms = move || started.elapsed().as_millis() as u64 + 1;
        // Privacy watchdog: without heartbeats from the host for this long, take the screen
        // down ourselves (the host-side guard cannot reach our windows if it is wedged).
        std::thread::Builder::new()
            .name("privacy-ui-watchdog".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(1));
                let beat = UI_PRIVACY_BEAT.load(Ordering::SeqCst);
                if beat != 0 && now_ms().saturating_sub(beat) > 10_000 {
                    tracing::warn!("privacy screen without heartbeats from the host; removing it");
                    UI_PRIVACY_BEAT.store(0, Ordering::SeqCst);
                    super::post(AppEvent::PrivacyDisengage {
                        confirm: std::sync::Arc::new(Mutex::new(None)),
                    });
                }
            })
            .ok();
        let name = endpoint::ui_endpoint(session);
        loop {
            let stream = tokio::select! {
                r = endpoint::connect(&name) => r,
                _ = crate::shutdown::wait() => return 0,
            };
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!("UI link: {e:#}");
                    super::set_console_status(false);
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                        _ = crate::shutdown::wait() => return 0,
                    }
                }
            };
            tracing::info!("connected to the session host");
            let (mut read, write) = tokio::io::split(stream);
            let (tx, _writer) = spawn_writer::<_, UiEvent>(write);
            wire_ui_callbacks(tx.clone());
            let quit = loop {
                let msg = tokio::select! {
                    m = read_message::<_, UiCommand>(&mut read) => m,
                    _ = crate::shutdown::wait() => break true,
                };
                match msg {
                    Ok(Some(UiCommand::Quit)) => break true,
                    Ok(Some(cmd)) => apply_command(cmd, &tx, now_ms()),
                    Ok(None) => break false,
                    Err(e) => {
                        tracing::warn!("UI link: {e:#}");
                        break false;
                    }
                }
            };
            // Whatever the host had us show is no longer backed by a session.
            UI_PRIVACY_BEAT.store(0, Ordering::SeqCst);
            super::post(AppEvent::PrivacyDisengage {
                confirm: std::sync::Arc::new(Mutex::new(None)),
            });
            super::post(AppEvent::AnnotationsEnded);
            super::post(AppEvent::SessionEnded);
            super::set_console_status(false);
            if quit {
                super::post(AppEvent::Quit);
                return 0;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

/// Route the app's user actions to the host.
fn wire_ui_callbacks(tx: mpsc::UnboundedSender<UiEvent>) {
    let t = tx.clone();
    super::set_global_disconnect(std::sync::Arc::new(move || {
        let _ = t.send(UiEvent::Disconnect);
    }));
    let t = tx.clone();
    super::set_pause_handler(std::sync::Arc::new(move |paused| {
        let _ = t.send(UiEvent::Pause { paused });
    }));
    let t = tx.clone();
    super::set_privacy_reveal_handler(std::sync::Arc::new(move || {
        let _ = t.send(UiEvent::PrivacyReveal);
    }));
    let t = tx;
    super::set_overrides_handler(std::sync::Arc::new(move |overrides| {
        let _ = t.send(UiEvent::SetOverrides { overrides });
    }));
}

fn apply_command(cmd: UiCommand, tx: &mpsc::UnboundedSender<UiEvent>, now_ms: u64) {
    match cmd {
        UiCommand::SessionStarted { operator } => {
            // Chat lines typed here and the window's Disconnect go to the host's session.
            let send_tx = tx.clone();
            let disc_tx = tx.clone();
            *super::CALLBACKS.lock() = Some(super::Callbacks {
                on_send: std::sync::Arc::new(move |text| {
                    let _ = send_tx.send(UiEvent::ChatSend { text });
                }),
                on_disconnect: std::sync::Arc::new(move || {
                    let _ = disc_tx.send(UiEvent::Disconnect);
                }),
            });
            super::post(AppEvent::SessionStarted { operator });
        }
        UiCommand::SessionEnded => {
            *super::CALLBACKS.lock() = None;
            super::post(AppEvent::SessionEnded);
        }
        UiCommand::Chat { from, text, ts_ms } => super::post(AppEvent::Chat { from, text, ts_ms }),
        UiCommand::Show { chat } => super::post(AppEvent::Show { chat }),
        UiCommand::Hide => super::post(AppEvent::Hide),
        UiCommand::ConsoleStatus { connected } => super::set_console_status(connected),
        UiCommand::DeviceInfo { name, id } => super::set_device_info(&name, &id),
        UiCommand::ConsoleInfo { url } => super::set_console_url(&url),
        UiCommand::Policy { json } => super::set_policy(&json),
        UiCommand::Branding => {
            crate::branding::reload();
            super::refresh_branding();
        }
        UiCommand::PermissionNeeded => super::permission_needed(),
        UiCommand::ControlPaused { paused } => super::set_control_paused_state(paused),
        UiCommand::Annotate { event } => super::post(AppEvent::Annotate(event)),
        UiCommand::AnnotationsEnded => super::post(AppEvent::AnnotationsEnded),
        UiCommand::PrivacyHeartbeat => {
            if UI_PRIVACY_BEAT.load(Ordering::SeqCst) != 0 {
                UI_PRIVACY_BEAT.store(now_ms, Ordering::SeqCst);
            }
            super::privacy_heartbeat();
        }
        UiCommand::PrivacyEngage { id, info, displays } => {
            let (confirm, rx) = crate::privacy::confirm_channel::<Result<(), String>>();
            UI_PRIVACY_BEAT.store(now_ms, Ordering::SeqCst);
            super::engage_privacy(info, displays, confirm);
            let tx = tx.clone();
            tokio::spawn(async move {
                let result = rx
                    .await
                    .unwrap_or_else(|_| Err("privacy screen was not confirmed".into()));
                if result.is_err() {
                    UI_PRIVACY_BEAT.store(0, Ordering::SeqCst);
                }
                let _ = tx.send(UiEvent::PrivacyEngaged { id, result });
            });
        }
        UiCommand::PrivacyDisengage { id } => {
            let (confirm, rx) = crate::privacy::confirm_channel::<()>();
            UI_PRIVACY_BEAT.store(0, Ordering::SeqCst);
            super::disengage_privacy(confirm);
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = rx.await;
                let _ = tx.send(UiEvent::PrivacyDisengaged { id });
            });
        }
        UiCommand::Approve {
            id,
            operator,
            timeout_ms,
        } => {
            let tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                let outcome =
                    crate::platform::approval_dialog(&operator, Duration::from_millis(timeout_ms))
                        .unwrap_or(ApprovalOutcome::Denied);
                let _ = tx.send(UiEvent::Approval {
                    id,
                    outcome: outcome.into(),
                });
            });
        }
        UiCommand::Quit => super::post(AppEvent::Quit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_and_events_roundtrip() {
        let cmd = UiCommand::PrivacyEngage {
            id: 3,
            info: PrivacyScreenInfo {
                operator: "Alice".into(),
                device: "PC-1".into(),
                started_ms: 1,
                backdrops: Default::default(),
            },
            displays: vec![],
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""type":"privacy_engage""#), "{json}");
        let back: UiCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, UiCommand::PrivacyEngage { id: 3, .. }));

        let ev = UiEvent::Approval {
            id: 9,
            outcome: Outcome::TimedOut,
        };
        let back: UiEvent = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert!(matches!(
            back,
            UiEvent::Approval {
                id: 9,
                outcome: Outcome::TimedOut
            }
        ));
    }

    #[test]
    fn replay_keeps_latest_state_and_drops_ended_sessions() {
        REPLAY.lock().clear();
        remember(&UiCommand::ConsoleStatus { connected: false });
        remember(&UiCommand::ConsoleStatus { connected: true });
        remember(&UiCommand::SessionStarted {
            operator: "Bob".into(),
        });
        assert_eq!(REPLAY.lock().len(), 2);
        assert!(REPLAY
            .lock()
            .iter()
            .any(|c| matches!(c, UiCommand::ConsoleStatus { connected: true })));
        remember(&UiCommand::SessionEnded);
        assert!(!REPLAY
            .lock()
            .iter()
            .any(|c| matches!(c, UiCommand::SessionStarted { .. })));
        REPLAY.lock().clear();
    }

    #[tokio::test]
    async fn approval_without_ui_is_denied() {
        let outcome = RemoteApprover
            .ask(
                &OperatorInfo {
                    id: "u".into(),
                    name: "Op".into(),
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(outcome, ApprovalOutcome::Denied);
    }
}
