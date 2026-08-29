//! Client-side cursor: the capture omits the system cursor and the agent streams the cursor
//! shape (PNG + hotspot, when it changes) and position (≤ 60 Hz, on change) on the control
//! channel, so the browser draws it locally and it never lags behind the video.
//!
//! [`CursorSource`] is polled from a dedicated thread per session
//! ([`crate::session`]); platform implementations live below. Positions are reported in
//! physical pixels of the display that contains the cursor.

use protocol::common::DisplayInfo;
use std::time::Duration;

/// One update from a [`CursorSource`].
#[derive(Debug, Clone, PartialEq)]
pub enum CursorUpdate {
    Shape {
        id: u32,
        png: Vec<u8>,
        hotspot_x: u32,
        hotspot_y: u32,
        width: u32,
        height: u32,
    },
    Position {
        display: u32,
        x: i32,
        y: i32,
        shape_id: u32,
        visible: bool,
    },
}

/// Blocking poll interface; implementations sleep internally to pace themselves.
pub trait CursorSource: Send {
    /// Wait up to `timeout` for the next update; `None` when nothing changed.
    fn next(&mut self, timeout: Duration) -> Option<CursorUpdate>;
}

/// Position poll rate (upper bound for `cursor_position` messages).
pub const POSITION_HZ: u32 = 60;
/// Shape poll rate (shapes change rarely; the check is cheap but goes through the main thread).
pub const SHAPE_HZ: u32 = 10;

/// Map a global *logical* point to `(display index, physical x, physical y)`.
pub fn locate(displays: &[DisplayInfo], x: f64, y: f64) -> Option<(u32, i32, i32)> {
    for d in displays {
        let scale = if d.scale > 0.0 { d.scale as f64 } else { 1.0 };
        let (lw, lh) = (d.width as f64 / scale, d.height as f64 / scale);
        let (dx, dy) = (x - d.x as f64, y - d.y as f64);
        if dx >= 0.0 && dy >= 0.0 && dx < lw && dy < lh {
            return Some((
                d.index,
                (dx * scale).round() as i32,
                (dy * scale).round() as i32,
            ));
        }
    }
    None
}

/// Stable id for a shape from its PNG bytes.
pub fn shape_id(png: &[u8]) -> u32 {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(png);
    u32::from_le_bytes([h[0], h[1], h[2], h[3]]) | 1
}

/// Width/height from a PNG header (IHDR).
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

/// PNG bytes plus the cursor's hotspot and size in logical units (points on macOS,
/// bitmap pixels on Windows).
#[allow(dead_code)]
type ShapeSample = (Vec<u8>, (f64, f64), (f64, f64));

