//! Software H.264 fallback using Cisco OpenH264 (always available, CPU heavy).
//!
//! * `UsageType::ScreenContentRealTime`, bitrate rate control, single spatial layer,
//!   no frame skipping, intra period = `fps * 2`.
//! * Input is converted from BGRA to I420 with a row based integer BT.709 (limited
//!   range) conversion into a reusable buffer; odd dimensions are cropped to even.
//! * OpenH264 emits Annex-B with SPS/PPS before every IDR.
//! * `force_keyframe` → `force_intra_frame`; `set_bitrate` uses the live
//!   `ENCODER_OPTION_BITRATE` option (falls back to re-creating the encoder).

use super::{EncodedFrame, Encoder, EncoderConfig};
use crate::capture::{to_bgra, Frame};
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use openh264::encoder::{
    BitRate, Encoder as H264Encoder, EncoderConfig as H264Config, FrameRate, FrameType,
    IntraFramePeriod, Profile, RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::YUVSource;
use openh264::{OpenH264API, Timestamp};
use protocol::common::VideoCodec;
use std::os::raw::{c_int, c_void};
use std::time::{Duration, Instant};

/// Mirrors OpenH264's `ENCODER_OPTION_BITRATE`.
const ENCODER_OPTION_BITRATE: c_int = 5;
/// Mirrors OpenH264's `SPATIAL_LAYER_ALL`.
const SPATIAL_LAYER_ALL: c_int = 4;

/// Mirrors OpenH264's `SBitrateInfo` (`TagBitrateInfo`).
#[repr(C)]
struct SBitrateInfo {
    layer: c_int,
    bitrate: c_int,
}

/// Reusable I420 planes with even dimensions.
pub struct I420Buffer {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420Buffer {
    pub fn new(width: usize, height: usize) -> Self {
        let (width, height) = (width & !1, height & !1);
        Self {
            width,
            height,
            y: vec![0; width * height],
            u: vec![0; (width / 2) * (height / 2)],
            v: vec![0; (width / 2) * (height / 2)],
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Resize the planes if the source size changed (crops odd sizes to even).
    fn ensure_size(&mut self, width: usize, height: usize) {
        let (width, height) = (width & !1, height & !1);
        if width != self.width || height != self.height {
            *self = Self::new(width, height);
        }
    }

    /// Convert strided BGRA into this buffer (BT.709 limited range, 2×2 chroma average).
    pub fn fill_from_bgra(&mut self, bgra: &[u8], stride: usize, width: usize, height: usize) {
        self.ensure_size(width, height);
        let (w, h) = (self.width, self.height);
        debug_assert!(bgra.len() >= stride * (h - 1) + w * 4);
        let cw = w / 2;

        for row in 0..h {
            let src = &bgra[row * stride..row * stride + w * 4];
            let y_row = &mut self.y[row * w..(row + 1) * w];
            for (x, px) in src.chunks_exact(4).enumerate() {
                y_row[x] = bgra_to_y(px[2], px[1], px[0]);
            }
        }

        for cy in 0..h / 2 {
            let r0 = &bgra[(cy * 2) * stride..(cy * 2) * stride + w * 4];
            let r1 = &bgra[(cy * 2 + 1) * stride..(cy * 2 + 1) * stride + w * 4];
            let u_row = &mut self.u[cy * cw..(cy + 1) * cw];
            let v_row = &mut self.v[cy * cw..(cy + 1) * cw];
            for cx in 0..cw {
                let o = cx * 8;
                // average the 2×2 block in RGB space before converting
                let b = (r0[o] as u32 + r0[o + 4] as u32 + r1[o] as u32 + r1[o + 4] as u32 + 2) / 4;
                let g =
                    (r0[o + 1] as u32 + r0[o + 5] as u32 + r1[o + 1] as u32 + r1[o + 5] as u32 + 2)
                        / 4;
                let r =
                    (r0[o + 2] as u32 + r0[o + 6] as u32 + r1[o + 2] as u32 + r1[o + 6] as u32 + 2)
                        / 4;
                let (u, v) = rgb_to_uv(r as u8, g as u8, b as u8);
                u_row[cx] = u;
                v_row[cx] = v;
            }
        }
    }
}

impl YUVSource for I420Buffer {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

/// BT.709 limited-range luma, fixed point (×256).
#[inline]
pub fn bgra_to_y(r: u8, g: u8, b: u8) -> u8 {
    let y = (47 * r as i32 + 157 * g as i32 + 16 * b as i32 + 128) >> 8;
    (y + 16).clamp(16, 235) as u8
}

/// BT.709 limited-range chroma, fixed point (×256).
#[inline]
pub fn rgb_to_uv(r: u8, g: u8, b: u8) -> (u8, u8) {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let u = ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
    (u.clamp(16, 240) as u8, v.clamp(16, 240) as u8)
}

fn build_config(cfg: &EncoderConfig) -> H264Config {
    let fps = cfg.fps.max(1);
    H264Config::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(
            cfg.bitrate_kbps.saturating_mul(1000).max(100_000),
        ))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .skip_frames(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 2))
        .profile(Profile::Main)
        .scene_change_detect(true)
        // Not supported by OpenH264 for screen content (it would warn and disable them).
        .background_detection(false)
        .adaptive_quantization(false)
        .num_threads(0)
        .vui(VuiConfig::bt709())
}

pub struct OpenH264Encoder {
    encoder: H264Encoder,
    cfg: EncoderConfig,
    yuv: I420Buffer,
    started: Option<Instant>,
    frames: u64,
}

impl OpenH264Encoder {
    fn new(cfg: &EncoderConfig) -> Result<Self> {
        if cfg.width < 16 || cfg.height < 16 {
            return Err(anyhow!("frame size {}x{} too small", cfg.width, cfg.height));
        }
        let api = OpenH264API::from_source();
        let encoder = H264Encoder::with_api_config(api, build_config(cfg))
            .map_err(|e| anyhow!("creating OpenH264 encoder: {e}"))?;
        Ok(Self {
            encoder,
            cfg: cfg.clone(),
            yuv: I420Buffer::new(cfg.width as usize, cfg.height as usize),
            started: None,
            frames: 0,
        })
    }

    fn pts_for(&mut self, frame: &Frame) -> Duration {
        let start = *self.started.get_or_insert(frame.captured_at);
        frame.captured_at.saturating_duration_since(start)
    }
}

impl Encoder for OpenH264Encoder {
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        let (bgra, stride) = to_bgra(frame).context("converting frame to BGRA")?;
        let (w, h) = (frame.width as usize, frame.height as usize);
        if bgra.len() < stride * (h - 1) + w * 4 {
            return Err(anyhow!(
                "BGRA buffer too small: {} bytes for {w}x{h} stride {stride}",
                bgra.len()
            ));
        }
        self.yuv.fill_from_bgra(&bgra, stride, w, h);

        if force_keyframe {
            self.encoder.force_intra_frame();
        }
        let pts = self.pts_for(frame);
        let stream = self
            .encoder
            .encode_at(&self.yuv, Timestamp::from_millis(pts.as_millis() as u64))
            .map_err(|e| anyhow!("OpenH264 encode: {e}"))?;
        let keyframe = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
        let data = stream.to_vec();
        self.frames += 1;
        if data.is_empty() {
            // Skipped frame (should not happen with skip_frames(false)).
            return Ok(Vec::new());
        }
        Ok(vec![EncodedFrame {
            data: Bytes::from(data),
            keyframe,
            pts,
        }])
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        let bps = kbps.saturating_mul(1000).max(100_000) as c_int;
        let mut info = SBitrateInfo {
            layer: SPATIAL_LAYER_ALL,
            bitrate: bps,
        };
        // SAFETY: `SBitrateInfo` mirrors OpenH264's `TagBitrateInfo` and the option id is
        // `ENCODER_OPTION_BITRATE`; the encoder only reads the struct during the call.
        let rc = unsafe {
            self.encoder.raw_api().set_option(
                ENCODER_OPTION_BITRATE,
                (&mut info as *mut SBitrateInfo).cast::<c_void>(),
            )
        };
        self.cfg.bitrate_kbps = kbps;
        if rc != 0 {
            tracing::warn!("OpenH264 live bitrate change failed ({rc}); re-creating encoder");
            let api = OpenH264API::from_source();
            self.encoder = H264Encoder::with_api_config(api, build_config(&self.cfg))
                .map_err(|e| anyhow!("re-creating OpenH264 encoder: {e}"))?;
        }
        Ok(())
    }

    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn is_hardware(&self) -> bool {
        false
    }
}

pub fn create(cfg: &EncoderConfig) -> Result<Box<dyn Encoder>> {
    if cfg.codec != VideoCodec::H264 {
        return Err(anyhow!("software encoder only supports H.264"));
    }
    Ok(Box::new(OpenH264Encoder::new(cfg)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luma_and_chroma_of_primaries() {
        // BT.709 limited range reference values (±1 rounding).
        assert!((bgra_to_y(255, 255, 255) as i32 - 235).abs() <= 1);
        assert_eq!(bgra_to_y(0, 0, 0), 16);
        assert!((bgra_to_y(255, 0, 0) as i32 - 63).abs() <= 1);
        assert!((bgra_to_y(0, 255, 0) as i32 - 173).abs() <= 1);
        assert!((bgra_to_y(0, 0, 255) as i32 - 32).abs() <= 1);
        let (u, v) = rgb_to_uv(128, 128, 128);
        assert_eq!((u, v), (128, 128));
        let (u, v) = rgb_to_uv(255, 0, 0);
        assert!((u as i32 - 102).abs() <= 1, "u={u}");
        assert!((v as i32 - 240).abs() <= 1, "v={v}");
        let (u, v) = rgb_to_uv(0, 0, 255);
        assert!((u as i32 - 240).abs() <= 1, "u={u}");
        assert!((v as i32 - 118).abs() <= 1, "v={v}");
    }

    #[test]
    fn bgra_to_i420_known_pixels_and_stride() {
        // 4x2 image with 8 bytes of row padding; left half red, right half blue.
        let w = 4;
        let h = 2;
        let stride = w * 4 + 8;
        let mut bgra = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                let o = y * stride + x * 4;
                if x < 2 {
                    bgra[o..o + 4].copy_from_slice(&[0, 0, 255, 255]); // red
                } else {
                    bgra[o..o + 4].copy_from_slice(&[255, 0, 0, 255]); // blue
                }
            }
        }
        let mut buf = I420Buffer::new(w, h);
        buf.fill_from_bgra(&bgra, stride, w, h);
        assert_eq!(buf.dimensions(), (4, 2));
        assert_eq!(buf.strides(), (4, 2, 2));
        assert!((buf.y()[0] as i32 - 63).abs() <= 1);
        assert!((buf.y()[3] as i32 - 32).abs() <= 1);
        let (u_red, v_red) = rgb_to_uv(255, 0, 0);
        let (u_blue, v_blue) = rgb_to_uv(0, 0, 255);
        assert_eq!(buf.u()[0], u_red);
        assert_eq!(buf.v()[0], v_red);
        assert_eq!(buf.u()[1], u_blue);
        assert_eq!(buf.v()[1], v_blue);
    }

    #[test]
    fn odd_sizes_are_cropped_to_even() {
        let buf = I420Buffer::new(5, 3);
        assert_eq!(buf.dimensions(), (4, 2));
        let stride = 5 * 4;
        let bgra = vec![0u8; stride * 3];
        let mut buf = I420Buffer::new(2, 2);
        buf.fill_from_bgra(&bgra, stride, 5, 3);
        assert_eq!(buf.dimensions(), (4, 2));
    }
}
