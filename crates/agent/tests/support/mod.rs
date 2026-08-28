//! Test doubles shared by the integration tests: a fake capturer/encoder that produce a
//! synthetic but structurally valid Annex-B H.264 stream, a fake audio source, a recording
//! input handler, recording approver/indicator/chat, and a fake clipboard backend — plus
//! helpers that drive the browser side of the data channels.

#![allow(dead_code)]

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use protocol::channel::{ControlMessage, InputEvent};
use protocol::common::{DisplayInfo, OperatorInfo, VideoCodec};
use protocol::files::FileMessage;
use remote_agent::approval::{ApprovalOutcome, Approver, Indicator, IndicatorHandle};
use remote_agent::audio::{AudioFormat, AudioSource};
use remote_agent::capture::{CaptureConfig, Capturer, Frame, FrameData};
use remote_agent::chat::{ChatHandle, ChatLine, ChatUi};
use remote_agent::clipboard::{ClipboardBackend, ClipboardContent, ClipboardWatch};
use remote_agent::encode::{EncodedFrame, Encoder, EncoderConfig};
use remote_agent::input::InputHandler;
use remote_agent::session::media::MediaFactory;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};

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
        std::thread::sleep(Duration::from_millis(10));
        let mut data = vec![0u8; (self.width * self.height * 4) as usize];
        for px in data.chunks_exact_mut(4) {
            px[0] = self.tick;
            px[1] = 0x40;
            px[2] = 0x80;
            px[3] = 0xff;
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

/// An encoder that emits a minimal but well-formed Annex-B H.264 access unit.
pub struct FakeEncoder {
    codec: VideoCodec,
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
        let keyframe = force_keyframe || self.counter == 1;
        let data = if keyframe {
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

/// Emits a 440 Hz sine at 48 kHz stereo in 10 ms blocks.
pub struct FakeAudioSource {
    phase: f32,
}

impl AudioSource for FakeAudioSource {
    fn format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        }
    }

    fn read(&mut self, _timeout: Duration) -> Result<Option<Vec<f32>>> {
        std::thread::sleep(Duration::from_millis(10));
        let mut out = Vec::with_capacity(480 * 2);
        for _ in 0..480 {
            let v = (self.phase).sin() * 0.2;
            self.phase += 2.0 * std::f32::consts::PI * 440.0 / 48_000.0;
            out.push(v);
            out.push(v);
        }
        Ok(Some(out))
    }

    fn stop(&mut self) {}
}

/// Media factory backed by the fakes above.
#[derive(Clone)]
pub struct FakeMedia {
    pub codecs: Vec<VideoCodec>,
    pub displays: Vec<DisplayInfo>,
    pub fps: u32,
    pub audio: bool,
}

impl Default for FakeMedia {
    fn default() -> Self {
        Self {
            codecs: vec![VideoCodec::H264],
            displays: test_displays(),
            fps: 30,
            audio: true,
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
            started: None,
            counter: 0,
        }))
    }

    fn audio_available(&self) -> bool {
        self.audio
    }

    fn create_audio_source(&self) -> Result<Box<dyn AudioSource>> {
        if !self.audio {
            anyhow::bail!("no audio in this fake");
        }
        Ok(Box::new(FakeAudioSource { phase: 0.0 }))
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

type SendCallback = Arc<dyn Fn(String) + Send + Sync>;

/// Chat UI that records lines and lets the test "type" as the device user.
#[derive(Clone, Default)]
pub struct RecordingChat {
    pub lines: Arc<Mutex<Vec<ChatLine>>>,
    pub visible: Arc<Mutex<Vec<bool>>>,
    pub on_send: Arc<Mutex<Option<SendCallback>>>,
}

struct RecordingChatHandle {
    chat: RecordingChat,
}

impl ChatHandle for RecordingChatHandle {
    fn push_line(&self, line: &ChatLine) {
        self.chat.lines.lock().unwrap().push(line.clone());
    }
    fn set_visible(&self, visible: bool) {
        self.chat.visible.lock().unwrap().push(visible);
    }
}

impl ChatUi for RecordingChat {
    fn open(
        &self,
        _operator: &OperatorInfo,
        on_send: Arc<dyn Fn(String) + Send + Sync>,
        _on_disconnect: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Box<dyn ChatHandle>> {
        *self.on_send.lock().unwrap() = Some(on_send);
        Ok(Box::new(RecordingChatHandle { chat: self.clone() }))
    }
}

impl RecordingChat {
    /// Simulate the device user typing a line.
    pub fn type_line(&self, text: &str) {
        let cb = self
            .on_send
            .lock()
            .unwrap()
            .clone()
            .expect("chat not opened");
        cb(text.to_string());
    }
}

/// Fake clipboard: records what the agent placed, and lets tests inject "changes".
#[derive(Clone, Default)]
pub struct FakeClipboard {
    pub texts: Arc<Mutex<Vec<String>>>,
    pub images: Arc<Mutex<Vec<PathBuf>>>,
    pub files: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    pub feed: Arc<Mutex<Option<mpsc::UnboundedSender<ClipboardContent>>>>,
}

impl ClipboardBackend for FakeClipboard {
    fn start_watch(&self, _rich: bool) -> ClipboardWatch {
        let (watch, tx) = ClipboardWatch::manual();
        *self.feed.lock().unwrap() = Some(tx);
        watch
    }
    fn set_text(&self, text: &str) -> Result<()> {
        self.texts.lock().unwrap().push(text.to_string());
        Ok(())
    }
    fn set_image_from_png(&self, path: &Path) -> Result<(u32, u32)> {
        let bytes = std::fs::read(path)?;
        let (_, w, h) = remote_agent::clipboard::decode_png(&bytes)?;
        self.images.lock().unwrap().push(path.to_path_buf());
        Ok((w, h))
    }
    fn set_files(&self, paths: &[PathBuf]) -> Result<()> {
        self.files.lock().unwrap().push(paths.to_vec());
        Ok(())
    }
}

impl FakeClipboard {
    pub fn inject(&self, content: ClipboardContent) {
        let tx = self
            .feed
            .lock()
            .unwrap()
            .clone()
            .expect("watch not started");
        tx.send(content).expect("session gone");
    }
}

// ─── browser-side data channel helpers ─────────────────────────────────────────────

/// Everything the "browser" receives on the control channel.
pub type ControlRx = mpsc::UnboundedReceiver<ControlMessage>;

/// Spawn a reader that decodes control messages; returns once the channel is open.
pub async fn read_control(dc: Arc<dyn DataChannel>) -> ControlRx {
    let (tx, rx) = mpsc::unbounded_channel();
    let (open_tx, open_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut open_tx = Some(open_tx);
        while let Some(ev) = dc.poll().await {
            match ev {
                DataChannelEvent::OnOpen => {
                    if let Some(t) = open_tx.take() {
                        let _ = t.send(());
                    }
                }
                DataChannelEvent::OnMessage(m) => {
                    if let Ok(msg) = serde_json::from_slice::<ControlMessage>(&m.data) {
                        let _ = tx.send(msg);
                    }
                }
                DataChannelEvent::OnClose => break,
                _ => {}
            }
        }
    });
    let _ = tokio::time::timeout(Duration::from_secs(10), open_rx).await;
    rx
}

/// What the "browser" receives on the files channel.
pub enum FilesEvent {
    Msg(FileMessage),
    Chunk(Bytes),
}

pub type FilesRx = mpsc::UnboundedReceiver<FilesEvent>;

pub async fn read_files(dc: Arc<dyn DataChannel>) -> FilesRx {
    let (tx, rx) = mpsc::unbounded_channel();
    let (open_tx, open_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut open_tx = Some(open_tx);
        while let Some(ev) = dc.poll().await {
            match ev {
                DataChannelEvent::OnOpen => {
                    if let Some(t) = open_tx.take() {
                        let _ = t.send(());
                    }
                }
                DataChannelEvent::OnMessage(m) => {
                    if m.data.first() == Some(&b'{') {
                        if let Ok(msg) = serde_json::from_slice::<FileMessage>(&m.data) {
                            let _ = tx.send(FilesEvent::Msg(msg));
                        }
                    } else {
                        let _ = tx.send(FilesEvent::Chunk(m.data.freeze()));
                    }
                }
                DataChannelEvent::OnClose => break,
                _ => {}
            }
        }
    });
    let _ = tokio::time::timeout(Duration::from_secs(10), open_rx).await;
    rx
}

