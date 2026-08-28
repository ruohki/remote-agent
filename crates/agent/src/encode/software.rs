//! Software H.264 fallback using Cisco OpenH264 (always available, CPU heavy).
//!
//! TODO(builder-windows): implement with the `openh264` crate:
//! * `EncoderConfig::new().max_frame_rate(fps).bitrate(Bitrate::from_bps(kbps*1000))
//!   .rate_control_mode(RateControlMode::Bitrate).usage_type(UsageType::ScreenContentRealTime)`
//! * Convert BGRA (via `crate::capture::to_bgra`) to I420 (`openh264::formats::YUVBuffer`
//!   or a hand-rolled SIMD-friendly loop) and call `encode`.
//! * OpenH264 output is already Annex-B with SPS/PPS before IDR frames.
//! * `force_keyframe` → `Encoder::force_intra_frame(true)`.

use super::{Encoder, EncoderConfig};
use anyhow::{bail, Result};

pub fn create(_cfg: &EncoderConfig) -> Result<Box<dyn Encoder>> {
    bail!("OpenH264 software encoder not implemented yet")
}
