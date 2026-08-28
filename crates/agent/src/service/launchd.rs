//! macOS LaunchAgent management.
//!
//! Screen recording and accessibility are per-user TCC permissions granted to a GUI process,
//! so the agent runs as a **LaunchAgent** in the console user's GUI session (not a system
//! daemon). Install writes `/Library/LaunchAgents/com.remoteagent.agent.plist` (available to
//! every user) and bootstraps it into the current console user's GUI domain.

use crate::cli::ServiceAction;
use crate::config::Paths;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "com.remoteagent.agent";
const PLIST_PATH: &str = "/Library/LaunchAgents/com.remoteagent.agent.plist";

pub fn handle(paths: &Paths, action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install => install(paths),
        ServiceAction::Uninstall => uninstall(),
        ServiceAction::Start => start(),
        ServiceAction::Stop => stop(),
        // launchd runs `remote-agent run` directly; there is no SCM entry point.
        ServiceAction::Run => crate::cli::run_agent_blocking(paths),
    }
}

fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("locating current executable")
}

fn console_uid() -> Option<u32> {
    crate::platform::macos::console_user().map(|(uid, _)| uid)
}

fn plist(exe: &std::path::Path, paths: &Paths) -> String {
    let logs = paths.log_dir();
    let logs = logs.to_string_lossy();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{logs}/launchd.out.log</string>
    <key>StandardErrorPath</key>
    <string>{logs}/launchd.err.log</string>
</dict>
</plist>
"#,
        exe = exe.display(),
    )
}

fn install(paths: &Paths) -> Result<()> {
    require_root()?;
    let exe = exe_path()?;
    std::fs::create_dir_all(paths.log_dir()).ok();
    let contents = plist(&exe, paths);
    std::fs::write(PLIST_PATH, contents).with_context(|| format!("writing {PLIST_PATH}"))?;
    // Root-owned, world-readable is correct for /Library/LaunchAgents.
    run("chmod", &["644", PLIST_PATH])?;
    println!("installed LaunchAgent at {PLIST_PATH}");

    if let Some(uid) = console_uid() {
        // Load into the current console user's GUI session so it starts immediately.
        let target = format!("gui/{uid}");
        // bootout first in case a stale copy is loaded (ignore failure).
        let _ = Command::new("launchctl")
            .args(["bootout", &target, PLIST_PATH])
            .status();
        run("launchctl", &["bootstrap", &target, PLIST_PATH])?;
        let _ = Command::new("launchctl")
            .args(["enable", &format!("gui/{uid}/{LABEL}")])
            .status();
        println!("bootstrapped into {target}");
    } else {
        println!("no console user logged in; the agent will start at the next login");
    }
    Ok(())
}

fn uninstall() -> Result<()> {
    require_root()?;
    if let Some(uid) = console_uid() {
        let target = format!("gui/{uid}");
        let _ = Command::new("launchctl")
            .args(["bootout", &target, PLIST_PATH])
            .status();
    }
    if std::path::Path::new(PLIST_PATH).exists() {
        std::fs::remove_file(PLIST_PATH).with_context(|| format!("removing {PLIST_PATH}"))?;
        println!("removed {PLIST_PATH}");
    } else {
        println!("LaunchAgent not installed");
    }
    Ok(())
}

fn start() -> Result<()> {
    let uid = console_uid().context("no console user is logged in")?;
    run(
        "launchctl",
        &["kickstart", "-k", &format!("gui/{uid}/{LABEL}")],
    )
}

fn stop() -> Result<()> {
    let uid = console_uid().context("no console user is logged in")?;
    // `kill` sends a signal to the running job without unloading it.
    run(
        "launchctl",
        &["kill", "SIGTERM", &format!("gui/{uid}/{LABEL}")],
    )
}

fn require_root() -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!("this command must be run as root (try `sudo remote-agent service …`)");
    }
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("running {cmd} {}", args.join(" ")))?;
    if !status.success() {
        bail!("{cmd} {} failed with {status}", args.join(" "));
    }
    Ok(())
}
