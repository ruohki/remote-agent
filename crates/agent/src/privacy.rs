//! Privacy screen: the device's own displays show a branded notice instead of the desktop
//! while an operator works, so a bystander at the machine cannot follow the session.
//!
//! This module owns the *guarantee* — the screen must always come back — and the policy
//! decision; the windows themselves live in [`crate::app`] (`app::privacy`). The session task
//! engages a [`PrivacyGuard`] and keeps it alive with [`PrivacyGuard::keepalive`]; a dedicated
//! OS thread (never tokio, never a destructor) releases the screen unconditionally when the
//! keepalives stop, when the hard cap elapses, or when the set of displays changes — and if
//! the UI thread fails to confirm a release in time, the process aborts so the service manager
//! brings up a fresh agent with the desktop visible. Shipped builds run with
//! `panic = "abort"`, so none of this relies on `Drop` (the `Drop` impl is a dev-build courtesy).
//!
//! What the screen shows is decided here too: a heavily downsampled snapshot of each display
//! taken at engage time ([`mosaic_data_url`]) — the colour mood of the desktop survives, no
//! structure does — behind the branded notice.

use crate::capture::{self, CaptureConfig};
use protocol::common::{DisplayInfo, PrivacyScreenReason, PrivacyScreenSupport};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Hard cap on one engagement, enforced by the watchdog's own clock.
pub const MAX_ENGAGED: Duration = Duration::from_secs(30 * 60);
/// The session must call [`PrivacyGuard::keepalive`] at least this often.
pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the UI thread gets to confirm that the windows are gone before the process aborts.
const RELEASE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);
/// How long window creation may take before the engagement is refused.
const ENGAGE_TIMEOUT: Duration = Duration::from_secs(4);
/// How long to wait for the first frame of a display when taking the backdrop snapshot.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(1500);
/// Mosaic width in cells; the height follows the display's aspect ratio.
const MOSAIC_COLS: u32 = 40;

/// Test seam (debug builds only): `REMOTE_AGENT_PRIVACY_FAKE=1` reports support and engages
/// without creating windows, so the session logic (gates, lift, lock, releases) can be
/// exercised by the loopback tests without a UI loop.
fn fake() -> bool {
    cfg!(debug_assertions) && std::env::var_os("REMOTE_AGENT_PRIVACY_FAKE").is_some()
}

/// What this agent can do on this machine right now.
pub fn support() -> PrivacyScreenSupport {
    if fake() {
        return PrivacyScreenSupport::ScreenOnly;
    }
    if cfg!(not(any(target_os = "macos", target_os = "windows"))) {
        return PrivacyScreenSupport::Unsupported;
    }
    if std::env::var_os("REMOTE_AGENT_SHOW_WINDOWS").is_some() {
        // Capture exclusion is disabled wholesale: the screen would be streamed to the operator.
        return PrivacyScreenSupport::Unsupported;
    }
    if !crate::app::is_running() {
        // No UI loop: no windows, no notice, no local escape.
        return PrivacyScreenSupport::Unsupported;
    }
    PrivacyScreenSupport::ScreenOnly
}

/// Everything the device-side surface shows.
#[derive(Debug, Clone, Default)]
pub struct PrivacyScreenInfo {
    pub operator: String,
    pub device: String,
    /// Session start, Unix epoch milliseconds (the surface shows the elapsed time).
    pub started_ms: u64,
    /// Backdrop image per display index (`data:image/png;base64,…`); missing = synthetic.
    pub backdrops: HashMap<u32, String>,
}

/// One-shot confirmation channel shared with the UI thread through an [`crate::app::AppEvent`]
/// (events must be `Clone`, senders are not).
pub type Confirm<T> = Arc<parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<T>>>>;

fn confirm_channel<T>() -> (Confirm<T>, tokio::sync::oneshot::Receiver<T>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    (Arc::new(parking_lot::Mutex::new(Some(tx))), rx)
}

/// A [`Confirm`] nobody waits on (fire-and-forget teardown).
pub fn no_confirm<T>() -> Confirm<T> {
    Arc::new(parking_lot::Mutex::new(None))
}

/// Complete a [`Confirm`] (no-op if already completed).
pub fn confirm<T>(c: &Confirm<T>, value: T) {
    if let Some(tx) = c.lock().take() {
        let _ = tx.send(value);
    }
}

type OnRelease = Box<dyn FnOnce(PrivacyScreenReason) + Send>;

struct Inner {
    engaged_at: Instant,
    last_keepalive: parking_lot::Mutex<Instant>,
    displays: Vec<DisplayInfo>,
    released: AtomicBool,
    on_release: parking_lot::Mutex<Option<OnRelease>>,
}

/// An engaged privacy screen. Releasing is idempotent and always tears the windows down; the
/// `on_release` callback given to [`PrivacyGuard::engage`] fires exactly once with the reason.
pub struct PrivacyGuard {
    inner: Arc<Inner>,
}

