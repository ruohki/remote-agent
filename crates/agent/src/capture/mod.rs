//! Screen capture abstraction.
//!
//! One [`Capturer`] captures one display. Frames are handed to the encoder as
//! [`Frame`]s; on each platform the preferred representation is a GPU-resident
//! handle so capture → encode is zero-copy:
//!
//! * macOS: ScreenCaptureKit delivers IOSurface-backed `CVPixelBuffer`s that
//!   VideoToolbox accepts directly ([`FrameData::PixelBuffer`]).
//! * Windows: DXGI desktop duplication delivers `ID3D11Texture2D`s that a Media
//!   Foundation hardware MFT accepts through the DXGI device manager
//!   ([`FrameData::D3d11Texture`]).
//! * [`FrameData::Bgra`] is the CPU fallback used by the software encoder.
//!
//! Capturers are driven from a dedicated std thread per session (see `session::video`).

use anyhow::Result;
use protocol::common::DisplayInfo;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// Pixel data of a captured frame.
pub enum FrameData {
    /// Tightly packed or strided 8-bit BGRA on the CPU.
    Bgra { data: Vec<u8>, stride: usize },
    #[cfg(target_os = "macos")]
    PixelBuffer(macos::PixelBuffer),
    #[cfg(target_os = "windows")]
    D3d11Texture(windows::Texture),
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Capture timestamp (monotonic).
    pub captured_at: Instant,
    pub data: FrameData,
}

/// Capture settings shared by all platforms.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Index into [`list_displays`].
    pub display_index: u32,
    pub max_fps: u32,
    pub show_cursor: bool,
}

pub trait Capturer: Send {
    /// Block until the next frame is available or `timeout` elapses.
    /// `Ok(None)` means timeout / no new frame (screen unchanged).
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>>;

    /// Current output size (changes when the display resolution changes).
    fn size(&self) -> (u32, u32);

    fn stop(&mut self);
}

/// Enumerate displays in a stable order (primary first).
pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    #[cfg(target_os = "macos")]
    {
        macos::list_displays()
    }
    #[cfg(target_os = "windows")]
    {
        windows::list_displays()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        anyhow::bail!("screen capture is not supported on this platform")
    }
}

pub fn create_capturer(cfg: &CaptureConfig) -> Result<Box<dyn Capturer>> {
    #[cfg(target_os = "macos")]
    {
        macos::create(cfg)
    }
    #[cfg(target_os = "windows")]
    {
        windows::create(cfg)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = cfg;
        anyhow::bail!("screen capture is not supported on this platform")
    }
}

/// Convert any frame into CPU BGRA (used by the software encoder and tests).
pub fn to_bgra(frame: &Frame) -> Result<(Vec<u8>, usize)> {
    match &frame.data {
        FrameData::Bgra { data, stride } => Ok((data.clone(), *stride)),
        #[cfg(target_os = "macos")]
        FrameData::PixelBuffer(pb) => pb.to_bgra(),
        #[cfg(target_os = "windows")]
        FrameData::D3d11Texture(tex) => tex.to_bgra(),
    }
}
