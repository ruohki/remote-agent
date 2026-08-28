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
use protocol::config::AgentConfig;
use protocol::PROTOCOL_VERSION;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

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
    config: Arc<RwLock<AgentConfig>>,
    media: Arc<SystemMedia>,
    start: Instant,
    last_displays: RwLock<Vec<DisplayInfo>>,
}

pub async fn run_agent(paths: Paths) -> Result<()> {
    let local = LocalConfig::load_required(&paths)?;
    let config = Arc::new(RwLock::new(local.effective()));
    tracing::info!(
        server = %local.server_url,
        device = %local.device_id,
        version = crate::AGENT_VERSION,
        mode = ?config.read().mode,
        "starting agent"
    );

    let media = Arc::new(SystemMedia);
    let state = Arc::new(AgentState {
        config: Arc::clone(&config),
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
    let indicator: Arc<dyn Indicator> = if interactive {
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
        crate::app::set_device_info(&config.read().display_name, &local.device_id);
    } else if interactive {
        // Fallback menu-bar item when the app window is not available.
        let sessions_for_menu = Arc::clone(&sessions);
        let status = format!("Remote support — connected to {}", local.server_url);
        crate::platform::install_menu_bar(
            &status,
            Arc::new(move || sessions_for_menu.end_all(EndReason::DeviceUserClosed)),
        );
    }

    // Buffer of messages waiting for a live socket (bounded so we don't grow unbounded while
    // offline; signaling is time-sensitive so old entries are dropped).
    let pending: Arc<parking_lot::Mutex<std::collections::VecDeque<AgentToConsole>>> =
        Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));

    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_once(&paths, &local, &state, &sessions, &mut out_rx, &pending).await {
            Ok(()) => {
                backoff = Duration::from_secs(1);
                tracing::info!("console closed the connection; reconnecting");
            }
            Err(e) => {
                tracing::warn!("hub connection error: {e:#}");
            }
        }
        // End any active session on disconnect (its ICE relay is gone).
        sessions.end_all(EndReason::AgentOffline);
        crate::app::set_console_status(false);
        let jitter = Duration::from_millis(rand::random::<u64>() % 500);
        tokio::time::sleep(backoff + jitter).await;
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
    let (ws, _resp) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .with_context(|| format!("connecting to {ws_url}"))?;
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
    };
    write
        .send(Message::text(serde_json::to_string(&hello)?))
        .await
        .context("sending hello")?;

    // ── wait for hello_ack ───────────────────────────────────────────────────────────
    let ack = tokio::time::timeout(Duration::from_secs(15), read.next())
        .await
        .context("timed out waiting for hello_ack")?
        .context("connection closed before hello_ack")?
        .context("reading hello_ack")?;
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
        }
        Some(ConsoleToAgent::Goodbye { reason }) => {
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
            // Inbound: console → agent.
            frame = read.next() => {
                let Some(frame) = frame else { return Ok(()); };
                let frame = frame.context("reading frame")?;
                match frame {
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
                    // The service manager restarts us; just exit cleanly.
                    std::process::exit(0);
                }
                Err(e) => tracing::error!("update failed: {e:#}"),
            }
        }
        ConsoleToAgent::Goodbye { reason } => {
            tracing::info!("console said goodbye: {reason}");
            return Ok(true);
        }
    }
    Ok(false)
}

fn build_heartbeat(state: &Arc<AgentState>) -> AgentToConsole {
    let (cpu, mem) = system_usage();
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

/// Apply a new config: update shared state and persist it to disk.
fn apply_config(paths: &Paths, local: &LocalConfig, state: &Arc<AgentState>, config: AgentConfig) {
    let mode = config.mode;
    *state.config.write() = config.clone();
    let mut persisted = local.clone();
    persisted.cached = Some(config);
    if let Err(e) = persisted.save(paths) {
        tracing::warn!("persisting config: {e:#}");
    }
    tracing::info!(?mode, "config applied");
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