pub async fn send_json<T: serde::Serialize>(dc: &Arc<dyn DataChannel>, msg: &T) {
    let text = serde_json::to_string(msg).unwrap();
    dc.send_text(&text).await.expect("send_text");
}

pub async fn send_bytes(dc: &Arc<dyn DataChannel>, bytes: &[u8]) {
    dc.send(BytesMut::from(bytes)).await.expect("send");
}

/// Wait for the next files message matching `pred` (chunks are returned separately).
pub async fn next_files_msg(
    rx: &mut FilesRx,
    chunks: &mut Vec<Bytes>,
    mut pred: impl FnMut(&FileMessage) -> bool,
) -> FileMessage {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(FilesEvent::Msg(m))) => {
                if pred(&m) {
                    return m;
                }
            }
            Ok(Some(FilesEvent::Chunk(c))) => chunks.push(c),
            Ok(None) => panic!("files channel closed"),
            Err(_) => panic!("timed out waiting for files message"),
        }
    }
}

/// Wait for the next control message matching `pred`.
pub async fn next_control(
    rx: &mut ControlRx,
    mut pred: impl FnMut(&ControlMessage) -> bool,
) -> ControlMessage {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(m)) => {
                if pred(&m) {
                    return m;
                }
            }
            Ok(None) => panic!("control channel closed"),
            Err(_) => panic!("timed out waiting for control message"),
        }
    }
}
