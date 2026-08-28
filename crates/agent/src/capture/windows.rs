//! Windows screen capture via DXGI Desktop Duplication (Windows 8+).
//!
//! * [`list_displays`] enumerates monitors with `EnumDisplayMonitors` and reports the
//!   physical pixel size from the current display mode, the monitor origin in the
//!   *process's* coordinate space, and `scale = physical / logical` (1.0 for a
//!   per-monitor-DPI-aware process, `dpi / 96` for a DPI-unaware one) so that
//!   `logical = physical / scale` always lands in the coordinate system `SendInput` uses.
//! * [`create`] opens `IDXGIOutput1::DuplicateOutput` on a D3D11 device created on the
//!   adapter that owns the monitor (a requirement of desktop duplication). Every acquired
//!   frame is copied into a pooled `D3D11_USAGE_DEFAULT` BGRA texture and released at once,
//!   the cursor is composited into that copy, and the texture is handed to the encoder as
//!   [`FrameData::D3d11Texture`] — the frame never leaves the GPU.
//! * `DXGI_ERROR_ACCESS_LOST` (desktop switch, mode change, driver reset) recreates the
//!   duplication; if the output size changed an error is returned so the pipeline recreates
//!   the capturer and encoder.
//!
//! The capturer runs as SYSTEM inside the interactive session (see `service`), which is
//! what allows the secure desktop / UAC prompts to be captured.

#[path = "windows/cursor.rs"]
pub mod cursor;
#[path = "windows/d3d.rs"]
pub mod d3d;

use super::{CaptureConfig, Capturer, Frame, FrameData};
use anyhow::{anyhow, bail, Context, Result};
use protocol::common::DisplayInfo;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::{Interface, BOOL, PCWSTR};
use windows::Win32::Foundation::{LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_FLAG,
    D3D11_RESOURCE_MISC_FLAG, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_INVALID_CALL, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW, ENUM_CURRENT_SETTINGS,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

pub use d3d::D3dDevice;

/// `MONITORINFO::dwFlags` bit marking the primary monitor (not exported by the `windows` crate).
const MONITORINFOF_PRIMARY: u32 = 1;

/// Number of pooled frame textures. The encoder consumes a frame before the next one is
/// captured, but two keep a still-referenced frame from being overwritten.
const POOL_SIZE: usize = 2;
/// How often to retry re-creating the duplication after `DXGI_ERROR_ACCESS_LOST`.
const ACCESS_LOST_RETRIES: u32 = 3;

// ─── Texture handle ─────────────────────────────────────────────────────────────────────

/// `ID3D11Texture2D` (BGRA, `D3D11_USAGE_DEFAULT`) plus the device it belongs to.
pub struct Texture {
    texture: ID3D11Texture2D,
    device: Arc<D3dDevice>,
    width: u32,
    height: u32,
}

// SAFETY: D3D11 resources are free-threaded; the capture and encode thread are the same
// thread and the context is multithread-protected.
unsafe impl Send for Texture {}

impl Texture {
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    pub fn device(&self) -> &Arc<D3dDevice> {
        &self.device
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Copy the texture into CPU memory as tightly packed BGRA (`stride = width * 4`).
    pub fn to_bgra(&self) -> Result<(Vec<u8>, usize)> {
        self.device.read_bgra(&self.texture)
    }
}

// ─── Display enumeration ────────────────────────────────────────────────────────────────

struct MonitorEntry {
    handle: HMONITOR,
    info: DisplayInfo,
}

unsafe extern "system" fn enum_monitors_callback(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the `*mut Vec<HMONITOR>` passed by `monitor_handles`.
    let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    monitors.push(monitor);
    TRUE
}

fn monitor_handles() -> Result<Vec<HMONITOR>> {
    let mut monitors: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only runs during this call and writes into `monitors`.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitors_callback),
            LPARAM(&mut monitors as *mut _ as isize),
        )
    };
    if !ok.as_bool() {
        bail!("EnumDisplayMonitors failed");
    }
    Ok(monitors)
}

