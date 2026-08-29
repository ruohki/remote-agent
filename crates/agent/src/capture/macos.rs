//! macOS screen capture via ScreenCaptureKit (macOS 12.3+).
//!
//! * [`list_displays`] enumerates displays with CoreGraphics (no TCC permission needed) and
//!   uses `NSScreen` for localized names when called on the main thread.
//! * [`create`] builds an `SCStream` for one display that delivers IOSurface-backed
//!   `CVPixelBuffer`s (BGRA). Only frames whose `SCStreamFrameInfoStatus` is `Complete` are
//!   forwarded, through a bounded channel (capacity 2, oldest dropped) into
//!   [`Capturer::next_frame`].
//! * A frame whose size differs from the configured size, or a stream that stopped with an
//!   error, surfaces as an `Err` from `next_frame` so the pipeline recreates the capturer.

use super::{CaptureConfig, Capturer, Frame, FrameData};
use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsBuiltin,
    CGDisplayIsMain, CGDisplayMode, CGGetActiveDisplayList, CGPreflightScreenCaptureAccess,
    CGRequestScreenCaptureAccess,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};
use protocol::common::DisplayInfo;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_DISPLAYS: usize = 16;
/// How long to wait for ScreenCaptureKit's asynchronous start/stop/enumeration calls.
const SCK_CALL_TIMEOUT: Duration = Duration::from_secs(10);

// ─── Pixel buffer wrapper ───────────────────────────────────────────────────────────────

/// Retained `CVPixelBufferRef` handed to VideoToolbox without copying.
pub struct PixelBuffer {
    inner: CFRetained<CVPixelBuffer>,
}

// SAFETY: CVPixelBuffer is a reference-counted CoreFoundation object that may be used from
// any thread; we only ever hand it to one thread at a time.
unsafe impl Send for PixelBuffer {}

impl PixelBuffer {
    pub fn new(inner: CFRetained<CVPixelBuffer>) -> Self {
        Self { inner }
    }

    /// The underlying buffer (a `CVImageBuffer`, accepted by `VTCompressionSessionEncodeFrame`).
    pub fn as_cv(&self) -> &CVPixelBuffer {
        &self.inner
    }

    pub fn width(&self) -> u32 {
        CVPixelBufferGetWidth(&self.inner) as u32
    }

    pub fn height(&self) -> u32 {
        CVPixelBufferGetHeight(&self.inner) as u32
    }

    pub fn pixel_format(&self) -> u32 {
        CVPixelBufferGetPixelFormatType(&self.inner)
    }

    /// Copy the pixels into a tightly packed BGRA buffer (stride = width * 4).
    pub fn to_bgra(&self) -> Result<(Vec<u8>, usize)> {
        if self.pixel_format() != kCVPixelFormatType_32BGRA {
            bail!(
                "pixel buffer is not 32BGRA (format {:#x})",
                self.pixel_format()
            );
        }
        let width = self.width() as usize;
        let height = self.height() as usize;
        // SAFETY: lock/unlock are paired; the base address is only read while locked.
        unsafe {
            let ret = CVPixelBufferLockBaseAddress(&self.inner, CVPixelBufferLockFlags::ReadOnly);
            if ret != 0 {
                bail!("CVPixelBufferLockBaseAddress failed: {ret}");
            }
            let base = CVPixelBufferGetBaseAddress(&self.inner) as *const u8;
            let src_stride = CVPixelBufferGetBytesPerRow(&self.inner);
            let result = if base.is_null() {
                Err(anyhow!("pixel buffer has no base address"))
            } else {
                let dst_stride = width * 4;
                let mut out = vec![0u8; dst_stride * height];
                for row in 0..height {
                    let src = std::slice::from_raw_parts(base.add(row * src_stride), dst_stride);
                    out[row * dst_stride..(row + 1) * dst_stride].copy_from_slice(src);
                }
                Ok((out, dst_stride))
            };
            CVPixelBufferUnlockBaseAddress(&self.inner, CVPixelBufferLockFlags::ReadOnly);
            result
        }
    }
}

// ─── Display enumeration ────────────────────────────────────────────────────────────────

struct DisplayEntry {
    id: CGDirectDisplayID,
    info: DisplayInfo,
}

fn active_display_ids() -> Result<Vec<CGDirectDisplayID>> {
    let mut ids = [0 as CGDirectDisplayID; MAX_DISPLAYS];
    let mut count: u32 = 0;
    // SAFETY: the buffer is large enough for MAX_DISPLAYS entries and count receives the
    // number actually written.
    let err = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    if err != objc2_core_graphics::CGError::Success {
        bail!("CGGetActiveDisplayList failed: {err:?}");
    }
    Ok(ids[..count as usize].to_vec())
}

