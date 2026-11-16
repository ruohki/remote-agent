//! Persistent connection to the management console (`/ws/agent`).
//!
//! [`Hub::run`] connects with exponential backoff, performs the hello/hello_ack handshake,
//! then multiplexes heartbeats, ping/pong, configuration and session signaling. Session
//! signaling goes to a [`Backend`]: the in-process [`crate::local_sessions::LocalSessions`]
//! for the standalone agent, or the session hosts behind the service
//! ([`crate::daemon::HostRouter`]). [`HubSink`] is the handle sessions push messages back
//! through; it survives reconnects (messages queue for the next socket).
//!
//! The hub also owns the device's policy: the console's configuration, the local restrictions
//! the person at the device set, and the effective configuration that results
//! ([`crate::link::HostSettings`]). It persists both to `agent.toml`.

use crate::config::{LocalConfig, Paths};
use crate::link::{HostSettings, HostStatus};
use anyhow::{anyhow, Context, Result};
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use parking_lot::{Mutex, RwLock};
use protocol::agent::{AgentToConsole, ConsoleToAgent};
use protocol::common::EndReason;
use protocol::config::{AgentConfig, LocalOverrides};
use protocol::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// The console no longer accepts this device's identity (deleted device, bad credentials) or
/// the user asked to enroll again. In app mode [`Hub::run`] returns this so the process can drop
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

/// The console retired this device (`ConsoleToAgent::Retire`): the enrollment was forgotten
/// and the hub must not reconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retired {
    pub reason: String,
}

impl std::fmt::Display for Retired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "retired by the console: {}", self.reason)
    }
}

impl std::error::Error for Retired {}

/// A process shutdown was requested (signal, Quit, update): the connection was closed cleanly
/// and the hub must not reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShuttingDown;

impl std::fmt::Display for ShuttingDown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shutting down")
    }
}

impl std::error::Error for ShuttingDown {}

/// `/ws/agent` close codes: the device identity is no longer valid (4401 bad credentials,
/// 4409 device deleted).
fn close_code_means_reenroll(code: u16) -> bool {
    matches!(code, 4401 | 4409)
}

/// `/ws/agent` close code for an unsupported protocol version.
const CLOSE_PROTOCOL_VERSION: u16 = 4426;

/// A `goodbye` whose reason says the device is gone (as opposed to "replaced by a newer
/// connection" or a console restart).
fn goodbye_means_reenroll(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    r.contains("deleted") || r.contains("unknown device") || r.contains("bad credentials")
}

/// A `goodbye` sent by a console that is restarting: come back soon, not after a long backoff.
fn goodbye_means_restart(reason: &str) -> bool {
    reason.to_ascii_lowercase().contains("restart")
}

/// The console pings every 30 s; this much silence means the connection is dead even though
/// no write has failed yet (NAT timeout, console host gone).
const SILENCE_LIMIT: Duration = Duration::from_secs(75);
/// Longest wait between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Longest wait while the console rejects this identity in service mode (it may come back:
/// a restored console database; the device may be restored).
const MAX_IDENTITY_BACKOFF: Duration = Duration::from_secs(300);

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

    /// Build a sink plus the receiving end of its channel. Used by the hub to route session
    /// output to the live socket, by session hosts to route it to the service, and by tests.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<AgentToConsole>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

/// Where console session signaling goes, and where the capabilities in `hello` come from.
pub trait Backend: Send + Sync + 'static {
    /// Capabilities, desktop state and signed-in user, for `hello` and heartbeats.
    fn status(&self) -> HostStatus;
    /// Session signaling: `session_request`, `ice_candidate`, `session_end`.
    fn deliver(&self, msg: ConsoleToAgent);
    /// The effective configuration or the local restrictions changed.
    fn settings_changed(&self, settings: &HostSettings);
    /// The console connection came up or went down.
    fn console_status(&self, connected: bool);
    fn end_all(&self, reason: EndReason);
    fn has_session(&self) -> bool;
    /// Resolves once no session is running (or after `timeout`).
    fn wait_idle(&self, timeout: Duration) -> BoxFuture<'_, ()>;
}

