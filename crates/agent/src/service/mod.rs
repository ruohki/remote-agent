//! Background service integration.
//!
//! TODO(builder-core):
//! * macOS: install a **LaunchAgent** (`/Library/LaunchAgents/com.remoteagent.agent.plist`,
//!   `RunAtLoad`, `KeepAlive`, runs `remote-agent run` in every GUI login session so screen
//!   recording / accessibility permissions apply). `launchctl bootstrap gui/<uid>` for the
//!   current console user after install; `bootout` on uninstall.
//! * Windows: register a service (`windows-service` crate, `SERVICE_AUTO_START`,
//!   LocalSystem). `service run` is the SCM entry point; it supervises a child
//!   `remote-agent run` started in the active console session via `WTSQueryUserToken` +
//!   `CreateProcessAsUserW` (desktop `winsta0\default`) and restarts it on exit / session change
//!   (`SERVICE_CONTROL_SESSIONCHANGE`).

use crate::cli::ServiceAction;
use crate::config::Paths;
use anyhow::Result;

pub fn handle(_paths: &Paths, action: ServiceAction) -> Result<()> {
    anyhow::bail!("service action {action:?} not implemented yet")
}
