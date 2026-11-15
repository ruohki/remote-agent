//! The service side of an installed agent.
//!
//! `service run` (Windows service / macOS LaunchDaemon) runs this: it owns the device
//! identity, the console connection, the policy and updates, and it is online whether or not
//! anyone is signed in. The work inside desktop sessions is done by session hosts
//! ([`crate::session_host`]) that connect over the local link; [`HostRouter`] is the hub's
//! [`Backend`] that hands each console session to the host serving the console's desktop.

use crate::config::{LocalConfig, Paths};
use crate::hub::{Backend, Hub, HubSink, IdentityPolicy};
use crate::link::endpoint::{self, Access};
use crate::link::frame::{read_message, spawn_writer};
use crate::link::{HostKind, HostSettings, HostStatus, HostToService, ServiceToHost};
use anyhow::{anyhow, Context, Result};
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use protocol::agent::{AgentCapabilities, AgentToConsole, ConsoleToAgent};
use protocol::common::{EndReason, SessionState};
use protocol::config::AgentConfig;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};

/// A connected session host.
struct Host {
    tx: mpsc::UnboundedSender<ServiceToHost>,
    kind: HostKind,
    pid: u32,
    os_session: u32,
    status: HostStatus,
    connected_at: Instant,
}

/// Routes console sessions to session hosts. See the module docs.
pub struct HostRouter {
    hosts: Mutex<HashMap<u64, Host>>,
    next_id: AtomicU64,
    /// Which host owns which console session (ICE and `session_end` follow the session).
    owners: Mutex<HashMap<String, u64>>,
    idle: Notify,
    /// Where session output goes: the current hub's sink.
    sink: Mutex<Option<HubSink>>,
    hub: Mutex<Weak<Hub>>,
    /// Settings to hand new hosts while no hub runs (not enrolled yet).
    fallback_settings: Mutex<HostSettings>,
}

impl HostRouter {
    pub fn new() -> Arc<Self> {
        let cfg = AgentConfig::default();
        Arc::new(Self {
            hosts: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            owners: Mutex::new(HashMap::new()),
            idle: Notify::new(),
            sink: Mutex::new(None),
            hub: Mutex::new(Weak::new()),
            fallback_settings: Mutex::new(HostSettings {
                config: cfg.clone(),
                console_config: cfg,
                overrides: Default::default(),
            }),
        })
    }

    fn hub(&self) -> Option<Arc<Hub>> {
        self.hub.lock().upgrade()
    }

    /// Attach a (new) hub: hosts get its identity and settings.
    fn attach(&self, hub: &Arc<Hub>, sink: HubSink) {
        *self.hub.lock() = Arc::downgrade(hub);
        *self.sink.lock() = Some(sink);
        let welcome = self.welcome();
        self.broadcast(|| welcome.clone());
    }

    fn detach(&self) {
        *self.hub.lock() = Weak::new();
        *self.sink.lock() = None;
    }

    fn welcome(&self) -> ServiceToHost {
        match self.hub() {
            Some(hub) => ServiceToHost::Welcome {
                device_id: hub.device_id().to_string(),
                server_url: hub.server_url().to_string(),
                console_connected: hub.is_connected(),
                settings: hub.settings(),
            },
            None => ServiceToHost::Welcome {
                device_id: String::new(),
                server_url: String::new(),
                console_connected: false,
                settings: self.fallback_settings.lock().clone(),
            },
        }
    }

    fn to_console(&self, msg: AgentToConsole) {
        match self.sink.lock().as_ref() {
            Some(s) => s.send(msg),
            None => tracing::debug!("no console connection; host message dropped"),
        }
    }

    fn broadcast(&self, msg: impl Fn() -> ServiceToHost) {
        for host in self.hosts.lock().values() {
            let _ = host.tx.send(msg());
        }
    }

    /// The host that gets new sessions: the one whose desktop is on the console, preferring a
    /// signed-in user's session over the login window, and the most recent on ties.
    fn active_host(&self) -> Option<u64> {
        let hosts = self.hosts.lock();
        hosts
            .iter()
            .filter(|(_, h)| h.status.on_console)
            .max_by_key(|(_, h)| {
                let rank = match h.kind {
                    HostKind::Console | HostKind::UserSession => 3,
                    HostKind::Manual => 2,
                    HostKind::LoginWindow => 1,
                };
                (rank, h.connected_at)
            })
            .map(|(id, _)| *id)
    }

    /// Tell every host to finish its session and exit (service stopping).
    pub fn shutdown_hosts(&self, reason: &str) {
        self.broadcast(|| ServiceToHost::Shutdown {
            reason: reason.to_string(),
        });
    }