fn monitor_info(handle: HMONITOR) -> Result<MONITORINFOEXW> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `info` is a properly sized MONITORINFOEXW.
    let ok =
        unsafe { GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) };
    if !ok.as_bool() {
        bail!("GetMonitorInfoW failed for monitor {:?}", handle.0);
    }
    Ok(info)
}

/// Physical pixel size of the monitor from its current display mode.
fn physical_size(device_name: &[u16; 32]) -> Option<(u32, u32)> {
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: device_name is NUL terminated (filled by GetMonitorInfoW) and mode is sized.
    let ok = unsafe {
        EnumDisplaySettingsW(
            PCWSTR(device_name.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    };
    if ok.as_bool() && mode.dmPelsWidth > 0 && mode.dmPelsHeight > 0 {
        Some((mode.dmPelsWidth, mode.dmPelsHeight))
    } else {
        None
    }
}

fn effective_dpi(handle: HMONITOR) -> u32 {
    let (mut x, mut y) = (0u32, 0u32);
    // SAFETY: out-pointers are valid.
    match unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut x, &mut y) } {
        Ok(()) if x > 0 => x,
        _ => 96,
    }
}

fn enumerate() -> Result<Vec<MonitorEntry>> {
    let mut entries = Vec::new();
    for (i, handle) in monitor_handles()?.into_iter().enumerate() {
        let info = match monitor_info(handle) {
            Ok(i) => i,
            Err(err) => {
                tracing::warn!("{err:#}");
                continue;
            }
        };
        let rc = info.monitorInfo.rcMonitor;
        let logical_w = (rc.right - rc.left).max(1) as u32;
        let logical_h = (rc.bottom - rc.top).max(1) as u32;
        let (width, height) = physical_size(&info.szDevice).unwrap_or((logical_w, logical_h));
        // Rotated displays report swapped mode dimensions; trust the monitor rect's aspect.
        let (width, height) = if (width > height) != (logical_w > logical_h) && width != height {
            (height, width)
        } else {
            (width, height)
        };
        let scale = width as f32 / logical_w as f32;
        let primary = info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
        let device = String::from_utf16_lossy(&info.szDevice)
            .trim_end_matches('\0')
            .to_string();
        let dpi = effective_dpi(handle);
        let name = if primary {
            format!("Display {} (primary, {dpi} dpi)", i + 1)
        } else {
            format!("Display {} ({dpi} dpi)", i + 1)
        };
        tracing::debug!(device, width, height, scale, primary, "monitor");
        entries.push(MonitorEntry {
            handle,
            info: DisplayInfo {
                index: 0,
                name,
                x: rc.left,
                y: rc.top,
                width,
                height,
                scale,
                primary,
            },
        });
    }
    // Primary first, then by position (left-to-right, top-to-bottom) for stable indices.
    entries.sort_by(|a, b| {
        b.info
            .primary
            .cmp(&a.info.primary)
            .then(a.info.x.cmp(&b.info.x))
            .then(a.info.y.cmp(&b.info.y))
    });
    for (i, e) in entries.iter_mut().enumerate() {
        e.info.index = i as u32;
    }
    if entries.is_empty() {
        bail!("no displays found");
    }
    Ok(entries)
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    Ok(enumerate()?.into_iter().map(|e| e.info).collect())
}

// ─── Capturer ───────────────────────────────────────────────────────────────────────────

struct DxgiCapturer {
    device: Arc<D3dDevice>,
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    width: u32,
    height: u32,
    pool: Vec<Option<ID3D11Texture2D>>,
    pool_next: usize,
    cursor: cursor::CursorCompositor,
    cursor_visible: bool,
    cursor_pos: POINT,
    show_cursor: bool,
    min_interval: Duration,
    last_frame_at: Option<Instant>,
    stopped: bool,
}