/// How the hub reacts when the console rejects the device identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityPolicy {
    /// Return [`Reenroll`] so the app can show the Connect screen.
    ReturnToConnect,
    /// Keep retrying with a long backoff (service, headless): a transient console problem must
    /// never make a fleet forget its identities.
    KeepTrying,
}

/// Policy state: console config + local restrictions → effective config.
struct Policy {
    console_config: AgentConfig,
    overrides: LocalOverrides,
}

impl Policy {
    fn settings(&self) -> HostSettings {
        HostSettings {
            config: self.overrides.apply(self.console_config.clone()),
            console_config: self.console_config.clone(),
            overrides: self.overrides.clone(),
        }
    }
}

/// What the last heartbeat (or hello) reported, to send only changes.
#[derive(Default)]
struct Reported {
    status: Option<HostStatus>,
    overrides: Option<LocalOverrides>,
}

/// The console-facing half of an agent.
pub struct Hub {
    paths: Paths,
    /// Identity with the device secret resolved into memory.
    local: LocalConfig,
    backend: Arc<dyn Backend>,
    policy: RwLock<Policy>,
    reported: Mutex<Reported>,
    out_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<AgentToConsole>>,
    /// Messages waiting for a live socket (bounded; signaling is time-sensitive).
    pending: Mutex<VecDeque<AgentToConsole>>,
    connected: AtomicBool,
    /// Protocol version announced in `hello`; drops to the minimum when a console refuses ours.
    hello_version: AtomicU32,
    identity: IdentityPolicy,
    start: Instant,
}

impl Hub {
    /// Load the identity (resolving the device secret) and the cached policy.
    pub fn load(paths: &Paths) -> Result<LocalConfig> {
        let mut local = LocalConfig::load_required(paths)?;
        crate::transport::check_console_url(&local.server_url)?;
        crate::transport::set_console_pin(
            local.console_tls_spki_sha256.as_deref(),
            &local.server_url,
        )
        .context("console TLS pin in agent.toml")?;
        let backend = crate::secrets::migrate_if_needed(paths, &mut local)?;
        let (secret, _) = crate::secrets::load(paths, &local)?;
        local.device_secret = secret;
        tracing::info!(
            secret_backend = backend.as_str(),
            tls_pin = crate::transport::console_pin_active(),
            "console credentials loaded"
        );
        Ok(local)
    }

    pub fn new(
        paths: Paths,
        local: LocalConfig,
        backend: Arc<dyn Backend>,
        out_rx: mpsc::UnboundedReceiver<AgentToConsole>,
        identity: IdentityPolicy,
    ) -> Arc<Self> {
        let policy = Policy {
            console_config: local.console_config(),
            overrides: local.overrides.clone(),
        };
        tracing::info!(
            server = %local.server_url,
            device = %local.device_id,
            version = crate::AGENT_VERSION,
            mode = ?policy.settings().config.mode,
            overrides = ?policy.overrides,
            "starting agent"
        );
        let hub = Arc::new(Self {
            paths,
            local,
            backend,
            policy: RwLock::new(policy),
            reported: Mutex::new(Reported::default()),
            out_rx: tokio::sync::Mutex::new(out_rx),
            pending: Mutex::new(VecDeque::new()),
            connected: AtomicBool::new(false),
            hello_version: AtomicU32::new(PROTOCOL_VERSION),
            identity,
            start: Instant::now(),
        });
        hub.backend.settings_changed(&hub.settings());
        hub
    }

    pub fn device_id(&self) -> &str {
        &self.local.device_id
    }

