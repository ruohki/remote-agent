//! Background service integration (launchd on macOS, Windows service on Windows).

use crate::cli::ServiceAction;
use crate::config::Paths;
use anyhow::Result;

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
