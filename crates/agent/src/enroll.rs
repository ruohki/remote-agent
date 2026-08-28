//! Enrollment: exchange a console-issued token for device credentials + config.
//!
//! TODO(builder-core): `POST {server}/api/enroll` with [`protocol::config::EnrollRequest`],
//! store the [`protocol::config::EnrollResponse`] into `LocalConfig` (use `server_url`
//! from the response as canonical), set `display_name` override if `name` was given.
//! Print a one-line summary. Exit non-zero on 4xx with the server's error message.

use crate::config::Paths;
use anyhow::Result;

pub async fn enroll(_paths: &Paths, server: &str, _token: &str, _name: Option<String>) -> Result<()> {
    anyhow::bail!("enrollment against {server} not implemented yet")
}
