//! Windows service management and the SCM entry point.
//!
//! The service runs as **LocalSystem** and auto-starts at boot. Because a LocalSystem service
//! lives in session 0 (no visible desktop), `service run` does not capture directly; instead
//! it *supervises* a child `remote-agent run` launched in the active console session with the
//! logged-in user's token (`CreateProcessAsUser` on `winsta0\default`). The child is restarted
//! when it exits or when the active session changes (fast user switching, RDP).

use crate::cli::ServiceAction;
use crate::config::Paths;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const SERVICE_NAME: &str = "RemoteAgent";
const DISPLAY_NAME: &str = "Remote Agent";

pub fn handle(paths: &Paths, action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install => install(),
        ServiceAction::Uninstall => uninstall(),
        ServiceAction::Start => start(),
        ServiceAction::Stop => stop(),
        ServiceAction::Run => run_service(paths),
    }
}

fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("locating current executable")
}

fn install() -> Result<()> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)
            .context("opening service manager (run as administrator)")?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path()?,
        // The SCM appends these to the ImagePath; `service run` is the SCM entry point.
        launch_arguments: vec![OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .context("creating service")?;
    service
        .set_description("Remote support agent: screen sharing and remote control")
        .ok();
    service.start(&[] as &[&str]).ok();
    println!("installed and started service {SERVICE_NAME}");
    Ok(())
}

fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening service manager (run as administrator)")?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        )
        .context("opening service")?;
    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            let _ = service.stop();
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    service.delete().context("deleting service")?;
    println!("removed service {SERVICE_NAME}");
    Ok(())
}

fn start() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(SERVICE_NAME, ServiceAccess::START)?;
    service.start(&[] as &[&str]).context("starting service")?;
    println!("started {SERVICE_NAME}");
    Ok(())
}

fn stop() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(SERVICE_NAME, ServiceAccess::STOP)?;
    service.stop().context("stopping service")?;
    println!("stopped {SERVICE_NAME}");
    Ok(())
}

// ─── SCM entry point ────────────────────────────────────────────────────────────────────

#[cfg(feature = "winservice")]
mod scm {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    windows_service::define_windows_service!(ffi_service_main, service_main);

    pub(super) fn dispatch() -> Result<()> {
        windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .context("starting service dispatcher")?;
        Ok(())
    }

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run() {
            tracing::error!("service failed: {e:#}");
        }
    }

    fn run() -> Result<()> {
        let stop = Arc::new(AtomicBool::new(false));
        let (session_tx, session_rx) = mpsc::channel::<()>();
        let stop_handler = Arc::clone(&stop);
        let session_tx2 = session_tx.clone();

        let status_handle = service_control_handler::register(SERVICE_NAME, move |control| {
            match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    stop_handler.store(true, Ordering::SeqCst);
                    let _ = session_tx2.send(());
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::SessionChange(_) => {
                    // Active session changed: restart the child in the new session.
                    let _ = session_tx2.send(());
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        })
        .context("registering service control handler")?;

        let running = ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP
                | ServiceControlAccept::SHUTDOWN
                | ServiceControlAccept::SESSION_CHANGE,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        };
        status_handle.set_service_status(running.clone())?;

        supervise(&stop, &session_rx);

        status_handle.set_service_status(ServiceStatus {
            current_state: ServiceState::Stopped,
            ..running
        })?;
        Ok(())
    }

    /// Keep one `remote-agent run` child alive in the active console session.
    fn supervise(stop: &Arc<AtomicBool>, session_rx: &mpsc::Receiver<()>) {
        let exe = match exe_path() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("cannot locate executable: {e:#}");
                return;
            }
        };
        let command_line = format!("\"{}\" run", exe.display());

        while !stop.load(Ordering::SeqCst) {
            match crate::platform::windows::spawn_in_active_session(&command_line) {
                Ok(child) => {
                    tracing::info!(session = child.session, "started agent child");
                    // Wait until the child exits, a session change / stop is signalled.
                    loop {
                        if stop.load(Ordering::SeqCst) {
                            child.terminate();
                            return;
                        }
                        if child.wait(Duration::from_millis(500)) {
                            tracing::warn!("agent child exited; restarting");
                            break;
                        }
                        // Session change → restart in the (possibly new) active session.
                        if session_rx.try_recv().is_ok() {
                            if stop.load(Ordering::SeqCst) {
                                child.terminate();
                                return;
                            }
                            tracing::info!("session changed; restarting agent child");
                            child.terminate();
                            break;
                        }
                    }
                }
                Err(e) => {
                    // No user logged in yet, or transient failure: wait and retry.
                    tracing::debug!("cannot start agent child: {e:#}");
                    let _ = session_rx.recv_timeout(Duration::from_secs(3));
                }
            }
        }
    }
}

fn run_service(_paths: &Paths) -> Result<()> {
    #[cfg(feature = "winservice")]
    {
        scm::dispatch()
    }
    #[cfg(not(feature = "winservice"))]
    {
        bail!("this binary was built without the `winservice` feature");
    }
}
