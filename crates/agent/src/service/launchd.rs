//! macOS launchd integration.
//!
//! * **LaunchDaemon** `com.remoteagent.service` (root, at boot): `service run` — identity,
//!   console connection, policy, updates. Online with nobody signed in.
//! * **LaunchAgent** `com.remoteagent.agent` (`LimitLoadToSessionType = Aqua`): `session-host`
//!   in every signed-in user's GUI session, with the tray, window and chat in-process. Screen
//!   Recording and Accessibility are TCC permissions of the executable, granted once per Mac.
//! * **LaunchAgent** `com.remoteagent.loginwindow` (`LoginWindow`): `session-host
//!   --login-window` at the login window, so an operator can sign in.
//!
//! Hosts connect to the daemon over `/var/run/remote-agent/service.sock`; the daemon routes
//! sessions to the host whose session is on the console.

use crate::cli::ServiceAction;
use crate::config::Paths;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const DAEMON_LABEL: &str = "com.remoteagent.service";
const DAEMON_PLIST: &str = "/Library/LaunchDaemons/com.remoteagent.service.plist";
const AGENT_LABEL: &str = "com.remoteagent.agent";
const AGENT_PLIST: &str = "/Library/LaunchAgents/com.remoteagent.agent.plist";
const LOGIN_LABEL: &str = "com.remoteagent.loginwindow";
const LOGIN_PLIST: &str = "/Library/LaunchAgents/com.remoteagent.loginwindow.plist";

pub fn handle(paths: &Paths, action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install => install(paths),
        ServiceAction::Uninstall => uninstall(),
        ServiceAction::Start => start(),
        ServiceAction::Stop => stop(),
        ServiceAction::Run => crate::daemon::run_blocking(&Paths::machine()),
    }
}

/// Whether the LaunchDaemon is installed (the service owns this machine's identity).
pub fn is_installed() -> bool {
    Path::new(DAEMON_PLIST).exists()
}

fn exe_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating current executable")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn console_uid() -> Option<u32> {
    crate::platform::macos::console_user().map(|(uid, _)| uid)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render a launchd plist.
fn plist(label: &str, args: &[&str], extra: &str, logs: &Path, log_name: &str) -> String {
    let args = args
        .iter()
        .map(|a| format!("        <string>{}</string>", xml_escape(a)))
        .collect::<Vec<_>>()
        .join("\n");
    let logs = xml_escape(&logs.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{args}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>5</integer>
{extra}    <key>StandardOutPath</key>
    <string>{logs}/{log_name}.out.log</string>
    <key>StandardErrorPath</key>
    <string>{logs}/{log_name}.err.log</string>
</dict>
</plist>
"#
    )
}

fn daemon_plist(exe: &Path, logs: &Path) -> String {
    plist(
        DAEMON_LABEL,
        &[&exe.to_string_lossy(), "service", "run"],
        "",
        logs,
        "service",
    )
}

fn agent_plist(exe: &Path) -> String {
    // User-session hosts cannot write the root-owned log dir; launchd's own stdout capture is
    // only for crashes before logging starts, so it goes to /tmp.
    plist(
        AGENT_LABEL,
        &[&exe.to_string_lossy(), "session-host"],
        "    <key>LimitLoadToSessionType</key>\n    <string>Aqua</string>\n    <key>ProcessType</key>\n    <string>Interactive</string>\n",
        Path::new("/tmp"),
        "remote-agent-host",
    )
}

fn login_plist(exe: &Path, logs: &Path) -> String {
    plist(
        LOGIN_LABEL,
        &[&exe.to_string_lossy(), "session-host", "--login-window"],
        "    <key>LimitLoadToSessionType</key>\n    <string>LoginWindow</string>\n    <key>ProcessType</key>\n    <string>Interactive</string>\n",
        logs,
        "loginwindow",
    )
}

fn write_plist(path: &str, contents: &str) -> Result<()> {
    std::fs::write(path, contents).with_context(|| format!("writing {path}"))?;
    run("chown", &["root:wheel", path])?;
    run("chmod", &["644", path])
}