impl PrivacyGuard {
    /// Show the privacy screen on every display. Fails without touching the screen when the
    /// device cannot do it, when no display is visible, or when the UI thread does not confirm
    /// within [`ENGAGE_TIMEOUT`].
    pub async fn engage(
        info: PrivacyScreenInfo,
        on_release: impl FnOnce(PrivacyScreenReason) + Send + 'static,
    ) -> Result<PrivacyGuard, PrivacyScreenReason> {
        if support() == PrivacyScreenSupport::Unsupported {
            return Err(PrivacyScreenReason::Unsupported);
        }
        let displays = tokio::task::spawn_blocking(capture::list_displays)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
        if displays.is_empty() && !fake() {
            tracing::warn!("privacy screen: no displays visible");
            return Err(PrivacyScreenReason::Failed);
        }
        if !fake() {
            let (confirm, rx) = confirm_channel::<Result<(), String>>();
            crate::app::engage_privacy(info, displays.clone(), confirm);
            match tokio::time::timeout(ENGAGE_TIMEOUT, rx).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => {
                    tracing::warn!("privacy screen: {e}");
                    return Err(PrivacyScreenReason::Failed);
                }
                Ok(Err(_)) | Err(_) => {
                    tracing::warn!("privacy screen: the UI did not confirm in time");
                    // Whatever came up must come down again.
                    crate::app::disengage_privacy(confirm_channel::<()>().0);
                    return Err(PrivacyScreenReason::Failed);
                }
            }
        }
        let inner = Arc::new(Inner {
            engaged_at: Instant::now(),
            last_keepalive: parking_lot::Mutex::new(Instant::now()),
            displays,
            released: AtomicBool::new(false),
            on_release: parking_lot::Mutex::new(Some(Box::new(on_release))),
        });
        spawn_watchdog(Arc::clone(&inner));
        tracing::info!("privacy screen engaged");
        Ok(PrivacyGuard { inner })
    }

    /// The session is alive and still wants the screen up. Call at least every
    /// [`KEEPALIVE_TIMEOUT`] / 2.
    pub fn keepalive(&self) {
        *self.inner.last_keepalive.lock() = Instant::now();
    }

    pub fn is_released(&self) -> bool {
        self.inner.released.load(Ordering::SeqCst)
    }

    /// Bring the desktop back. Idempotent; the first reason wins.
    pub fn release(&self, reason: PrivacyScreenReason) {
        release(&self.inner, reason);
    }
}

impl Drop for PrivacyGuard {
    fn drop(&mut self) {
        // Dev-build courtesy only: release builds abort on panic and skip destructors.
        release(&self.inner, PrivacyScreenReason::SessionEnded);
    }
}

