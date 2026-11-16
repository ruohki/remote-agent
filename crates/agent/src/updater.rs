//! Self-update: download the new build, verify its SHA-256, and put it in place.
//!
//! Runs in the process that owns the console connection — the service on an installed
//! machine (LocalSystem / root, so it can write the install location), the app itself when
//! standalone. The caller requests a clean shutdown afterwards; the service manager starts the
//! new build (Windows: the service exits with a failure code its recovery actions restart;
//! launchd: `KeepAlive`).
//!
//! Payloads:
//! * a bare executable (Windows `.exe`, a macOS binary outside a bundle): swapped in next to
//!   the running one (on Windows the running image is renamed aside first);
//! * a zip holding an `.app` (macOS): verified with `codesign`, then the whole bundle the
//!   running binary lives in is replaced, so the signature and notarization stay intact.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Device credentials for the console's authenticated package download.
pub struct DeviceAuth {
    pub device_id: String,
    pub device_secret: String,
}

/// Download `url`, check it against `sha256_hex`, and install it.
pub async fn apply_update(
    version: &str,
    url: &str,
    sha256_hex: &str,
    auth: Option<&DeviceAuth>,
) -> Result<()> {
    if version == crate::AGENT_VERSION {
        bail!("already running {version}");
    }
    let exe = std::env::current_exe().context("locating current executable")?;
    tracing::info!(%version, target = %exe.display(), "downloading update");

    let client = crate::transport::http_client(url, Duration::from_secs(600))?;
    let mut req = client.get(url);
    if let Some(a) = auth {
        req = req.header(
            "Authorization",
            format!("Device {}:{}", a.device_id, a.device_secret),
        );
    }
    let bytes = req
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .context("update download returned an error status")?
        .bytes()
        .await
        .context("reading update body")?;

    let got = hex::encode(Sha256::digest(&bytes));
    if !got.eq_ignore_ascii_case(sha256_hex) {
        bail!("update checksum mismatch: expected {sha256_hex}, got {got}");
    }
    if let Some(found) = protocol::bakery::find_agent_version(&bytes) {
        if found != version {
            bail!("update announced {version} but the download is {found}");
        }
    }

    let is_zip = bytes.starts_with(b"PK\x03\x04");
    let exe_clone = exe.clone();
    let version_owned = version.to_string();
    tokio::task::spawn_blocking(move || {
        if is_zip {
            install_bundle(&exe_clone, &bytes, &version_owned)
        } else {
            install_binary(&exe_clone, &bytes, &version_owned)
        }
    })
    .await
    .context("update install task")??;

    tracing::info!("update {version} installed");
    Ok(())
}

/// Replace the running executable with `bytes`.
fn install_binary(exe: &Path, bytes: &[u8], version: &str) -> Result<()> {
    let dir = exe.parent().context("executable has no parent directory")?;
    let tmp = dir.join(format!(".remote-agent-update-{version}"));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context("setting update permissions")?;
        std::fs::rename(&tmp, exe).context("replacing executable")?;
    }
    #[cfg(windows)]
    {
        // A running image cannot be overwritten but can be renamed. The old copy is removed
        // at the next start (`cleanup_after_update`).
        let old = dir.join(".remote-agent-old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).context("moving current executable aside")?;
        if let Err(e) = std::fs::rename(&tmp, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(e).context("installing new executable");
        }
    }
    Ok(())
}

/// The `.app` bundle `exe` lives in (`…/X.app/Contents/MacOS/remote-agent`).
fn enclosing_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then(|| app.to_path_buf())
}

/// Replace the bundle around the running executable with the `.app` inside `zip`.
fn install_bundle(exe: &Path, zip: &[u8], version: &str) -> Result<()> {
    let Some(app) = enclosing_bundle(exe) else {
        bail!("update is an app bundle but this agent does not run from one");
    };
    let parent = app.parent().context("bundle has no parent directory")?;
    let work = tempfile::Builder::new()
        .prefix(".remote-agent-update-")
        .tempdir_in(parent)
        .context("creating update work dir")?;
    let zip_path = work.path().join("update.zip");
    std::fs::write(&zip_path, zip).context("writing update archive")?;
    let unpacked = work.path().join("unpacked");
    run("ditto", &["-x", "-k", &s(&zip_path), &s(&unpacked)])?;
    let new_app = std::fs::read_dir(&unpacked)
        .context("reading unpacked update")?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().map(|e| e == "app").unwrap_or(false))
        .context("update archive holds no .app bundle")?;
    // Refuse a bundle whose signature does not hold; the old one stays in place.
    if cfg!(target_os = "macos") {
        run(
            "codesign",
            &["--verify", "--deep", "--strict", &s(&new_app)],
        )
        .context("the downloaded bundle's signature does not verify")?;
    }
    let aside = work.path().join(format!("previous-{version}.app"));
    std::fs::rename(&app, &aside).context("moving the current bundle aside")?;
    if let Err(e) = std::fs::rename(&new_app, &app) {
        let _ = std::fs::rename(&aside, &app);
        return Err(e).context("installing the new bundle");
    }
    // `work` (with the previous bundle) is removed on drop; the running process keeps its
    // mapped image until it exits.
    Ok(())
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        bail!(
            "{cmd} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Remove what an earlier update left behind (Windows keeps the replaced image until the next
/// start). Best effort.
pub fn cleanup_after_update() {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let _ = std::fs::remove_file(dir.join(".remote-agent-old.exe"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_is_found_from_the_inner_executable() {
        assert_eq!(
            enclosing_bundle(Path::new(
                "/Applications/Remote Support.app/Contents/MacOS/remote-agent"
            )),
            Some(PathBuf::from("/Applications/Remote Support.app"))
        );
        assert_eq!(
            enclosing_bundle(Path::new("/usr/local/bin/remote-agent")),
            None
        );
    }

    #[tokio::test]
    async fn same_version_is_refused_before_downloading() {
        let err = apply_update(crate::AGENT_VERSION, "http://127.0.0.1:1/x", "00", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already running"));
    }
}