/// Localized display names keyed by display id, only available on the main thread.
fn screen_names() -> std::collections::HashMap<CGDirectDisplayID, String> {
    let mut names = std::collections::HashMap::new();
    let Some(mtm) = MainThreadMarker::new() else {
        return names;
    };
    let key = NSString::from_str("NSScreenNumber");
    for screen in objc2_app_kit::NSScreen::screens(mtm).iter() {
        let desc = screen.deviceDescription();
        if let Some(num) = desc.objectForKey(&key) {
            // NSScreenNumber is an NSNumber holding the CGDirectDisplayID.
            let id: u32 = unsafe { msg_send![&*num, unsignedIntValue] };
            names.insert(id, screen.localizedName().to_string());
        }
    }
    names
}

fn enumerate() -> Result<Vec<DisplayEntry>> {
    let ids = active_display_ids()?;
    let names = screen_names();
    let mut entries = Vec::with_capacity(ids.len());
    let mut external_count = 0;
    for id in ids {
        let bounds = CGDisplayBounds(id);
        let mode = CGDisplayCopyDisplayMode(id);
        let (pixel_w, pixel_h, point_w) = match &mode {
            Some(m) => (
                CGDisplayMode::pixel_width(Some(m)),
                CGDisplayMode::pixel_height(Some(m)),
                CGDisplayMode::width(Some(m)),
            ),
            None => (
                objc2_core_graphics::CGDisplayPixelsWide(id),
                objc2_core_graphics::CGDisplayPixelsHigh(id),
                bounds.size.width as usize,
            ),
        };
        if pixel_w == 0 || pixel_h == 0 {
            continue;
        }
        let scale = if point_w > 0 {
            pixel_w as f32 / point_w as f32
        } else {
            1.0
        };
        let builtin = CGDisplayIsBuiltin(id);
        let name = names.get(&id).cloned().unwrap_or_else(|| {
            if builtin {
                "Built-in Display".to_string()
            } else {
                external_count += 1;
                format!("Display {external_count}")
            }
        });
        entries.push(DisplayEntry {
            id,
            info: DisplayInfo {
                index: 0,
                name,
                x: bounds.origin.x.round() as i32,
                y: bounds.origin.y.round() as i32,
                width: pixel_w as u32,
                height: pixel_h as u32,
                scale,
                primary: CGDisplayIsMain(id),
            },
        });
    }
    // Primary first, then left-to-right / top-to-bottom — a stable order across calls.
    entries.sort_by(|a, b| {
        b.info
            .primary
            .cmp(&a.info.primary)
            .then(a.info.x.cmp(&b.info.x))
            .then(a.info.y.cmp(&b.info.y))
            .then(a.id.cmp(&b.id))
    });
    for (i, e) in entries.iter_mut().enumerate() {
        e.info.index = i as u32;
    }
    Ok(entries)
}

pub fn list_displays() -> Result<Vec<DisplayInfo>> {
    Ok(enumerate()?.into_iter().map(|e| e.info).collect())
}

// ─── ScreenCaptureKit plumbing ──────────────────────────────────────────────────────────

/// `Retained<T>` that may cross threads. ScreenCaptureKit objects are documented as
/// thread-safe; we still only use them from the capture thread and the SCK callback queue.
struct SendRetained<T>(Retained<T>);
// SAFETY: see type docs.
unsafe impl<T> Send for SendRetained<T> {}

/// Fetch the shareable content synchronously (SCK only offers an async API).
fn shareable_content() -> Result<SendRetained<SCShareableContent>> {
    let (tx, rx) =
        crossbeam_channel::bounded::<Result<SendRetained<SCShareableContent>, String>>(1);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if !error.is_null() {
                // SAFETY: SCK hands us a valid NSError pointer.
                let err = unsafe { &*error };
                Err(err.localizedDescription().to_string())
            } else if content.is_null() {
                Err("no shareable content returned".to_string())
            } else {
                // SAFETY: SCK hands us a valid, +0 pointer that we retain.
                match unsafe { Retained::retain(content) } {
                    Some(c) => Ok(SendRetained(c)),
                    None => Err("failed to retain SCShareableContent".to_string()),
                }
            };
            let _ = tx.try_send(result);
        },
    );
    // SAFETY: the block outlives the call (SCK copies it).
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, false, &block,
        );
    }
    match rx.recv_timeout(SCK_CALL_TIMEOUT) {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(msg)) => {
            bail!("SCShareableContent failed: {msg} (is Screen Recording permission granted?)")
        }
        Err(_) => bail!("timed out waiting for SCShareableContent"),
    }
}