fn release(inner: &Arc<Inner>, reason: PrivacyScreenReason) {
    if inner.released.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::info!(?reason, "privacy screen released");
    let (confirm, rx) = confirm_channel::<()>();
    if fake() {
        crate::privacy::confirm(&confirm, ());
    } else {
        crate::app::disengage_privacy(confirm);
    }
    if let Some(cb) = inner.on_release.lock().take() {
        cb(reason);
    }
    // The UI thread must confirm that the windows are gone. If it cannot (wedged main thread),
    // the machine would stay covered: leave, and let the service manager restart us.
    std::thread::Builder::new()
        .name("privacy-release".into())
        .spawn(move || {
            let deadline = Instant::now() + RELEASE_CONFIRM_TIMEOUT;
            let mut rx = rx;
            loop {
                match rx.try_recv() {
                    Ok(()) => return,
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => return,
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                }
                if !crate::app::is_running() {
                    return;
                }
                if Instant::now() >= deadline {
                    tracing::error!(
                        "privacy screen release not confirmed within {RELEASE_CONFIRM_TIMEOUT:?}; \
                         aborting so the desktop comes back"
                    );
                    std::process::abort();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
        .ok();
}

/// Decide whether the watchdog must release, from plain facts (kept pure for tests).
fn watchdog_verdict(
    now: Instant,
    engaged_at: Instant,
    last_keepalive: Instant,
    displays_then: &[DisplayInfo],
    displays_now: Option<&[DisplayInfo]>,
) -> Option<PrivacyScreenReason> {
    if now.duration_since(last_keepalive) > KEEPALIVE_TIMEOUT {
        return Some(PrivacyScreenReason::Watchdog);
    }
    if now.duration_since(engaged_at) > MAX_ENGAGED {
        return Some(PrivacyScreenReason::Timeout);
    }
    if let Some(now_displays) = displays_now {
        if now_displays != displays_then {
            return Some(PrivacyScreenReason::DisplaysChanged);
        }
    }
    None
}

fn spawn_watchdog(inner: Arc<Inner>) {
    std::thread::Builder::new()
        .name("privacy-watchdog".into())
        .spawn(move || {
            let mut tick: u32 = 0;
            while !inner.released.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(500));
                tick = tick.wrapping_add(1);
                // Display topology every ~2 s (cheap CoreGraphics / Win32 calls).
                let displays_now = if tick % 4 == 0 {
                    capture::list_displays().ok()
                } else {
                    None
                };
                let verdict = watchdog_verdict(
                    Instant::now(),
                    inner.engaged_at,
                    *inner.last_keepalive.lock(),
                    &inner.displays,
                    displays_now.as_deref(),
                );
                if let Some(reason) = verdict {
                    tracing::warn!(?reason, "privacy screen watchdog releasing");
                    release(&inner, reason);
                    return;
                }
            }
        })
        .ok();
}

// ─── backdrop snapshot ───────────────────────────────────────────────────────────────────

/// A `data:image/png;base64,…` mosaic of `display`: one frame, box-averaged down to
/// [`MOSAIC_COLS`] cells across. Blocking (opens a capture stream); `None` when no frame
/// arrives in time — the surface then draws a synthetic backdrop.
pub fn mosaic_data_url(display_index: u32) -> Option<String> {
    let cfg = CaptureConfig {
        display_index,
        max_fps: 10,
        show_cursor: false,
    };
    let mut cap = match capture::create_capturer(&cfg) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(
                display_index,
                "privacy backdrop: capture unavailable: {e:#}"
            );
            return None;
        }
    };
    let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
    let mut frame = None;
    while Instant::now() < deadline {
        match cap.next_frame(Duration::from_millis(200)) {
            Ok(Some(f)) => {
                frame = Some(f);
                break;
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    cap.stop();
    let frame = frame?;
    let (bgra, stride) = capture::to_bgra(&frame).ok()?;
    let png = mosaic_png(&bgra, stride, frame.width, frame.height)?;
    use base64::Engine as _;
    Some(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    ))
}

/// Downsample a BGRA frame to a tiny RGBA mosaic and PNG-encode it.
fn mosaic_png(bgra: &[u8], stride: usize, width: u32, height: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || stride < width as usize * 4 {
        return None;
    }
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height as usize {
        let row = bgra.get(y * stride..y * stride + width as usize * 4)?;
        for px in row.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    let src = crate::branding::Rgba {
        width,
        height,
        data: rgba,
    };
    let cols = MOSAIC_COLS.min(width);
    let rows = ((cols as u64 * height as u64) / width as u64).clamp(1, cols as u64) as u32;
    let small = crate::branding::resize(&src, cols, rows);
    Some(crate::branding::encode_png(&small))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(index: u32, width: u32) -> DisplayInfo {
        DisplayInfo {
            index,
            name: format!("D{index}"),
            x: 0,
            y: 0,
            width,
            height: 1080,
            scale: 1.0,
            primary: index == 0,
        }
    }

    #[test]
    fn watchdog_releases_on_missing_keepalive_timeout_and_topology_change() {
        let t0 = Instant::now();
        let then = vec![display(0, 1920), display(1, 2560)];
        // Healthy.
        assert_eq!(
            watchdog_verdict(t0 + Duration::from_secs(1), t0, t0, &then, Some(&then)),
            None
        );
        // Keepalive stale.
        assert_eq!(
            watchdog_verdict(t0 + KEEPALIVE_TIMEOUT * 2, t0, t0, &then, None),
            Some(PrivacyScreenReason::Watchdog)
        );
        // Hard cap, keepalive fresh.
        let late = t0 + MAX_ENGAGED + Duration::from_secs(1);
        assert_eq!(
            watchdog_verdict(late, t0, late, &then, None),
            Some(PrivacyScreenReason::Timeout)
        );
        // A display unplugged.
        let now = vec![display(0, 1920)];
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(
            watchdog_verdict(t1, t0, t1, &then, Some(&now)),
            Some(PrivacyScreenReason::DisplaysChanged)
        );
        // Resolution changed.
        let now = vec![display(0, 1920), display(1, 1920)];
        assert_eq!(
            watchdog_verdict(t1, t0, t1, &then, Some(&now)),
            Some(PrivacyScreenReason::DisplaysChanged)
        );
    }

    #[test]
    fn mosaic_is_tiny_and_keeps_the_average_colour() {
        // 400x200 frame: left half pure red, right half pure blue (BGRA).
        let (w, h) = (400u32, 200u32);
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let o = (y * w as usize + x) * 4;
                if x < 200 {
                    bgra[o + 2] = 255; // R
                } else {
                    bgra[o] = 255; // B
                }
                bgra[o + 3] = 255;
            }
        }
        let png = mosaic_png(&bgra, w as usize * 4, w, h).expect("png");
        let img = crate::branding::decode_png(&png).expect("decodes");
        assert_eq!((img.width, img.height), (40, 20));
        // Leftmost cell red, rightmost blue, and nothing finer than a cell survives.
        let px = |x: u32, y: u32| {
            let o = ((y * img.width + x) * 4) as usize;
            (img.data[o], img.data[o + 1], img.data[o + 2])
        };
        assert!(px(0, 10).0 > 200 && px(0, 10).2 < 40);
        assert!(px(39, 10).2 > 200 && px(39, 10).0 < 40);
        assert!(png.len() < 4096, "mosaic PNG is {} bytes", png.len());
    }

    #[test]
    fn support_is_unsupported_without_a_ui_loop() {
        // Unit tests never run the app loop.
        assert_eq!(support(), PrivacyScreenSupport::Unsupported);
    }
}
