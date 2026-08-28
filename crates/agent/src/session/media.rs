//! Abstraction over the platform capture/encode modules so the session code can be driven
//! with fakes in tests.

use crate::audio::AudioSource;
use crate::capture::{CaptureConfig, Capturer};
use crate::encode::{Encoder, EncoderConfig};
use anyhow::Result;
use protocol::common::{DisplayInfo, VideoCodec};

/// Factory for capturers and encoders plus display/codec discovery.
pub trait MediaFactory: Send + Sync + 'static {
    fn list_displays(&self) -> Result<Vec<DisplayInfo>>;
    /// Codecs this machine can encode, best first.
    fn available_codecs(&self) -> Vec<VideoCodec>;
    fn create_capturer(&self, cfg: &CaptureConfig) -> Result<Box<dyn Capturer>>;
    fn create_encoder(&self, cfg: &EncoderConfig) -> Result<Box<dyn Encoder>>;
    /// Whether the OS currently allows screen capture (macOS Screen Recording permission).
    /// Sessions wait for it instead of failing when it is missing.
    fn capture_permission_granted(&self) -> bool {
        true
    }
    /// Whether system audio capture exists on this platform.
    fn audio_available(&self) -> bool {
        false
    }
    /// Start a system-audio loopback capture.
    fn create_audio_source(&self) -> Result<Box<dyn AudioSource>> {
        anyhow::bail!("system audio capture is not available")
    }
}

/// The real thing: delegates to [`crate::capture`] and [`crate::encode`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemMedia;

impl MediaFactory for SystemMedia {
    fn list_displays(&self) -> Result<Vec<DisplayInfo>> {
        crate::capture::list_displays()
    }

    fn capture_permission_granted(&self) -> bool {
        crate::platform::screen_capture_allowed()
    }

    fn available_codecs(&self) -> Vec<VideoCodec> {
        crate::encode::available_codecs()
    }

    fn create_capturer(&self, cfg: &CaptureConfig) -> Result<Box<dyn Capturer>> {
        crate::capture::create_capturer(cfg)
    }

    fn create_encoder(&self, cfg: &EncoderConfig) -> Result<Box<dyn Encoder>> {
        crate::encode::create_encoder(cfg)
    }

    fn audio_available(&self) -> bool {
        crate::audio::available()
    }

    fn create_audio_source(&self) -> Result<Box<dyn AudioSource>> {
        crate::audio::create_source()
    }
}

/// Pick the codec for a session.
///
/// `offered` is the browser's list in preference order (from the SDP offer), `available`
/// what this machine can encode. When both sides support `preferred` it wins, otherwise the
/// browser's first supported choice.
pub fn choose_codec(
    offered: &[VideoCodec],
    available: &[VideoCodec],
    preferred: VideoCodec,
) -> Option<VideoCodec> {
    if offered.contains(&preferred) && available.contains(&preferred) {
        return Some(preferred);
    }
    offered.iter().copied().find(|c| available.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_wins_when_both_support_it() {
        let c = choose_codec(
            &[VideoCodec::H264, VideoCodec::H265],
            &[VideoCodec::H265, VideoCodec::H264],
            VideoCodec::H265,
        );
        assert_eq!(c, Some(VideoCodec::H265));
    }

    #[test]
    fn falls_back_to_browser_order() {
        let c = choose_codec(
            &[VideoCodec::H264],
            &[VideoCodec::H265, VideoCodec::H264],
            VideoCodec::H265,
        );
        assert_eq!(c, Some(VideoCodec::H264));
        let c = choose_codec(&[VideoCodec::H265], &[VideoCodec::H264], VideoCodec::H265);
        assert_eq!(c, None);
    }
}