/// Run an SCK completion-handler based call synchronously.
fn call_with_completion(
    what: &str,
    f: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> Result<()> {
    let (tx, rx) = crossbeam_channel::bounded::<Option<String>>(1);
    let block = RcBlock::new(move |error: *mut NSError| {
        let msg = if error.is_null() {
            None
        } else {
            // SAFETY: valid NSError pointer from SCK.
            Some(unsafe { &*error }.localizedDescription().to_string())
        };
        let _ = tx.try_send(msg);
    });
    f(&block);
    match rx.recv_timeout(SCK_CALL_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(msg)) => bail!("{what} failed: {msg}"),
        Err(_) => bail!("timed out waiting for {what}"),
    }
}

/// State shared between the SCK delegate object and the capturer.
struct OutputState {
    tx: Sender<Frame>,
    rx: Receiver<Frame>,
    expected: (u32, u32),
    error: Mutex<Option<String>>,
    dropped: AtomicU64,
    delivered: AtomicU64,
    skipped: AtomicU64,
}

impl OutputState {
    fn set_error(&self, msg: String) {
        if let Ok(mut e) = self.error.lock() {
            if e.is_none() {
                *e = Some(msg);
            }
        }
    }

    fn take_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }

    /// Push a frame, dropping the oldest queued frame when the consumer is behind.
    fn push(&self, frame: Frame) {
        match self.tx.try_send(frame) {
            Ok(()) => {
                self.delivered.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(frame)) => {
                let _ = self.rx.try_recv();
                self.dropped.fetch_add(1, Ordering::Relaxed);
                if self.tx.try_send(frame).is_ok() {
                    self.delivered.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

struct DelegateIvars {
    state: Arc<OutputState>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the class does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "RemoteAgentStreamOutput"]
    #[ivars = DelegateIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        #[allow(non_snake_case)]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            let state = &self.ivars().state;
            match frame_status(sample_buffer) {
                Some(SCFrameStatus::Complete) => {}
                _ => {
                    state.skipped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
            // SAFETY: the sample buffer is valid for the duration of the callback.
            let Some(image) = (unsafe { sample_buffer.image_buffer() }) else {
                return;
            };
            let pb = PixelBuffer::new(image);
            let (w, h) = (pb.width(), pb.height());
            state.push(Frame {
                width: w,
                height: h,
                captured_at: Instant::now(),
                data: FrameData::PixelBuffer(pb),
            });
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        #[allow(non_snake_case)]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, error: &NSError) {
            let msg = format!(
                "stream stopped: {} (code {})",
                error.localizedDescription(),
                error.code()
            );
            tracing::warn!("ScreenCaptureKit {msg}");
            self.ivars().state.set_error(msg);
        }
    }
);

impl StreamOutput {
    fn new(state: Arc<OutputState>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { state });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

/// Read `SCStreamFrameInfoStatus` from the sample buffer's attachments.
fn frame_status(sample_buffer: &CMSampleBuffer) -> Option<SCFrameStatus> {
    // SAFETY: plain accessor on a valid sample buffer.
    let attachments = unsafe { sample_buffer.sample_attachments_array(false) }?;
    // SAFETY: sample attachment arrays always hold CFDictionaries.
    let dict = unsafe { attachments.cast_unchecked::<CFDictionary>() }.get(0)?;
    // SAFETY: reading a ScreenCaptureKit constant.
    let key: &CFString = unsafe { SCStreamFrameInfoStatus }.as_ref();
    // SAFETY: the attachment dictionary is keyed by CFStrings.
    let value = unsafe { dict.cast_unchecked::<CFString, CFType>() }.get(key)?;
    let num = value.downcast::<CFNumber>().ok()?;
    num.as_i64().map(|v| SCFrameStatus(v as isize))
}

struct SckCapturer {
    stream: SendRetained<SCStream>,
    output: SendRetained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    state: Arc<OutputState>,
    size: (u32, u32),
    started: Instant,
    stopped: bool,
}

impl Capturer for SckCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        if let Some(err) = self.state.take_error() {
            bail!("capture stream error: {err}");
        }
        match self.state.rx.recv_timeout(timeout) {
            Ok(frame) => {
                if (frame.width, frame.height) != self.size {
                    bail!(
                        "display reconfigured: frame is {}x{}, expected {}x{}",
                        frame.width,
                        frame.height,
                        self.size.0,
                        self.size.1
                    );
                }
                Ok(Some(frame))
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if let Some(err) = self.state.take_error() {
                    bail!("capture stream error: {err}");
                }
                Ok(None)
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                bail!("capture channel closed")
            }
        }
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let stream = &self.stream.0;
        let result = call_with_completion("SCStream stopCapture", |block| {
            // SAFETY: the stream is valid; SCK copies the block.
            unsafe { stream.stopCaptureWithCompletionHandler(Some(block)) };
        });
        if let Err(err) = result {
            // Stopping an already-stopped stream reports an error; not fatal.
            tracing::debug!("{err:#}");
        }
        // SAFETY: the output object was added with the same type.
        let _ = unsafe {
            stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*self.output.0),
                SCStreamOutputType::Screen,
            )
        };
        tracing::debug!(
            delivered = self.state.delivered.load(Ordering::Relaxed),
            dropped = self.state.dropped.load(Ordering::Relaxed),
            skipped = self.state.skipped.load(Ordering::Relaxed),
            uptime_s = self.started.elapsed().as_secs(),
            "ScreenCaptureKit stream stopped"
        );
    }
}