/// Platform cursor source, if the platform provides one.
pub fn create_source() -> Option<Box<dyn CursorSource>> {
    #[cfg(target_os = "macos")]
    {
        macos::create().map(|s| Box::new(s) as Box<dyn CursorSource>)
    }
    #[cfg(target_os = "windows")]
    {
        windows::create().map(|s| Box::new(s) as Box<dyn CursorSource>)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Shared pacing logic: a position sample at `POSITION_HZ`, a shape check every
/// `POSITION_HZ / SHAPE_HZ` samples, position emitted only on change.
struct Pacer {
    displays: Vec<DisplayInfo>,
    last_pos: Option<(u32, i32, i32, bool)>,
    shape_id: u32,
    /// Display scale the current shape was emitted for; a different scale re-emits it.
    shape_scale: f64,
    ticks: u32,
}

impl Pacer {
    fn new(displays: Vec<DisplayInfo>) -> Self {
        Self {
            displays,
            last_pos: None,
            shape_id: 0,
            shape_scale: 0.0,
            ticks: 0,
        }
    }

    /// Scale factor of the display the cursor was last seen on (primary display before the
    /// first position, `1.0` without displays).
    fn current_scale(&self) -> f64 {
        let idx = self.last_pos.map(|p| p.0);
        self.displays
            .iter()
            .find(|d| Some(d.index) == idx)
            .or_else(|| self.displays.iter().find(|d| d.primary))
            .or_else(|| self.displays.first())
            .map(|d| if d.scale > 0.0 { d.scale as f64 } else { 1.0 })
            .unwrap_or(1.0)
    }

    fn position_update(&mut self, global: Option<(f64, f64)>) -> Option<CursorUpdate> {
        let sample = match global.and_then(|(x, y)| locate(&self.displays, x, y)) {
            Some((d, x, y)) => (d, x, y, true),
            None => match self.last_pos {
                Some((d, x, y, _)) => (d, x, y, false),
                None => return None,
            },
        };
        if self.last_pos == Some(sample) {
            return None;
        }
        self.last_pos = Some(sample);
        Some(CursorUpdate::Position {
            display: sample.0,
            x: sample.1,
            y: sample.2,
            shape_id: self.shape_id,
            visible: sample.3,
        })
    }

    fn shape_due(&mut self) -> bool {
        self.ticks = self.ticks.wrapping_add(1);
        self.ticks % (POSITION_HZ / SHAPE_HZ).max(1) == 1
    }

    /// `hotspot` and `size` are the cursor's logical geometry (points); both are reported in
    /// physical pixels of the display (`scale`) so the browser can size the image with the
    /// picture regardless of the PNG's own pixel density.
    fn shape_update(
        &mut self,
        png: Vec<u8>,
        hotspot: (f64, f64),
        size: (f64, f64),
        scale: f64,
    ) -> Option<CursorUpdate> {
        let id = shape_id(&png);
        if id == self.shape_id && scale == self.shape_scale {
            return None;
        }
        png_size(&png)?;
        self.shape_id = id;
        self.shape_scale = scale;
        let px = |v: f64| (v * scale).round().max(0.0) as u32;
        Some(CursorUpdate::Shape {
            id,
            png,
            hotspot_x: px(hotspot.0),
            hotspot_y: px(hotspot.1),
            width: px(size.0).max(1),
            height: px(size.1).max(1),
        })
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSCursor};
    use objc2_core_graphics::CGEvent;
    use objc2_foundation::NSDictionary;
    use std::collections::VecDeque;
    use std::time::Instant;

    pub struct MacCursor {
        pacer: Pacer,
        queue: VecDeque<CursorUpdate>,
        next_tick: Instant,
        /// Main-thread shape queries are only possible while the app loop runs.
        shapes: bool,
    }

    pub fn create() -> Option<MacCursor> {
        let displays = crate::capture::list_displays().ok()?;
        if displays.is_empty() {
            return None;
        }
        Some(MacCursor {
            pacer: Pacer::new(displays),
            queue: VecDeque::new(),
            next_tick: Instant::now(),
            shapes: crate::platform::main_loop_running(),
        })
    }

    /// Current global cursor position in logical points (top-left origin).
    fn global_position() -> Option<(f64, f64)> {
        let ev = CGEvent::new(None)?;
        let p = CGEvent::location(Some(&ev));
        Some((p.x, p.y))
    }

    /// Current system cursor as PNG plus its hotspot and size in points (main thread).
    fn current_shape() -> Option<ShapeSample> {
        crate::platform::run_on_main(|| {
            // Deprecated by Apple but still the only public way to read the *system*
            // cursor (`currentCursor` only knows this app's own cursor).
            #[allow(deprecated)]
            let cursor = NSCursor::currentSystemCursor()?;
            let image = cursor.image();
            let hot = cursor.hotSpot();
            let size = image.size();
            let tiff = image.TIFFRepresentation()?;
            let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
            let props = NSDictionary::new();
            // SAFETY: valid rep and (empty) properties dictionary.
            let png = unsafe {
                rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props)
            }?;
            Some((png.to_vec(), (hot.x, hot.y), (size.width, size.height)))
        })
        .ok()
        .flatten()
    }

    impl CursorSource for MacCursor {
        fn next(&mut self, timeout: Duration) -> Option<CursorUpdate> {
            if let Some(u) = self.queue.pop_front() {
                return Some(u);
            }
            let now = Instant::now();
            if self.next_tick > now {
                let wait = (self.next_tick - now).min(timeout);
                std::thread::sleep(wait);
                if self.next_tick > Instant::now() {
                    return None;
                }
            }
            self.next_tick = Instant::now() + Duration::from_secs_f64(1.0 / POSITION_HZ as f64);
            if self.shapes && self.pacer.shape_due() {
                if let Some((png, hot, size)) = current_shape() {
                    let scale = self.pacer.current_scale();
                    if let Some(u) = self.pacer.shape_update(png, hot, size, scale) {
                        self.queue.push_back(u);
                    }
                }
            }
            if let Some(u) = self.pacer.position_update(global_position()) {
                self.queue.push_back(u);
            }
            self.queue.pop_front()
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::*;
    use ::windows::Win32::Foundation::POINT;
    use ::windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS, HBITMAP,
    };
    use ::windows::Win32::UI::WindowsAndMessaging::{
        GetCursorInfo, GetCursorPos, GetIconInfo, CURSORINFO, CURSOR_SHOWING, ICONINFO,
    };
    use std::collections::VecDeque;
    use std::time::Instant;

    pub struct WinCursor {
        pacer: Pacer,
        queue: VecDeque<CursorUpdate>,
        next_tick: Instant,
        last_handle: isize,
    }

    pub fn create() -> Option<WinCursor> {
        let displays = crate::capture::list_displays().ok()?;
        if displays.is_empty() {
            return None;
        }
        Some(WinCursor {
            pacer: Pacer::new(displays),
            queue: VecDeque::new(),
            next_tick: Instant::now(),
            last_handle: 0,
        })
    }

    fn global_position() -> Option<(f64, f64, bool)> {
        let mut p = POINT::default();
        // SAFETY: plain Win32 call with a valid out pointer.
        unsafe { GetCursorPos(&mut p) }.ok()?;
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: cbSize initialised.
        let visible = unsafe { GetCursorInfo(&mut info) }.is_ok() && info.flags == CURSOR_SHOWING;
        Some((p.x as f64, p.y as f64, visible))
    }

    /// Read a bitmap as top-down BGRA.
    fn bitmap_bgra(hbm: HBITMAP) -> Option<(Vec<u8>, u32, u32)> {
        // SAFETY: GDI calls with properly sized structures.
        unsafe {
            let dc = GetDC(None);
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    ..Default::default()
                },
                ..Default::default()
            };
            if GetDIBits(dc, hbm, 0, 0, None, &mut info, DIB_RGB_COLORS) == 0 {
                ReleaseDC(None, dc);
                return None;
            }
            let (w, h) = (
                info.bmiHeader.biWidth.max(0) as u32,
                info.bmiHeader.biHeight.unsigned_abs(),
            );
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB.0;
            info.bmiHeader.biHeight = -(h as i32);
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let rows = GetDIBits(
                dc,
                hbm,
                0,
                h,
                Some(buf.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            );
            ReleaseDC(None, dc);
            (rows != 0).then_some((buf, w, h))
        }
    }

    /// Current cursor as PNG + hotspot; `None` when unchanged (`last_handle`).
    /// Cursor bitmaps are already in physical pixels, so the size is reported with scale 1.
    fn current_shape(last_handle: &mut isize) -> Option<ShapeSample> {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: cbSize initialised.
        unsafe { GetCursorInfo(&mut info) }.ok()?;
        let handle = info.hCursor.0 as isize;
        if handle == 0 || handle == *last_handle {
            return None;
        }
        *last_handle = handle;
        let mut icon = ICONINFO::default();
        // SAFETY: valid cursor handle; the bitmaps returned are owned by us.
        unsafe { GetIconInfo(info.hCursor.into(), &mut icon) }.ok()?;
        let color = (!icon.hbmColor.is_invalid())
            .then(|| bitmap_bgra(icon.hbmColor))
            .flatten();
        let mask = bitmap_bgra(icon.hbmMask);
        // SAFETY: bitmaps from GetIconInfo must be deleted by the caller.
        unsafe {
            let _ = DeleteObject(icon.hbmColor.into());
            let _ = DeleteObject(icon.hbmMask.into());
        }
        let (mut bgra, w, h) = match (color, mask) {
            (Some((c, w, h)), Some((m, mw, mh)))
                if (mw, mh) == (w, h) || (mw, mh) == (w, h * 2) =>
            {
                // Colour cursor: the mask's top half is the AND mask (1 = transparent).
                let mut c = c;
                let opaque = c.chunks_exact(4).any(|p| p[3] != 0);
                for (i, px) in c.chunks_exact_mut(4).enumerate() {
                    let masked = m[i * 4] != 0;
                    if !opaque {
                        px[3] = if masked { 0 } else { 255 };
                    } else if masked {
                        px[3] = 0;
                    }
                }
                (c, w, h)
            }
            (None, Some((m, w, h2))) => {
                // Monochrome cursor: AND mask (top) + XOR mask (bottom).
                let h = h2 / 2;
                let mut out = vec![0u8; (w * h * 4) as usize];
                for i in 0..(w * h) as usize {
                    let and = m[i * 4] != 0;
                    let xor = m[(i + (w * h) as usize) * 4] != 0;
                    let (v, a) = match (and, xor) {
                        (false, false) => (0u8, 255u8),
                        (false, true) => (255, 255),
                        (true, false) => (0, 0),
                        (true, true) => (255, 255), // inverted → white
                    };
                    out[i * 4..i * 4 + 4].copy_from_slice(&[v, v, v, a]);
                }
                (out, w, h)
            }
            _ => return None,
        };
        // Premultiplied alpha is not expected by the browser; convert BGRA → RGBA for PNG.
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let png = crate::branding::encode_png(&crate::branding::Rgba::from_rgba(w, h, bgra));
        Some((
            png,
            (icon.xHotspot as f64, icon.yHotspot as f64),
            (w as f64, h as f64),
        ))
    }

    impl CursorSource for WinCursor {
        fn next(&mut self, timeout: Duration) -> Option<CursorUpdate> {
            if let Some(u) = self.queue.pop_front() {
                return Some(u);
            }
            let now = Instant::now();
            if self.next_tick > now {
                let wait = (self.next_tick - now).min(timeout);
                std::thread::sleep(wait);
                if self.next_tick > Instant::now() {
                    return None;
                }
            }
            self.next_tick = Instant::now() + Duration::from_secs_f64(1.0 / POSITION_HZ as f64);
            if self.pacer.shape_due() {
                if let Some((png, hot, size)) = current_shape(&mut self.last_handle) {
                    if let Some(u) = self.pacer.shape_update(png, hot, size, 1.0) {
                        self.queue.push_back(u);
                    }
                }
            }
            let pos = global_position();
            let global = pos.and_then(|(x, y, v)| v.then_some((x, y)));
            if let Some(u) = self.pacer.position_update(global) {
                self.queue.push_back(u);
            }
            self.queue.pop_front()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn displays() -> Vec<DisplayInfo> {
        vec![
            DisplayInfo {
                index: 0,
                name: "A".into(),
                x: 0,
                y: 0,
                width: 2880,
                height: 1800,
                scale: 2.0,
                primary: true,
            },
            DisplayInfo {
                index: 1,
                name: "B".into(),
                x: 1440,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                primary: false,
            },
        ]
    }

    #[test]
    fn locate_maps_logical_points_to_physical_pixels() {
        let d = displays();
        assert_eq!(locate(&d, 10.0, 20.0), Some((0, 20, 40)));
        assert_eq!(locate(&d, 1500.0, 100.0), Some((1, 60, 100)));
        assert_eq!(locate(&d, -5.0, 0.0), None);
        assert_eq!(locate(&d, 5000.0, 0.0), None);
    }

    #[test]
    fn pacer_emits_positions_only_on_change_and_hides_off_screen() {
        let mut p = Pacer::new(displays());
        assert!(matches!(
            p.position_update(Some((10.0, 10.0))),
            Some(CursorUpdate::Position {
                display: 0,
                x: 20,
                y: 20,
                visible: true,
                ..
            })
        ));
        assert_eq!(p.position_update(Some((10.0, 10.0))), None);
        assert!(matches!(
            p.position_update(None),
            Some(CursorUpdate::Position { visible: false, .. })
        ));
        assert_eq!(p.position_update(None), None);
    }

    #[test]
    fn shape_updates_dedupe_by_content() {
        let mut p = Pacer::new(displays());
        let png = crate::branding::encode_png(&crate::branding::Rgba::new(4, 4));
        assert!(matches!(
            p.shape_update(png.clone(), (1.0, 2.0), (4.0, 4.0), 1.0),
            Some(CursorUpdate::Shape {
                width: 4,
                height: 4,
                hotspot_x: 1,
                hotspot_y: 2,
                ..
            })
        ));
        assert_eq!(p.shape_update(png, (1.0, 2.0), (4.0, 4.0), 1.0), None);
        assert_eq!(png_size(b"nope"), None);
    }

    #[test]
    fn shape_geometry_follows_the_display_scale() {
        let mut p = Pacer::new(displays());
        // Before any position: the primary (Retina) display's scale applies.
        assert_eq!(p.current_scale(), 2.0);
        let png = crate::branding::encode_png(&crate::branding::Rgba::new(32, 32));
        assert!(matches!(
            p.shape_update(png.clone(), (4.0, 2.0), (16.0, 16.0), p.current_scale()),
            Some(CursorUpdate::Shape {
                width: 32,
                height: 32,
                hotspot_x: 8,
                hotspot_y: 4,
                ..
            })
        ));
        // Moving to the 1× display re-emits the same shape at its native size.
        p.position_update(Some((1500.0, 100.0)));
        assert_eq!(p.current_scale(), 1.0);
        assert!(matches!(
            p.shape_update(png.clone(), (4.0, 2.0), (16.0, 16.0), p.current_scale()),
            Some(CursorUpdate::Shape {
                width: 16,
                height: 16,
                hotspot_x: 4,
                hotspot_y: 2,
                ..
            })
        ));
        assert_eq!(p.shape_update(png, (4.0, 2.0), (16.0, 16.0), 1.0), None);
    }
}
