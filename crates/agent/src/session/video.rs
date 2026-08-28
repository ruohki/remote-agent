//! Capture → encode pipeline running on a dedicated OS thread per session.
//!
//! The thread owns a [`Capturer`] and an [`Encoder`]. Encoded access units are handed to the
//! async side through a bounded `tokio::sync::mpsc` channel with `try_send`: when the network
//! side is behind, frames are dropped rather than queued (and a keyframe is re-requested if a
//! keyframe was dropped) so latency never accumulates.
//!
//! Performance behaviour (see `PERFORMANCE.md`):
//! * **encode on change** — the capturers only deliver changed frames; nothing is re-encoded
//!   in a tight loop. After [`IDLE_REFRESH`] without a change the last frame is encoded once
//!   more as a P-frame so the browser's jitter buffer keeps ticking.
//! * **infinite GOP** — keyframes only when forced (start, display/size/viewport change,
//!   PLI/FIR, dropped keyframe).
//! * **viewport scaling** — [`VideoPipeline::set_viewport`] caps the encoded size (debounced
//!   [`VIEWPORT_DEBOUNCE`]); the capturer keeps running, only the encoder is rebuilt.
//! * **congestion control** — [`VideoPipeline::set_target_bitrate`] / `set_target_fps`
//!   from the RTCP feedback loop adjust the encoder live without reopening.
//! * the thread runs at real-time / user-interactive priority and every frame carries
//!   `captured_at → encoded_at → written_at` timings that feed the per-second stats.

use super::media::MediaFactory;
use crate::capture::{CaptureConfig, Capturer, Frame};
use crate::encode::{EncodedFrame, Encoder, EncoderConfig};
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use protocol::common::VideoCodec;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// Static configuration of a pipeline (runtime knobs go through commands).
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub display_index: u32,
    pub codec: VideoCodec,
    pub max_fps: u32,
    pub max_bitrate_kbps: u32,
    pub show_cursor: bool,
    /// Initial viewport cap (browser tile size in physical pixels), `None` = full size.
    pub viewport: Option<(u32, u32)>,
}

/// Rolling statistics published once per second.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineStats {
    pub codec: VideoCodec,
    pub fps: f32,
    pub bitrate_kbps: u32,
    /// Captured display size.
    pub width: u32,
    pub height: u32,
    /// Encoded picture size (see [`PipelineEvent::Started`]).
    pub encoded_width: u32,
    pub encoded_height: u32,
    /// Average capture → written latency in milliseconds over the last window.
    pub pipeline_ms: f32,
    /// Average capture → encoded latency and encode duration (milliseconds).
    pub capture_to_encoded_ms: f32,
    pub encode_ms: f32,
    /// Keyframes and idle refresh frames in the window.
    pub keyframes: u32,
    pub idle_refreshes: u32,
    pub hardware: bool,
    pub display_index: u32,
    pub encoded_frames: u64,
    pub dropped_frames: u64,
    /// Bitrate the encoder is currently asked for (after congestion control).
    pub target_bitrate_kbps: u32,
}

/// Asynchronous notifications from the pipeline thread.
#[derive(Debug)]
pub enum PipelineEvent {
    /// The pipeline gave up (e.g. capture permission revoked); the session should end.
    Failed(String),
    /// Capturer/encoder were (re)created. `width`/`height` is the captured display size,
    /// `encoded_width`/`encoded_height` the picture size actually sent (hardware encoders may
    /// downscale very large displays, viewport scaling shrinks further); browser mouse
    /// coordinates refer to the latter.
    Started {
        display_index: u32,
        width: u32,
        height: u32,
        encoded_width: u32,
        encoded_height: u32,
        codec: VideoCodec,
        hardware: bool,
    },
}

enum Command {
    SelectDisplay(u32),
    SetQuality {
        max_fps: Option<u32>,
        max_bitrate_kbps: Option<u32>,
    },
    SetViewport(Option<(u32, u32)>),
    /// Congestion-controlled target (≤ the configured cap).
    SetTargetBitrate(u32),
    /// Frame-rate ladder from congestion control (≤ `max_fps`); pacing only, no reopen.
    SetTargetFps(u32),
    Stop,
}

/// Requests a keyframe from the pipeline (see [`VideoPipeline::keyframe_requester`]).
#[derive(Clone)]
pub struct KeyframeRequester(Arc<AtomicBool>);