/// Install or update the daemon and agents, then (re)load them. Idempotent.
fn install(_paths: &Paths) -> Result<()> {
    require_root()?;
    let exe = exe_path()?;
    let machine = Paths::machine();
    std::fs::create_dir_all(machine.log_dir()).ok();
    // The state dir is world-readable (branding for user-session hosts); agent.toml itself is
    // written 0600.
    run("chmod", &["755", &machine.dir.to_string_lossy()]).ok();

    migrate_identity(&machine);

    // Stop whatever runs now (older layouts ran the whole agent as the LaunchAgent).
    let uid = console_uid();
    if let Some(uid) = uid {
        let _ = launchctl(&["bootout", &format!("gui/{uid}/{AGENT_LABEL}")]);
    }
    let _ = launchctl(&["bootout", &format!("system/{DAEMON_LABEL}")]);

    write_plist(DAEMON_PLIST, &daemon_plist(&exe, &machine.log_dir()))?;
    write_plist(AGENT_PLIST, &agent_plist(&exe))?;
    write_plist(LOGIN_PLIST, &login_plist(&exe, &machine.log_dir()))?;
    println!("installed {DAEMON_PLIST}, {AGENT_PLIST}, {LOGIN_PLIST}");

    launchctl(&["bootstrap", "system", DAEMON_PLIST]).context("loading the LaunchDaemon")?;
    let _ = launchctl(&["enable", &format!("system/{DAEMON_LABEL}")]);
    println!("started {DAEMON_LABEL}");
    match uid {
        Some(uid) => {
            let target = format!("gui/{uid}");
            launchctl(&["bootstrap", &target, AGENT_PLIST])
                .context("loading the LaunchAgent into the signed-in session")?;
            let _ = launchctl(&["enable", &format!("{target}/{AGENT_LABEL}")]);
            println!("started {AGENT_LABEL} in {target}");
        }
        None => {
            println!("nobody is signed in; the login window host starts with the next login window")
        }
    }
    Ok(())
}

/// An identity enrolled from the app lives in the user's home and its secret in their
/// keychain; the daemon cannot reach either. Copy what is reachable; otherwise say so.
fn migrate_identity(machine: &Paths) {
    use crate::config::LocalConfig;
    let enrolled = LocalConfig::load(machine)
        .ok()
        .flatten()
        .map(|c| c.is_enrolled())
        .unwrap_or(false);
    if enrolled {
        match crate::secrets::reprotect_for_machine(machine) {
            Ok(true) => println!("device secret moved into the service's configuration"),
            Ok(false) => {}
            Err(e) => eprintln!("warning: {e:#}"),
        }
        return;
    }
    eprintln!(
        "note: this Mac is not enrolled for the service yet; run `remote-agent enroll --server … --token …`"
    );
}

fn uninstall() -> Result<()> {
    require_root()?;
    if let Some(uid) = console_uid() {
        let _ = launchctl(&["bootout", &format!("gui/{uid}/{AGENT_LABEL}")]);
    }
    let _ = launchctl(&["bootout", &format!("system/{DAEMON_LABEL}")]);
    for path in [DAEMON_PLIST, AGENT_PLIST, LOGIN_PLIST] {
        if Path::new(path).exists() {
            std::fs::remove_file(path).with_context(|| format!("removing {path}"))?;
            println!("removed {path}");
        }
    }
    Ok(())
}

fn start() -> Result<()> {
    require_root()?;
    launchctl(&["kickstart", &format!("system/{DAEMON_LABEL}")])
}

fn stop() -> Result<()> {
    require_root()?;
    // `kill` signals the job without unloading it; KeepAlive brings it back unless unloaded.
    launchctl(&["kill", "SIGTERM", &format!("system/{DAEMON_LABEL}")])
}

/// Restart the daemon when it is installed (after `enroll`, so it picks the identity up).
pub fn restart_if_running() {
    // SAFETY: geteuid has no preconditions.
    if !is_installed() || unsafe { libc::geteuid() } != 0 {
        return;
    }
    if launchctl(&["kickstart", "-k", &format!("system/{DAEMON_LABEL}")]).is_ok() {
        println!("restarted {DAEMON_LABEL}");
    }
}

fn require_root() -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!("this command must be run as root (try `sudo remote-agent service …`)");
    }
    Ok(())
}

fn launchctl(args: &[&str]) -> Result<()> {
    run("launchctl", args)
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("running {cmd} {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "{cmd} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plists_are_wellformed_and_scoped() {
        let exe = Path::new("/Applications/Remote Support.app/Contents/MacOS/remote-agent");
        let logs = Path::new("/Library/Application Support/RemoteAgent/logs");
        let d = daemon_plist(exe, logs);
        assert!(d.contains("<string>service</string>\n        <string>run</string>"));
        assert!(!d.contains("LimitLoadToSessionType"));
        let a = agent_plist(exe);
        assert!(a.contains("<string>Aqua</string>"));
        assert!(a.contains("<string>session-host</string>"));
        let l = login_plist(exe, logs);
        assert!(l.contains("<string>LoginWindow</string>"));
        assert!(l.contains("--login-window"));
        assert!(xml_escape("a&b<c>").contains("&amp;"));
    }
}
