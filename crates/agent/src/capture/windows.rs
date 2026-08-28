//! Windows screen capture via DXGI Desktop Duplication (Windows 8+; works from the
//! interactive session incl. UAC/secure desktop when running as SYSTEM in that session).
//!
//! TODO(builder-windows): implement with the `windows` crate:
//! * `list_displays` via `EnumDisplayMonitors` / `IDXGIAdapter::EnumOutputs`, DPI via
//!   `GetDpiForMonitor`.
//! * `IDXGIOutputDuplication::AcquireNextFrame` on a D3D11 device created with
//!   `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` (needed for the Media Foundation encoder and
//!   the `ID3D11VideoProcessor` BGRA→NV12 conversion). Copy the acquired texture into a
//!   pool texture (`CopyResource`) and release the frame immediately.
//! * Handle `DXGI_ERROR_ACCESS_LOST` (mode change, desktop switch) by re-creating the
//!   duplication. Draw the cursor from `DXGI_OUTDUPL_POINTER_SHAPE_INFO` when `show_cursor`.

use super::{CaptureConfig, Capturer};
use anyhow::{bail, Result};
use protocol::common::DisplayInfo;

/// `ID3D11Texture2D` (BGRA) plus the device it belongs to.
pub struct Texture {
    // TODO(builder-windows): hold `windows::Win32::Graphics::Direct3D11::ID3D11Texture2D`
    // and an `Arc` to the shared D3D11 device/context.
    _private: (),
}

// SAFETY: D3D11 objects are free-threaded when created with a multithreaded device;
// the capture thread and encoder thread are the same thread anyway.
unsafe impl Send for Texture {}

impl Texture {
    pub fn to_bgra(&self) -> Result<(Vec<u8>, usize)> {
        bail!("Texture::to_bgra not implemented yet")
    }
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    bail!("Windows display enumeration not implemented yet")
}

pub fn create(_cfg: &CaptureConfig) -> Result<Box<dyn Capturer>> {
    bail!("Windows DXGI capture not implemented yet")
}
