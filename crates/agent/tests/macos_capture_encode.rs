//! End-to-end capture → encode through the public API (macOS only).
//!
//! Needs the Screen Recording permission for the process running the tests, hence `#[ignore]`:
//! `cargo test -p remote-agent --release --test macos_capture_encode -- --ignored --nocapture`
#![cfg(target_os = "macos")]

use protocol::common::VideoCodec;
use remote_agent::capture::{create_capturer, list_displays, CaptureConfig};
use remote_agent::encode::videotoolbox::{annexb_nals, h264_nal_type, hevc_nal_type};
use remote_agent::encode::{available_codecs, create_encoder, EncoderConfig};
use std::time::{Duration, Instant};

#[test]
fn macos_public_display_enumeration_and_codecs() {
    let displays = list_displays().expect("list_displays");
    assert!(!displays.is_empty());
    assert!(displays[0].primary);
    let codecs = available_codecs();
    assert_eq!(
        *codecs.last().unwrap(),
        VideoCodec::H264,
        "H264 is always last"
    );
    eprintln!("displays: {}, codecs: {codecs:?}", displays.len());
}

#[test]
#[ignore]
fn macos_public_capture_encode_roundtrip() {
    for codec in available_codecs() {
        let mut cap = create_capturer(&CaptureConfig {
            display_index: 0,
            max_fps: 30,
            show_cursor: true,
        })
        .expect("capturer");
        let (width, height) = cap.size();
        let mut enc = create_encoder(&EncoderConfig {
            codec,
            width,
            height,
            fps: 30,
            bitrate_kbps: 6000,
            max_output: None,
        })
        .expect("encoder");
        assert_eq!(
            enc.codec(),
            codec,
            "hardware encoder for {codec:?} expected"
        );
        assert!(enc.is_hardware());

        let start = Instant::now();
        let mut encoded = Vec::new();
        let mut latencies = Vec::new();
        while encoded.len() < 20 && start.elapsed() < Duration::from_secs(5) {
            let Some(frame) = cap
                .next_frame(Duration::from_millis(100))
                .expect("next_frame")
            else {
                continue;
            };
            let out = enc.encode(&frame, encoded.len() == 10).expect("encode");
            if !out.is_empty() {
                latencies.push(frame.captured_at.elapsed());
            }
            encoded.extend(out);
        }
        cap.stop();
        assert!(
            encoded.len() >= 10,
            "{codec:?}: only {} frames",
            encoded.len()
        );
        assert!(encoded[0].keyframe);
        assert!(
            encoded.iter().skip(1).any(|f| f.keyframe),
            "forced keyframe missing"
        );
        for f in &encoded {
            assert!(f.data.starts_with(&[0, 0, 0, 1]));
            let nals = annexb_nals(&f.data);
            let first = nals.first().expect("at least one NAL");
            match codec {
                VideoCodec::H265 => {
                    let t = hevc_nal_type(first).unwrap();
                    assert_eq!(f.keyframe, t == 32, "HEVC keyframes start with a VPS: {t}");
                }
                VideoCodec::H264 => {
                    let t = h264_nal_type(first).unwrap();
                    assert_eq!(f.keyframe, t == 7, "H264 keyframes start with an SPS: {t}");
                }
            }
        }
        let avg = latencies.iter().sum::<Duration>() / latencies.len().max(1) as u32;
        eprintln!(
            "{codec:?} {width}x{height}: {} frames, capture→encoded avg {avg:?}, avg {} bytes",
            encoded.len(),
            encoded.iter().map(|f| f.data.len()).sum::<usize>() / encoded.len()
        );
    }
}
