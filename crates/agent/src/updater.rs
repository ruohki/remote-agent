//! Best-effort self-update: download the new binary, verify its SHA-256, and swap it in.
//!
//! The service manager (launchd `KeepAlive` / Windows service auto-restart) restarts the
//! agent after [`crate::hub`] exits, so this only needs to place the new binary and return.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Download `url`, check it against `sha256_hex`, and replace the running executable.
pub async fn apply_update(version: &str, url: &str, sha256_hex: &str) -> Result<()> {
    let exe = std::env::current_exe().context("locating current executable")?;
    tracing::info!(%version, target = %exe.display(), "downloading update");

    let client = crate::transport::http_client(url, Duration::from_secs(300))?;
    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .context("update download returned an error status")?
        .bytes()
        .await
        .context("reading update body")?;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let got = hex::encode(hasher.finalize());
    if !got.eq_ignore_ascii_case(sha256_hex) {
        bail!("update checksum mismatch: expected {sha256_hex}, got {got}");
    }

    // Write next to the current binary, then atomically rename over it. On Windows a running
    // exe cannot be overwritten, so rename the old one aside first.
    let dir = exe.parent().context("executable has no parent directory")?;
    let tmp = dir.join(format!(".remote-agent-update-{version}"));
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context("setting update permissions")?;
        std::fs::rename(&tmp, &exe).context("replacing executable")?;
    }
    #[cfg(windows)]
    {
        let old = dir.join(".remote-agent-old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old).context("moving current executable aside")?;
        if let Err(e) = std::fs::rename(&tmp, &exe) {
            // Roll back so we are not left without a binary.
            let _ = std::fs::rename(&old, &exe);
            return Err(e).context("installing new executable");
        }
    }

    tracing::info!("update {version} installed");
    Ok(())
}