    pub fn host_count(&self) -> usize {
        self.hosts.lock().len()
    }

    fn release_sessions_of(&self, host: u64, reason: EndReason) {
        let ended: Vec<String> = {
            let mut owners = self.owners.lock();
            let ids: Vec<String> = owners
                .iter()
                .filter(|(_, h)| **h == host)
                .map(|(s, _)| s.clone())
                .collect();
            for id in &ids {
                owners.remove(id);
            }
            ids
        };
        for session_id in ended {
            self.to_console(AgentToConsole::SessionState {
                session_id,
                state: SessionState::Ended,
                reason: Some(reason),
            });
        }
        self.idle.notify_waiters();
    }

    /// Serve one host connection until it closes.
    async fn serve_host(self: Arc<Self>, stream: endpoint::BoxStream) -> Result<()> {
        let (mut read, write) = tokio::io::split(stream);
        let (tx, writer) = spawn_writer::<_, ServiceToHost>(write);
        let hello = tokio::time::timeout(Duration::from_secs(10), read_message(&mut read))
            .await
            .context("no hello from the session host")??
            .context("session host closed before hello")?;
        let HostToService::Hello {
            version,
            pid,
            os_session,
            kind,
            status,
        } = hello
        else {
            return Err(anyhow!("expected hello, got {hello:?}"));
        };
        if version != crate::AGENT_VERSION {
            tracing::warn!(%version, ours = crate::AGENT_VERSION, "session host runs another version");
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        tracing::info!(id, pid, os_session, ?kind, desktop = ?status.desktop, "session host connected");
        let _ = tx.send(self.welcome());
        self.hosts.lock().insert(
            id,
            Host {
                tx,
                kind,
                pid,
                os_session,
                status,
                connected_at: Instant::now(),
            },
        );

        let result = loop {
            match read_message::<_, HostToService>(&mut read).await {
                Ok(Some(msg)) => self.on_host_message(id, msg),
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        let gone = self.hosts.lock().remove(&id);
        if let Some(h) = gone {
            tracing::info!(
                id,
                pid = h.pid,
                os_session = h.os_session,
                "session host disconnected"
            );
        }
        self.release_sessions_of(id, EndReason::AgentOffline);
        writer.abort();
        result
    }

    fn on_host_message(&self, id: u64, msg: HostToService) {
        match msg {
            HostToService::Status(status) => {
                let was_active = self.active_host() == Some(id);
                if let Some(h) = self.hosts.lock().get_mut(&id) {
                    h.status = status;
                }
                // A host that left the console (macOS user switch) gives up its sessions.
                if was_active && self.active_host() != Some(id) {
                    if let Some(h) = self.hosts.lock().get(&id) {
                        let _ = h.tx.send(ServiceToHost::EndSessions {
                            reason: EndReason::AgentOffline,
                        });
                    }
                }
            }
            HostToService::Console(msg) => {
                if let AgentToConsole::SessionState {
                    session_id,
                    state: SessionState::Ended,
                    ..
                } = &msg
                {
                    if self.owners.lock().remove(session_id).is_some() {
                        self.idle.notify_waiters();
                    }
                }
                self.to_console(msg);
            }
            HostToService::SetOverrides(ov) => match self.hub() {
                Some(hub) => hub.set_overrides(ov),
                None => tracing::warn!("local restrictions changed before enrollment; ignored"),
            },
            HostToService::SecureAttention => {
                tracing::info!("secure attention sequence requested by the session host");
                std::thread::spawn(crate::platform::send_secure_attention_here);
            }
            HostToService::Hello { .. } => tracing::debug!("duplicate hello ignored"),
        }
    }
}

impl Backend for HostRouter {
    fn status(&self) -> HostStatus {
        if let Some(id) = self.active_host() {
            if let Some(h) = self.hosts.lock().get(&id) {
                return h.status.clone();
            }
        }
        // No desktop process yet (boot, between sessions): online, but nothing to show.
        HostStatus {
            capabilities: AgentCapabilities {
                codecs: vec![],
                displays: vec![],
                input: false,
                clipboard: false,
                privacy_screen: Default::default(),
            },
            desktop: crate::platform::desktop_state(),
            logged_in_user: crate::platform::logged_in_user(),
            on_console: true,
        }
    }

    fn deliver(&self, msg: ConsoleToAgent) {
        match &msg {
            ConsoleToAgent::SessionRequest { session_id, .. } => {
                let Some(host) = self.active_host() else {
                    tracing::warn!(%session_id, "session requested but no desktop is reachable");
                    self.to_console(AgentToConsole::SessionState {
                        session_id: session_id.clone(),
                        state: SessionState::Ended,
                        reason: Some(EndReason::ConnectionFailed),
                    });
                    return;
                };
                self.owners.lock().insert(session_id.clone(), host);
                if let Some(h) = self.hosts.lock().get(&host) {
                    let _ = h.tx.send(ServiceToHost::Console(msg));
                }
            }
            ConsoleToAgent::IceCandidate { session_id, .. }
            | ConsoleToAgent::SessionEnd { session_id, .. } => {
                let owner = self.owners.lock().get(session_id).copied();
                let hosts = self.hosts.lock();
                match owner.and_then(|o| hosts.get(&o)) {
                    Some(h) => {
                        let _ = h.tx.send(ServiceToHost::Console(msg));
                    }
                    None => {
                        tracing::debug!(%session_id, "signaling for an unknown session dropped")
                    }
                }
            }
            _ => {}
        }
    }

    fn settings_changed(&self, settings: &HostSettings) {
        *self.fallback_settings.lock() = settings.clone();
        self.broadcast(|| ServiceToHost::Settings(settings.clone()));
    }

    fn console_status(&self, connected: bool) {
        self.broadcast(|| ServiceToHost::ConsoleStatus { connected });
    }

    fn end_all(&self, reason: EndReason) {
        self.broadcast(|| ServiceToHost::EndSessions { reason });
    }

    fn has_session(&self) -> bool {
        !self.owners.lock().is_empty()
    }

    fn wait_idle(&self, timeout: Duration) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + timeout;
            while self.has_session() {
                if tokio::time::timeout_at(deadline, self.idle.notified())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    }
}

/// Accept session hosts until the process ends.
async fn accept_hosts(router: Arc<HostRouter>) -> Result<()> {
    let name = endpoint::service_endpoint();
    let mut listener = endpoint::Listener::bind(&name, Access::Privileged)
        .await
        .with_context(|| format!("listening on {name}"))?;
    tracing::info!(%name, "service link listening");
    loop {
        let (stream, peer) = listener.accept().await?;
        if let Err(e) = endpoint::check_peer(&peer) {
            tracing::warn!("{e:#}");
            continue;
        }
        let r = Arc::clone(&router);
        tokio::spawn(async move {
            if let Err(e) = r.serve_host(stream).await {
                tracing::warn!("session host link: {e:#}");
            }
        });
    }
}

/// Run the service until shutdown. `router` is created by the caller so the platform service
/// code can also reach it (Windows: stop the hosts before the supervisor exits).
pub async fn run(paths: Paths, router: Arc<HostRouter>) -> Result<()> {
    crate::updater::cleanup_after_update();
    // Branding is fetched here and cached in the machine directory; hosts re-read it.
    let weak = Arc::downgrade(&router);
    crate::branding::on_change(move |_| {
        if let Some(r) = weak.upgrade() {
            r.broadcast(|| ServiceToHost::BrandingChanged);
        }
    });
    let accept = tokio::spawn(accept_hosts(Arc::clone(&router)));
    let result = run_hub_loop(&paths, &router).await;
    router.shutdown_hosts(&crate::shutdown::reason().unwrap_or_else(|| "service stopping".into()));
    // Give hosts a moment to end their sessions and say so.
    let deadline = Instant::now() + crate::shutdown::SESSION_END_GRACE;
    while router.host_count() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    accept.abort();
    result
}

async fn run_hub_loop(paths: &Paths, router: &Arc<HostRouter>) -> Result<()> {
    loop {
        if crate::shutdown::requested() {
            return Ok(());
        }
        if !wait_for_enrollment(paths).await {
            return Ok(()); // shutdown while waiting
        }
        let local = match Hub::load(paths) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("loading the device identity: {e:#}");
                if sleep_or_shutdown(Duration::from_secs(30)).await {
                    return Ok(());
                }
                continue;
            }
        };
        let (sink, out_rx) = HubSink::channel();
        let hub = Hub::new(
            paths.clone(),
            local,
            Arc::clone(router) as Arc<dyn Backend>,
            out_rx,
            IdentityPolicy::KeepTrying,
        );
        router.attach(&hub, sink);
        let result = hub.run().await;
        router.detach();
        match result {
            Ok(()) => return Ok(()),
            Err(e) if e.is::<crate::hub::Retired>() => {
                tracing::warn!("{e:#}; idle until enrolled again");
            }
            Err(e) => {
                tracing::error!("console connection: {e:#}");
                if sleep_or_shutdown(Duration::from_secs(10)).await {
                    return Ok(());
                }
            }
        }
    }
}

/// Wait until `agent.toml` holds an identity, enrolling with a baked token when there is one.
/// Returns false when shutdown was requested first.
async fn wait_for_enrollment(paths: &Paths) -> bool {
    let mut announced = false;
    loop {
        let local = LocalConfig::load(paths).ok().flatten();
        if local.as_ref().map(|c| c.is_enrolled()).unwrap_or(false) {
            return true;
        }
        match crate::enroll::auto_enroll_if_baked(paths).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => tracing::warn!("automatic enrollment failed: {e:#}"),
        }
        if !announced {
            tracing::info!(
                config = %paths.config_file().display(),
                "not enrolled; waiting (run `remote-agent enroll --server … --token …`)"
            );
            announced = true;
        }
        if sleep_or_shutdown(Duration::from_secs(10)).await {
            return false;
        }
    }
}

