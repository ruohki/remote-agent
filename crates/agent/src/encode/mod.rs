//! Video encoders producing Annex-B H.265 / H.264 access units for the WebRTC track.
//!
//! Selection order in [`create_encoder`]:
//! 1. platform hardware encoder for the requested codec (VideoToolbox / Media Foundation)
//! 2. platform hardware H.264 if H.265 was requested but unavailable
//! 3. software H.264 (OpenH264)
//!
//! Requirements for every implementation:
//! * Output is **Annex-B** (`00 00 00 01` start codes). Parameter sets (VPS/SPS/PPS)
//!   MUST be emitted in-band before every keyframe — the WebRTC payloader and the
//!   browser decoder rely on that.
//! * Low-latency settings: no B-frames, real-time mode, 1 frame in flight,
//!   keyframe interval ≈ 2–4 s (browser requests extra keyframes via PLI/FIR
//!   → [`Encoder::encode`] with `force_keyframe = true`).
//! * `encode` may return zero or more frames (asynchronous encoders drain what is ready).

use crate::capture::Frame;
use anyhow::Result;
use bytes::Bytes;
use protocol::common::VideoCodec;
use std::time::Duration;

pub mod software;
#[cfg(target_os = "windows")]
pub mod mediafoundation;
#[cfg(target_os = "macos")]
pub mod videotoolbox;

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

pub struct EncodedFrame {
    /// Annex-B access unit.
    pub data: Bytes,
    pub keyframe: bool,
    /// Presentation timestamp relative to encoder start.
    pub pts: Duration,
}

pub trait Encoder: Send {
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>>;
    fn set_bitrate(&mut self, kbps: u32) -> Result<()>;
    fn codec(&self) -> VideoCodec;
    fn is_hardware(&self) -> bool;
}

/// Codecs this machine can encode, best first. Cached after the first probe.
pub fn available_codecs() -> Vec<VideoCodec> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<VideoCodec>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let mut v = Vec::new();
            if hardware_supports(VideoCodec::H265) {
                v.push(VideoCodec::H265);
            }
            // H.264 is always available thanks to the software fallback.
            v.push(VideoCodec::H264);
            v
        })
        .clone()
}

fn hardware_supports(codec: VideoCodec) -> bool {
    #[cfg(target_os = "macos")]
    {
        videotoolbox::supports(codec)
    }
    #[cfg(target_os = "windows")]
    {
        mediafoundation::supports(codec)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = codec;
        false
    }
}

pub fn create_encoder(cfg: &EncoderConfig) -> Result<Box<dyn Encoder>> {
    // 1. hardware, requested codec
    if let Some(enc) = try_hardware(cfg) {
        return Ok(enc);
    }
    // 2. hardware H.264 fallback
    if cfg.codec != VideoCodec::H264 {
        let mut h264 = cfg.clone();
        h264.codec = VideoCodec::H264;
        if let Some(enc) = try_hardware(&h264) {
            return Ok(enc);
        }
    }
    // 3. software H.264
    let mut sw = cfg.clone();
    sw.codec = VideoCodec::H264;
    software::create(&sw)
}

fn try_hardware(cfg: &EncoderConfig) -> Option<Box<dyn Encoder>> {
    #[cfg(target_os = "macos")]
    {
        match videotoolbox::create(cfg) {
            Ok(e) => return Some(e),
            Err(err) => tracing::warn!("VideoToolbox {:?} unavailable: {err:#}", cfg.codec),
        }
    }
    #[cfg(target_os = "windows")]
    {
        match mediafoundation::create(cfg) {
            Ok(e) => return Some(e),
            Err(err) => tracing::warn!("Media Foundation {:?} unavailable: {err:#}", cfg.codec),
        }
    }
    let _ = cfg;
    None
}
