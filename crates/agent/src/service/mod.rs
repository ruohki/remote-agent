//! Background service integration (launchd on macOS, Windows service on Windows).

use crate::cli::ServiceAction;
use crate::config::Paths;
use anyhow::{Context, Result};

#[cfg(target_os = "macos")]
mod launchd;
#[cfg(target_os = "windows")]
mod windows_service_impl;

pub fn handle(paths: &Paths, action: ServiceAction) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        launchd::handle(paths, action)
    }
    #[cfg(target_os = "windows")]
    {
        windows_service_impl::handle(paths, action)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (paths, action);
        anyhow::bail!("service management is not supported on this platform")
    }
}

/// Whether the agent is already installed as a service / launch agent on this machine.
pub fn is_installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::path::Path::new("/Library/LaunchAgents/com.remoteagent.agent.plist").exists()
    }
    #[cfg(target_os = "windows")]
    {
        windows_service_impl::is_installed()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// Install the service with an administrator/UAC prompt (used by the app's "Install" button).
/// Re-launches `remote-agent service install` elevated and returns a human-readable status.
pub fn install_elevated() -> Result<String> {
    let exe = std::env::current_exe().context("locating current executable")?;
    #[cfg(target_os = "macos")]
    {
        // `osascript … with administrator privileges` shows the standard macOS auth dialog.
        let script = format!(
            "do shell script \"{} service install\" with administrator privileges",
            shell_quote(&exe.to_string_lossy())
        );
        let out = std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output()
            .context("running osascript")?;
        if out.status.success() {
            Ok("Installed. It will start automatically when you sign in.".to_string())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("-128") || err.to_lowercase().contains("cancel") {
                anyhow::bail!("cancelled");
            }
            anyhow::bail!("{}", err.trim())
        }
    }
    #[cfg(target_os = "windows")]
    {
        windows_service_impl::install_elevated(&exe)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = exe;
        anyhow::bail!("not supported on this platform")
    }
}

#[cfg(target_os = "macos")]
fn shell_quote(s: &str) -> String {
    // Escape for embedding inside the AppleScript "do shell script" double-quoted string.
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
