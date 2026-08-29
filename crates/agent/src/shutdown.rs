//! Coordinated process shutdown.
//!
//! Every "please stop" arrives here: `SIGTERM` / `SIGINT` / `SIGHUP` (`launchctl kill`,
//! `launchctl bootout`, Ctrl-C), the Windows console control events, the tray *Quit* item and
//! the updater. Until now none of them were handled — the process simply died, leaving the
//! active session without a `session_state: ended`, injected modifier keys possibly held down
//! and the console waiting for a heartbeat timeout.
//!
//! [`request`] flips a process-wide flag and wakes every [`wait`]er. The hub reacts by ending
//! the active session (keys released, overlays and the session bar removed, `ended` sent),
//! flushing what the session still wants to say, closing the console socket and returning; the
//! agent loop then exits with the worker's code. Shipped builds use `panic = "abort"`, so
//! nothing here relies on destructors: the paths that need cleanup poll [`requested`] or await
//! [`wait`] explicitly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static REQUESTED: AtomicBool = AtomicBool::new(false);
static NOTIFY: tokio::sync::Notify = tokio::sync::Notify::const_new();
static REASON: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

/// How long the worker gets to finish cleanly after a *Quit* before the process is forced out.
pub const FORCE_EXIT_AFTER: Duration = Duration::from_secs(6);

/// How long the hub waits for the active session to end before closing the console socket.
pub const SESSION_END_GRACE: Duration = Duration::from_secs(3);

/// Ask the process to shut down. The first call wins and is logged with its `reason`; later
/// calls are no-ops.
pub fn request(reason: &str) {
    if REQUESTED.swap(true, Ordering::SeqCst) {
        tracing::debug!(reason, "shutdown already requested");
        return;
    }
    *REASON.lock() = Some(reason.to_string());
    tracing::info!(reason, "shutdown requested");
    NOTIFY.notify_waiters();
    NOTIFY.notify_one();
}

/// Whether a shutdown has been requested.
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// Why the shutdown was requested (the first reason given).
pub fn reason() -> Option<String> {
    REASON.lock().clone()
}

/// Resolve once a shutdown has been requested (immediately if it already was).
pub async fn wait() {
    loop {
        if requested() {
            return;
        }
        let notified = NOTIFY.notified();
        tokio::pin!(notified);
        // Register before re-checking the flag so a `request` in between cannot be missed.
        notified.as_mut().enable();
        if requested() {
            return;
        }
        notified.await;
    }
}

/// Listen for the OS stop signals and turn them into [`request`]. Must be called from within a
/// tokio runtime (the listeners are spawned on it). Idempotent.
pub fn install_handlers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            for (kind, name) in [
                (SignalKind::terminate(), "SIGTERM"),
                (SignalKind::interrupt(), "SIGINT"),
                (SignalKind::hangup(), "SIGHUP"),
            ] {
                match signal(kind) {
                    Ok(mut stream) => {
                        tokio::spawn(async move {
                            if stream.recv().await.is_some() {
                                request(name);
                            }
                        });
                    }
                    Err(e) => tracing::warn!("installing the {name} handler: {e}"),
                }
            }
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows;
            macro_rules! listen {
                ($ctor:expr, $name:literal) => {
                    match $ctor {
                        Ok(mut stream) => {
                            tokio::spawn(async move {
                                if stream.recv().await.is_some() {
                                    request($name);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::debug!(concat!("installing the ", $name, " handler: {}"), e)
                        }
                    }
                };
            }
            listen!(windows::ctrl_c(), "CTRL_C");
            listen!(windows::ctrl_break(), "CTRL_BREAK");
            listen!(windows::ctrl_close(), "CTRL_CLOSE");
            listen!(windows::ctrl_logoff(), "CTRL_LOGOFF");
            listen!(windows::ctrl_shutdown(), "CTRL_SHUTDOWN");
        }
        tracing::debug!("shutdown signal handlers installed");
    });
}

/// Backstop for the interactive *Quit* paths: give the worker [`FORCE_EXIT_AFTER`] to finish
/// cleanly, then leave regardless (a wedged session must not keep the process alive after the
/// user asked it to quit).
pub fn force_exit_after(delay: Duration, code: i32) {
    std::thread::Builder::new()
        .name("force-exit".into())
        .spawn(move || {
            std::thread::sleep(delay);
            tracing::warn!("shutdown did not complete within {delay:?}; exiting");
            std::process::exit(code);
        })
        .ok();
}

/// *Quit* from the tray / menu bar: request a clean shutdown and arm the backstop.
pub fn quit(reason: &str) {
    request(reason);
    force_exit_after(FORCE_EXIT_AFTER, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_resolves_after_request_and_immediately_afterwards() {
        // The flag is process-global; this is the only test that touches it in this binary's
        // unit tests, and it only ever sets it.
        let waiter = tokio::spawn(async {
            wait().await;
            true
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        request("unit test");
        request("second call is ignored");
        assert!(tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter woke")
            .unwrap());
        assert!(requested());
        assert_eq!(reason().as_deref(), Some("unit test"));
        // A later waiter must not block.
        tokio::time::timeout(Duration::from_millis(200), wait())
            .await
            .expect("wait returns at once after a request");
    }
}