// SAFETY: COM pointers move between threads; usage is confined to the pipeline thread.
unsafe impl Send for DxgiCapturer {}

impl DxgiCapturer {
    fn new(dev: Arc<D3dDevice>, output: IDXGIOutput, cfg: &CaptureConfig) -> Result<Self> {
        let output: IDXGIOutput1 = output
            .cast()
            .context("IDXGIOutput1 is required (Windows 8+)")?;
        let mut me = Self {
            device: dev,
            output,
            duplication: None,
            width: 0,
            height: 0,
            pool: (0..POOL_SIZE).map(|_| None).collect(),
            pool_next: 0,
            cursor: cursor::CursorCompositor::default(),
            cursor_visible: false,
            cursor_pos: POINT::default(),
            show_cursor: cfg.show_cursor,
            min_interval: Duration::from_secs_f64(1.0 / cfg.max_fps.max(1) as f64),
            last_frame_at: None,
            stopped: false,
        };
        let (w, h) = me.open_duplication()?;
        me.width = w;
        me.height = h;
        Ok(me)
    }

    /// (Re)create the duplication and return the output size it reports.
    fn open_duplication(&mut self) -> Result<(u32, u32)> {
        self.duplication = None;
        // SAFETY: the device lives on the output's adapter (see d3d::for_monitor).
        let dup = unsafe { self.output.DuplicateOutput(&self.device.device) }
            .context("IDXGIOutput1::DuplicateOutput (is another duplication session active, or is the desktop not accessible from this session?)")?;
        // SAFETY: plain COM call.
        let desc = unsafe { dup.GetDesc() };
        self.duplication = Some(dup);
        Ok((desc.ModeDesc.Width, desc.ModeDesc.Height))
    }

    fn pool_texture(&mut self) -> Result<ID3D11Texture2D> {
        let idx = self.pool_next;
        self.pool_next = (self.pool_next + 1) % POOL_SIZE;
        if let Some(tex) = &self.pool[idx] {
            return Ok(tex.clone());
        }
        let tex = self.device.create_texture(
            self.width,
            self.height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
            D3D11_CPU_ACCESS_FLAG(0),
            D3D11_RESOURCE_MISC_FLAG(0),
        )?;
        self.pool[idx] = Some(tex.clone());
        Ok(tex)
    }

    fn update_pointer(&mut self, dup: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            self.cursor_visible = info.PointerPosition.Visible.as_bool();
            self.cursor_pos = info.PointerPosition.Position;
        }
        if info.PointerShapeBufferSize > 0 {
            let required = info.PointerShapeBufferSize;
            let buf = self.cursor.shape_buffer(required);
            let mut written = 0u32;
            let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: buf has at least `required` bytes.
            let res = unsafe {
                dup.GetFramePointerShape(
                    required,
                    buf.as_mut_ptr().cast(),
                    &mut written,
                    &mut shape_info,
                )
            };
            match res {
                Ok(()) => self.cursor.set_shape_info(shape_info),
                Err(err) => tracing::debug!("GetFramePointerShape failed: {err}"),
            }
        }
    }

    /// One acquire attempt. `Ok(None)` = timeout; `Err` with `AccessLost` marker = recreate.
    fn acquire(&mut self, timeout: Duration) -> Result<AcquireResult> {
        let dup = self
            .duplication
            .clone()
            .ok_or_else(|| anyhow!("duplication not open"))?;
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        // SAFETY: out-pointers are valid; the frame is released below.
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(AcquireResult::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Ok(AcquireResult::AccessLost),
            Err(e) if e.code() == DXGI_ERROR_INVALID_CALL => {
                // A frame is still held (should not happen); release and report as retryable.
                // SAFETY: plain COM call.
                let _ = unsafe { dup.ReleaseFrame() };
                return Ok(AcquireResult::Retry);
            }
            Err(e) => return Err(e).context("AcquireNextFrame"),
        }
        let captured_at = Instant::now();
        let result = (|| -> Result<ID3D11Texture2D> {
            let resource =
                resource.ok_or_else(|| anyhow!("AcquireNextFrame returned no resource"))?;
            let frame_tex: ID3D11Texture2D =
                resource.cast().context("frame resource is not a texture")?;
            self.update_pointer(&dup, &info);
            let target = self.pool_texture()?;
            // SAFETY: both textures are BGRA of the duplication size.
            unsafe { self.device.context.CopyResource(&target, &frame_tex) };
            Ok(target)
        })();
        // SAFETY: always release, even on error, or the next acquire fails.
        if let Err(err) = unsafe { dup.ReleaseFrame() } {
            tracing::debug!("ReleaseFrame: {err}");
        }
        let target = result?;
        if self.show_cursor && self.cursor_visible && self.cursor.has_shape() {
            let (w, h, x, y) = (
                self.width,
                self.height,
                self.cursor_pos.x,
                self.cursor_pos.y,
            );
            if let Err(err) = self.cursor.composite(&self.device, &target, w, h, x, y) {
                tracing::debug!("cursor composite failed: {err:#}");
            }
        }
        Ok(AcquireResult::Frame(Frame {
            width: self.width,
            height: self.height,
            captured_at,
            data: FrameData::D3d11Texture(Texture {
                texture: target,
                device: Arc::clone(&self.device),
                width: self.width,
                height: self.height,
            }),
        }))
    }
}