impl KeyframeRequester {
    pub fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Handle to a running pipeline; dropping it stops the thread.
pub struct VideoPipeline {
    cmd_tx: Sender<Command>,
    keyframe: Arc<AtomicBool>,
    stats_rx: watch::Receiver<PipelineStats>,
    thread: Option<JoinHandle<()>>,
}

impl VideoPipeline {
    /// Open the capturer and encoder synchronously (so configuration errors surface here),
    /// then start the worker thread.
    pub fn start(
        media: Arc<dyn MediaFactory>,
        cfg: PipelineConfig,
        frame_tx: mpsc::Sender<EncodedFrame>,
        event_tx: mpsc::UnboundedSender<PipelineEvent>,
    ) -> Result<Self> {
        // The first ScreenCaptureKit start after a (re)install can fail transiently while the
        // OS evaluates the capture permission (error -3805 / start timeout); retry before
        // giving up on the whole session.
        let mut attempt = 0;
        let worker = loop {
            attempt += 1;
            match Worker::open(Arc::clone(&media), cfg.clone()) {
                Ok(w) => break w,
                Err(e) if attempt < OPEN_ATTEMPTS => {
                    tracing::warn!("opening capture pipeline failed (attempt {attempt}/{OPEN_ATTEMPTS}): {e:#}");
                    std::thread::sleep(Duration::from_millis(750 * attempt as u64));
                }
                Err(e) => return Err(e.context("opening capture pipeline")),
            }
        };
        let _ = event_tx.send(worker.started_event());
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let keyframe = Arc::new(AtomicBool::new(true));
        let (stats_tx, stats_rx) = watch::channel(worker.stats_snapshot());
        let kf = Arc::clone(&keyframe);
        let thread = std::thread::Builder::new()
            .name("video-pipeline".into())
            .spawn(move || {
                raise_thread_priority();
                worker.run(cmd_rx, frame_tx, event_tx, kf, stats_tx)
            })
            .context("spawning video pipeline thread")?;
        Ok(Self {
            cmd_tx,
            keyframe,
            stats_rx,
            thread: Some(thread),
        })
    }

    /// Ask for the next encoded frame to be a keyframe.
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    /// Cheap clonable handle for requesting keyframes from other tasks (RTCP reader).
    pub fn keyframe_requester(&self) -> KeyframeRequester {
        KeyframeRequester(Arc::clone(&self.keyframe))
    }

    pub fn select_display(&self, index: u32) {
        let _ = self.cmd_tx.send(Command::SelectDisplay(index));
    }

    pub fn set_quality(&self, max_fps: Option<u32>, max_bitrate_kbps: Option<u32>) {
        let _ = self.cmd_tx.send(Command::SetQuality {
            max_fps,
            max_bitrate_kbps,
        });
    }

    /// Cap the encoded picture to the browser's rendered size (`None` = full resolution).
    pub fn set_viewport(&self, viewport: Option<(u32, u32)>) {
        let _ = self.cmd_tx.send(Command::SetViewport(viewport));
    }

    /// Congestion-controlled target bitrate (clamped to the configured cap by the worker).
    pub fn set_target_bitrate(&self, kbps: u32) {
        let _ = self.cmd_tx.send(Command::SetTargetBitrate(kbps));
    }

    /// Congestion-controlled frame rate (pacing only).
    pub fn set_target_fps(&self, fps: u32) {
        let _ = self.cmd_tx.send(Command::SetTargetFps(fps));
    }

    pub fn stats(&self) -> watch::Receiver<PipelineStats> {
        self.stats_rx.clone()
    }

    /// Stop the thread and wait for it to finish.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(Command::Stop);
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                tracing::error!("video pipeline thread panicked");
            }
        }
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Raise the calling thread to real-time / user-interactive priority so capture → encode
/// never waits behind ordinary work.
pub fn raise_thread_priority() {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: plain libc call on the current thread; relative priority 0.
        let rc = unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
        };
        if rc != 0 {
            tracing::debug!("pthread_set_qos_class_self_np failed: {rc}");
        }
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
        };
        // SAFETY: plain Win32 calls on the current thread.
        if let Err(e) =
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) }
        {
            tracing::debug!("SetThreadPriority failed: {e}");
        }
    }
}

