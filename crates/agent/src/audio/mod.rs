//! System audio capture for the session's Opus track.
//!
//! * [`AudioSource`] — platform loopback capture delivering interleaved `f32` PCM
//!   (ScreenCaptureKit audio on macOS, WASAPI loopback on Windows).
//! * [`opus_enc::FrameEncoder`] — resamples/downmixes whatever the source produces to
//!   48 kHz stereo and encodes 20 ms Opus packets.
//!
//! Audio never blocks video: the session runs the source + encoder on its own thread and
//! drops packets when the network side is behind.

use anyhow::Result;
use std::time::Duration;

pub mod opus_enc;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// PCM layout of an [`AudioSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// A running loopback capture.
pub trait AudioSource: Send {
    fn format(&self) -> AudioFormat;
    /// Next block of interleaved `f32` samples (any length), `Ok(None)` on timeout.
    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<f32>>>;
    fn stop(&mut self);
}

/// Whether this platform can capture system audio at all (permission not checked).
pub fn available() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

/// Start capturing the system output mix.
pub fn create_source() -> Result<Box<dyn AudioSource>> {
    #[cfg(target_os = "macos")]
    {
        macos::create()
    }
    #[cfg(target_os = "windows")]
    {
        windows::create()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        anyhow::bail!("system audio capture is not supported on this platform")
    }
}
