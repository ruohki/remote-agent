//! macOS screen capture via ScreenCaptureKit (macOS 12.3+).
//!
//! TODO(builder-macos): implement with `objc2-screen-capture-kit`:
//! * `list_displays` from `SCShareableContent` / `CGGetActiveDisplayList`, with
//!   `NSScreen` for names and backing scale.
//! * `SCStream` with an `SCContentFilter` for the chosen display, pixel format
//!   `kCVPixelFormatType_32BGRA` (or NV12 `420v` if the encoder prefers it),
//!   `minimumFrameInterval = 1/max_fps`, `showsCursor`.
//! * An `SCStreamOutput` delegate that retains the `CVPixelBuffer` of each
//!   `CMSampleBuffer` (only frames whose `SCStreamFrameInfoStatus` is complete)
//!   and pushes it through a bounded channel to `next_frame`.

use super::{CaptureConfig, Capturer};
use anyhow::{bail, Result};
use protocol::common::DisplayInfo;

/// Retained `CVPixelBufferRef` handed to VideoToolbox without copying.
pub struct PixelBuffer {
    // TODO(builder-macos): wrap `objc2_core_video::CVPixelBuffer` (CFRetained).
    _private: (),
}

// SAFETY: CVPixelBuffer is a thread-safe CoreFoundation object.
unsafe impl Send for PixelBuffer {}

impl PixelBuffer {
    pub fn to_bgra(&self) -> Result<(Vec<u8>, usize)> {
        bail!("PixelBuffer::to_bgra not implemented yet")
    }
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    bail!("macOS display enumeration not implemented yet")
}

pub fn create(_cfg: &CaptureConfig) -> Result<Box<dyn Capturer>> {
    bail!("macOS ScreenCaptureKit capture not implemented yet")
}
