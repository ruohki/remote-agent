//! A session host: runs sessions inside one desktop session on behalf of the service.
//!
//! The service owns the console connection; it relays session signaling here over the local
//! link ([`crate::link`]) and forwards what the sessions say back to the console. The host
//! reports what its desktop can do ([`HostStatus`]) whenever that changes — on Windows the
//! same host serves the sign-in screen, the lock screen, UAC prompts and the user's desktop,
//! so its desktop state moves with the console.
//!
//! When the service goes away (restart, update) the host ends its sessions and reconnects;
//! when the service says [`ServiceToHost::Shutdown`] it ends them and exits.

use crate::hub::HubSink;
use crate::link::endpoint;
use crate::link::frame::{read_message, spawn_writer};
use crate::link::{HostKind, HostStatus, HostToService, ServiceToHost};
use crate::local_sessions::{LocalSessions, UiDeps};
use anyhow::{anyhow, Context, Result};
use protocol::common::EndReason;
use protocol::config::AgentConfig;
use std::sync::Arc;
use std::time::Duration;

/// How often the host re-reads its desktop state and capabilities.
const STATUS_POLL: Duration = Duration::from_secs(2);

/// Why the host loop ended.
enum Ended {
    /// Shutdown requested (signal or the service said so): exit the process.
    Exit,
    /// The service link dropped: reconnect.
    LinkLost,
}

/// Run a session host until shutdown.
pub async fn run(kind: HostKind) -> Result<()> {
    crate::shutdown::install_handlers();
    let ui = if crate::app::remote::is_host() {
        UiDeps::remote_app()
    } else {
        UiDeps::detect()
    };
    if crate::platform::main_loop_running() {
        crate::platform::install_input_guard();
    }
    let (sink, mut out_rx) = HubSink::channel();
    let sessions = LocalSessions::new(sink, AgentConfig::default(), ui);
    sessions.wire_app();

    let mut delay = Duration::from_millis(250);
    loop {
        match connect_and_serve(kind, &sessions, &mut out_rx).await {
            Ok(Ended::Exit) => {
                tracing::info!("session host stopping");
                return Ok(());
            }
            Ok(Ended::LinkLost) => {
                tracing::warn!("service link lost; reconnecting");
                delay = Duration::from_millis(250);
            }
            Err(e) => tracing::debug!("service link: {e:#}"),
        }
        sessions.sessions.end_all(EndReason::AgentOffline);
        crate::app::set_console_status(false);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = crate::shutdown::wait() => {
                sessions.sessions.wait_idle(crate::shutdown::SESSION_END_GRACE).await;
                return Ok(());
            }
        }
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

async fn connect_and_serve(
    kind: HostKind,
    sessions: &Arc<LocalSessions>,
    out_rx: &mut tokio::sync::mpsc::UnboundedReceiver<protocol::agent::AgentToConsole>,
) -> Result<Ended> {
    let name = endpoint::service_endpoint();
    let stream = tokio::select! {
        r = endpoint::connect(&name) => r?,
        _ = crate::shutdown::wait() => return Ok(Ended::Exit),
    };
    let (mut read, write) = tokio::io::split(stream);
    let (tx, writer) = spawn_writer::<_, HostToService>(write);

    let mut status = sessions.host_status();
    tx.send(HostToService::Hello {
        version: crate::AGENT_VERSION.to_string(),
        pid: std::process::id(),
        os_session: crate::platform::os_session_id(),
        kind,
        status: status.clone(),
    })
    .map_err(|_| anyhow!("link writer closed"))?;

    let welcome = tokio::time::timeout(
        Duration::from_secs(10),
        read_message::<_, ServiceToHost>(&mut read),
    )
    .await
    .context("no welcome from the service")??
    .context("service closed the link before the welcome")?;
    let ServiceToHost::Welcome {
        device_id,
        server_url,
        console_connected,
        settings,
    } = welcome
    else {
        return Err(anyhow!("expected welcome, got {welcome:?}"));
    };
    tracing::info!(%device_id, console_connected, "connected to the service");
    sessions.apply_settings(&settings);
    crate::app::set_device_info(&settings.config.display_name, &device_id);
    crate::app::set_console_url(&server_url);
    crate::app::set_console_status(console_connected);

    // Device-side restriction changes and Ctrl+Alt+Del go to the service.
    let t = tx.clone();
    let overrides = Arc::new(move |ov| {
        let _ = t.send(HostToService::SetOverrides(ov));
    });
    crate::app::set_overrides_handler(overrides.clone());
    crate::app::remote::set_overrides_sink(overrides);
    let t = tx.clone();
    crate::platform::set_secure_attention_handler(Arc::new(move || {
        let _ = t.send(HostToService::SecureAttention);
    }));

    let mut poll = tokio::time::interval(STATUS_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
        tokio::select! {
            msg = out_rx.recv() => {
                let Some(msg) = msg else { break Ok(Ended::Exit) };
                if tx.send(HostToService::Console(msg)).is_err() {
                    break Ok(Ended::LinkLost);
                }
            }
            frame = read_message::<_, ServiceToHost>(&mut read) => {
                match frame {
                    Ok(Some(msg)) => {
                        if let Some(end) = handle(msg, sessions, &device_id).await {
                            break Ok(end);
                        }
                    }
                    Ok(None) => break Ok(Ended::LinkLost),
                    Err(e) => {
                        tracing::warn!("service link: {e:#}");
                        break Ok(Ended::LinkLost);
                    }
                }
            }
            _ = poll.tick() => {
                let now = sessions.host_status();
                if now != status {
                    log_status_change(&status, &now);
                    status = now.clone();
                    let _ = tx.send(HostToService::Status(now));
                }
            }
            _ = crate::shutdown::wait() => {
                sessions.sessions.end_all(EndReason::AgentOffline);
                drain_until_idle(sessions, out_rx, &tx).await;
                break Ok(Ended::Exit);
            }
        }
    };
    // Let the last frames (session_state: ended) reach the service.
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    result
}

/// Forward session output until the sessions are idle (bounded by the session-end grace).
async fn drain_until_idle(
    sessions: &Arc<LocalSessions>,
    out_rx: &mut tokio::sync::mpsc::UnboundedReceiver<protocol::agent::AgentToConsole>,
    tx: &tokio::sync::mpsc::UnboundedSender<HostToService>,
) {
    let idle = sessions
        .sessions
        .wait_idle(crate::shutdown::SESSION_END_GRACE);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            msg = out_rx.recv() => match msg {
                Some(m) => { let _ = tx.send(HostToService::Console(m)); }
                None => break,
            },
            _ = &mut idle => break,
        }
    }
    while let Ok(m) = out_rx.try_recv() {
        let _ = tx.send(HostToService::Console(m));
    }
}

