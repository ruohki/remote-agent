//! GDI screen capture: the fallback when DXGI desktop duplication cannot run.
//!
//! Duplication needs a Direct3D 11 device on the adapter that drives the monitor. Virtual
//! machines without a 3D-capable display adapter (Hyper-V, Proxmox/QEMU std/virtio VGA, many
//! VDI and server installs) have none — `D3D11CreateDevice` fails with
//! `DXGI_ERROR_UNSUPPORTED` — and an agent that only knows duplication shows nothing at all.
//! `BitBlt` from the screen DC works everywhere, including the sign-in screen and UAC prompts
//! for a SYSTEM process attached to the input desktop. It is slower and has no dirty
//! information, so unchanged frames are detected by comparison and not delivered.

use super::super::{CaptureConfig, Capturer, Frame, FrameData};
use anyhow::{bail, Context, Result};
use protocol::common::DisplayInfo;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
    DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};

pub struct GdiCapturer {
    /// Source rectangle in this process's coordinate space.
    src: (i32, i32, i32, i32),
    width: u32,
    height: u32,
    mem_dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    previous: Vec<u8>,
    min_interval: Duration,
    last_capture: Option<Instant>,
    /// Deliver at least one frame this often even when nothing changed (keeps late joiners
    /// and the encoder's idle refresh fed).
    refresh: Duration,
    last_delivered: Option<Instant>,
    stopped: bool,
}

// SAFETY: the GDI objects are only touched from the thread that owns the capturer at a time
// (`Capturer: Send`, never shared).
unsafe impl Send for GdiCapturer {}

impl GdiCapturer {
    pub fn new(display: &DisplayInfo, cfg: &CaptureConfig) -> Result<Self> {
        crate::platform::windows_desktop::follow_input_desktop();
        let (width, height) = (display.width.max(1), display.height.max(1));
        let scale = if display.scale > 0.0 {
            display.scale
        } else {
            1.0
        };
        let logical_w = (width as f32 / scale).round().max(1.0) as i32;
        let logical_h = (height as f32 / scale).round().max(1.0) as i32;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Negative height: top-down rows, matching the BGRA layout the encoders expect.
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: standard DIB section setup; every object is released in Drop.
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                bail!("GetDC(screen) failed");
            }
            let mem_dc = CreateCompatibleDC(Some(screen));
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(screen), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
            ReleaseDC(None, screen);
            let bitmap = bitmap.context("CreateDIBSection")?;
            if mem_dc.is_invalid() || bits.is_null() {
                let _ = DeleteObject(bitmap.into());
                bail!("creating the capture bitmap failed");
            }
            let old = SelectObject(mem_dc, bitmap.into());
            tracing::info!(
                display = cfg.display_index,
                width,
                height,
                "GDI capture started (no Direct3D 11 device for this display)"
            );
            Ok(Self {
                src: (display.x, display.y, logical_w, logical_h),
                width,
                height,
                mem_dc,
                bitmap,
                old,
                bits: bits as *mut u8,
                previous: Vec::new(),
                min_interval: Duration::from_secs_f64(1.0 / cfg.max_fps.clamp(1, 30) as f64),
                last_capture: None,
                refresh: Duration::from_secs(1),
                last_delivered: None,
                stopped: false,
            })
        }
    }

    fn grab(&mut self) -> Result<()> {
        crate::platform::windows_desktop::follow_input_desktop();
        let (x, y, w, h) = self.src;
        // SAFETY: both DCs are valid for the duration of the blit.
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                bail!("GetDC(screen) failed");
            }
            let r = if w as u32 == self.width && h as u32 == self.height {
                BitBlt(
                    self.mem_dc,
                    0,
                    0,
                    w,
                    h,
                    Some(screen),
                    x,
                    y,
                    SRCCOPY | CAPTUREBLT,
                )
            } else {
                SetStretchBltMode(self.mem_dc, HALFTONE);
                StretchBlt(
                    self.mem_dc,
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    Some(screen),
                    x,
                    y,
                    w,
                    h,
                    SRCCOPY | CAPTUREBLT,
                )
                .ok()
            };
            ReleaseDC(None, screen);
            r.context("BitBlt from the screen")?;
        }
        Ok(())
    }
}

impl Capturer for GdiCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        if self.stopped {
            bail!("capturer stopped");
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(last) = self.last_capture {
                let next = last + self.min_interval;
                let now = Instant::now();
                if next > now {
                    if next > deadline {
                        std::thread::sleep(deadline.saturating_duration_since(now));
                        return Ok(None);
                    }
                    std::thread::sleep(next - now);
                }
            }
            self.last_capture = Some(Instant::now());
            self.grab()?;
            let len = self.width as usize * self.height as usize * 4;
            // SAFETY: the DIB section holds exactly `len` bytes (32 bpp, top-down).
            let pixels = unsafe { std::slice::from_raw_parts(self.bits, len) };
            let due = self
                .last_delivered
                .map(|t| t.elapsed() >= self.refresh)
                .unwrap_or(true);
            if pixels != self.previous.as_slice() || due {
                self.previous.clear();
                self.previous.extend_from_slice(pixels);
                self.last_delivered = Some(Instant::now());
                return Ok(Some(Frame {
                    width: self.width,
                    height: self.height,
                    captured_at: Instant::now(),
                    data: FrameData::Bgra {
                        data: self.previous.clone(),
                        stride: self.width as usize * 4,
                    },
                }));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn stop(&mut self) {
        self.stopped = true;
    }
}

impl Drop for GdiCapturer {
    fn drop(&mut self) {
        // SAFETY: releasing the objects created in `new`, once.
        unsafe {
            SelectObject(self.mem_dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.mem_dc);
        }
    }
}