/// Idle refresh: after this long without a changed frame the last frame is encoded again
/// (as a P-frame) so the receiver's jitter buffer keeps moving.
pub const IDLE_REFRESH: Duration = Duration::from_secs(1);
/// Viewport changes are coalesced for this long before the encoder is rebuilt.
pub const VIEWPORT_DEBOUNCE: Duration = Duration::from_millis(250);

/// Decide whether an idle refresh is due: `last_change` is when the last real frame was
/// encoded, `last_refresh` when the last refresh (or real frame) was sent.
pub fn idle_refresh_due(now: Instant, last_sent: Option<Instant>) -> bool {
    last_sent.is_some_and(|t| now.duration_since(t) >= IDLE_REFRESH)
}

/// Effective viewport cap: `None` (full size) when the viewport covers the display.
pub fn effective_viewport(display: (u32, u32), viewport: Option<(u32, u32)>) -> Option<(u32, u32)> {
    match viewport {
        Some((w, h)) if w > 0 && h > 0 && (w < display.0 || h < display.1) => Some((w, h)),
        _ => None,
    }
}

struct Worker {
    media: Arc<dyn MediaFactory>,
    cfg: PipelineConfig,
    capturer: Box<dyn Capturer>,
    encoder: Box<dyn Encoder>,
    width: u32,
    height: u32,
    encoded: (u32, u32),
    last_frame: Option<Frame>,
    last_encode_at: Option<Instant>,
    /// When the last frame (real or refresh) was sent — drives the idle refresh.
    last_sent_at: Option<Instant>,
    /// Viewport requested by the browser, applied after [`VIEWPORT_DEBOUNCE`].
    pending_viewport: Option<(Option<(u32, u32)>, Instant)>,
    /// Congestion-controlled target bitrate (≤ cfg.max_bitrate_kbps).
    target_bitrate_kbps: u32,
    /// Congestion-controlled pacing fps (≤ cfg.max_fps).
    pace_fps: u32,
    /// Unix epoch millisecond of the last frame's capture (for correlating with the rig).
    last_capture_epoch_ms: u64,
    consecutive_errors: u32,
    // stats window
    window_start: Instant,
    window_frames: u32,
    window_bytes: u64,
    window_latency_ms: f32,
    window_capture_to_encoded_ms: f32,
    window_encode_ms: f32,
    window_keyframes: u32,
    window_idle_refreshes: u32,
    encoded_total: u64,
    dropped_total: u64,
    last_stats: PipelineStats,
}

/// After this many consecutive capture/encode failures the pipeline is recreated; after
/// that many recreation failures it gives up.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;
/// Attempts to open capturer + encoder when a session starts.
const OPEN_ATTEMPTS: u32 = 3;
const MAX_REOPEN_ATTEMPTS: u32 = 3;

/// `(capturer, encoder, width, height)` returned by [`Worker::open_devices`].
type OpenedDevices = (Box<dyn Capturer>, Box<dyn Encoder>, u32, u32);

impl Worker {
    fn open(media: Arc<dyn MediaFactory>, cfg: PipelineConfig) -> Result<Self> {
        let (capturer, encoder, width, height) = Self::open_devices(&*media, &cfg)?;
        let encoded = encoder.output_size().unwrap_or((width, height));
        let now = Instant::now();
        let target = cfg.max_bitrate_kbps.max(100);
        let last_stats = PipelineStats {
            codec: encoder.codec(),
            fps: 0.0,
            bitrate_kbps: 0,
            width,
            height,
            encoded_width: encoded.0,
            encoded_height: encoded.1,
            pipeline_ms: 0.0,
            capture_to_encoded_ms: 0.0,
            encode_ms: 0.0,
            keyframes: 0,
            idle_refreshes: 0,
            hardware: encoder.is_hardware(),
            display_index: cfg.display_index,
            encoded_frames: 0,
            dropped_frames: 0,
            target_bitrate_kbps: target,
        };
        Ok(Self {
            media,
            pace_fps: cfg.max_fps.max(1),
            cfg,
            capturer,
            encoder,
            width,
            height,
            encoded,
            last_frame: None,
            last_encode_at: None,
            last_sent_at: None,
            pending_viewport: None,
            target_bitrate_kbps: target,
            last_capture_epoch_ms: 0,
            consecutive_errors: 0,
            window_start: now,
            window_frames: 0,
            window_bytes: 0,
            window_latency_ms: 0.0,
            window_capture_to_encoded_ms: 0.0,
            window_encode_ms: 0.0,
            window_keyframes: 0,
            window_idle_refreshes: 0,
            encoded_total: 0,
            dropped_total: 0,
            last_stats,
        })
    }

