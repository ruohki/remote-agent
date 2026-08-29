//! Persistent connection to the management console (`/ws/agent`).
//!
//! [`run_agent`] connects with exponential backoff, performs the hello/hello_ack handshake,
//! then multiplexes: heartbeats, ping/pong, config updates, and session signaling forwarded
//! to a [`SessionManager`]. [`HubSink`] is the handle the session code uses to push messages
//! back to the console (it survives reconnects: messages are queued to the current socket).

use crate::approval::{
    ApprovalOutcome, Approver, AutoApprover, Indicator, NativeApprover, NativeIndicator,
    NoIndicator,
};
use crate::chat::{ChatUi, NativeChatUi, NoChatUi};
use crate::config::{LocalConfig, Paths};
use crate::input::{Injector, InputHandler};
use crate::session::media::{MediaFactory, SystemMedia};
use crate::session::{SessionDeps, SessionManager, SessionRequest};
use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use protocol::agent::{AgentCapabilities, AgentToConsole, ConsoleToAgent};
use protocol::common::{DisplayInfo, EndReason};
use protocol::config::{AgentConfig, LocalOverrides};
use protocol::PROTOCOL_VERSION;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// The console no longer accepts this device's identity (deleted device, bad credentials) or
/// the user asked to enroll again. `run_agent` returns this in app mode so the process can drop
/// the identity and show the Connect screen instead of reconnecting forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reenroll {
    pub reason: String,
}

impl Reenroll {
    /// What the Connect screen shows.
    pub fn notice(&self) -> String {
        match self.reason.as_str() {
            "requested" => "Enroll this device again.".into(),
            "device deleted" | "unknown device" => {
                "This device was removed from the console. Enroll it again.".into()
            }
            "bad credentials" => {
                "The console no longer accepts this device. Enroll it again.".into()
            }
            other => format!("Enroll this device again ({other})."),
        }
    }
}

impl std::fmt::Display for Reenroll {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "re-enrollment needed: {}", self.reason)
    }
}

impl std::error::Error for Reenroll {}

/// A process shutdown was requested (signal, Quit, update): the connection was closed cleanly
/// and `run_agent` must not reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShuttingDown;

impl std::fmt::Display for ShuttingDown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shutting down")
    }
}

impl std::error::Error for ShuttingDown {}

/// Close codes `/ws/agent` uses when the device identity is no longer valid
/// (4401 bad credentials, 4409 device deleted).
fn close_code_means_reenroll(code: u16) -> bool {
    matches!(code, 4401 | 4409)
}

/// A `goodbye` whose reason says the device is gone (as opposed to "replaced by a newer
/// connection" or a console restart).
fn goodbye_means_reenroll(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    r.contains("deleted") || r.contains("unknown device") || r.contains("bad credentials")
}

/// Handle for sending messages to the console. Cloneable and reconnect-safe.
#[derive(Clone)]
pub struct HubSink {
    tx: mpsc::UnboundedSender<AgentToConsole>,
}

impl HubSink {
    pub fn send(&self, msg: AgentToConsole) {
        if self.tx.send(msg).is_err() {
            tracing::debug!("hub sink closed; message dropped");
        }
    }

    /// Build a sink plus the receiving end of its channel. Used by the run loop to route
    /// session output to the live socket, and by tests to observe what the agent emits.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<AgentToConsole>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

/// Shared runtime state driven by the hub and read by heartbeats/sessions.
struct AgentState {
    /// Effective config sessions run with = `overrides.apply(console_config)`.
    config: Arc<RwLock<AgentConfig>>,
    /// The console's policy, before local restrictions.
    console_config: RwLock<AgentConfig>,
    /// Local restrictions set by the person at the device.
    overrides: RwLock<LocalOverrides>,
    /// Last overrides reported in a heartbeat (to send only on change).
    last_overrides: RwLock<LocalOverrides>,
    media: Arc<SystemMedia>,
    start: Instant,
    last_displays: RwLock<Vec<DisplayInfo>>,
}

impl AgentState {
    /// Recompute the effective config from the console policy and local overrides, store it, and
    /// return it.
    fn recompute(&self) -> AgentConfig {
        let eff = self
            .overrides
            .read()
            .apply(self.console_config.read().clone());
        *self.config.write() = eff.clone();
        eff
    }

