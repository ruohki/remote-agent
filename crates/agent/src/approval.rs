//! Help-me mode approval prompt and the on-screen "session active" indicator.
//!
//! TODO(builder-core): implement
//! * `ask_approval(operator, timeout)` — native modal dialog ("Alice wants to control this
//!   computer. Allow?") with Allow / Deny, auto-deny after `timeout`. Use `rfd::AsyncMessageDialog`
//!   (native NSAlert / TaskDialog) — it must run on the main thread on macOS, so route
//!   through `platform::run_on_main`.
//! * `SessionIndicator` — small always-on-top window/banner showing the operator name and a
//!   "Disconnect" button that ends the session (`ControlMessage::SessionEndedByUser`).
//!   Implement natively (NSPanel / plain Win32 window); skip when
//!   `AgentConfig.show_session_indicator == false`.

use anyhow::Result;
use protocol::common::OperatorInfo;
use std::time::Duration;

pub async fn ask_approval(_operator: &OperatorInfo, _timeout: Duration) -> Result<bool> {
    anyhow::bail!("approval prompt not implemented yet")
}