    fn encoder_config(
        cfg: &PipelineConfig,
        width: u32,
        height: u32,
        bitrate_kbps: u32,
    ) -> EncoderConfig {
        EncoderConfig {
            codec: cfg.codec,
            width,
            height,
            fps: cfg.max_fps.max(1),
            bitrate_kbps: bitrate_kbps.max(100),
            max_output: effective_viewport((width, height), cfg.viewport),
        }
    }

    fn open_devices(media: &dyn MediaFactory, cfg: &PipelineConfig) -> Result<OpenedDevices> {
        let capture_cfg = CaptureConfig {
            display_index: cfg.display_index,
            max_fps: cfg.max_fps.max(1),
            show_cursor: cfg.show_cursor,
        };
        let capturer = media
            .create_capturer(&capture_cfg)
            .with_context(|| format!("creating capturer for display {}", cfg.display_index))?;
        let (width, height) = capturer.size();
        let encoder = media
            .create_encoder(&Self::encoder_config(
                cfg,
                width,
                height,
                cfg.max_bitrate_kbps,
            ))
            .with_context(|| format!("creating {:?} encoder for {width}x{height}", cfg.codec))?;
        tracing::info!(
            display = cfg.display_index,
            width,
            height,
            codec = ?encoder.codec(),
            hardware = encoder.is_hardware(),
            viewport = ?cfg.viewport,
            "video pipeline opened"
        );
        Ok((capturer, encoder, width, height))
    }

    fn started_event(&self) -> PipelineEvent {
        PipelineEvent::Started {
            display_index: self.cfg.display_index,
            width: self.width,
            height: self.height,
            encoded_width: self.encoded.0,
            encoded_height: self.encoded.1,
            codec: self.encoder.codec(),
            hardware: self.encoder.is_hardware(),
        }
    }

    fn stats_snapshot(&self) -> PipelineStats {
        self.last_stats.clone()
    }

    fn after_reopen(&mut self, event_tx: &mpsc::UnboundedSender<PipelineEvent>) {
        self.encoded = self
            .encoder
            .output_size()
            .unwrap_or((self.width, self.height));
        self.consecutive_errors = 0;
        self.last_stats.width = self.width;
        self.last_stats.height = self.height;
        self.last_stats.encoded_width = self.encoded.0;
        self.last_stats.encoded_height = self.encoded.1;
        self.last_stats.codec = self.encoder.codec();
        self.last_stats.hardware = self.encoder.is_hardware();
        self.last_stats.display_index = self.cfg.display_index;
        let _ = event_tx.send(self.started_event());
    }

    /// Recreate capturer + encoder (display switch, fps change, capture failure).
    fn reopen(&mut self, event_tx: &mpsc::UnboundedSender<PipelineEvent>) -> Result<()> {
        self.capturer.stop();
        let (capturer, encoder, width, height) = Self::open_devices(&*self.media, &self.cfg)?;
        self.capturer = capturer;
        self.encoder = encoder;
        self.width = width;
        self.height = height;
        self.last_frame = None;
        self.last_sent_at = None;
        self.target_bitrate_kbps = self
            .target_bitrate_kbps
            .min(self.cfg.max_bitrate_kbps.max(100));
        if self.target_bitrate_kbps < self.cfg.max_bitrate_kbps {
            let _ = self.encoder.set_bitrate(self.target_bitrate_kbps);
        }
        self.after_reopen(event_tx);
        Ok(())
    }

    /// Recreate only the encoder (viewport change); the capturer keeps running.
    fn reopen_encoder(&mut self, event_tx: &mpsc::UnboundedSender<PipelineEvent>) -> Result<()> {
        let encoder = self
            .media
            .create_encoder(&Self::encoder_config(
                &self.cfg,
                self.width,
                self.height,
                self.target_bitrate_kbps,
            ))
            .with_context(|| format!("re-creating {:?} encoder", self.cfg.codec))?;
        self.encoder = encoder;
        self.after_reopen(event_tx);
        tracing::info!(
            display = self.cfg.display_index,
            viewport = ?self.cfg.viewport,
            encoded_width = self.encoded.0,
            encoded_height = self.encoded.1,
            "encoder rebuilt for viewport"
        );
        Ok(())
    }