impl Drop for SckCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn create(cfg: &CaptureConfig) -> Result<Box<dyn Capturer>> {
    create_excluding(cfg, &[])
}

/// Like [`create`], additionally removing `window_ids` (CoreGraphics window numbers, i.e.
/// `NSWindow.windowNumber`) from the stream through the content filter — the documented way
/// to keep a window out of a capture, independent of `sharingType`. Windows that are not
/// shareable content (already closed, other session) are skipped with a warning.
pub fn create_excluding(cfg: &CaptureConfig, window_ids: &[u32]) -> Result<Box<dyn Capturer>> {
    if !CGPreflightScreenCaptureAccess() {
        // Ask the system for the permission: this shows the TCC prompt the first time
        // and registers the app in System Settings → Screen Recording. Without this call
        // a freshly installed app never even appears in that list.
        static REQUESTED: std::sync::Once = std::sync::Once::new();
        REQUESTED.call_once(|| {
            let granted = CGRequestScreenCaptureAccess();
            tracing::info!(granted, "requested Screen Recording permission");
        });
        if !CGPreflightScreenCaptureAccess() {
            bail!(
                "Screen Recording permission not granted; enable it for this app in \
                 System Settings → Privacy & Security → Screen Recording, then quit and \
                 reopen the app"
            );
        }
    }
    let entries = enumerate()?;
    let entry = entries
        .iter()
        .find(|e| e.info.index == cfg.display_index)
        .with_context(|| {
            format!(
                "display index {} not found ({} displays)",
                cfg.display_index,
                entries.len()
            )
        })?;
    let (width, height) = (entry.info.width, entry.info.height);

    let content = shareable_content()?;
    // SAFETY: plain accessor.
    let displays = unsafe { content.0.displays() };
    let sc_display: Retained<SCDisplay> = displays
        .iter()
        // SAFETY: plain accessor.
        .find(|d| unsafe { d.displayID() } == entry.id)
        .with_context(|| format!("display {} is not shareable", entry.id))?;

    // The agent's own windows (chat, banner, dialogs) are kept out of the capture by giving
    // them `NSWindowSharingType::None` (see `platform::macos`), so a plain full-display filter
    // is used here. That per-window exclusion covers windows created after the stream starts and
    // avoids the SCK connection problems seen when excluding the capturing application itself.
    // Callers that want the documented exclusion as well pass the window numbers in.
    let excluded: Vec<Retained<SCWindow>> = if window_ids.is_empty() {
        Vec::new()
    } else {
        // SAFETY: plain accessors on the shareable content / window objects.
        let windows = unsafe { content.0.windows() };
        windows
            .iter()
            .filter(|w| window_ids.contains(&unsafe { w.windowID() }))
            .collect()
    };
    if excluded.len() != window_ids.len() {
        tracing::warn!(
            requested = window_ids.len(),
            found = excluded.len(),
            "some windows to exclude from the capture are not shareable content"
        );
    }
    let excluded = NSArray::from_retained_slice(&excluded);
    // SAFETY: constructing SCK objects with valid arguments.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &sc_display,
            &excluded,
        )
    };

    let fps = cfg.max_fps.clamp(1, 240);
    // SAFETY: setters on a freshly created configuration object.
    let config = unsafe {
        let c = SCStreamConfiguration::new();
        c.setWidth(width as usize);
        c.setHeight(height as usize);
        c.setPixelFormat(kCVPixelFormatType_32BGRA);
        c.setMinimumFrameInterval(CMTime::new(1, fps as i32));
        c.setShowsCursor(cfg.show_cursor);
        c.setQueueDepth(3);
        c.setCapturesAudio(false);
        c.setScalesToFit(false);
        c.setColorSpaceName(objc2_core_graphics::kCGColorSpaceSRGB);
        c
    };

    let (tx, rx) = crossbeam_channel::bounded::<Frame>(2);
    let state = Arc::new(OutputState {
        tx,
        rx,
        expected: (width, height),
        error: Mutex::new(None),
        dropped: AtomicU64::new(0),
        delivered: AtomicU64::new(0),
        skipped: AtomicU64::new(0),
    });
    let output = StreamOutput::new(state.clone());
    let queue = DispatchQueue::new("com.remoteagent.capture", DispatchQueueAttr::SERIAL);

    // SAFETY: valid filter/config; the delegate object is kept alive by the capturer.
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*output)),
        )
    };
    // SAFETY: the output object is retained for the lifetime of the stream.
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*output),
            SCStreamOutputType::Screen,
            Some(&queue),
        )
    }
    .map_err(|e| anyhow!("addStreamOutput failed: {}", e.localizedDescription()))?;

    call_with_completion("SCStream startCapture", |block| {
        // SAFETY: valid stream; SCK copies the block.
        unsafe { stream.startCaptureWithCompletionHandler(Some(block)) };
    })?;

    tracing::info!(
        display = entry.info.index,
        name = %entry.info.name,
        width,
        height,
        fps,
        "ScreenCaptureKit stream started"
    );

    Ok(Box::new(SckCapturer {
        stream: SendRetained(stream),
        output: SendRetained(output),
        _queue: queue,
        size: state.expected,
        state,
        started: Instant::now(),
        stopped: false,
    }))
}

