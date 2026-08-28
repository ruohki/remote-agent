//! macOS hardware encoding via VideoToolbox (`VTCompressionSession`).
//!
//! TODO(builder-macos): implement with `objc2-video-toolbox` / `objc2-core-media`:
//! * `supports(codec)`: `VTCopyVideoEncoderList` contains an entry for
//!   `kCMVideoCodecType_HEVC` / `kCMVideoCodecType_H264` with
//!   `kVTVideoEncoderList_IsHardwareAccelerated`; alternatively try creating a session
//!   with `kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder`.
//! * Session properties: `RealTime = true`, `AllowFrameReordering = false`,
//!   `MaxKeyFrameInterval = fps * 2`, `AverageBitRate`, `DataRateLimits`,
//!   `ExpectedFrameRate`, `ProfileLevel` (HEVC_Main_AutoLevel / H264_High_AutoLevel),
//!   `MaxFrameDelayCount = 0`.
//! * Feed the `CVPixelBuffer` from [`crate::capture::FrameData::PixelBuffer`] directly
//!   (`VTCompressionSessionEncodeFrame`); convert `Bgra` frames via a pixel buffer pool.
//! * In the output callback convert the AVCC/HVCC `CMSampleBuffer` to Annex-B: prepend
//!   VPS/SPS/PPS from `CMVideoFormatDescriptionGetH264ParameterSetAtIndex` /
//!   `…HEVCParameterSetAtIndex` on keyframes (no `kCMSampleAttachmentKey_NotSync`),
//!   then rewrite 4-byte length prefixes to start codes.

use super::{Encoder, EncoderConfig};
use anyhow::{bail, Result};
use protocol::common::VideoCodec;

pub fn supports(_codec: VideoCodec) -> bool {
    false
}

pub fn create(_cfg: &EncoderConfig) -> Result<Box<dyn Encoder>> {
    bail!("VideoToolbox encoder not implemented yet")
}
