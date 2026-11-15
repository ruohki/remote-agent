//! Windows service management and the SCM entry point.
//!
//! The service (`RemoteAgent`, LocalSystem, automatic start) is the agent on an installed
//! machine:
//!
//! * the **daemon** ([`crate::daemon`]) runs on a tokio runtime inside the service process:
//!   identity, console connection, policy, updates, Ctrl+Alt+Del;
//! * the **supervisor** (this module, on the SCM thread) keeps two children alive in the
//!   session attached to the physical console:
//!   - the **session host** (`session-host`), started with the service's SYSTEM token moved
//!     into that session — it follows the input desktop, so it works on the sign-in screen,
//!     the lock screen, UAC prompts and the user's desktop alike, with nobody signed in;
//!   - the **UI** (`ui --session N`), started with the signed-in user's token while someone
//!     is signed in: tray, window, chat, session bar, approval prompts, privacy screen.
//!
//! Children are restarted with backoff when they exit and replaced when the console moves to
//! another session (fast user switching, sign-out). Locking the screen changes nothing: the
//! host simply follows the lock screen. Stopping asks the host over the link to end its
//! session first; only stragglers are terminated. A kill-on-close job ties both children to the
//! service process, so a crashed service leaves nothing behind.

use crate::cli::ServiceAction;
use crate::config::Paths;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;
#[cfg(feature = "winservice")]
use std::sync::mpsc;
use std::time::Duration;
use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const SERVICE_NAME: &str = "RemoteAgent";
const DISPLAY_NAME: &str = "Remote Agent";
/// Service-specific exit code after an update: the recovery actions restart the service
/// (with the new executable) because the exit counts as a failure.
#[cfg(feature = "winservice")]
const EXIT_RESTART_FOR_UPDATE: u32 = 0x5250;

pub fn handle(paths: &Paths, action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install => install(paths),
        ServiceAction::Uninstall => uninstall(),
        ServiceAction::Start => start(),
        ServiceAction::Stop => stop(),
        ServiceAction::Run => run_service(paths),
    }
}

fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("locating current executable")
}

fn service_info() -> Result<ServiceInfo> {
    Ok(ServiceInfo {
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
    })
}

/// Install or update the service, configure recovery, prepare the machine, start it.
/// Idempotent: re-running it (installer re-run, upgrade, RMM retry) updates in place.
fn install(paths: &Paths) -> Result<()> {
    use windows_service::service::{
        ServiceAction as ScAction, ServiceActionType, ServiceFailureActions,
        ServiceFailureResetPeriod,
    };
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("opening service manager (run as administrator)")?;
    let info = service_info()?;
    let access = ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::QUERY_STATUS;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(existing) => {
            existing
                .change_config(&info)
                .context("updating the existing service")?;
            println!("updated service {SERVICE_NAME}");
            existing
        }
        Err(_) => {
            let created = manager
                .create_service(&info, access)
                .context("creating service")?;
            println!("installed service {SERVICE_NAME}");
            created
        }
    };
    service
        .set_description("Remote support agent: screen sharing and remote control")
        .ok();
    // Restart after crashes and after an update (which exits with a failure code on purpose).
    let actions = ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ScAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(2),
            },
            ScAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(10),
            },
            ScAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(30),
            },
        ]),
    };
    if let Err(e) = service.update_failure_actions(actions) {
        eprintln!("warning: configuring service recovery failed: {e}");
    }
    if let Err(e) = service.set_failure_actions_on_non_crash_failures(true) {
        eprintln!("warning: configuring service recovery failed: {e}");
    }
    match crate::platform::windows::allow_service_sas() {
        Ok(true) => println!("allowed services to send Ctrl+Alt+Del (SoftwareSASGeneration)"),
        Ok(false) => {}
        Err(e) => eprintln!("warning: Ctrl+Alt+Del will not work: {e:#}"),
    }
    migrate_identity(paths);

    // (Re)start so the running service is the binary just installed.
    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            let _ = service.stop();
            wait_for(&service, ServiceState::Stopped, Duration::from_secs(20));
        }
    }
    service.start(&[] as &[&str]).context("starting service")?;
    println!("started service {SERVICE_NAME}");
    Ok(())
}

/// Bring an identity enrolled under the old layout to where the service can use it: a
/// per-user config (enrolled from the app) is copied to the machine directory, and a secret
/// protected for one user is re-protected for the machine. Runs as the installing admin.
fn migrate_identity(paths: &Paths) {
    use crate::config::LocalConfig;
    let machine = Paths::machine();
    let machine_enrolled = LocalConfig::load(&machine)
        .ok()
        .flatten()
        .map(|c| c.is_enrolled())
        .unwrap_or(false);
    if !machine_enrolled && !paths.is_machine() {
        if let Ok(Some(user_cfg)) = LocalConfig::load(paths) {
            if user_cfg.is_enrolled() {
                match crate::secrets::load(paths, &user_cfg) {
                    Ok((secret, _)) => {
                        let mut cfg = user_cfg.clone();
                        crate::secrets::store(&machine, &mut cfg, &secret);
                        match cfg.save(&machine) {
                            Ok(()) => println!(
                                "moved the enrollment from {} to {}",
                                paths.dir.display(),
                                machine.dir.display()
                            ),
                            Err(e) => eprintln!("warning: moving the enrollment: {e:#}"),
                        }
                    }
                    Err(e) => eprintln!("warning: reading the existing enrollment: {e:#}"),
                }
            }
        }
    }
    match crate::secrets::reprotect_for_machine(&machine) {
        Ok(true) => println!("device secret re-protected for the service"),
        Ok(false) => {}
        Err(e) => eprintln!("warning: {e:#}"),
    }
}

