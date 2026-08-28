//! Exercises the OpenH264 software encoder (`src/encode/software.rs`) on every platform.
//!
//! The agent is a binary crate, so `software.rs` is pulled in with `#[path]` together with
//! small shims (at this crate's root, mirroring `crate::capture` / `crate::encode::*`) for the
//! items it depends on.

#![allow(dead_code, unused_imports, clippy::all)]

use std::time::{Duration, Instant};

mod capture {
    use anyhow::Result;
    use std::time::Instant;

    pub enum FrameData {
        Bgra { data: Vec<u8>, stride: usize },
    }

    pub struct Frame {
        pub width: u32,
        pub height: u32,
        pub captured_at: Instant,
        pub data: FrameData,
    }

    pub fn to_bgra(frame: &Frame) -> Result<(Vec<u8>, usize)> {
        match &frame.data {
            FrameData::Bgra { data, stride } => Ok((data.clone(), *stride)),
        }
    }
}

// `software.rs` refers to its siblings as `super::…`; at the test crate root that is us.
use anyhow::Result;
use bytes::Bytes;

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

pub struct EncodedFrame {
    pub data: Bytes,
    pub keyframe: bool,
    pub pts: Duration,
}

pub trait Encoder: Send {
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>>;
    fn set_bitrate(&mut self, kbps: u32) -> Result<()>;
    fn codec(&self) -> VideoCodec;
    fn is_hardware(&self) -> bool;
    fn output_size(&self) -> Option<(u32, u32)> {
        None
    }
}

#[path = "../src/encode/software.rs"]
pub mod software;

mod encode {
    pub use crate::software;
}

use capture::{Frame, FrameData};
use protocol::common::VideoCodec;

const W: u32 = 1280;
const H: u32 = 720;

/// Moving gradient with a sharp box so the encoder has real work to do.
fn synth_frame(index: u32, start: Instant) -> Frame {
    let stride = W as usize * 4;
    let mut data = vec![0u8; stride * H as usize];
    let shift = (index * 7) as usize;
    for y in 0..H as usize {
        for x in 0..W as usize {
            let o = y * stride + x * 4;
            let inside = x > 100 + shift && x < 400 + shift && y > 100 && y < 400;
            let (b, g, r) = if inside {
                (20u8, 200u8, 255u8)
            } else {
                (
                    ((x + shift) & 255) as u8,
                    ((y + shift) & 255) as u8,
                    ((x ^ y) & 255) as u8,
                )
            };
            data[o] = b;
            data[o + 1] = g;
            data[o + 2] = r;
            data[o + 3] = 255;
        }
    }
    Frame {
        width: W,
        height: H,
        captured_at: start + Duration::from_millis(index as u64 * 33),
        data: FrameData::Bgra { data, stride },
    }
}

/// Split an Annex-B stream into NAL unit payload slices (without start codes).
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::new();
    for (n, &s) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map(|e| e - 3).unwrap_or(data.len());
        // trim the leading zero of a 4-byte start code belonging to the next NAL
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        units.push(&data[s..end]);
    }
    units
}

fn h264_nal_types(data: &[u8]) -> Vec<u8> {
    nal_units(data)
        .iter()
        .filter(|n| !n.is_empty())
        .map(|n| n[0] & 0x1f)
        .collect()
}

fn config() -> EncoderConfig {
    EncoderConfig {
        codec: VideoCodec::H264,
        width: W,
        height: H,
        fps: 30,
        bitrate_kbps: 2000,
    }
}

#[test]
fn software_encoder_produces_annex_b_h264() {
    let mut enc = encode::software::create(&config()).expect("create OpenH264 encoder");
    assert_eq!(enc.codec(), VideoCodec::H264);
    assert!(!enc.is_hardware());

    let start = Instant::now();
    let mut total = 0usize;
    let mut keyframes = 0usize;
    let mut first_checked = false;
    for i in 0..60 {
        let frame = synth_frame(i, start);
        let out = enc.encode(&frame, false).expect("encode");
        assert_eq!(out.len(), 1, "one access unit per frame");
        let au = &out[0];
        assert!(
            au.data.starts_with(&[0, 0, 0, 1]) || au.data.starts_with(&[0, 0, 1]),
            "Annex-B start code"
        );
        assert_eq!(au.pts, Duration::from_millis(i as u64 * 33));
        total += au.data.len();
        if au.keyframe {
            keyframes += 1;
        }
        let types = h264_nal_types(&au.data);
        if !first_checked {
            first_checked = true;
            assert!(au.keyframe, "first frame must be a keyframe");
            let sps = types.iter().position(|&t| t == 7).expect("SPS present");
            let pps = types.iter().position(|&t| t == 8).expect("PPS present");
            let idr = types.iter().position(|&t| t == 5).expect("IDR present");
            assert!(sps < idr && pps < idr, "SPS/PPS precede IDR: {types:?}");
        } else if au.keyframe {
            assert!(
                types.contains(&7) && types.contains(&8),
                "keyframes carry SPS/PPS"
            );
        } else {
            assert!(types.contains(&1), "P frame has non-IDR slice: {types:?}");
        }
    }
    assert!(keyframes >= 1);
    let avg = total / 60;
    // 2000 kbps at 30 fps ≈ 8.3 KB per frame; allow a generous window.
    assert!(
        avg > 500 && avg < 60_000,
        "average frame size {avg} bytes out of range"
    );
}

#[test]
fn force_keyframe_and_bitrate_change() {
    let mut enc = encode::software::create(&config()).expect("create encoder");
    let start = Instant::now();
    for i in 0..5 {
        enc.encode(&synth_frame(i, start), false).unwrap();
    }
    let out = enc.encode(&synth_frame(5, start), true).unwrap();
    assert!(out[0].keyframe, "forced keyframe");
    let types = h264_nal_types(&out[0].data);
    assert!(
        types.contains(&5) && types.contains(&7) && types.contains(&8),
        "{types:?}"
    );

    // Lower the bitrate a lot and check that frames get smaller on average.
    let mut before = 0usize;
    for i in 6..26 {
        before += enc.encode(&synth_frame(i, start), false).unwrap()[0]
            .data
            .len();
    }
    enc.set_bitrate(200).unwrap();
    let mut after = 0usize;
    for i in 26..46 {
        after += enc.encode(&synth_frame(i, start), false).unwrap()[0]
            .data
            .len();
    }
    assert!(
        after < before,
        "bitrate change should reduce output: before={before} after={after}"
    );
}

#[test]
fn rejects_non_h264_and_tiny_frames() {
    let mut cfg = config();
    cfg.codec = VideoCodec::H265;
    assert!(encode::software::create(&cfg).is_err());
    let mut cfg = config();
    cfg.width = 8;
    assert!(encode::software::create(&cfg).is_err());
}