    /// JSON policy blob for the app Settings screen: console policy, local overrides, effective.
    fn policy_json(&self) -> String {
        let console = self.console_config.read().clone();
        let overrides = self.overrides.read().clone();
        let effective = self.config.read().clone();
        serde_json::json!({
            "console": {
                "mode": console.mode,
                "allow_input": console.allow_input,
                "allow_audio": console.allow_audio,
                "allow_clipboard": console.allow_clipboard,
                "allow_file_transfer": console.allow_file_transfer,
                "allow_annotations": console.allow_annotations,
            },
            "overrides": overrides,
            "effective": {
                "mode": effective.mode,
                "allow_input": effective.allow_input,
                "allow_audio": effective.allow_audio,
                "allow_clipboard": effective.allow_clipboard,
                "allow_file_transfer": effective.allow_file_transfer,
                "allow_annotations": effective.allow_annotations,
            },
        })
        .to_string()
    }
}

pub async fn run_agent(paths: Paths) -> Result<()> {
    let mut local = LocalConfig::load_required(&paths)?;
    crate::transport::check_console_url(&local.server_url)?;
    crate::transport::set_console_pin(local.console_tls_spki_sha256.as_deref(), &local.server_url)
        .context("console TLS pin in agent.toml")?;
    // Resolve the device secret (keychain / DPAPI / file) into this in-memory copy only.
    let backend = crate::secrets::migrate_if_needed(&paths, &mut local)?;
    let (secret, _) = crate::secrets::load(&paths, &local)?;
    local.device_secret = secret;
    tracing::info!(
        secret_backend = backend.as_str(),
        tls_pin = crate::transport::console_pin_active(),
        "console credentials loaded"
    );
    let console_config = local.console_config();
    let overrides = local.overrides.clone();
    let config = Arc::new(RwLock::new(overrides.apply(console_config.clone())));
    tracing::info!(
        server = %local.server_url,
        device = %local.device_id,
        version = crate::AGENT_VERSION,
        mode = ?config.read().mode,
        overrides = ?overrides,
        "starting agent"
    );

    let media = Arc::new(SystemMedia);
    let state = Arc::new(AgentState {
        config: Arc::clone(&config),
        console_config: RwLock::new(console_config),
        overrides: RwLock::new(overrides.clone()),
        last_overrides: RwLock::new(overrides),
        media,
        start: Instant::now(),
        last_displays: RwLock::new(Vec::new()),
    });

    // Session sink: messages flow to whichever socket is currently connected.
    let (hub_sink, mut out_rx) = HubSink::channel();

    // Choose interactive vs headless helpers depending on whether the UI loop is available.
    let interactive = crate::platform::main_loop_running();
    let approver: Arc<dyn Approver> = if interactive {
        Arc::new(NativeApprover)
    } else {
        // Without a GUI loop we cannot prompt; deny help-me sessions rather than allow silently.
        tracing::warn!("no UI loop: help-me approval prompts will be denied");
        Arc::new(AutoApprover(ApprovalOutcome::Denied))
    };
    let indicator: Arc<dyn Indicator> = if crate::app::is_running() {
        // The branded session bar window replaces the native panel.
        Arc::new(crate::app::AppIndicator)
    } else if interactive {
        Arc::new(NativeIndicator)
    } else {
        Arc::new(NoIndicator)
    };
    let chat: Arc<dyn ChatUi> = if crate::app::is_running() {
        // The branded application window hosts chat, status and controls.
        Arc::new(crate::app::AppChatUi)
    } else if interactive {
        Arc::new(NativeChatUi)
    } else {
        Arc::new(NoChatUi)
    };
    let input_factory: crate::session::InputFactory =
        Arc::new(|| Ok(Box::new(Injector::new()?) as Box<dyn InputHandler>));

    let deps = SessionDeps {
        media: state.media.clone(),
        input: input_factory,
        approver,
        indicator,
        chat,
        clipboard: Arc::new(crate::clipboard::SystemClipboard),
        hub: hub_sink.clone(),
        config: Arc::clone(&config),
        annotations: if crate::app::is_running() {
            Arc::new(crate::app::AppAnnotations)
        } else {
            Arc::new(crate::annotate::NoAnnotations)
        },
    };
    let sessions = SessionManager::new(deps);

    // Remote input must never operate our own windows.
    if interactive {
        crate::platform::install_input_guard();
    }
    // Wire the app window / tray to this device: the "End session" action, plus device identity
    // on the status screen. Console connection status is posted from `connect_once`.
    if crate::app::is_running() {
        let sessions_for_menu = Arc::clone(&sessions);
        crate::app::set_global_disconnect(Arc::new(move || {
            sessions_for_menu.end_all(EndReason::DeviceUserClosed)
        }));
        let sessions_for_pause = Arc::clone(&sessions);
        crate::app::set_pause_handler(Arc::new(move |paused| {
            sessions_for_pause.set_control_paused(paused)
        }));
        crate::app::set_device_info(&config.read().display_name, &local.device_id);
        crate::app::set_console_url(&local.server_url);
    } else if interactive {
        // Fallback menu-bar item when the app window is not available.
        let sessions_for_menu = Arc::clone(&sessions);
        let status = format!("Remote support — connected to {}", local.server_url);
        crate::platform::install_menu_bar(
            &status,
            Arc::new(move || sessions_for_menu.end_all(EndReason::DeviceUserClosed)),
        );
    }

    // The app Settings screen changes the local restrictions: persist them, recompute the
    // effective config, apply it live to any running session, and refresh the policy shown.
    if crate::app::is_running() {
        let ov_state = Arc::clone(&state);
        let ov_sessions = Arc::clone(&sessions);
        let ov_paths = paths.clone();
        crate::app::set_overrides_handler(Arc::new(move |ov: LocalOverrides| {
            // Persist onto the latest on-disk config so we don't clobber a concurrent update.
            let mut lc = LocalConfig::load(&ov_paths)
                .ok()
                .flatten()
                .unwrap_or_default();
            lc.overrides = ov.clone();
            if let Err(e) = lc.save(&ov_paths) {
                tracing::warn!("persisting overrides: {e:#}");
            }
            *ov_state.overrides.write() = ov;
            let eff = ov_state.recompute();
            ov_sessions.apply_overrides(eff);
            crate::app::set_policy(&ov_state.policy_json());
        }));
        crate::app::set_policy(&state.policy_json());
    }

    // Runtime branding: fetched from the console (public endpoint) on start, on demand and
    // every 10 minutes; cached in the config dir so restarts are branded immediately.
    let branding_task = tokio::spawn(crate::branding::refresh_loop(
        local.server_url.clone(),
        paths.clone(),
    ));

    // Buffer of messages waiting for a live socket (bounded so we don't grow unbounded while
    // offline; signaling is time-sensitive so old entries are dropped).
    let pending: Arc<parking_lot::Mutex<std::collections::VecDeque<AgentToConsole>>> =
        Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));

    let mut backoff = Duration::from_secs(1);
    loop {
        let app = crate::app::is_running();
        let outcome = tokio::select! {
            r = connect_once(&paths, &local, &state, &sessions, &mut out_rx, &pending) => r,
            // Settings → "Enroll again" (only meaningful with the app window).
            _ = crate::app::reenroll_requested(), if app => Err(Reenroll { reason: "requested".into() }.into()),
        };
        match outcome {
            Ok(()) => {
                backoff = Duration::from_secs(1);
                tracing::info!("console closed the connection; reconnecting");
            }
            // In app mode hand the identity problem to the Connect screen; headless keeps
            // retrying with backoff as before (the service manager owns the process).
            Err(e) if app && e.is::<Reenroll>() => {
                sessions.end_all(EndReason::DeviceUserClosed);
                crate::app::set_console_status(false);
                branding_task.abort();
                return Err(e);
            }
            // Stop requested: `connect_once` already ended the session and closed the socket
            // (or never got to connect). Do not reconnect.
            Err(e) if e.is::<ShuttingDown>() => {
                sessions.end_all(EndReason::AgentOffline);
                sessions.wait_idle(crate::shutdown::SESSION_END_GRACE).await;
                crate::app::set_console_status(false);
                branding_task.abort();
                tracing::info!(reason = ?crate::shutdown::reason(), "agent stopped");
                return Ok(());
            }
            Err(e) => {
                tracing::warn!("hub connection error: {e:#}");
            }
        }
        // End any active session on disconnect (its ICE relay is gone).
        sessions.end_all(EndReason::AgentOffline);
        crate::app::set_console_status(false);
        let jitter = Duration::from_millis(rand::random::<u64>() % 500);
        tokio::select! {
            _ = tokio::time::sleep(backoff + jitter) => {}
            _ = crate::shutdown::wait() => {
                sessions.wait_idle(crate::shutdown::SESSION_END_GRACE).await;
                branding_task.abort();
                tracing::info!(reason = ?crate::shutdown::reason(), "agent stopped while offline");
                return Ok(());
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// One connection lifecycle. Returns `Ok` on a clean close, `Err` on failure.
async fn connect_once(
    paths: &Paths,
    local: &LocalConfig,
    state: &Arc<AgentState>,
    sessions: &Arc<SessionManager>,
    out_rx: &mut mpsc::UnboundedReceiver<AgentToConsole>,
    pending: &Arc<parking_lot::Mutex<std::collections::VecDeque<AgentToConsole>>>,
) -> Result<()> {
    let ws_url = ws_url(&local.server_url)?;
    tracing::info!(%ws_url, "connecting to console");
    let (ws, _resp) = tokio::select! {
        r = crate::transport::ws_connect(&ws_url) => r?,
        _ = crate::shutdown::wait() => return Err(ShuttingDown.into()),
    };
    let (mut write, mut read) = ws.split();

    // ── hello ──────────────────────────────────────────────────────────────────────
    let displays = state.media.list_displays().unwrap_or_default();
    *state.last_displays.write() = displays.clone();
    let hello = AgentToConsole::Hello {
        protocol_version: PROTOCOL_VERSION,
        device_id: local.device_id.clone(),
        device_secret: local.device_secret.clone(),
        agent_version: crate::AGENT_VERSION.to_string(),
        hostname: hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default(),
        os: protocol::common::Os::current(),
        arch: protocol::common::Arch::current(),
        mode: state.config.read().mode,
        capabilities: AgentCapabilities {
            codecs: state.media.available_codecs(),
            displays,
            input: true,
            clipboard: true,
        },
        logged_in_user: crate::platform::logged_in_user(),
        local_overrides: state.overrides.read().clone(),
    };
    *state.last_overrides.write() = state.overrides.read().clone();
    write
        .send(Message::text(serde_json::to_string(&hello)?))
        .await
        .context("sending hello")?;

    // ── wait for hello_ack ───────────────────────────────────────────────────────────
    let ack = tokio::select! {
        // Prefer the ack when both are ready: nothing is lost by taking the normal path.
        biased;
        r = tokio::time::timeout(Duration::from_secs(15), read.next()) => r
            .context("timed out waiting for hello_ack")?
            .context("connection closed before hello_ack")?
            .context("reading hello_ack")?,
        _ = crate::shutdown::wait() => {
            let _ = write.send(Message::Close(Some(close_frame()))).await;
            return Err(ShuttingDown.into());
        }
    };
    if let Message::Close(frame) = &ack {
        let (code, reason) = frame
            .as_ref()
            .map(|f| (u16::from(f.code), f.reason.to_string()))
            .unwrap_or((1005, String::new()));
        if close_code_means_reenroll(code) {
            return Err(Reenroll { reason }.into());
        }
        return Err(anyhow!("console closed before hello_ack ({code} {reason})"));
    }
    match parse(&ack)? {
        Some(ConsoleToAgent::HelloAck {
            protocol_version,
            config,
            ..
        }) => {
            if protocol_version != PROTOCOL_VERSION {
                tracing::warn!(
                    server = protocol_version,
                    ours = PROTOCOL_VERSION,
                    "protocol version mismatch"
                );
            }
            apply_config(paths, local, state, config);
            tracing::info!("connected");
            crate::app::set_console_status(true);
            crate::app::set_policy(&state.policy_json());
            crate::branding::request_refresh();
        }
        Some(ConsoleToAgent::Goodbye { reason }) => {
            if goodbye_means_reenroll(&reason) {
                return Err(Reenroll { reason }.into());
            }
            return Err(anyhow!("console rejected connection: {reason}"));
        }
        other => return Err(anyhow!("expected hello_ack, got {other:?}")),
    }

    // Flush anything buffered while offline (take the queue first so the lock is not held
    // across the awaits below).
    let buffered: Vec<AgentToConsole> = pending.lock().drain(..).collect();
    for msg in buffered {
        write
            .send(Message::text(serde_json::to_string(&msg)?))
            .await?;
    }

    // ── main loop ──────────────────────────────────────────────────────────────────
    let heartbeat_interval =
        Duration::from_secs(state.config.read().heartbeat_interval_s.max(5) as u64);
    let mut heartbeat = tokio::time::interval(heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let ping_nonce = AtomicU64::new(0);

    loop {
        tokio::select! {
            // Outbound: session → console.
            msg = out_rx.recv() => {
                let Some(msg) = msg else { return Ok(()); };
                if let Err(e) = write.send(Message::text(serde_json::to_string(&msg)?)).await {
                    // Re-queue so it is retried after reconnect.
                    let mut q = pending.lock();
                    if q.len() < 64 { q.push_back(msg); }
                    return Err(anyhow!("send failed: {e}"));
                }
            }
            // Heartbeat.
            _ = heartbeat.tick() => {
                let hb = build_heartbeat(state);
                write.send(Message::text(serde_json::to_string(&hb)?)).await.context("heartbeat")?;
            }
            // Stop requested: end the session, let it say goodbye, close the socket.
            _ = crate::shutdown::wait() => {
                graceful_close(sessions, &mut write, out_rx).await;
                return Err(ShuttingDown.into());
            }
            // Inbound: console → agent.
            frame = read.next() => {
                let Some(frame) = frame else { return Ok(()); };
                let frame = frame.context("reading frame")?;
                match frame {
                    Message::Close(Some(f)) if close_code_means_reenroll(u16::from(f.code)) => {
                        return Err(Reenroll { reason: f.reason.to_string() }.into());
                    }
                    Message::Close(_) => return Ok(()),
                    Message::Ping(p) => { write.send(Message::Pong(p)).await.ok(); }
                    Message::Pong(_) => {}
                    _ => {
                        if let Some(msg) = parse(&frame)? {
                            if handle_message(paths, local, state, sessions, &mut write, &ping_nonce, msg).await? {
                                return Ok(()); // goodbye
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Handle one console→agent message. Returns `Ok(true)` to close the connection.
async fn handle_message<W>(
    paths: &Paths,
    local: &LocalConfig,
    state: &Arc<AgentState>,
    sessions: &Arc<SessionManager>,
    write: &mut W,
    ping_nonce: &AtomicU64,
    msg: ConsoleToAgent,
) -> Result<bool>
where
    W: SinkExt<Message> + Unpin,
    <W as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    match msg {
        ConsoleToAgent::HelloAck { .. } => {} // ignored after handshake
        ConsoleToAgent::ConfigUpdate { config } => {
            apply_config(paths, local, state, config);
            sessions.apply_overrides(state.config.read().clone());
            crate::branding::request_refresh();
        }
        ConsoleToAgent::SessionRequest {
            session_id,
            operator,
            offer,
            ice_servers,
            role,
            shadow_of,
            notify_operator,
            ..
        } => {
            sessions.start(SessionRequest {
                session_id,
                operator,
                offer,
                ice_servers,
                role,
                shadow_of,
                notify_operator,
            });
        }
        ConsoleToAgent::IceCandidate {
            session_id,
            candidate,
        } => {
            sessions.add_ice_candidate(&session_id, candidate);
        }
        ConsoleToAgent::SessionEnd { session_id, reason } => {
            sessions.end(&session_id, reason);
        }
        ConsoleToAgent::Ping { nonce } => {
            ping_nonce.store(nonce, Ordering::Relaxed);
            let pong = AgentToConsole::Pong { nonce };
            if let Err(e) = write
                .send(Message::text(serde_json::to_string(&pong)?))
                .await
            {
                return Err(anyhow!("pong send failed: {e}"));
            }
        }
        ConsoleToAgent::Update {
            version,
            url,
            sha256,
        } => {
            tracing::info!(%version, %url, "update requested");
            match crate::updater::apply_update(&version, &url, &sha256).await {
                Ok(()) => {
                    tracing::info!("update applied; restarting");
                    // Clean shutdown (session ended, socket closed); the service manager
                    // restarts us.
                    crate::shutdown::request("update applied");
                }
                Err(e) => tracing::error!("update failed: {e:#}"),
            }
        }
        ConsoleToAgent::Goodbye { reason } => {
            tracing::info!("console said goodbye: {reason}");
            if goodbye_means_reenroll(&reason) {
                return Err(Reenroll { reason }.into());
            }
            return Ok(true);
        }
    }
    Ok(false)
}

/// A stop was requested while connected: end the active session, forward what it still has
/// to say (its `session_state: ended` above all), then close the socket. Bounded by
/// [`crate::shutdown::SESSION_END_GRACE`] so a wedged session cannot hold the process.
async fn graceful_close<W>(
    sessions: &Arc<SessionManager>,
    write: &mut W,
    out_rx: &mut mpsc::UnboundedReceiver<AgentToConsole>,
) where
    W: SinkExt<Message> + Unpin,
    <W as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    let had_session = sessions.active_session_id().is_some();
    sessions.end_all(EndReason::AgentOffline);
    let grace = if had_session {
        crate::shutdown::SESSION_END_GRACE
    } else {
        Duration::from_millis(200)
    };
    let idle = sessions.wait_idle(grace);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            msg = out_rx.recv() => match msg {
                Some(msg) => forward(write, &msg).await,
                None => break,
            },
            _ = &mut idle => break,
        }
    }
    // The session task is done (or out of time): flush whatever it queued last.
    while let Ok(msg) = out_rx.try_recv() {
        forward(write, &msg).await;
    }
    if let Err(e) = write.send(Message::Close(Some(close_frame()))).await {
        tracing::debug!("close frame during shutdown: {e}");
    }
    tracing::info!(had_session, "console connection closed for shutdown");
}

/// The close frame sent to the console when shutting down (normal closure, reason attached).
fn close_frame() -> tokio_tungstenite::tungstenite::protocol::CloseFrame {
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    CloseFrame {
        code: CloseCode::Normal,
        reason: format!(
            "agent shutting down: {}",
            crate::shutdown::reason().unwrap_or_default()
        )
        .into(),
    }
}

async fn forward<W>(write: &mut W, msg: &AgentToConsole)
where
    W: SinkExt<Message> + Unpin,
    <W as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    match serde_json::to_string(msg) {
        Ok(text) => {
            if let Err(e) = write.send(Message::text(text)).await {
                tracing::debug!("send during shutdown: {e}");
            }
        }
        Err(e) => tracing::debug!("serialising during shutdown: {e}"),
    }
}

fn build_heartbeat(state: &Arc<AgentState>) -> AgentToConsole {
    let (cpu, mem) = system_usage();
    // Report local overrides only when they changed since last time.
    let ov_now = state.overrides.read().clone();
    let ov_changed = {
        let mut last = state.last_overrides.write();
        if *last != ov_now {
            *last = ov_now.clone();
            true
        } else {
            false
        }
    };
    // Report displays only when they changed since last time.
    let displays = state.media.list_displays().ok();
    let changed = match &displays {
        Some(d) => {
            let mut last = state.last_displays.write();
            if *last != *d {
                *last = d.clone();
                true
            } else {
                false
            }
        }
        None => false,
    };
    AgentToConsole::Heartbeat {
        uptime_s: state.start.elapsed().as_secs(),
        logged_in_user: crate::platform::logged_in_user(),
        cpu_percent: cpu,
        mem_percent: mem,
        displays: if changed { displays } else { None },
        local_overrides: if ov_changed { Some(ov_now) } else { None },
    }
}

/// Cheap CPU/memory sampling. Returns `(cpu%, mem%)`.
fn system_usage() -> (Option<f32>, Option<f32>) {
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static SYS: OnceLock<Mutex<sysinfo::System>> = OnceLock::new();
    let sys = SYS.get_or_init(|| Mutex::new(sysinfo::System::new()));
    let Ok(mut sys) = sys.lock() else {
        return (None, None);
    };
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    let cpu = Some(sys.global_cpu_usage());
    let total = sys.total_memory();
    let mem = if total > 0 {
        Some(sys.used_memory() as f32 / total as f32 * 100.0)
    } else {
        None
    };
    (cpu, mem)
}

/// Apply a new console config: store it, recompute the effective config (with local overrides),
/// persist, and push the policy to the app.
fn apply_config(paths: &Paths, _local: &LocalConfig, state: &Arc<AgentState>, config: AgentConfig) {
    let mode = config.mode;
    *state.console_config.write() = config.clone();
    let eff = state.recompute();
    // Persist onto the latest on-disk config (preserves device id/secret and current overrides).
    if let Ok(Some(mut persisted)) = LocalConfig::load(paths) {
        persisted.cached = Some(config);
        if let Err(e) = persisted.save(paths) {
            tracing::warn!("persisting config: {e:#}");
        }
    }
    crate::app::set_policy(&state.policy_json());
    tracing::info!(?mode, effective_mode = ?eff.mode, "config applied");
}

fn ws_url(server_url: &str) -> Result<String> {
    let base = server_url.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("wss://{base}")
    };
    Ok(format!("{ws}{}", protocol::AGENT_WS_PATH))
}

fn parse(msg: &Message) -> Result<Option<ConsoleToAgent>> {
    let text = match msg {
        Message::Text(t) => t.as_str().to_owned(),
        Message::Binary(b) => String::from_utf8_lossy(b).into_owned(),
        _ => return Ok(None),
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("parsing console message: {text}"))
}

#[allow(dead_code)]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_rejections_are_recognised() {
        assert!(close_code_means_reenroll(4409));
        assert!(close_code_means_reenroll(4401));
        for code in [1000, 1001, 1011, 4400, 4426, 4429] {
            assert!(!close_code_means_reenroll(code), "{code}");
        }
        assert!(goodbye_means_reenroll("device deleted"));
        assert!(goodbye_means_reenroll("unknown device"));
        assert!(!goodbye_means_reenroll("replaced by a newer connection"));
        assert!(!goodbye_means_reenroll("bye"));
    }

    #[test]
    fn reenroll_notice_is_user_facing() {
        assert!(Reenroll {
            reason: "device deleted".into()
        }
        .notice()
        .contains("removed from the console"));
        assert_eq!(
            Reenroll {
                reason: "requested".into()
            }
            .notice(),
            "Enroll this device again."
        );
        let e: anyhow::Error = Reenroll {
            reason: "bad credentials".into(),
        }
        .into();
        assert!(e.is::<Reenroll>());
        assert!(e.context("hub").is::<Reenroll>());
    }

    #[test]
    fn ws_url_derivation() {
        assert_eq!(
            ws_url("https://c.example.com").unwrap(),
            "wss://c.example.com/ws/agent"
        );
        assert_eq!(
            ws_url("http://localhost:8080/").unwrap(),
            "ws://localhost:8080/ws/agent"
        );
        assert_eq!(
            ws_url("c.example.com").unwrap(),
            "wss://c.example.com/ws/agent"
        );
    }
}
