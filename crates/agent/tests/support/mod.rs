//! Test doubles shared by the integration tests: a fake capturer/encoder that produce a
//! synthetic but structurally valid Annex-B H.264 stream, a recording input handler, and a
//! recording approver/indicator.

#![allow(dead_code)]

use anyhow::Result;
use bytes::Bytes;
use protocol::channel::InputEvent;
use protocol::common::{DisplayInfo, OperatorInfo, VideoCodec};
use remote_agent::approval::{ApprovalOutcome, Approver, Indicator, IndicatorHandle};
use remote_agent::capture::{CaptureConfig, Capturer, Frame, FrameData};
use remote_agent::encode::{EncodedFrame, Encoder, EncoderConfig};
use remote_agent::input::InputHandler;
use remote_agent::session::media::MediaFactory;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Two displays so display-switching can be exercised.
pub fn test_displays() -> Vec<DisplayInfo> {
    vec![
        DisplayInfo {
            index: 0,
            name: "Primary".into(),
            x: 0,
            y: 0,
            width: 320,
            height: 240,
            scale: 1.0,
            primary: true,
        },
        DisplayInfo {
            index: 1,
            name: "Second".into(),
            x: 320,
            y: 0,
            width: 320,
            height: 240,
            scale: 1.0,
            primary: false,
        },
    ]
}

/// A capturer that yields a solid-colour BGRA frame at a fixed cadence.
pub struct FakeCapturer {
    width: u32,
    height: u32,
    tick: u8,
}

impl Capturer for FakeCapturer {
    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<Frame>> {
        self.tick = self.tick.wrapping_add(1);
        let mut data = vec![0u8; (self.width * self.height * 4) as usize];
        for px in data.chunks_exact_mut(4) {
            px[0] = self.tick; // B
            px[1] = 0x40; // G
            px[2] = 0x80; // R
            px[3] = 0xff; // A
        }
        Ok(Some(Frame {
            width: self.width,
            height: self.height,
            captured_at: Instant::now(),
            data: FrameData::Bgra {
                data,
                stride: (self.width * 4) as usize,
            },
        }))
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn stop(&mut self) {}
}

/// An encoder that emits a minimal but well-formed Annex-B H.264 access unit: on keyframes
/// SPS + PPS + IDR, otherwise a non-IDR slice. Enough for the RTP packetizer to accept and
/// for the receiving side to see packets flow.
pub struct FakeEncoder {
    codec: VideoCodec,
    fps: u32,
    started: Option<Instant>,
    counter: u64,
}

fn annexb(nals: &[&[u8]]) -> Bytes {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    Bytes::from(out)
}

impl Encoder for FakeEncoder {
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        let start = *self.started.get_or_insert(frame.captured_at);
        let pts = frame.captured_at.saturating_duration_since(start);
        self.counter += 1;
        // First frame is always a keyframe.
        let keyframe = force_keyframe || self.counter == 1;
        let data = if keyframe {
            // SPS (type 7), PPS (type 8), IDR slice (type 5). Payloads are placeholder bytes.
            annexb(&[
                &[0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01, 0x6c, 0x80],
                &[0x68, 0xce, 0x3c, 0x80],
                &[0x65, 0x88, 0x84, 0x00, 0x21, 0xff, 0xff],
            ])
        } else {
            annexb(&[&[0x41, 0x9a, 0x00, 0x10, 0x20, 0x30]])
        };
        Ok(vec![EncodedFrame {
            data,
            keyframe,
            pts,
        }])
    }

    fn set_bitrate(&mut self, _kbps: u32) -> Result<()> {
        Ok(())
    }

    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn is_hardware(&self) -> bool {
        false
    }
}

/// Media factory backed by the fakes above.
#[derive(Clone)]
pub struct FakeMedia {
    pub codecs: Vec<VideoCodec>,
    pub displays: Vec<DisplayInfo>,
    pub fps: u32,
}

impl Default for FakeMedia {
    fn default() -> Self {
        Self {
            codecs: vec![VideoCodec::H264],
            displays: test_displays(),
            fps: 30,
        }
    }
}

impl MediaFactory for FakeMedia {
    fn list_displays(&self) -> Result<Vec<DisplayInfo>> {
        Ok(self.displays.clone())
    }

    fn available_codecs(&self) -> Vec<VideoCodec> {
        self.codecs.clone()
    }

    fn create_capturer(&self, cfg: &CaptureConfig) -> Result<Box<dyn Capturer>> {
        let d = self
            .displays
            .iter()
            .find(|d| d.index == cfg.display_index)
            .cloned()
            .unwrap_or_else(|| self.displays[0].clone());
        Ok(Box::new(FakeCapturer {
            width: d.width,
            height: d.height,
            tick: 0,
        }))
    }

    fn create_encoder(&self, cfg: &EncoderConfig) -> Result<Box<dyn Encoder>> {
        Ok(Box::new(FakeEncoder {
            codec: cfg.codec,
            fps: self.fps.max(1),
            started: None,
            counter: 0,
        }))
    }
}

/// Records every input event it receives.
#[derive(Clone, Default)]
pub struct RecordingInput {
    pub events: Arc<Mutex<Vec<InputEvent>>>,
    pub releases: Arc<AtomicU64>,
}

impl InputHandler for RecordingInput {
    fn set_display(&mut self, _display: &DisplayInfo, _video_size: (u32, u32)) {}

    fn handle(&mut self, event: InputEvent) -> Result<()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }

    fn release_all(&mut self) {
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

/// Approver returning a fixed outcome and counting calls.
pub struct RecordingApprover {
    pub outcome: ApprovalOutcome,
    pub calls: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl Approver for RecordingApprover {
    async fn ask(&self, _operator: &OperatorInfo, _timeout: Duration) -> Result<ApprovalOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.outcome)
    }
}

pub struct NoopIndicator;
struct NoopHandle;
impl IndicatorHandle for NoopHandle {}
impl Indicator for NoopIndicator {
    fn show(
        &self,
        _operator: &OperatorInfo,
        _on_disconnect: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Box<dyn IndicatorHandle>> {
        Ok(Box::new(NoopHandle))
    }
}
