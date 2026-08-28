//! Capture → encode pipeline running on a dedicated OS thread per session.
//!
//! The thread owns a [`Capturer`] and an [`Encoder`]. Encoded access units are handed to the
//! async side through a bounded `tokio::sync::mpsc` channel with `try_send`: when the network
//! side is behind, frames are dropped rather than queued (and a keyframe is re-requested if a
//! keyframe was dropped) so latency never accumulates.

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
    /// Average capture → encoded latency in milliseconds over the last window.
    pub pipeline_ms: f32,
    pub hardware: bool,
    pub display_index: u32,
    pub encoded_frames: u64,
    pub dropped_frames: u64,
}

/// Asynchronous notifications from the pipeline thread.
#[derive(Debug)]
pub enum PipelineEvent {
    /// The pipeline gave up (e.g. capture permission revoked); the session should end.
    Failed(String),
    /// Capturer/encoder were (re)created. `width`/`height` is the captured display size,
    /// `encoded_width`/`encoded_height` the picture size actually sent (hardware encoders may
    /// downscale very large displays); browser mouse coordinates refer to the latter.
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
            .spawn(move || worker.run(cmd_rx, frame_tx, event_tx, kf, stats_tx))
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
    consecutive_errors: u32,
    // stats window
    window_start: Instant,
    window_frames: u32,
    window_bytes: u64,
    window_latency_ms: f32,
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
        let last_stats = PipelineStats {
            codec: encoder.codec(),
            fps: 0.0,
            bitrate_kbps: 0,
            width,
            height,
            encoded_width: encoded.0,
            encoded_height: encoded.1,
            pipeline_ms: 0.0,
            hardware: encoder.is_hardware(),
            display_index: cfg.display_index,
            encoded_frames: 0,
            dropped_frames: 0,
        };
        Ok(Self {
            media,
            cfg,
            capturer,
            encoder,
            width,
            height,
            encoded,
            last_frame: None,
            last_encode_at: None,
            consecutive_errors: 0,
            window_start: now,
            window_frames: 0,
            window_bytes: 0,
            window_latency_ms: 0.0,
            encoded_total: 0,
            dropped_total: 0,
            last_stats,
        })
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
            .create_encoder(&EncoderConfig {
                codec: cfg.codec,
                width,
                height,
                fps: cfg.max_fps.max(1),
                bitrate_kbps: cfg.max_bitrate_kbps.max(100),
            })
            .with_context(|| format!("creating {:?} encoder for {width}x{height}", cfg.codec))?;
        tracing::info!(
            display = cfg.display_index,
            width,
            height,
            codec = ?encoder.codec(),
            hardware = encoder.is_hardware(),
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

    fn reopen(&mut self, event_tx: &mpsc::UnboundedSender<PipelineEvent>) -> Result<()> {
        self.capturer.stop();
        let (capturer, encoder, width, height) = Self::open_devices(&*self.media, &self.cfg)?;
        self.capturer = capturer;
        self.encoder = encoder;
        self.width = width;
        self.height = height;
        self.encoded = self.encoder.output_size().unwrap_or((width, height));
        self.last_frame = None;
        self.consecutive_errors = 0;
        self.last_stats.width = width;
        self.last_stats.height = height;
        self.last_stats.encoded_width = self.encoded.0;
        self.last_stats.encoded_height = self.encoded.1;
        self.last_stats.codec = self.encoder.codec();
        self.last_stats.hardware = self.encoder.is_hardware();
        self.last_stats.display_index = self.cfg.display_index;
        let _ = event_tx.send(self.started_event());
        Ok(())
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
            // ── commands ────────────────────────────────────────────────────────────
            let mut needs_reopen = false;
            loop {
                match cmd_rx.try_recv() {
                    Ok(Command::Stop) | Err(TryRecvError::Disconnected) => {
                        self.capturer.stop();
                        return;
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
                                needs_reopen = true;
                            }
                        }
                        if let Some(kbps) = max_bitrate_kbps {
                            if kbps != self.cfg.max_bitrate_kbps && kbps > 0 {
                                self.cfg.max_bitrate_kbps = kbps;
                                if let Err(e) = self.encoder.set_bitrate(kbps) {
                                    tracing::warn!("set_bitrate({kbps}) failed: {e:#}");
                                    needs_reopen = true;
                                }
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            if needs_reopen {
                match self.reopen(&event_tx) {
                    Ok(()) => keyframe.store(true, Ordering::Relaxed),
                    Err(e) => {
                        let _ = event_tx.send(PipelineEvent::Failed(format!("{e:#}")));
                        return;
                    }
                }
            }

            // ── capture ─────────────────────────────────────────────────────────────
            let frame_interval = Duration::from_secs_f64(1.0 / self.cfg.max_fps.max(1) as f64);
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
                    // Frame pacing guard: never encode faster than max_fps.
                    if !force {
                        if let Some(last) = self.last_encode_at {
                            if last.elapsed() < frame_interval.mul_f32(0.85) {
                                self.publish_stats_if_due(&stats_tx);
                                continue;
                            }
                        }
                    }
                    f
                }
                None => {
                    // Nothing changed on screen. Re-encode the last frame only when a keyframe
                    // was requested (e.g. packet loss) so the viewer recovers even on an idle screen.
                    if force {
                        keyframe.store(true, Ordering::Relaxed);
                        if let Some(last) = self.last_frame.take() {
                            keyframe.store(false, Ordering::Relaxed);
                            self.encode_and_send(last, true, &frame_tx, &keyframe);
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

            self.encode_and_send(frame, force, &frame_tx, &keyframe);
            self.publish_stats_if_due(&stats_tx);
        }
    }

    fn encode_and_send(
        &mut self,
        frame: Frame,
        force: bool,
        frame_tx: &mpsc::Sender<EncodedFrame>,
        keyframe: &AtomicBool,
    ) {
        let captured_at = frame.captured_at;
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
        self.last_frame = Some(frame);
        self.last_encode_at = Some(Instant::now());
        for f in encoded {
            let latency = captured_at.elapsed().as_secs_f32() * 1000.0;
            let is_key = f.keyframe;
            let bytes = f.data.len() as u64;
            match frame_tx.try_send(f) {
                Ok(()) => {
                    self.window_frames += 1;
                    self.window_bytes += bytes;
                    self.window_latency_ms += latency;
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
        self.last_stats = PipelineStats {
            codec: self.encoder.codec(),
            fps: frames / secs,
            bitrate_kbps: ((self.window_bytes as f32 * 8.0 / secs) / 1000.0) as u32,
            width: self.width,
            height: self.height,
            encoded_width: self.encoded.0,
            encoded_height: self.encoded.1,
            pipeline_ms: if self.window_frames > 0 {
                self.window_latency_ms / frames
            } else {
                0.0
            },
            hardware: self.encoder.is_hardware(),
            display_index: self.cfg.display_index,
            encoded_frames: self.encoded_total,
            dropped_frames: self.dropped_total,
        };
        let _ = stats_tx.send(self.last_stats.clone());
        self.window_start = Instant::now();
        self.window_frames = 0;
        self.window_bytes = 0;
        self.window_latency_ms = 0.0;
    }
}
