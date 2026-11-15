use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "remote-agent", version, about = "Remote desktop agent")]
pub struct Cli {
    /// Override the configuration directory (default: platform config dir).
    #[arg(long, global = true, env = "REMOTE_AGENT_CONFIG_DIR")]
    pub config_dir: Option<std::path::PathBuf>,

    /// Log filter, e.g. `info` or `remote_agent=debug`.
    #[arg(long, global = true, env = "REMOTE_AGENT_LOG", default_value = "info")]
    pub log: String,

    /// Allow plain http:// / ws:// console URLs outside localhost and private networks
    /// (credentials travel unencrypted — development only).
    #[arg(long, global = true, env = "REMOTE_AGENT_INSECURE")]
    pub insecure: bool,

    /// No subcommand launches the branded application window (double-click).
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the agent in the foreground (what the service / launch agent executes).
    Run,
    /// Print the baked (branding / config) trailer of this binary, if any.
    BakeInfo,
    /// Enroll this machine with a management console using an enrollment token.
    Enroll {
        /// Console base URL, e.g. https://remote.example.com
        #[arg(long)]
        server: String,
        /// Enrollment token created in the console.
        #[arg(long)]
        token: String,
        /// Optional display name (defaults to hostname).
        #[arg(long)]
        name: Option<String>,
        /// Do nothing when this machine is already enrolled with this console (installer
        /// re-runs, RMM retries: the token may be used up by then).
        #[arg(long)]
        if_needed: bool,
    },
    /// Manage the background service (launchd on macOS, Windows service on Windows).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Print enrollment and connectivity status.
    Status,
    /// Check capture / encoder / permission prerequisites and print a report.
    Doctor,
    /// Remove enrollment and local state (does not uninstall the service).
    Reset,
    /// Work inside a desktop session for the installed service (started by the service on
    /// Windows, by launchd on macOS).
    #[command(hide = true)]
    SessionHost {
        /// macOS: serve the login window (nobody signed in) instead of a user session.
        #[arg(long)]
        login_window: bool,
        /// Started by hand (tests, diagnostics) rather than by the service manager.
        #[arg(long)]
        manual: bool,
    },
    /// Windows: the signed-in user's tray, window, chat and prompts for the SYSTEM session host
    /// of `--session`.
    #[command(hide = true)]
    Ui {
        #[arg(long)]
        session: u32,
    },
    /// Measure whether this agent's own windows (and a would-be privacy screen) stay out of
    /// the screen capture on this machine. Field diagnostic; shows test windows briefly.
    #[command(hide = true)]
    PrivacyProbe {
        /// Only probe this display index (default: every display).
        #[arg(long)]
        display: Option<u32>,
        /// Print the report as JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Skip the tests that show the real session bar and annotation overlay.
        #[arg(long)]
        skip_app: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServiceAction {
    /// Register the service so the agent starts at boot / login.
    Install,
    /// Unregister the service.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the installed service.
    Stop,
    /// Entry point used by the service manager itself (not for interactive use).
    #[command(hide = true)]
    Run,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    // WebView2 keeps its profile next to the executable unless told otherwise — under
    // Program Files, which the signed-in user cannot write ("Access is denied" and no window).
    #[cfg(windows)]
    if std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_none() {
        if let Some(base) = directories::BaseDirs::new() {
            let dir = base.data_local_dir().join("RemoteAgent").join("WebView2");
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", dir);
        }
    }
    // Session hosts and UI processes read the machine state the service keeps (branding) but
    // run as the signed-in user on macOS / for the UI, so they log to a per-user directory.
    let desktop_role = matches!(
        cli.command,
        Some(Command::SessionHost { .. }) | Some(Command::Ui { .. })
    );
    let paths = if desktop_role && cli.config_dir.is_none() {
        crate::config::Paths::machine()
    } else {
        crate::config::Paths::resolve(cli.config_dir.clone())?
    };
    let log_dir = if desktop_role {
        crate::config::Paths::writable_log_dir(&paths)
    } else {
        paths.log_dir()
    };
    let prefix = match cli.command {
        Some(Command::SessionHost { .. }) => "session-host",
        Some(Command::Ui { .. }) => "ui",
        Some(Command::Service {
            action: ServiceAction::Run,
        }) => "service",
        _ => "remote-agent",
    };
    init_logging(&cli.log, &log_dir, prefix);
    crate::transport::set_insecure(cli.insecure);
    crate::branding::init(&paths);

    match cli.command {
        // Service mode: the launch agent / service runs the agent behind the app loop, but the
        // window stays in the tray until a session shows it.
        Some(Command::Run) => run_in_app(&paths, false),
        // No subcommand: the branded application (double-click). Window shown, install offered
        // when not yet installed as a service.
        None => run_in_app(&paths, true),
        Some(Command::BakeInfo) => crate::baked::print_info(),
        Some(Command::Enroll {
            server,
            token,
            name,
            if_needed,
        }) => {
            if if_needed && crate::enroll::enrolled_with(&paths, &server) {
                println!(
                    "already enrolled with {}; nothing to do",
                    server.trim_end_matches('/')
                );
                return Ok(());
            }
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(crate::enroll::enroll(&paths, &server, &token, name))?
                .print_summary();
            // A running service picks the new identity up on restart.
            crate::service::restart_if_running();
            Ok(())
        }
        Some(Command::SessionHost {
            login_window,
            manual,
        }) => run_session_host(&paths, login_window, manual),
        Some(Command::Ui { session }) => run_ui(session),
        Some(Command::Service { action }) => crate::service::handle(&paths, action),
        Some(Command::Status) => crate::config::print_status(&paths),
        Some(Command::Doctor) => crate::platform::doctor(&paths),
        Some(Command::Reset) => crate::config::reset(&paths),
        Some(Command::PrivacyProbe {
            display,
            json,
            skip_app,
        }) => crate::probe::run_in_app(
            &paths,
            crate::probe::ProbeOptions {
                display,
                json,
                skip_app,
            },
        ),
    }
}

/// A session host for the installed service.
///
/// * Windows (started by the service as SYSTEM): headless; its app UI is forwarded to the
///   signed-in user's `ui` process over the UI link.
/// * macOS user session (LaunchAgent): inside the app loop, so tray, window, chat, session bar
///   and privacy screen are in-process.
/// * macOS login window: headless.
fn run_session_host(paths: &crate::config::Paths, login_window: bool, manual: bool) -> Result<()> {
    use crate::link::HostKind;
    crate::app::set_state_dir(crate::config::Paths::writable_log_dir(paths));
    let kind = if manual {
        HostKind::Manual
    } else if cfg!(windows) {
        HostKind::Console
    } else if login_window {
        HostKind::LoginWindow
    } else {
        HostKind::UserSession
    };
    let headless = cfg!(windows) || login_window;
    if headless {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(async move {
            #[cfg(windows)]
            {
                let session = crate::platform::os_session_id();
                tokio::spawn(async move {
                    if let Err(e) = crate::app::remote::serve_host(session).await {
                        tracing::error!("UI link: {e:#}");
                    }
                });
            }
            crate::session_host::run(kind).await
        });
    }
    let code = crate::app::run(
        move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("session host runtime: {e}");
                    return 1;
                }
            };
            match rt.block_on(crate::session_host::run(kind)) {
                Ok(()) => 0,
                Err(e) => {
                    tracing::error!("session host exited: {e:#}");
                    1
                }
            }
        },
        crate::app::AppOptions {
            show_on_start: false,
            installable: false,
        },
    );
    if code == 0 {
        Ok(())
    } else {
        anyhow::bail!("session host exited with code {code}")
    }
}

