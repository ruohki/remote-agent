//! Windows hardware encoding via Media Foundation transforms (H.264 / HEVC encoder MFTs).
//!
//! TODO(builder-windows): implement with the `windows` crate:
//! * `supports(codec)`: `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE |
//!   MFT_ENUM_FLAG_SORTANDFILTER, None, output = MFVideoFormat_HEVC / MFVideoFormat_H264)`.
//! * Create the MFT, set `MF_TRANSFORM_ASYNC_UNLOCK`, hand it the D3D11 device via
//!   `IMFDXGIDeviceManager` (`MFT_MESSAGE_SET_D3D_MANAGER`), set output type
//!   (codec, size, fps, `MF_MT_AVG_BITRATE`, `MF_MT_INTERLACE_MODE` progressive) then
//!   input type `MFVideoFormat_NV12`. Low-latency knobs through `ICodecAPI`:
//!   `CODECAPI_AVLowLatencyMode = 1`, `CODECAPI_AVEncCommonRateControlMode = CBR`,
//!   `CODECAPI_AVEncMPVGOPSize = fps*2`, `CODECAPI_AVEncMPVDefaultBPictureCount = 0`,
//!   `CODECAPI_AVEncVideoForceKeyFrame` for `force_keyframe`.
//! * Convert the BGRA capture texture to NV12 on the GPU with `ID3D11VideoProcessor`
//!   and wrap the NV12 texture with `MFCreateDXGISurfaceBuffer` → `IMFSample`.
//! * Async MFT event loop (`METransformNeedInput` / `METransformHaveOutput`), output is
//!   already Annex-B for MF encoders; ensure parameter sets precede keyframes
//!   (`MF_MT_MPEG_SEQUENCE_HEADER` if the MFT does not emit them in-band).

use super::{Encoder, EncoderConfig};
use anyhow::{bail, Result};
use protocol::common::VideoCodec;

pub fn supports(_codec: VideoCodec) -> bool {
    false
}

pub fn create(_cfg: &EncoderConfig) -> Result<Box<dyn Encoder>> {
    bail!("Media Foundation encoder not implemented yet")
}