fn wait_for(service: &windows_service::service::Service, state: ServiceState, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        match service.query_status() {
            Ok(s) if s.current_state == state => return,
            Ok(_) => std::thread::sleep(Duration::from_millis(250)),
            Err(_) => return,
        }
    }
}

fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening service manager (run as administrator)")?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(s) => s,
        Err(_) => {
            println!("service {SERVICE_NAME} is not installed");
            return Ok(());
        }
    };
    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            let _ = service.stop();
            wait_for(&service, ServiceState::Stopped, Duration::from_secs(20));
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
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
    )?;
    service.stop().context("stopping service")?;
    wait_for(&service, ServiceState::Stopped, Duration::from_secs(20));
    println!("stopped {SERVICE_NAME}");
    Ok(())
}

/// Restart the service when it runs (after `enroll`, so it picks the new identity up).
pub fn restart_if_running() {
    let Ok(manager) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return;
    };
    let Ok(service) = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    ) else {
        return;
    };
    if matches!(service.query_status(), Ok(s) if s.current_state == ServiceState::Running) {
        let _ = service.stop();
        wait_for(&service, ServiceState::Stopped, Duration::from_secs(20));
        if service.start(&[] as &[&str]).is_ok() {
            println!("restarted service {SERVICE_NAME}");
        }
    }
}

// ─── SCM entry point ────────────────────────────────────────────────────────────────────

#[cfg(feature = "winservice")]
mod scm {
    use super::*;
    use crate::daemon::HostRouter;
    use crate::platform::windows::{
        active_console_session, session_user, spawn_in_active_session, spawn_system_in_session,
        ActiveProcess, KillOnCloseJob,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;
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
        let (wake_tx, wake_rx) = mpsc::channel::<()>();
        let stop_handler = Arc::clone(&stop);
        let wake = wake_tx.clone();

        let status_handle = service_control_handler::register(SERVICE_NAME, move |control| {
            match control {
                ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                    stop_handler.store(true, Ordering::SeqCst);
                    crate::shutdown::request("service stop");
                    let _ = wake.send(());
                    ServiceControlHandlerResult::NoError
                }
                // Any session event: the supervisor re-checks which session is on the console.
                // Lock / unlock need nothing (the host follows the lock screen), but waking
                // early is harmless.
                ServiceControl::SessionChange(_) => {
                    let _ = wake.send(());
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

        // The daemon: its own thread and runtime.
        let paths = Paths::machine();
        let router = HostRouter::new();
        let daemon_router = Arc::clone(&router);
        let daemon_stop = Arc::clone(&stop);
        let daemon_wake = wake_tx.clone();
        let daemon = std::thread::Builder::new()
            .name("daemon".into())
            .spawn(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!("service runtime: {e}");
                        return;
                    }
                };
                if let Err(e) = rt.block_on(crate::daemon::run(paths, daemon_router)) {
                    tracing::error!("service: {e:#}");
                }
                // An update (or a fatal error) ends the daemon: stop the service too.
                daemon_stop.store(true, Ordering::SeqCst);
                let _ = daemon_wake.send(());
            })
            .context("starting the daemon thread")?;

        supervise(&stop, &wake_rx, &router);

        status_handle.set_service_status(ServiceStatus {
            current_state: ServiceState::StopPending,
            wait_hint: Duration::from_secs(10),
            ..running.clone()
        })?;
        crate::shutdown::request("service stop");
        let _ = daemon.join();

        let updated = crate::shutdown::reason().as_deref() == Some("update applied");
        status_handle.set_service_status(ServiceStatus {
            current_state: ServiceState::Stopped,
            exit_code: if updated {
                ServiceExitCode::ServiceSpecific(EXIT_RESTART_FOR_UPDATE)
            } else {
                ServiceExitCode::Win32(0)
            },
            ..running
        })?;
        Ok(())
    }

    /// One supervised child with restart backoff.
    struct Child {
        proc: Option<ActiveProcess>,
        started: Instant,
        backoff: Duration,
        next_try: Instant,
        what: &'static str,
    }

    impl Child {
        fn new(what: &'static str) -> Self {
            Self {
                proc: None,
                started: Instant::now(),
                backoff: Duration::from_secs(1),
                next_try: Instant::now(),
                what,
            }
        }

