//! Small OS helpers shared by the other modules.
//!
//! * [`run_main_loop`] / [`run_on_main`] — on macOS AppKit UI (approval dialog, session
//!   indicator) must run on the main thread, so `remote-agent run` pumps an `NSApplication`
//!   run loop there and drives tokio from a worker thread. Other platforms run the worker
//!   inline.
//! * [`logged_in_user`], [`approval_dialog`], [`show_indicator`], [`secure_attention`] and
//!   permission checks are implemented per platform in the submodules.

use crate::approval::{ApprovalOutcome, IndicatorHandle};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

static MAIN_LOOP_RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether [`run_main_loop`] is pumping the platform event loop (macOS only).
pub fn main_loop_running() -> bool {
    MAIN_LOOP_RUNNING.load(Ordering::Relaxed)
}

/// Run `work` (the agent) while the platform's UI loop is served on the calling thread.
///
/// Must be called from the process main thread. On macOS `work` runs on a worker thread and
/// the process exits with its return code when it finishes; elsewhere `work` runs inline and
/// its code is returned.
pub fn run_main_loop(work: impl FnOnce() -> i32 + Send + 'static) -> i32 {
    #[cfg(target_os = "macos")]
    {
        MAIN_LOOP_RUNNING.store(true, Ordering::Relaxed);
        macos::run_app_loop(work)
    }
    #[cfg(not(target_os = "macos"))]
    {
        work()
    }
}

/// Run `f` on the main thread and wait for its result.
///
/// On macOS this requires [`run_main_loop`] to be active (or being called on the main thread
/// already); elsewhere `f` simply runs inline.
pub fn run_on_main<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    #[cfg(target_os = "macos")]
    {
        macos::run_on_main(f)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(f())
    }
}

/// Name of the user owning the interactive console session, if any.
pub fn logged_in_user() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        macos::console_user().map(|(_, name)| name)
    }
    #[cfg(target_os = "windows")]
    {
        windows::console_user()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var("USER").ok()
    }
}

/// Blocking native yes/no dialog with a timeout.
pub fn approval_dialog(operator: &str, timeout: Duration) -> Result<ApprovalOutcome> {
    #[cfg(target_os = "macos")]
    {
        macos::approval_dialog(operator, timeout)
    }
    #[cfg(target_os = "windows")]
    {
        windows::approval_dialog(operator, timeout)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (operator, timeout);
        anyhow::bail!("approval dialog is not supported on this platform")
    }
}

/// Show the always-on-top session banner; dropping the handle hides it.
pub fn show_indicator(
    operator: &str,
    on_disconnect: Arc<dyn Fn() + Send + Sync>,
) -> Result<Box<dyn IndicatorHandle>> {
    #[cfg(target_os = "macos")]
    {
        macos::show_indicator(operator, on_disconnect)
    }
    #[cfg(target_os = "windows")]
    {
        windows::show_indicator(operator, on_disconnect)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (operator, on_disconnect);
        anyhow::bail!("session indicator is not supported on this platform")
    }
}

/// Send the secure attention sequence (Ctrl+Alt+Del) where the platform has one.
pub fn secure_attention() {
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = windows::send_sas() {
            tracing::warn!("SendSAS failed: {e:#}");
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        tracing::info!("secure attention sequence not applicable on this platform");
    }
}

/// Whether screen capture is permitted (always true outside macOS).
pub fn screen_capture_allowed() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::screen_capture_allowed()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Whether input injection is permitted (Accessibility on macOS; always true elsewhere).
pub fn accessibility_allowed() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::accessibility_allowed()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// `remote-agent doctor`: prerequisites report.
pub fn doctor(paths: &crate::config::Paths) -> Result<()> {
    println!("remote-agent {}", crate::AGENT_VERSION);
    println!(
        "os              : {:?} / {:?}",
        protocol::common::Os::current(),
        protocol::common::Arch::current()
    );
    println!("config dir      : {}", paths.dir.display());
    match crate::config::LocalConfig::load(paths) {
        Ok(Some(cfg)) if cfg.is_enrolled() => {
            println!(
                "enrolled        : yes ({} as {})",
                cfg.server_url, cfg.device_id
            )
        }
        Ok(_) => println!("enrolled        : no"),
        Err(e) => println!("enrolled        : error reading config: {e:#}"),
    }
    println!(
        "logged-in user  : {}",
        logged_in_user().unwrap_or_else(|| "unknown".into())
    );

    #[cfg(target_os = "macos")]
    {
        let capture = screen_capture_allowed();
        println!(
            "screen recording: {}",
            if capture { "granted" } else { "NOT granted" }
        );
        if !capture && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            println!("                  requesting permission (System Settings → Privacy → Screen Recording)…");
            macos::request_screen_capture();
        }
        let ax = accessibility_allowed();
        println!(
            "accessibility   : {}",
            if ax {
                "granted"
            } else {
                "NOT granted (System Settings → Privacy → Accessibility)"
            }
        );
    }

    match crate::capture::list_displays() {
        Ok(d) => {
            println!("displays        : {}", d.len());
            for disp in d {
                println!(
                    "                  [{}] {} {}x{} @{} ({},{}){}",
                    disp.index,
                    disp.name,
                    disp.width,
                    disp.height,
                    disp.scale,
                    disp.x,
                    disp.y,
                    if disp.primary { " primary" } else { "" }
                );
            }
        }
        Err(e) => println!("displays        : error: {e:#}"),
    }
    println!("encoders        : {:?}", crate::encode::available_codecs());

    if let Ok(Some(cfg)) = crate::config::LocalConfig::load(paths) {
        if cfg.is_enrolled() {
            let rt = tokio::runtime::Runtime::new()?;
            let reachable = rt.block_on(async {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .ok()?;
                let url = format!("{}/api/info", cfg.server_url.trim_end_matches('/'));
                client.get(url).send().await.ok().map(|r| r.status())
            });
            match reachable {
                Some(status) => println!("console         : reachable (HTTP {status})"),
                None => println!("console         : NOT reachable"),
            }
        }
    }
    Ok(())
}