async fn handle(
    msg: ServiceToHost,
    sessions: &Arc<LocalSessions>,
    device_id: &str,
) -> Option<Ended> {
    match msg {
        ServiceToHost::Console(m) => sessions.deliver(m),
        ServiceToHost::Settings(s) => {
            sessions.apply_settings(&s);
            crate::app::set_device_info(&s.config.display_name, device_id);
        }
        ServiceToHost::ConsoleStatus { connected } => crate::app::set_console_status(connected),
        ServiceToHost::BrandingChanged => {
            crate::branding::reload();
            crate::app::refresh_branding();
        }
        ServiceToHost::EndSessions { reason } => sessions.sessions.end_all(reason),
        ServiceToHost::Shutdown { reason } => {
            tracing::info!(%reason, "service asked the host to stop");
            crate::shutdown::request(&reason);
            return None; // the shutdown arm drains and exits
        }
        ServiceToHost::Welcome { .. } => tracing::debug!("duplicate welcome ignored"),
    }
    None
}

fn log_status_change(old: &HostStatus, new: &HostStatus) {
    if old.desktop != new.desktop {
        tracing::info!(from = ?old.desktop, to = ?new.desktop, "desktop changed");
    }
    if old.capabilities.displays != new.capabilities.displays {
        tracing::info!(
            displays = new.capabilities.displays.len(),
            "displays changed"
        );
    }
    if old.on_console != new.on_console {
        tracing::info!(on_console = new.on_console, "console ownership changed");
    }
}
