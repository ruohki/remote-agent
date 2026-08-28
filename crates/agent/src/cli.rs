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

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the agent in the foreground (what the service / launch agent executes).
    Run,
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
    let paths = crate::config::Paths::resolve(cli.config_dir.clone())?;
    init_logging(&cli.log, &paths);

    match cli.command {
        Command::Run => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(crate::hub::run_agent(paths))
        }
        Command::Enroll { server, token, name } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(crate::enroll::enroll(&paths, &server, &token, name))
        }
        Command::Service { action } => crate::service::handle(&paths, action),
        Command::Status => crate::config::print_status(&paths),
        Command::Doctor => crate::platform::doctor(),
        Command::Reset => crate::config::reset(&paths),
    }
}

fn init_logging(filter: &str, paths: &crate::config::Paths) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = fmt::layer().with_target(false).with_writer(std::io::stderr);

    // Also log to a rolling file inside the config dir when we can create it.
    if let Ok(()) = std::fs::create_dir_all(paths.log_dir()) {
        let appender = tracing_appender::rolling::daily(paths.log_dir(), "remote-agent.log");
        let file_layer = fmt::layer().with_ansi(false).with_target(false).with_writer(appender);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(stderr_layer)
            .with(file_layer)
            .init();
    } else {
        tracing_subscriber::registry().with(env_filter).with(stderr_layer).init();
    }
}