    fn apply_target_bitrate(&mut self, kbps: u32) {
        let kbps = kbps.clamp(100, self.cfg.max_bitrate_kbps.max(100));
        if kbps == self.target_bitrate_kbps {
            return;
        }
        self.target_bitrate_kbps = kbps;
        if let Err(e) = self.encoder.set_bitrate(kbps) {
            tracing::debug!("set_bitrate({kbps}) failed: {e:#}");
        }
    }

    /// Returns `true` when the pipeline should exit.
    fn handle_commands(
        &mut self,
        cmd_rx: &Receiver<Command>,
        event_tx: &mpsc::UnboundedSender<PipelineEvent>,
        keyframe: &AtomicBool,
    ) -> bool {
        let mut needs_reopen = false;
        loop {
            match cmd_rx.try_recv() {
                Ok(Command::Stop) | Err(TryRecvError::Disconnected) => {
                    self.capturer.stop();
                    return true;
                }
                Ok(Command::SelectDisplay(idx)) => {
                    if idx != self.cfg.display_index {
                        self.cfg.display_index = idx;
                        needs_reopen = true;
                    }
                }
                Ok(Command::SetQuality {
                    max_fps,
                    max_bitrate_kbps,
                }) => {
                    if let Some(fps) = max_fps {
                        if fps != self.cfg.max_fps && fps > 0 {
                            self.cfg.max_fps = fps;
                            self.pace_fps = self.pace_fps.min(fps).max(1);
                            needs_reopen = true;
                        }
                    }
                    if let Some(kbps) = max_bitrate_kbps {
                        if kbps != self.cfg.max_bitrate_kbps && kbps > 0 {
                            self.cfg.max_bitrate_kbps = kbps;
                            self.target_bitrate_kbps = kbps;
                            if let Err(e) = self.encoder.set_bitrate(kbps) {
                                tracing::warn!("set_bitrate({kbps}) failed: {e:#}");
                                needs_reopen = true;
                            }
                        }
                    }
                }
                Ok(Command::SetViewport(v)) => {
                    let v = effective_viewport((self.width, self.height), v);
                    if v != self.cfg.viewport {
                        self.pending_viewport = Some((v, Instant::now()));
                    } else {
                        self.pending_viewport = None;
                    }
                }
                Ok(Command::SetTargetBitrate(kbps)) => self.apply_target_bitrate(kbps),
                Ok(Command::SetTargetFps(fps)) => {
                    self.pace_fps = fps.clamp(1, self.cfg.max_fps.max(1));
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if needs_reopen {
            match self.reopen(event_tx) {
                Ok(()) => keyframe.store(true, Ordering::Relaxed),
                Err(e) => {
                    let _ = event_tx.send(PipelineEvent::Failed(format!("{e:#}")));
                    return true;
                }
            }
        }
        if let Some((viewport, since)) = self.pending_viewport {
            if since.elapsed() >= VIEWPORT_DEBOUNCE {
                self.pending_viewport = None;
                self.cfg.viewport = viewport;
                match self.reopen_encoder(event_tx) {
                    Ok(()) => keyframe.store(true, Ordering::Relaxed),
                    Err(e) => tracing::warn!("viewport change failed: {e:#}"),
                }
            }
        }
        false
    }

    fn run(
        mut self,
        cmd_rx: Receiver<Command>,
        frame_tx: mpsc::Sender<EncodedFrame>,
        event_tx: mpsc::UnboundedSender<PipelineEvent>,
        keyframe: Arc<AtomicBool>,
        stats_tx: watch::Sender<PipelineStats>,
    ) {
        let mut reopen_attempts = 0;
        loop {
            if self.handle_commands(&cmd_rx, &event_tx, &keyframe) {
                return;
            }

            // ── capture ─────────────────────────────────────────────────────────────
            let frame_interval = Duration::from_secs_f64(1.0 / self.pace_fps.max(1) as f64);
            let frame = match self.capturer.next_frame(Duration::from_millis(100)) {
                Ok(f) => {
                    self.consecutive_errors = 0;
                    f
                }
                Err(e) => {
                    self.consecutive_errors += 1;
                    tracing::warn!(
                        "capture error ({}/{MAX_CONSECUTIVE_ERRORS}): {e:#}",
                        self.consecutive_errors
                    );
                    if self.consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        reopen_attempts += 1;
                        if reopen_attempts > MAX_REOPEN_ATTEMPTS {
                            let _ = event_tx.send(PipelineEvent::Failed(format!(
                                "capture keeps failing: {e:#}"
                            )));
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                        if let Err(e) = self.reopen(&event_tx) {
                            tracing::warn!("reopening capture failed: {e:#}");
                        }
                    } else {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    continue;
                }
            };

            let force = keyframe.swap(false, Ordering::Relaxed);
            let frame = match frame {
                Some(f) => {
                    // Frame pacing guard: never encode faster than the (congestion-adjusted)
                    // frame rate. The skipped frame is kept as the latest picture so an idle
                    // refresh or keyframe request encodes the newest content.
                    if !force {
                        if let Some(last) = self.last_encode_at {
                            if last.elapsed() < frame_interval.mul_f32(0.85) {
                                self.last_frame = Some(f);
                                self.publish_stats_if_due(&stats_tx);
                                continue;
                            }
                        }
                    }
                    f
                }
                None => {
                    // Nothing changed on screen.
                    let now = Instant::now();
                    if force {
                        // Keyframe requested (packet loss / new viewer): re-encode the last
                        // picture so the viewer recovers even on an idle screen.
                        if let Some(last) = self.last_frame.take() {
                            self.encode_and_send(last, true, false, &frame_tx, &keyframe);
                        } else {
                            keyframe.store(true, Ordering::Relaxed);
                        }
                    } else if idle_refresh_due(now, self.last_sent_at) {
                        if let Some(last) = self.last_frame.take() {
                            self.encode_and_send(last, false, true, &frame_tx, &keyframe);
                        }
                    }
                    self.publish_stats_if_due(&stats_tx);
                    continue;
                }
            };

            if frame.width != self.width || frame.height != self.height {
                tracing::info!(
                    "display size changed {}x{} → {}x{}",
                    self.width,
                    self.height,
                    frame.width,
                    frame.height
                );
                if let Err(e) = self.reopen(&event_tx) {
                    let _ = event_tx.send(PipelineEvent::Failed(format!("{e:#}")));
                    return;
                }
                keyframe.store(true, Ordering::Relaxed);
                continue;
            }

            self.encode_and_send(frame, force, false, &frame_tx, &keyframe);
            self.publish_stats_if_due(&stats_tx);
        }
    }

    fn encode_and_send(
        &mut self,
        frame: Frame,
        force: bool,
        idle_refresh: bool,
        frame_tx: &mpsc::Sender<EncodedFrame>,
        keyframe: &AtomicBool,
    ) {
        let captured_at = frame.captured_at;
        let encode_start = Instant::now();
        let encoded = match self.encoder.encode(&frame, force) {
            Ok(v) => v,
            Err(e) => {
                self.consecutive_errors += 1;
                tracing::warn!("encode error: {e:#}");
                if force {
                    keyframe.store(true, Ordering::Relaxed);
                }
                self.last_frame = Some(frame);
                return;
            }
        };
        let encoded_at = Instant::now();
        self.last_capture_epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
            .saturating_sub(encoded_at.duration_since(captured_at).as_millis() as u64);
        self.last_frame = Some(frame);
        self.last_encode_at = Some(encoded_at);
        if idle_refresh {
            self.window_idle_refreshes += 1;
        }
        for f in encoded {
            let is_key = f.keyframe;
            let bytes = f.data.len() as u64;
            match frame_tx.try_send(f) {
                Ok(()) => {
                    let written_at = Instant::now();
                    self.last_sent_at = Some(written_at);
                    self.window_frames += 1;
                    self.window_bytes += bytes;
                    self.window_latency_ms +=
                        written_at.duration_since(captured_at).as_secs_f32() * 1000.0;
                    self.window_capture_to_encoded_ms +=
                        encoded_at.duration_since(captured_at).as_secs_f32() * 1000.0;
                    self.window_encode_ms +=
                        encoded_at.duration_since(encode_start).as_secs_f32() * 1000.0;
                    if is_key {
                        self.window_keyframes += 1;
                    }
                    self.encoded_total += 1;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.dropped_total += 1;
                    if is_key {
                        keyframe.store(true, Ordering::Relaxed);
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            }
        }
    }

    fn publish_stats_if_due(&mut self, stats_tx: &watch::Sender<PipelineStats>) {
        let elapsed = self.window_start.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let secs = elapsed.as_secs_f32();
        let frames = self.window_frames as f32;
        let avg = |sum: f32| {
            if self.window_frames > 0 {
                sum / frames
            } else {
                0.0
            }
        };
        self.last_stats = PipelineStats {
            codec: self.encoder.codec(),
            fps: frames / secs,
            bitrate_kbps: ((self.window_bytes as f32 * 8.0 / secs) / 1000.0) as u32,
            width: self.width,
            height: self.height,
            encoded_width: self.encoded.0,
            encoded_height: self.encoded.1,
            pipeline_ms: avg(self.window_latency_ms),
            capture_to_encoded_ms: avg(self.window_capture_to_encoded_ms),
            encode_ms: avg(self.window_encode_ms),
            keyframes: self.window_keyframes,
            idle_refreshes: self.window_idle_refreshes,
            hardware: self.encoder.is_hardware(),
            display_index: self.cfg.display_index,
            encoded_frames: self.encoded_total,
            dropped_frames: self.dropped_total,
            target_bitrate_kbps: self.target_bitrate_kbps,
        };
        if tracing::enabled!(target: "perf", tracing::Level::INFO) {
            tracing::info!(
                target: "perf",
                display = self.cfg.display_index,
                fps = format_args!("{:.1}", self.last_stats.fps),
                kbps = self.last_stats.bitrate_kbps,
                target_kbps = self.target_bitrate_kbps,
                pace_fps = self.pace_fps,
                size = format_args!("{}x{}", self.encoded.0, self.encoded.1),
                cap_to_enc_ms = format_args!("{:.1}", self.last_stats.capture_to_encoded_ms),
                encode_ms = format_args!("{:.1}", self.last_stats.encode_ms),
                cap_to_sent_ms = format_args!("{:.1}", self.last_stats.pipeline_ms),
                keyframes = self.window_keyframes,
                idle = self.window_idle_refreshes,
                dropped = self.dropped_total,
                last_capture_epoch_ms = self.last_capture_epoch_ms,
                "perf"
            );
        }
        let _ = stats_tx.send(self.last_stats.clone());
        self.window_start = Instant::now();
        self.window_frames = 0;
        self.window_bytes = 0;
        self.window_latency_ms = 0.0;
        self.window_capture_to_encoded_ms = 0.0;
        self.window_encode_ms = 0.0;
        self.window_keyframes = 0;
        self.window_idle_refreshes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_refresh_timer() {
        let t0 = Instant::now();
        assert!(
            !idle_refresh_due(t0, None),
            "nothing sent yet: nothing to refresh"
        );
        assert!(!idle_refresh_due(t0 + Duration::from_millis(500), Some(t0)));
        assert!(idle_refresh_due(t0 + IDLE_REFRESH, Some(t0)));
    }

    #[test]
    fn viewport_cap_only_when_smaller_than_display() {
        assert_eq!(effective_viewport((5120, 2160), None), None);
        assert_eq!(effective_viewport((5120, 2160), Some((5120, 2160))), None);
        assert_eq!(effective_viewport((5120, 2160), Some((6000, 3000))), None);
        assert_eq!(
            effective_viewport((5120, 2160), Some((2560, 1080))),
            Some((2560, 1080))
        );
        assert_eq!(effective_viewport((5120, 2160), Some((0, 0))), None);
        // Fit keeps aspect and even dimensions.
        assert_eq!(
            crate::encode::fit_within(5120, 2160, (2000, 2000)),
            (2000, 842)
        );
        assert_eq!(
            crate::encode::fit_within(1920, 1080, (2000, 2000)),
            (1920, 1080)
        );
        assert_eq!(
            crate::encode::fit_within(1921, 1081, (2000, 2000)),
            (1920, 1080)
        );
    }
}