enum AcquireResult {
    Frame(Frame),
    Timeout,
    AccessLost,
    Retry,
}

impl Capturer for DxgiCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        if self.stopped {
            bail!("capturer stopped");
        }
        let deadline = Instant::now() + timeout;
        // Frame pacing: never deliver faster than max_fps.
        if let Some(last) = self.last_frame_at {
            let next = last + self.min_interval;
            let now = Instant::now();
            if next > now {
                std::thread::sleep((next - now).min(timeout));
            }
        }
        let mut access_lost = 0;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if self.duplication.is_none() {
                let (w, h) = self
                    .open_duplication()
                    .context("re-creating desktop duplication")?;
                if (w, h) != (self.width, self.height) {
                    bail!(
                        "display mode changed ({}x{} → {w}x{h}); capturer must be re-created",
                        self.width,
                        self.height
                    );
                }
                self.pool.iter_mut().for_each(|t| *t = None);
            }
            match self.acquire(remaining)? {
                AcquireResult::Frame(frame) => {
                    self.last_frame_at = Some(Instant::now());
                    return Ok(Some(frame));
                }
                AcquireResult::Timeout => return Ok(None),
                AcquireResult::Retry => continue,
                AcquireResult::AccessLost => {
                    access_lost += 1;
                    if access_lost > ACCESS_LOST_RETRIES {
                        bail!("desktop duplication access lost repeatedly");
                    }
                    tracing::info!(
                        "desktop duplication access lost; re-creating (attempt {access_lost})"
                    );
                    self.duplication = None;
                    // Give the desktop switch / mode change a moment to settle.
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn stop(&mut self) {
        self.stopped = true;
        self.duplication = None;
        self.pool.iter_mut().for_each(|t| *t = None);
    }
}

pub fn create(cfg: &CaptureConfig) -> Result<Box<dyn Capturer>> {
    let entries = enumerate()?;
    let entry = entries
        .into_iter()
        .find(|e| e.info.index == cfg.display_index)
        .ok_or_else(|| anyhow!("display index {} does not exist", cfg.display_index))?;
    let (dev, output) = d3d::for_monitor(entry.handle)?;
    let capturer = DxgiCapturer::new(dev, output, cfg)?;
    tracing::info!(
        display = cfg.display_index,
        width = capturer.width,
        height = capturer.height,
        fps = cfg.max_fps,
        "DXGI desktop duplication started"
    );
    Ok(Box::new(capturer))
}
