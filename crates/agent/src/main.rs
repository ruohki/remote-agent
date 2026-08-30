//! Binary entry point; all logic lives in the `remote_agent` library crate.

// Link for the GUI subsystem on Windows: launching the app (double-click, or the service
// spawning `run` in the user's session) must not pop a console window. The CLI subcommands
// still print — `attach_parent_console` re-attaches stdout/stderr to the calling console
// before any output happens.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    #[cfg(target_os = "windows")]
    remote_agent::platform::windows::attach_parent_console();

    let code = match remote_agent::cli::run() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("error: {err:#}");
            tracing::error!("fatal: {err:#}");
            1
        }
    };
    std::process::exit(code);
}
