//! System audio pipeline: capture thread → Opus packets → the session's audio track.
//!
//! Runs on its own OS thread so a stalled encoder or a slow source can never delay video.
//! Encoded packets go through a bounded channel with drop-on-full semantics.

use crate::audio::opus_enc::FrameEncoder;
use crate::audio::AudioSource;
use anyhow::{Context, Result};
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc;

/// One encoded Opus packet (20 ms).
pub struct AudioPacket {
    pub data: Bytes,
    pub duration: Duration,
}

pub const OPUS_BITRATE_BPS: i32 = 96_000;
const FRAME_DURATION: Duration = Duration::from_millis(20);

pub struct AudioPipeline {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioPipeline {
    /// Start capturing from `source` and encoding into `tx`.
    pub fn start(mut source: Box<dyn AudioSource>, tx: mpsc::Sender<AudioPacket>) -> Result<Self> {
        let format = source.format();
        let mut encoder = FrameEncoder::new(format, OPUS_BITRATE_BPS)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("audio-pipeline".into())
            .spawn(move || {
                let mut dropped: u64 = 0;
                let mut sent: u64 = 0;
                while !stop_flag.load(Ordering::Relaxed) {
                    let pcm = match source.read(Duration::from_millis(100)) {
                        Ok(Some(p)) => p,
                        Ok(None) => continue,
                        Err(e) => {
                            tracing::warn!("audio capture stopped: {e:#}");
                            break;
                        }
                    };
                    let packets = match encoder.push(&pcm) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!("audio encode: {e:#}");
                            continue;
                        }
                    };
                    for p in packets {
                        match tx.try_send(AudioPacket {
                            data: Bytes::from(p),
                            duration: FRAME_DURATION,
                        }) {
                            Ok(()) => sent += 1,
                            Err(mpsc::error::TrySendError::Full(_)) => dropped += 1,
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                source.stop();
                                return;
                            }
                        }
                    }
                }
                source.stop();
                tracing::debug!(sent, dropped, "audio pipeline stopped");
            })
            .context("spawning audio pipeline thread")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}