/// Sleep, or return true early when shutdown is requested.
async fn sleep_or_shutdown(d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = crate::shutdown::wait() => true,
    }
}

/// Entry point for `service run` on platforms where the service manager runs the process
/// directly (launchd).
pub fn run_blocking(paths: &Paths) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        crate::shutdown::install_handlers();
        run(paths.clone(), HostRouter::new()).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::common::DesktopState;

    fn status(on_console: bool) -> HostStatus {
        HostStatus {
            capabilities: AgentCapabilities {
                codecs: vec![],
                displays: vec![],
                input: true,
                clipboard: true,
                privacy_screen: Default::default(),
            },
            desktop: DesktopState::User,
            logged_in_user: None,
            on_console,
        }
    }

    fn add(
        router: &HostRouter,
        kind: HostKind,
        on_console: bool,
    ) -> (u64, mpsc::UnboundedReceiver<ServiceToHost>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = router.next_id.fetch_add(1, Ordering::SeqCst);
        router.hosts.lock().insert(
            id,
            Host {
                tx,
                kind,
                pid: 1,
                os_session: 1,
                status: status(on_console),
                connected_at: Instant::now(),
            },
        );
        (id, rx)
    }

    #[test]
    fn user_session_on_console_beats_the_login_window() {
        let r = HostRouter::new();
        let (login, _a) = add(&r, HostKind::LoginWindow, true);
        assert_eq!(r.active_host(), Some(login));
        let (user, _b) = add(&r, HostKind::UserSession, true);
        assert_eq!(r.active_host(), Some(user));
        let (_bg, _c) = add(&r, HostKind::UserSession, false);
        assert_eq!(
            r.active_host(),
            Some(user),
            "a background session never wins"
        );
    }

    #[tokio::test]
    async fn sessions_follow_their_host_and_end_when_it_leaves() {
        let r = HostRouter::new();
        let (sink, mut console) = HubSink::channel();
        *r.sink.lock() = Some(sink);
        let (host, mut rx) = add(&r, HostKind::Console, true);
        r.deliver(ConsoleToAgent::SessionRequest {
            session_id: "s1".into(),
            operator: protocol::common::OperatorInfo {
                id: "u".into(),
                name: "Op".into(),
            },
            offer: protocol::common::SessionDescription {
                kind: "offer".into(),
                sdp: "v=0".into(),
            },
            ice_servers: vec![],
            role: Default::default(),
            shadow_of: None,
            notify_operator: true,
            privacy_screen_allowed: false,
        });
        assert!(matches!(
            rx.recv().await,
            Some(ServiceToHost::Console(
                ConsoleToAgent::SessionRequest { .. }
            ))
        ));
        assert!(r.has_session());
        r.hosts.lock().remove(&host);
        r.release_sessions_of(host, EndReason::AgentOffline);
        assert!(!r.has_session());
        assert!(matches!(
            console.recv().await,
            Some(AgentToConsole::SessionState {
                state: SessionState::Ended,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn no_host_means_the_session_fails_at_once() {
        let r = HostRouter::new();
        let (sink, mut console) = HubSink::channel();
        *r.sink.lock() = Some(sink);
        r.deliver(ConsoleToAgent::SessionRequest {
            session_id: "s2".into(),
            operator: protocol::common::OperatorInfo {
                id: "u".into(),
                name: "Op".into(),
            },
            offer: protocol::common::SessionDescription {
                kind: "offer".into(),
                sdp: "v=0".into(),
            },
            ice_servers: vec![],
            role: Default::default(),
            shadow_of: None,
            notify_operator: true,
            privacy_screen_allowed: false,
        });
        assert!(matches!(
            console.recv().await,
            Some(AgentToConsole::SessionState {
                state: SessionState::Ended,
                reason: Some(EndReason::ConnectionFailed),
                ..
            })
        ));
    }
}