/// The Windows per-user UI process for the SYSTEM session host of `session`.
fn run_ui(session: u32) -> Result<()> {
    let code = crate::app::run(
        move || crate::app::remote::run_ui_client(session),
        crate::app::AppOptions {
            show_on_start: false,
            installable: false,
        },
    );
    if code == 0 {
        Ok(())
    } else {
        anyhow::bail!("UI exited with code {code}")
    }
}

/// Run the agent behind the application UI loop (window + tray). `app_mode` shows the window at
/// launch and offers the install action; service mode keeps it in the tray.
fn run_in_app(paths: &crate::config::Paths, app_mode: bool) -> Result<()> {
    // With the service installed, the service owns the identity and the console connection; a
    // second agent here would fight it for the same device. On Windows a double-click opens
    // the UI of the running session host instead.
    if crate::service::is_installed() && !paths.is_machine_override() {
        #[cfg(windows)]
        if app_mode {
            return run_ui(crate::platform::os_session_id());
        }
        anyhow::bail!(
            "the {} service runs this device; see `remote-agent status`",
            crate::branding::product_name()
        );
    }
    let opts = crate::app::AppOptions {
        show_on_start: app_mode,
        installable: app_mode && !crate::service::is_installed(),
    };
    crate::app::set_state_dir(paths.dir.clone());
    let paths = paths.clone();
    let code = crate::app::run(
        move || match run_agent_blocking(&paths) {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("agent exited: {e:#}");
                1
            }
        },
        opts,
    );
    if code == 0 {
        Ok(())
    } else {
        anyhow::bail!("agent exited with code {code}")
    }
}

/// Run the agent event loop to completion on the current thread (used by `run` and by the
/// service managers). Builds its own tokio runtime.
///
/// Before the hub starts, [`crate::startup::ensure_enrolled`] gets credentials: a baked token
/// enrolls silently, otherwise the app window asks for console URL + token (headless runs fail
/// with the CLI hint as before). When the console later rejects the device (deleted / bad
/// credentials) or the user chooses "Enroll again", the identity is dropped and the Connect
/// screen comes back without restarting the process.
pub fn run_agent_blocking(paths: &crate::config::Paths) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        crate::shutdown::install_handlers();
        let mut notice = None;
        // Set once the person asked to enroll again, so the Connect screen is shown rather
        // than a baked binary quietly re-enrolling itself with the console it came from.
        let mut requested = false;
        loop {
            // The Connect screen can wait forever; a stop request must not.
            tokio::select! {
                r = crate::startup::ensure_enrolled(paths, notice.take(), requested) => r?,
                _ = crate::shutdown::wait() => {
                    tracing::info!("shutting down before enrollment completed");
                    return Ok(());
                }
            }
            match crate::hub::run_agent(paths.clone()).await {
                Err(e) if e.is::<crate::hub::Reenroll>() && crate::app::is_running() => {
                    tracing::warn!("{e:#}; returning to the Connect screen");
                    crate::startup::forget_enrollment(paths)?;
                    notice = e.downcast_ref::<crate::hub::Reenroll>().map(|r| r.notice());
                    requested = true;
                }
                other => return other,
            }
        }
    })
}

fn init_logging(filter: &str, log_dir: &std::path::Path, prefix: &str) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = fmt::layer()
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr);

    // Also log to a rolling file inside the config dir when we can create it.
    let appender = std::fs::create_dir_all(log_dir).ok().and_then(|_| {
        tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(prefix)
            .filename_suffix("log")
            .max_log_files(14)
            .build(log_dir)
            .ok()
    });
    if let Some(appender) = appender {
        let file_layer = fmt::layer()
            .with_ansi(false)
            .with_target(false)
            .with_writer(appender);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(stderr_layer)
            .with(file_layer)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(stderr_layer)
            .init();
    }
}