        /// Reap an exited child and schedule the restart (exponential while it keeps dying
        /// young, reset after a minute of uptime).
        fn reap(&mut self) {
            let exited = self
                .proc
                .as_ref()
                .map(|p| p.wait(Duration::ZERO))
                .unwrap_or(false);
            if !exited {
                return;
            }
            let lived = self.started.elapsed();
            self.proc = None;
            if lived > Duration::from_secs(60) {
                self.backoff = Duration::from_secs(1);
            } else {
                self.backoff = (self.backoff * 2).min(Duration::from_secs(60));
            }
            self.next_try = Instant::now() + self.backoff;
            tracing::warn!(
                child = self.what,
                lived_s = lived.as_secs(),
                retry_in_s = self.backoff.as_secs(),
                "child exited"
            );
        }

        fn session(&self) -> Option<u32> {
            self.proc.as_ref().map(|p| p.session)
        }

        fn running(&self) -> bool {
            self.proc.is_some()
        }

        fn started(&mut self, p: ActiveProcess, job: &Option<KillOnCloseJob>) {
            if let Some(job) = job {
                job.assign(&p);
            }
            tracing::info!(
                child = self.what,
                pid = p.pid,
                session = p.session,
                "started"
            );
            self.proc = Some(p);
            self.started = Instant::now();
        }

        fn failed(&mut self, e: anyhow::Error) {
            self.backoff = (self.backoff * 2).min(Duration::from_secs(60));
            self.next_try = Instant::now() + self.backoff;
            tracing::debug!(child = self.what, "cannot start: {e:#}");
        }

        /// Wait up to `grace` for a voluntary exit, then terminate.
        fn stop(&mut self, grace: Duration) {
            if let Some(p) = self.proc.take() {
                if !p.wait(grace) {
                    tracing::warn!(child = self.what, pid = p.pid, "did not exit; terminating");
                    p.terminate();
                }
            }
        }
    }

    /// Keep the session host and the UI alive in the console session.
    fn supervise(stop: &Arc<AtomicBool>, wake: &mpsc::Receiver<()>, router: &Arc<HostRouter>) {
        let exe = match exe_path() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("cannot locate executable: {e:#}");
                return;
            }
        };
        let job = KillOnCloseJob::new()
            .map_err(|e| tracing::warn!("job object: {e:#}"))
            .ok();
        let mut host = Child::new("session host");
        let mut ui = Child::new("ui");

        while !stop.load(Ordering::SeqCst) {
            host.reap();
            ui.reap();
            let console = active_console_session();

            // The console moved to another session: replace both children.
            if host.running() && host.session() != console {
                tracing::info!(from = ?host.session(), to = ?console, "console session changed");
                router.shutdown_hosts("console session changed");
                host.stop(Duration::from_secs(4));
                ui.stop(Duration::from_secs(1));
                host.next_try = Instant::now();
                ui.next_try = Instant::now();
            }
            if ui.running() && ui.session() != console {
                ui.stop(Duration::from_secs(1));
            }

            if let Some(session) = console {
                if !host.running() && Instant::now() >= host.next_try {
                    let cmd = format!("\"{}\" session-host", exe.display());
                    match spawn_system_in_session(&cmd, session) {
                        Ok(p) => host.started(p, &job),
                        Err(e) => host.failed(e),
                    }
                }
                // The UI needs someone signed in (it runs with their token).
                let signed_in = session_user(session).is_some();
                if host.running() && signed_in && !ui.running() && Instant::now() >= ui.next_try {
                    let cmd = format!("\"{}\" ui --session {session}", exe.display());
                    match spawn_in_active_session(&cmd) {
                        Ok(p) if p.session == session => ui.started(p, &job),
                        Ok(p) => {
                            // The console moved between the two calls; try again.
                            p.terminate();
                        }
                        Err(e) => ui.failed(e),
                    }
                }
            }
            let _ = wake.recv_timeout(Duration::from_millis(500));
        }

        // Stopping: the daemon tells the host to end its session; give it time to say so.
        router.shutdown_hosts(
            &crate::shutdown::reason().unwrap_or_else(|| "service stopping".into()),
        );
        host.stop(Duration::from_secs(6));
        ui.stop(Duration::from_secs(1));
        drop(job);
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

/// Whether the `RemoteAgent` service exists in the SCM.
pub fn is_installed() -> bool {
    let Ok(manager) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return false;
    };
    manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
        .is_ok()
}

/// Re-launch `remote-agent service install` with a UAC elevation prompt.
pub fn install_elevated(exe: &std::path::Path) -> Result<String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let verb = HSTRING::from("runas");
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from("service install");
    // SAFETY: valid wide strings; ShellExecuteW returns an HINSTANCE-sized status (>32 = ok).
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_HIDE,
        )
    };
    if result.0 as isize > 32 {
        Ok("Installed. It will start automatically at boot.".to_string())
    } else if result.0 as isize == 1223 {
        bail!("cancelled")
    } else {
        bail!("elevation failed (code {})", result.0 as isize)
    }
}