    pub fn server_url(&self) -> &str {
        &self.local.server_url
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Effective configuration, console policy and local restrictions.
    pub fn settings(&self) -> HostSettings {
        self.policy.read().settings()
    }

    /// The person at the device changed the local restrictions: persist, recompute, apply.
    pub fn set_overrides(&self, overrides: LocalOverrides) {
        // Persist onto the latest on-disk config so a concurrent config update is not lost.
        match LocalConfig::load(&self.paths) {
            Ok(Some(mut on_disk)) => {
                on_disk.overrides = overrides.clone();
                if let Err(e) = on_disk.save(&self.paths) {
                    tracing::warn!("persisting local restrictions: {e:#}");
                }
            }
            Ok(None) => tracing::warn!("persisting local restrictions: agent.toml is gone"),
            Err(e) => tracing::warn!("persisting local restrictions: {e:#}"),
        }
        self.policy.write().overrides = overrides;
        let settings = self.settings();
        tracing::info!(effective = ?settings.config.mode, "local restrictions changed");
        self.backend.settings_changed(&settings);
    }

    /// Store a new console configuration and apply it.
    fn apply_config(&self, config: AgentConfig) {
        let mode = config.mode;
        self.policy.write().console_config = config.clone();
        // Persist onto the latest on-disk config (keeps identity and current restrictions).
        if let Ok(Some(mut on_disk)) = LocalConfig::load(&self.paths) {
            on_disk.cached = Some(config);
            if let Err(e) = on_disk.save(&self.paths) {
                tracing::warn!("persisting config: {e:#}");
            }
        }
        let settings = self.settings();
        self.backend.settings_changed(&settings);
        tracing::info!(?mode, effective_mode = ?settings.config.mode, "config applied");
    }

    fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::SeqCst);
        self.backend.console_status(connected);
    }

    /// Connect, and reconnect, until shutdown (`Ok`), identity loss in app mode
    /// ([`Reenroll`]) or retirement ([`Retired`]).
    pub async fn run(self: &Arc<Self>) -> Result<()> {
        let branding_task = tokio::spawn(crate::branding::refresh_loop(
            self.local.server_url.clone(),
            self.paths.clone(),
        ));
        let result = self.run_loop().await;
        branding_task.abort();
        self.set_connected(false);
        result
    }

    async fn run_loop(self: &Arc<Self>) -> Result<()> {
        let mut backoff = Duration::from_secs(1);
        loop {
            let app = crate::app::is_running();
            let mut reached = false;
            let outcome = tokio::select! {
                r = self.connect_once(&mut reached) => r,
                // Settings → "Enroll again" (only meaningful with the app window).
                _ = crate::app::reenroll_requested(), if app => {
                    Err(Reenroll { reason: "requested".into() }.into())
                }
            };
            // A connection that got as far as hello_ack proves the console is healthy: start
            // the next attempt from the bottom of the backoff.
            if reached {
                backoff = Duration::from_secs(1);
            }
            let mut next = backoff;
            let mut identity_error = false;
            match outcome {
                Ok(Disconnect::Closed) => {
                    tracing::info!("console closed the connection; reconnecting");
                }
                Ok(Disconnect::Restarting) => {
                    tracing::info!("console is restarting; reconnecting shortly");
                    next = Duration::from_secs(2);
                }
                Err(e) if e.is::<ShuttingDown>() => {
                    self.backend.end_all(EndReason::AgentOffline);
                    self.backend
                        .wait_idle(crate::shutdown::SESSION_END_GRACE)
                        .await;
                    tracing::info!(reason = ?crate::shutdown::reason(), "agent stopped");
                    return Ok(());
                }
                Err(e) if e.is::<Retired>() => {
                    self.backend.end_all(EndReason::AgentOffline);
                    return Err(e);
                }
                Err(e) if e.is::<Reenroll>() => {
                    self.backend.end_all(EndReason::DeviceUserClosed);
                    if self.identity == IdentityPolicy::ReturnToConnect && app {
                        return Err(e);
                    }
                    tracing::warn!("{e:#}; retrying later");
                    identity_error = true;
                    next = backoff.max(Duration::from_secs(30));
                    backoff = (next * 2).min(MAX_IDENTITY_BACKOFF);
                }
                Err(e) => {
                    tracing::warn!("hub connection error: {e:#}");
                }
            }
            // End any active session on disconnect (its ICE relay is gone).
            self.backend.end_all(EndReason::AgentOffline);
            self.set_connected(false);
            tokio::select! {
                _ = tokio::time::sleep(jittered(next)) => {}
                _ = crate::shutdown::wait() => {
                    self.backend.wait_idle(crate::shutdown::SESSION_END_GRACE).await;
                    tracing::info!(reason = ?crate::shutdown::reason(), "agent stopped while offline");
                    return Ok(());
                }
            }
            if !identity_error {
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }

    fn hello(&self) -> AgentToConsole {
        let status = self.backend.status();
        let settings = self.settings();
        let version = self.hello_version.load(Ordering::SeqCst);
        let v2 = version >= 2;
        *self.reported.lock() = Reported {
            status: Some(status.clone()),
            overrides: Some(settings.overrides.clone()),
        };
        AgentToConsole::Hello {
            protocol_version: version,
            device_id: self.local.device_id.clone(),
            device_secret: self.local.device_secret.clone(),
            agent_version: crate::AGENT_VERSION.to_string(),
            hostname: hostname::get()
                .map(|h| h.to_string_lossy().into_owned())
                .unwrap_or_default(),
            os: protocol::common::Os::current(),
            arch: protocol::common::Arch::current(),
            mode: settings.config.mode,
            capabilities: status.capabilities,
            logged_in_user: status.logged_in_user,
            local_overrides: settings.overrides,
            machine_id: if v2 {
                crate::platform::machine_id()
            } else {
                None
            },
            desktop: v2.then_some(status.desktop),
        }
    }

    fn heartbeat(&self) -> AgentToConsole {
        let (cpu, mem) = system_usage();
        let status = self.backend.status();
        let overrides = self.settings().overrides;
        let v2 = self.hello_version.load(Ordering::SeqCst) >= 2;
        let mut reported = self.reported.lock();
        let last = reported.status.clone();
        let displays_changed = last
            .as_ref()
            .map(|l| l.capabilities.displays != status.capabilities.displays)
            .unwrap_or(true);
        let desktop_changed = last.as_ref().map(|l| l.desktop) != Some(status.desktop);
        // Codecs, input and the privacy screen depend on which desktop the agent can reach.
        let caps_changed = last
            .as_ref()
            .map(|l| {
                let (a, b) = (&l.capabilities, &status.capabilities);
                a.codecs != b.codecs
                    || a.input != b.input
                    || a.clipboard != b.clipboard
                    || a.privacy_screen != b.privacy_screen
            })
            .unwrap_or(true);
        let overrides_changed = reported.overrides.as_ref() != Some(&overrides);
        reported.status = Some(status.clone());
        reported.overrides = Some(overrides.clone());
        AgentToConsole::Heartbeat {
            uptime_s: self.start.elapsed().as_secs(),
            logged_in_user: status.logged_in_user,
            cpu_percent: cpu,
            mem_percent: mem,
            displays: displays_changed.then(|| status.capabilities.displays.clone()),
            local_overrides: overrides_changed.then_some(overrides),
            desktop: (v2 && desktop_changed).then_some(status.desktop),
            capabilities: (v2 && caps_changed).then_some(status.capabilities),
        }
    }

    /// One connection lifecycle. `reached` is set once hello_ack arrived.
    async fn connect_once(self: &Arc<Self>, reached: &mut bool) -> Result<Disconnect> {
        let ws_url = ws_url(&self.local.server_url)?;
        tracing::info!(%ws_url, "connecting to console");
        let (ws, _resp) = tokio::select! {
            r = crate::transport::ws_connect(&ws_url) => r?,
            _ = crate::shutdown::wait() => return Err(ShuttingDown.into()),
        };
        let (mut write, mut read) = ws.split();

        // ── hello ──────────────────────────────────────────────────────────────────
        write
            .send(Message::text(serde_json::to_string(&self.hello())?))
            .await
            .context("sending hello")?;

        // ── wait for hello_ack ───────────────────────────────────────────────────────
        let ack = tokio::select! {
            biased;
            r = tokio::time::timeout(Duration::from_secs(20), read.next()) => r
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
            if code == CLOSE_PROTOCOL_VERSION
                && self.hello_version.load(Ordering::SeqCst) > MIN_PROTOCOL_VERSION
            {
                // An older console: speak its version from now on.
                tracing::warn!("console refused protocol {PROTOCOL_VERSION}; falling back to {MIN_PROTOCOL_VERSION}");
                self.hello_version
                    .store(MIN_PROTOCOL_VERSION, Ordering::SeqCst);
            }
            return Err(anyhow!("console closed before hello_ack ({code} {reason})"));
        }
        match parse(&ack) {
            Some(ConsoleToAgent::HelloAck {
                protocol_version,
                config,
                ..
            }) => {
                if protocol_version < self.hello_version.load(Ordering::SeqCst) {
                    tracing::info!(
                        console = protocol_version,
                        "console speaks an older protocol; using {protocol_version}"
                    );
                    self.hello_version
                        .store(protocol_version.max(MIN_PROTOCOL_VERSION), Ordering::SeqCst);
                }
                *reached = true;
                self.apply_config(config);
                tracing::info!("connected");
                self.set_connected(true);
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

        // Flush anything buffered while offline (drain first: no lock across awaits).
        let buffered: Vec<AgentToConsole> = self.pending.lock().drain(..).collect();
        for msg in buffered {
            write
                .send(Message::text(serde_json::to_string(&msg)?))
                .await?;
        }

        // ── main loop ──────────────────────────────────────────────────────────────
        let heartbeat_interval =
            Duration::from_secs(self.settings().config.heartbeat_interval_s.max(5) as u64);
        let mut heartbeat = tokio::time::interval(heartbeat_interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_inbound = Instant::now();
        let mut silence = tokio::time::interval(Duration::from_secs(5));
        let mut out_rx = self.out_rx.lock().await;

        loop {
            tokio::select! {
                // Outbound: session → console.
                msg = out_rx.recv() => {
                    let Some(msg) = msg else { return Ok(Disconnect::Closed); };
                    if let Err(e) = write.send(Message::text(serde_json::to_string(&msg)?)).await {
                        // Re-queue so it is retried after reconnect.
                        let mut q = self.pending.lock();
                        if q.len() < 64 { q.push_back(msg); }
                        return Err(anyhow!("send failed: {e}"));
                    }
                }
                _ = heartbeat.tick() => {
                    let hb = self.heartbeat();
                    write.send(Message::text(serde_json::to_string(&hb)?)).await.context("heartbeat")?;
                }
                _ = silence.tick() => {
                    if last_inbound.elapsed() > SILENCE_LIMIT {
                        return Err(anyhow!("console silent for {}s", last_inbound.elapsed().as_secs()));
                    }
                }
                // Stop requested: end the session, let it say goodbye, close the socket.
                _ = crate::shutdown::wait() => {
                    self.graceful_close(&mut write, &mut out_rx).await;
                    return Err(ShuttingDown.into());
                }
                frame = read.next() => {
                    let Some(frame) = frame else { return Ok(Disconnect::Closed); };
                    let frame = frame.context("reading frame")?;
                    last_inbound = Instant::now();
                    match frame {
                        Message::Close(Some(f)) if close_code_means_reenroll(u16::from(f.code)) => {
                            return Err(Reenroll { reason: f.reason.to_string() }.into());
                        }
                        Message::Close(_) => return Ok(Disconnect::Closed),
                        Message::Ping(p) => { write.send(Message::Pong(p)).await.ok(); }
                        Message::Pong(_) => {}
                        _ => {
                            let Some(msg) = parse(&frame) else { continue };
                            if let Some(end) = self.handle_message(&mut write, msg).await? {
                                return Ok(end);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Handle one console→agent message. `Some` ends the connection.
    async fn handle_message<W>(
        &self,
        write: &mut W,
        msg: ConsoleToAgent,
    ) -> Result<Option<Disconnect>>
    where
        W: SinkExt<Message> + Unpin,
        <W as futures_util::Sink<Message>>::Error: std::fmt::Display,
    {
        match msg {
            ConsoleToAgent::HelloAck { .. } => {} // ignored after handshake
            ConsoleToAgent::ConfigUpdate { config } => {
                self.apply_config(config);
                crate::branding::request_refresh();
            }
            msg @ (ConsoleToAgent::SessionRequest { .. }
            | ConsoleToAgent::IceCandidate { .. }
            | ConsoleToAgent::SessionEnd { .. }) => self.backend.deliver(msg),
            ConsoleToAgent::Ping { nonce } => {
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
                // Downloads can take minutes: never inside the read loop (heartbeats would
                // stop and the console would drop us).
                let auth = crate::updater::DeviceAuth {
                    device_id: self.local.device_id.clone(),
                    device_secret: self.local.device_secret.clone(),
                };
                tokio::spawn(async move {
                    tracing::info!(%version, %url, "update requested");
                    match crate::updater::apply_update(&version, &url, &sha256, Some(&auth)).await {
                        Ok(()) => {
                            tracing::info!("update applied; restarting");
                            crate::shutdown::request("update applied");
                        }
                        Err(e) => tracing::error!("update failed: {e:#}"),
                    }
                });
            }
            ConsoleToAgent::Goodbye { reason } => {
                tracing::info!("console said goodbye: {reason}");
                if goodbye_means_reenroll(&reason) {
                    return Err(Reenroll { reason }.into());
                }
                return Ok(Some(if goodbye_means_restart(&reason) {
                    Disconnect::Restarting
                } else {
                    Disconnect::Closed
                }));
            }
            ConsoleToAgent::Retire { reason } => {
                tracing::warn!(%reason, "console retired this device; forgetting the enrollment");
                if let Err(e) = crate::startup::forget_enrollment(&self.paths) {
                    tracing::error!("forgetting the enrollment: {e:#}");
                }
                return Err(Retired { reason }.into());
            }
        }
        Ok(None)
    }

    /// A stop was requested while connected: end the active session, forward what it still
    /// has to say (its `session_state: ended` above all), then close the socket. Bounded by
    /// [`crate::shutdown::SESSION_END_GRACE`] so a wedged session cannot hold the process.
    async fn graceful_close<W>(
        &self,
        write: &mut W,
        out_rx: &mut mpsc::UnboundedReceiver<AgentToConsole>,
    ) where
        W: SinkExt<Message> + Unpin,
        <W as futures_util::Sink<Message>>::Error: std::fmt::Display,
    {
        let had_session = self.backend.has_session();
        self.backend.end_all(EndReason::AgentOffline);
        let grace = if had_session {
            crate::shutdown::SESSION_END_GRACE
        } else {
            Duration::from_millis(200)
        };
        let idle = self.backend.wait_idle(grace);
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
        while let Ok(msg) = out_rx.try_recv() {
            forward(write, &msg).await;
        }
        if let Err(e) = write.send(Message::Close(Some(close_frame()))).await {
            tracing::debug!("close frame during shutdown: {e}");
        }
        tracing::info!(had_session, "console connection closed for shutdown");
    }
}

/// How a connection ended without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disconnect {
    Closed,
    /// The console announced a restart.
    Restarting,
}

/// `base` scaled by a random factor in [0.5, 1.5): agents that lost the console together
/// must not come back together.
fn jittered(base: Duration) -> Duration {
    let factor = 0.5 + (rand::random::<u32>() as f64 / u32::MAX as f64);
    base.mul_f64(factor)
}

/// The standalone agent: the hub plus in-process sessions and the app UI. Used by `run` and
/// the double-clicked app when no service owns the console connection.
pub async fn run_agent(paths: Paths) -> Result<()> {
    let local = Hub::load(&paths)?;
    let (sink, out_rx) = HubSink::channel();
    let sessions = crate::local_sessions::LocalSessions::new(
        sink,
        local.overrides.apply(local.console_config()),
        crate::local_sessions::UiDeps::detect(),
    );
    if crate::platform::main_loop_running() {
        crate::platform::install_input_guard();
    }
    sessions.wire_app();
    let hub = Hub::new(
        paths,
        local,
        sessions.clone() as Arc<dyn Backend>,
        out_rx,
        if crate::app::is_running() {
            IdentityPolicy::ReturnToConnect
        } else {
            IdentityPolicy::KeepTrying
        },
    );
    if crate::app::is_running() {
        let h = Arc::clone(&hub);
        crate::app::set_overrides_handler(Arc::new(move |ov| h.set_overrides(ov)));
        crate::app::set_device_info(&hub.settings().config.display_name, hub.device_id());
        crate::app::set_console_url(hub.server_url());
    } else if crate::platform::main_loop_running() {
        // Fallback menu-bar item when the app window is not available.
        let s = Arc::clone(&sessions.sessions);
        let status = format!("Remote support — connected to {}", hub.server_url());
        crate::platform::install_menu_bar(
            &status,
            Arc::new(move || s.end_all(EndReason::DeviceUserClosed)),
        );
    }
    let result = hub.run().await;
    sessions.end_all(EndReason::AgentOffline);
    result
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

/// Cheap CPU/memory sampling. Returns `(cpu%, mem%)`.
fn system_usage() -> (Option<f32>, Option<f32>) {
    use std::sync::OnceLock;
    static SYS: OnceLock<std::sync::Mutex<sysinfo::System>> = OnceLock::new();
    let sys = SYS.get_or_init(|| std::sync::Mutex::new(sysinfo::System::new()));
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

/// Decode a console message. Unknown or malformed messages are logged and skipped: a newer
/// console may send types this agent does not know, and that must not cost the connection.
fn parse(msg: &Message) -> Option<ConsoleToAgent> {
    let text = match msg {
        Message::Text(t) => t.as_str().to_owned(),
        Message::Binary(b) => String::from_utf8_lossy(b).into_owned(),
        _ => return None,
    };
    match serde_json::from_str(&text) {
        Ok(m) => Some(m),
        Err(e) => {
            let kind = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
                .unwrap_or_else(|| "?".into());
            tracing::warn!(%kind, "skipping console message this agent does not understand: {e}");
            None
        }
    }
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
        assert!(goodbye_means_restart("console restarting"));
        assert!(!goodbye_means_restart("replaced by a newer connection"));
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

    #[test]
    fn unknown_console_messages_are_skipped_not_fatal() {
        assert!(parse(&Message::text(r#"{"type":"from_the_future","x":1}"#)).is_none());
        assert!(parse(&Message::text("not json")).is_none());
        assert!(matches!(
            parse(&Message::text(r#"{"type":"ping","nonce":7}"#)),
            Some(ConsoleToAgent::Ping { nonce: 7 })
        ));
        assert!(matches!(
            parse(&Message::text(r#"{"type":"retire","reason":"deleted"}"#)),
            Some(ConsoleToAgent::Retire { .. })
        ));
    }

    #[test]
    fn jitter_stays_within_half_to_one_and_a_half() {
        for _ in 0..1000 {
            let d = jittered(Duration::from_secs(10));
            assert!(
                d >= Duration::from_secs(5) && d < Duration::from_secs(15),
                "{d:?}"
            );
        }
    }
}