// Silence the unused-import lint for the helper used only in `to_bgra` on some paths.
#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<PixelBuffer>();
    is_send::<SckCapturer>();
    let _ = std::ptr::null::<c_void>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_list_displays_primary_first() {
        let displays = list_displays().expect("list_displays");
        if displays.is_empty() {
            // Unsigned rebuilds lose the Screen Recording grant (preflight may still report the
            // cached grant); enumeration then yields nothing and there is nothing to assert.
            eprintln!("skipping: no displays visible (Screen Recording permission)");
            return;
        }
        assert!(displays[0].primary, "primary display first");
        for (i, d) in displays.iter().enumerate() {
            assert_eq!(d.index, i as u32);
            assert!(d.width > 0 && d.height > 0);
            assert!(d.scale >= 1.0, "scale {} for {}", d.scale, d.name);
            assert!(!d.name.is_empty());
        }
        eprintln!("displays: {displays:#?}");
    }

    /// Needs the Screen Recording permission for the test binary's parent (Terminal/IDE).
    #[test]
    #[ignore]
    fn macos_capture_30_frames() {
        let cfg = CaptureConfig {
            display_index: 0,
            max_fps: 30,
            show_cursor: true,
        };
        let mut cap = create(&cfg).expect("create capturer");
        let (w, h) = cap.size();
        assert!(w > 0 && h > 0);
        let start = Instant::now();
        let mut frames = 0;
        let mut first_latency = None;
        while frames < 30 && start.elapsed() < Duration::from_secs(3) {
            if let Some(f) = cap
                .next_frame(Duration::from_millis(200))
                .expect("next_frame")
            {
                assert_eq!((f.width, f.height), (w, h));
                if first_latency.is_none() {
                    first_latency = Some(start.elapsed());
                }
                if frames == 0 {
                    let (bgra, stride) = super::super::to_bgra(&f).expect("to_bgra");
                    assert_eq!(stride, w as usize * 4);
                    assert_eq!(bgra.len(), stride * h as usize);
                }
                frames += 1;
            }
        }
        eprintln!(
            "captured {frames} frames of {w}x{h} in {:?} (first after {:?})",
            start.elapsed(),
            first_latency.unwrap_or_default()
        );
        // A static screen yields fewer Complete frames; require a healthy minimum.
        assert!(frames >= 10, "captured only {frames} frames in 3 s");
        cap.stop();
    }
}
