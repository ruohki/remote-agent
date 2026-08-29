//! End-to-end loopback: drive a real [`SessionManager`] with fake capture/encode/audio/
//! input/clipboard, and a second webrtc-rs peer connection standing in for the browser.
//!
//! Covers: multi-display track binding, audio negotiation, resumable uploads and downloads
//! over the `files` channel, the remote file browser, clipboard image sync in both
//! directions, chat, input, and help-me denial.

mod support;

use bytes::Bytes;
use parking_lot::RwLock;
use protocol::agent::{AgentToConsole, SessionEvent};
use protocol::channel::{ChatParty, ClipboardKind, ControlMessage, InputEvent, MouseButton};
use protocol::common::{
    DeviceMode, EndReason, IceCandidate, OperatorInfo, SessionDescription, SessionRole,
    SessionState,
};
use protocol::config::AgentConfig;
use protocol::files::{
    decode_chunk, decode_chunk_any, encode_chunk_header, encode_chunk_header_v2, ChunkCodec,
    FileMessage, TransferDirection, TransferKind, CHUNK_HEADER_LEN, CHUNK_HEADER_V2_LEN,
    MAX_CHUNK_BYTES,
};
use remote_agent::approval::AutoApprover;
use remote_agent::clipboard::ClipboardContent;
use remote_agent::hub::HubSink;
use remote_agent::input::InputHandler;
use remote_agent::session::{SessionDeps, SessionManager, SessionRequest};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use support::*;
use tokio::sync::mpsc;

use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS,
};
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind,
};
use webrtc::data_channel::DataChannel;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    register_default_interceptors, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription, Registry,
};
use webrtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};

/// The "browser" side event handler.
struct BrowserHandler {
    ice_tx: mpsc::UnboundedSender<IceCandidate>,
    state_tx: mpsc::UnboundedSender<RTCPeerConnectionState>,
    track_tx: mpsc::UnboundedSender<Arc<dyn TrackRemote>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for BrowserHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            let _ = self.ice_tx.send(IceCandidate {
                candidate: init.candidate,
                sdp_mid: init.sdp_mid,
                sdp_mline_index: init.sdp_mline_index,
                username_fragment: init.username_fragment,
            });
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.state_tx.send(state);
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.track_tx.send(track);
    }
}

fn h264_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "".into(),
                },
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "pli".into(),
                },
                RTCPFeedback {
                    typ: "ccm".into(),
                    parameter: "fir".into(),
                },
            ],
        },
        payload_type: 102,
    }
}

fn opus_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
    }
}

async fn recv_timeout<T>(rx: &mut mpsc::UnboundedReceiver<T>, what: &str) -> T {
    tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("channel closed waiting for {what}"))
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();
}

struct Options {
    video_transceivers: usize,
    audio: bool,
    config: AgentConfig,
    media: FakeMedia,
    /// Whether a UI exists that can draw annotations (false = headless service mode).
    annotations_available: bool,
    /// Also open the unordered/unreliable `input-fast` channel.
    fast_input: bool,
    /// The console granted the operator the privacy screen (`manage` permission).
    privacy_screen_allowed: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            video_transceivers: 1,
            audio: false,
            config: AgentConfig {
                max_fps: 30,
                ..AgentConfig::default()
            },
            media: FakeMedia::default(),
            annotations_available: true,
            fast_input: false,
            privacy_screen_allowed: true,
        }
    }
}

/// A connected agent session + browser peer with all three data channels open.
struct Harness {
    sessions: Arc<SessionManager>,
    browser: Arc<dyn PeerConnection>,
    session_id: String,
    hub_events: mpsc::UnboundedReceiver<AgentToConsole>,
    /// Browser-side peer connection state changes (after `Connected`).
    states: mpsc::UnboundedReceiver<RTCPeerConnectionState>,
    tracks: mpsc::UnboundedReceiver<Arc<dyn TrackRemote>>,
    control_dc: Arc<dyn DataChannel>,
    control_rx: ControlRx,
    input_dc: Arc<dyn DataChannel>,
    fast_input_dc: Option<Arc<dyn DataChannel>>,
    files_dc: Arc<dyn DataChannel>,
    files_rx: FilesRx,
    input_events: Arc<std::sync::Mutex<Vec<InputEvent>>>,
    releases: Arc<AtomicU64>,
    chat: RecordingChat,
    clipboard: FakeClipboard,
    annotations: RecordingAnnotations,
}

impl Harness {
    async fn connect(opts: Options) -> Self {
        init_tracing();
        let input_events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let releases = Arc::new(AtomicU64::new(0));
        let rec = RecordingInput {
            events: input_events.clone(),
            releases: releases.clone(),
        };
        let input_factory: remote_agent::session::InputFactory = {
            let rec = rec.clone();
            Arc::new(move || Ok(Box::new(rec.clone()) as Box<dyn InputHandler>))
        };
        let (hub, mut hub_rx) = HubSink::channel();
        let chat = RecordingChat::default();
        let clipboard = FakeClipboard::default();
        let annotations = RecordingAnnotations {
            available: opts.annotations_available,
            ..Default::default()
        };
        let deps = SessionDeps {
            media: Arc::new(opts.media),
            input: input_factory,
            approver: Arc::new(AutoApprover(
                remote_agent::approval::ApprovalOutcome::Approved,
            )),
            indicator: Arc::new(NoopIndicator),
            chat: Arc::new(chat.clone()),
            clipboard: Arc::new(clipboard.clone()),
            hub,
            config: Arc::new(RwLock::new(opts.config)),
            annotations: Arc::new(annotations.clone()),
        };
        let sessions = SessionManager::new(deps);

        // ── browser side ────────────────────────────────────────────────────────────
        let (ice_tx, mut ice_rx) = mpsc::unbounded_channel();
        let (state_tx, mut state_rx) = mpsc::unbounded_channel();
        let (track_tx, tracks) = mpsc::unbounded_channel();
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_codec(h264_codec(), RtpCodecKind::Video)
            .unwrap();
        media_engine
            .register_codec(opus_codec(), RtpCodecKind::Audio)
            .unwrap();
        let registry = register_default_interceptors(Registry::new(), &mut media_engine).unwrap();
        let browser = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(BrowserHandler {
                ice_tx,
                state_tx,
                track_tx,
            }))
            .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
            .build()
            .await
            .unwrap();
        let browser: Arc<dyn PeerConnection> = Arc::new(browser);

        // One recvonly video transceiver per display (display order), optional audio,
        // then the three data channels — all before the offer.
        for _ in 0..opts.video_transceivers {
            browser
                .add_transceiver_from_kind(
                    RtpCodecKind::Video,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
        }
        if opts.audio {
            browser
                .add_transceiver_from_kind(
                    RtpCodecKind::Audio,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
        }
        let input_dc = browser
            .create_data_channel(protocol::INPUT_CHANNEL_LABEL, None)
            .await
            .unwrap();
        let fast_input_dc = if opts.fast_input {
            Some(
                browser
                    .create_data_channel(
                        protocol::FAST_INPUT_CHANNEL_LABEL,
                        Some(rtc::data_channel::RTCDataChannelInit {
                            ordered: false,
                            max_retransmits: Some(0),
                            ..Default::default()
                        }),
                    )
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let control_dc = browser
            .create_data_channel(protocol::CONTROL_CHANNEL_LABEL, None)
            .await
            .unwrap();
        let files_dc = browser
            .create_data_channel(protocol::FILES_CHANNEL_LABEL, None)
            .await
            .unwrap();

        let offer = browser.create_offer(None).await.unwrap();
        browser.set_local_description(offer.clone()).await.unwrap();

        let session_id = format!("ses_{}", uuid::Uuid::new_v4().simple());
        sessions.start(SessionRequest {
            session_id: session_id.clone(),
            operator: OperatorInfo {
                id: "op1".into(),
                name: "Tester".into(),
            },
            offer: SessionDescription {
                kind: "offer".into(),
                sdp: offer.sdp.clone(),
            },
            ice_servers: vec![],
            role: SessionRole::Operator,
            shadow_of: None,
            notify_operator: true,
            privacy_screen_allowed: opts.privacy_screen_allowed,
        });

        // Relay agent → browser signaling; everything else goes to `hub_events`.
        let (events_tx, hub_events) = mpsc::unbounded_channel();
        let browser_relay = browser.clone();
        let (answer_done_tx, mut answer_done_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(msg) = hub_rx.recv().await {
                match msg {
                    AgentToConsole::SessionAnswer { answer, .. } => {
                        let desc = RTCSessionDescription::answer(answer.sdp).unwrap();
                        browser_relay.set_remote_description(desc).await.unwrap();
                        let _ = answer_done_tx.send(());
                    }
                    AgentToConsole::IceCandidate { candidate, .. } => {
                        let _ = browser_relay
                            .add_ice_candidate(RTCIceCandidateInit {
                                candidate: candidate.candidate,
                                sdp_mid: candidate.sdp_mid,
                                sdp_mline_index: candidate.sdp_mline_index,
                                username_fragment: candidate.username_fragment,
                                url: None,
                            })
                            .await;
                    }
                    other => {
                        let _ = events_tx.send(other);
                    }
                }
            }
        });
        let sessions_ice = sessions.clone();
        let sid2 = session_id.clone();
        tokio::spawn(async move {
            while let Some(c) = ice_rx.recv().await {
                sessions_ice.add_ice_candidate(&sid2, c);
            }
        });

        recv_timeout(&mut answer_done_rx, "session answer").await;
        let mut connected = false;
        for _ in 0..40 {
            let state = recv_timeout(&mut state_rx, "connection state").await;
            if state == RTCPeerConnectionState::Connected {
                connected = true;
                break;
            }
            if matches!(
                state,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
            ) {
                panic!("browser connection failed: {state:?}");
            }
        }
        assert!(connected, "browser never reached Connected");

        let control_rx = read_control(control_dc.clone()).await;
        let files_rx = read_files(files_dc.clone()).await;
        // input channel open
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(webrtc::data_channel::DataChannelEvent::OnOpen) = input_dc.poll().await
                {
                    break;
                }
            }
        })
        .await
        .expect("input channel never opened");

        if let Some(dc) = &fast_input_dc {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(webrtc::data_channel::DataChannelEvent::OnOpen) = dc.poll().await {
                        break;
                    }
                }
            })
            .await
            .expect("input-fast channel never opened");
        }

        Self {
            sessions,
            browser,
            session_id,
            hub_events,
            states: state_rx,
            tracks,
            control_dc,
            control_rx,
            input_dc,
            fast_input_dc,
            files_dc,
            files_rx,
            input_events,
            releases,
            chat,
            clipboard,
            annotations,
        }
    }

    async fn control(&self, msg: &ControlMessage) {
        send_bytes(&self.control_dc, &serde_json::to_vec(msg).unwrap()).await;
    }

    async fn files(&self, msg: &FileMessage) {
        send_json(&self.files_dc, msg).await;
    }

    /// Next session event matching `pred`.
    async fn event(&mut self, mut pred: impl FnMut(&SessionEvent) -> bool) -> SessionEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let msg = tokio::time::timeout_at(deadline, self.hub_events.recv())
                .await
                .expect("timed out waiting for session event")
                .expect("hub closed");
            if let AgentToConsole::SessionEvent { event, .. } = msg {
                if pred(&event) {
                    return event;
                }
            }
        }
    }

    async fn end(self) {
        self.sessions
            .end(&self.session_id, EndReason::OperatorClosed);
        self.sessions.wait_idle(Duration::from_secs(10)).await;
        assert!(self.sessions.active_session_id().is_none());
        self.browser.close().await.ok();
    }
}

async fn wait_rtp(track: &Arc<dyn TrackRemote>) -> bool {
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(evt) = track.poll().await {
            if let TrackRemoteEvent::OnRtpPacket(_) = evt {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

fn sha(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn chunk_frame(id: u32, offset: u64, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0u8; CHUNK_HEADER_LEN];
    encode_chunk_header(id, offset, &mut f);
    f.extend_from_slice(payload);
    f
}

/// Upload `data` from `offset` in chunks; returns after all chunks were handed to SCTP.
async fn send_chunks(h: &Harness, id: u32, data: &[u8], mut offset: u64, until: u64) {
    while offset < until {
        let end = ((offset as usize) + MAX_CHUNK_BYTES - CHUNK_HEADER_LEN).min(until as usize);
        send_bytes(
            &h.files_dc,
            &chunk_frame(id, offset, &data[offset as usize..end]),
        )
        .await;
        offset = end as u64;
    }
}

fn synthetic(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32)
        .map(|i| (i.wrapping_mul(2654435761).wrapping_add(seed) >> 13) as u8)
        .collect()
}

// ───────────────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_display_binding_and_input() {
    let mut h = Harness::connect(Options {
        video_transceivers: 2,
        ..Default::default()
    })
    .await;

    // display_info: two displays, only the primary streams by default.
    let info = next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;
    // JSON control messages must travel as text frames (a browser gets binary as a Blob).
    assert_eq!(
        BINARY_CONTROL_FRAMES.load(Ordering::SeqCst),
        0,
        "agent sent JSON on the control channel as binary frames"
    );
    let ControlMessage::DisplayInfo {
        displays,
        active,
        current,
        audio,
    } = info
    else {
        unreachable!()
    };
    assert_eq!(displays.len(), 2);
    assert_eq!(active, vec![0]);
    assert_eq!(current, 0);
    assert!(!audio);

    // Enable the second display, then both tracks surface (`on_track` fires once RTP
    // flows on a track). Bound in m-line order: track i is "screen-i".
    h.control(&ControlMessage::SetActiveDisplays {
        indices: vec![0, 1],
    })
    .await;
    let info = next_control(
        &mut h.control_rx,
        |m| matches!(m, ControlMessage::DisplayInfo { active, .. } if active.len() == 2),
    )
    .await;
    assert!(matches!(info, ControlMessage::DisplayInfo { active, .. } if active == vec![0, 1]));
    let t0 = recv_timeout(&mut h.tracks, "track 0").await;
    let t1 = recv_timeout(&mut h.tracks, "track 1").await;
    let mut ids = vec![t0.track_id().await, t1.track_id().await];
    ids.sort();
    assert_eq!(ids, vec!["screen-0".to_string(), "screen-1".to_string()]);
    // m-line order equals display order: the transceiver with mid i carries "screen-i"
    // (mids are assigned in m-line order by the offerer; `get_transceivers` order is not
    // guaranteed, so key by mid).
    let transceivers = h.browser.get_transceivers().await;
    let mut video_i = 0;
    for t in &transceivers {
        let Ok(Some(r)) = t.receiver().await else {
            continue;
        };
        if r.track().kind().await != RtpCodecKind::Video {
            continue;
        }
        let mid = t
            .mid()
            .await
            .ok()
            .flatten()
            .expect("video transceiver has a mid");
        let mline: usize = mid.parse().expect("numeric mid");
        assert_eq!(r.track().track_id().await, format!("screen-{mline}"));
        video_i += 1;
    }
    assert_eq!(video_i, 2);
    assert!(wait_rtp(&t0).await, "no RTP on first track");
    assert!(wait_rtp(&t1).await, "no RTP on second track");
    let ev = h
        .event(|e| matches!(e, SessionEvent::DisplaysChanged { .. }))
        .await;
    assert_eq!(ev, SessionEvent::DisplaysChanged { active: vec![0, 1] });
    // Per-display stats.
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::Stats { display: 1, .. })
    })
    .await;

    // select_display on a streaming tile only moves the pointer target…
    h.control(&ControlMessage::SelectDisplay { index: 1 }).await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { current: 1, active, .. } if active == &vec![0, 1])
    })
    .await;
    // …while on a non-streaming display it switches the stream (single-tile semantics).
    h.control(&ControlMessage::SetActiveDisplays { indices: vec![0] })
        .await;
    next_control(
        &mut h.control_rx,
        |m| matches!(m, ControlMessage::DisplayInfo { active, .. } if active == &vec![0]),
    )
    .await;
    h.control(&ControlMessage::SelectDisplay { index: 1 }).await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { current: 1, active, .. } if active == &vec![1])
    })
    .await;

    // Input reaches the injector.
    let ev = InputEvent::MouseDown {
        button: MouseButton::Left,
    };
    send_bytes(&h.input_dc, &serde_json::to_vec(&ev).unwrap()).await;
    let got = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if h.input_events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, InputEvent::MouseDown { .. }))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(got, "input event never reached the injector");

    let releases = h.releases.clone();
    h.end().await;
    assert!(releases.load(Ordering::SeqCst) >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_track_negotiation() {
    let mut h = Harness::connect(Options {
        video_transceivers: 1,
        audio: true,
        ..Default::default()
    })
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { audio: false, .. })
    })
    .await;
    h.control(&ControlMessage::SetAudio { enabled: true }).await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { audio: true, .. })
    })
    .await;
    assert_eq!(
        h.event(|e| matches!(e, SessionEvent::AudioChanged { .. }))
            .await,
        SessionEvent::AudioChanged { enabled: true }
    );
    // Tracks surface once RTP flows; find the audio one.
    let mut audio_track = None;
    for _ in 0..2 {
        let t = recv_timeout(&mut h.tracks, "track").await;
        if t.kind().await == RtpCodecKind::Audio {
            audio_track = Some(t);
            break;
        }
    }
    let audio_track = audio_track.expect("no audio track negotiated");
    assert_eq!(audio_track.track_id().await, "system-audio");
    assert!(wait_rtp(&audio_track).await, "no Opus RTP received");

    h.control(&ControlMessage::SetAudio { enabled: false })
        .await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { audio: false, .. })
    })
    .await;
    h.end().await;
}

/// The default single-display session must push `Stats { display: 0 }` on the control channel
/// (the browser overlay depends on it). Regression guard: stats used to be gated behind an
/// unmet condition after the multi-display refactor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_arrive_for_each_active_display() {
    let mut h = Harness::connect(Options::default()).await;
    // Default: only display 0 streams; a Stats frame for it arrives within ~3 s.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        next_control(&mut h.control_rx, |m| {
            matches!(m, ControlMessage::Stats { display: 0, .. })
        })
        .await
    })
    .await
    .expect("no Stats{display:0} within 3s");
    h.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_upload_resumes_across_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let config = AgentConfig {
        max_fps: 30,
        transfer_dir: Some(dir.display().to_string()),
        ..AgentConfig::default()
    };
    let data = synthetic(5 * 1024 * 1024, 7);
    let token = "upload-token-1".to_string();

    // ── session 1: send about half, then drop the session ─────────────────────
    let mut h = Harness::connect(Options {
        config: config.clone(),
        ..Default::default()
    })
    .await;
    h.files(&FileMessage::Offer {
        transfer_id: 1,
        token: token.clone(),
        name: "big.bin".into(),
        size: data.len() as u64,
        kind: TransferKind::File,
        direction: TransferDirection::ToDevice,
        dest_dir: None,
        group: None,
        sha256: None,
    })
    .await;
    let mut chunks = Vec::new();
    let accept = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Accept { .. })
    })
    .await;
    assert!(matches!(
        accept,
        FileMessage::Accept {
            transfer_id: 1,
            offset: 0,
            codecs: Some(_),
        }
    ));
    assert!(matches!(
        h.event(|e| matches!(e, SessionEvent::TransferStarted { .. }))
            .await,
        SessionEvent::TransferStarted {
            offset: 0,
            direction: TransferDirection::ToDevice,
            ..
        }
    ));
    let half = 2_600_000u64;
    send_chunks(&h, 1, &data, 0, half).await;
    // Wait until the agent acked at least 2 MiB so we know bytes landed on disk.
    let ack = next_files_msg(
        &mut h.files_rx,
        &mut chunks,
        |m| matches!(m, FileMessage::Ack { offset, .. } if *offset >= 2 * 1024 * 1024),
    )
    .await;
    let FileMessage::Ack { offset: acked, .. } = ack else {
        unreachable!()
    };
    h.end().await;

    let part = dir.join("big.bin.part");
    assert!(part.exists(), "partial kept after the session dropped");
    let sidecar = remote_agent::transfer::sidecar::Sidecar::read(&part).unwrap();
    assert!(sidecar.received >= acked);
    assert_eq!(sidecar.token, token);

    // ── session 2: re-offer with the same token → resume ──────────────────────
    let mut h = Harness::connect(Options {
        config,
        ..Default::default()
    })
    .await;
    h.files(&FileMessage::Offer {
        transfer_id: 3,
        token: token.clone(),
        name: "big.bin".into(),
        size: data.len() as u64,
        kind: TransferKind::File,
        direction: TransferDirection::ToDevice,
        dest_dir: None,
        group: None,
        sha256: None,
    })
    .await;
    let accept = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Accept { .. })
    })
    .await;
    let FileMessage::Accept { offset: resume, .. } = accept else {
        unreachable!()
    };
    assert!(
        resume >= acked && resume <= half,
        "resume offset {resume} (acked {acked})"
    );
    assert!(matches!(
        h.event(|e| matches!(e, SessionEvent::TransferStarted { .. })).await,
        SessionEvent::TransferStarted { offset, .. } if offset == resume
    ));
    send_chunks(&h, 3, &data, resume, data.len() as u64).await;
    h.files(&FileMessage::Complete {
        transfer_id: 3,
        sha256: sha(&data),
    })
    .await;
    let done = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Done { .. })
    })
    .await;
    let FileMessage::Done {
        ok, path, error, ..
    } = done
    else {
        unreachable!()
    };
    assert!(ok, "upload failed: {error:?}");
    let final_path = PathBuf::from(path.unwrap());
    assert_eq!(final_path, dir.join("big.bin"));
    assert_eq!(sha(&std::fs::read(&final_path).unwrap()), sha(&data));
    assert!(!part.exists());
    assert!(matches!(
        h.event(|e| matches!(e, SessionEvent::TransferCompleted { .. }))
            .await,
        SessionEvent::TransferCompleted {
            direction: TransferDirection::ToDevice,
            ..
        }
    ));
    h.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_download_with_resume_and_browser_ops() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let data = synthetic(3 * 1024 * 1024 + 123, 11);
    let src = dir.join("report.bin");
    std::fs::write(&src, &data).unwrap();
    let config = AgentConfig {
        max_fps: 30,
        transfer_dir: Some(dir.display().to_string()),
        ..AgentConfig::default()
    };
    let mut h = Harness::connect(Options {
        config,
        ..Default::default()
    })
    .await;
    let mut chunks = Vec::new();

    // ── remote browser ───────────────────────────────────────────────────────────
    h.files(&FileMessage::List { path: None }).await;
    let roots = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Listing { .. })
    })
    .await;
    let FileMessage::Listing { entries, error, .. } = roots else {
        unreachable!()
    };
    assert!(error.is_none());
    assert!(entries
        .iter()
        .any(|e| e.name == "Transfers" && e.path.is_some()));

    h.files(&FileMessage::List {
        path: Some(dir.display().to_string()),
    })
    .await;
    let listing = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Listing { .. })
    })
    .await;
    let FileMessage::Listing { entries, .. } = listing else {
        unreachable!()
    };
    assert!(entries
        .iter()
        .any(|e| e.name == "report.bin" && e.size == data.len() as u64));

    h.files(&FileMessage::List {
        path: Some("../relative".into()),
    })
    .await;
    let bad = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Listing { .. })
    })
    .await;
    assert!(matches!(bad, FileMessage::Listing { error: Some(_), .. }));

    let sub = dir.join("made");
    h.files(&FileMessage::Mkdir {
        path: sub.display().to_string(),
    })
    .await;
    assert!(matches!(
        next_files_msg(&mut h.files_rx, &mut chunks, |m| matches!(
            m,
            FileMessage::OpResult { .. }
        ))
        .await,
        FileMessage::OpResult { ok: true, .. }
    ));
    assert!(sub.is_dir());
    let renamed = dir.join("renamed");
    h.files(&FileMessage::Rename {
        from: sub.display().to_string(),
        to: renamed.display().to_string(),
    })
    .await;
    assert!(matches!(
        next_files_msg(&mut h.files_rx, &mut chunks, |m| matches!(
            m,
            FileMessage::OpResult { .. }
        ))
        .await,
        FileMessage::OpResult { ok: true, .. }
    ));
    h.files(&FileMessage::Delete {
        path: renamed.display().to_string(),
    })
    .await;
    assert!(matches!(
        next_files_msg(&mut h.files_rx, &mut chunks, |m| matches!(
            m,
            FileMessage::OpResult { .. }
        ))
        .await,
        FileMessage::OpResult { ok: true, .. }
    ));
    assert!(!renamed.exists());
    h.files(&FileMessage::Request {
        transfer_id: 5,
        path: "relative/path".into(),
    })
    .await;
    assert!(matches!(
        next_files_msg(&mut h.files_rx, &mut chunks, |m| matches!(
            m,
            FileMessage::Reject { .. }
        ))
        .await,
        FileMessage::Reject { transfer_id: 5, .. }
    ));

    // ── download, resumed from 1 MiB ─────────────────────────────────────────────
    h.files(&FileMessage::Request {
        transfer_id: 7,
        path: src.display().to_string(),
    })
    .await;
    let offer = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Offer { .. })
    })
    .await;
    let FileMessage::Offer {
        transfer_id,
        size,
        name,
        direction,
        ..
    } = offer
    else {
        unreachable!()
    };
    assert_eq!(transfer_id, 7, "offer reuses the request id");
    assert_eq!(size, data.len() as u64);
    assert_eq!(name, "report.bin");
    assert_eq!(direction, TransferDirection::ToOperator);
    let resume = 1024 * 1024u64;
    h.files(&FileMessage::Accept {
        transfer_id: 7,
        offset: resume,
        codecs: None,
    })
    .await;
    let complete = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Complete { .. })
    })
    .await;
    let FileMessage::Complete { sha256, .. } = complete else {
        unreachable!()
    };
    assert_eq!(sha256, sha(&data));
    let mut out = vec![0u8; data.len()];
    let mut min_off = u64::MAX;
    for c in &chunks {
        let (id, off, payload) = decode_chunk(c).unwrap();
        assert_eq!(id, 7);
        assert!(payload.len() <= MAX_CHUNK_BYTES);
        min_off = min_off.min(off);
        out[off as usize..off as usize + payload.len()].copy_from_slice(payload);
    }
    assert_eq!(min_off, resume);
    assert_eq!(&out[resume as usize..], &data[resume as usize..]);
    h.files(&FileMessage::Done {
        transfer_id: 7,
        ok: true,
        error: None,
        path: None,
    })
    .await;
    assert!(matches!(
        h.event(|e| matches!(e, SessionEvent::TransferCompleted { .. }))
            .await,
        SessionEvent::TransferCompleted {
            direction: TransferDirection::ToOperator,
            ..
        }
    ));
    h.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_image_both_directions_and_chat() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let config = AgentConfig {
        max_fps: 30,
        transfer_dir: Some(dir.display().to_string()),
        ..AgentConfig::default()
    };
    let mut h = Harness::connect(Options {
        config,
        ..Default::default()
    })
    .await;
    let mut chunks = Vec::new();

    // ── device → operator: clipboard image ───────────────────────────────────────
    let rgba: Vec<u8> = (0..32 * 16)
        .flat_map(|i| [i as u8, 0x80, 0x20, 0xff])
        .collect();
    let png = remote_agent::clipboard::encode_png(&arboard::ImageData {
        width: 32,
        height: 16,
        bytes: rgba.clone().into(),
    })
    .unwrap();
    h.clipboard.inject(ClipboardContent::Image {
        png: png.clone(),
        width: 32,
        height: 16,
    });
    let avail = next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::ClipboardAvailable { .. })
    })
    .await;
    assert!(matches!(
        avail,
        ControlMessage::ClipboardAvailable {
            kind: ClipboardKind::Image,
            ..
        }
    ));
    h.files(&FileMessage::RequestClipboard).await;
    let offer = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Offer { .. })
    })
    .await;
    let FileMessage::Offer {
        transfer_id,
        kind,
        size,
        ..
    } = offer
    else {
        unreachable!()
    };
    assert_eq!(kind, TransferKind::ClipboardImage);
    assert_eq!(size, png.len() as u64);
    assert_eq!(transfer_id % 2, 0, "agent-initiated offers use even ids");
    h.files(&FileMessage::Accept {
        transfer_id,
        offset: 0,
        codecs: None,
    })
    .await;
    let complete = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Complete { .. })
    })
    .await;
    let FileMessage::Complete { sha256, .. } = complete else {
        unreachable!()
    };
    let mut received = vec![0u8; png.len()];
    for c in &chunks {
        let (_, off, payload) = decode_chunk(c).unwrap();
        received[off as usize..off as usize + payload.len()].copy_from_slice(payload);
    }
    assert_eq!(sha(&received), sha256);
    assert_eq!(received, png);
    h.files(&FileMessage::Done {
        transfer_id,
        ok: true,
        error: None,
        path: None,
    })
    .await;
    assert!(matches!(
        h.event(|e| matches!(e, SessionEvent::ClipboardSync { .. }))
            .await,
        SessionEvent::ClipboardSync {
            direction: TransferDirection::ToOperator,
            ..
        }
    ));

    // ── operator → device: clipboard image ───────────────────────────────────────
    chunks.clear();
    h.files(&FileMessage::Offer {
        transfer_id: 9,
        token: "clip-1".into(),
        name: "paste.png".into(),
        size: png.len() as u64,
        kind: TransferKind::ClipboardImage,
        direction: TransferDirection::ToDevice,
        dest_dir: None,
        group: None,
        sha256: None,
    })
    .await;
    next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Accept { transfer_id: 9, .. })
    })
    .await;
    send_chunks(&h, 9, &png, 0, png.len() as u64).await;
    h.files(&FileMessage::Complete {
        transfer_id: 9,
        sha256: sha(&png),
    })
    .await;
    let done = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(m, FileMessage::Done { transfer_id: 9, .. })
    })
    .await;
    assert!(matches!(done, FileMessage::Done { ok: true, .. }));
    assert!(matches!(
        h.event(|e| matches!(e, SessionEvent::ClipboardSync { direction: TransferDirection::ToDevice, .. })).await,
        SessionEvent::ClipboardSync { summary, .. } if summary.contains("32×16")
    ));
    let placed = h.clipboard.images.lock().unwrap().clone();
    assert_eq!(placed.len(), 1);
    assert!(placed[0].starts_with(dir.join("Clipboard")));

    // ── clipboard text still works and is not echoed ─────────────────────────────
    h.control(&ControlMessage::ClipboardSet {
        text: "hello".into(),
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if h.clipboard
                .texts
                .lock()
                .unwrap()
                .contains(&"hello".to_string())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("clipboard text not placed");

    // ── chat ─────────────────────────────────────────────────────────────────────
    h.control(&ControlMessage::Chat {
        from: ChatParty::Operator,
        text: "  Hello there  ".into(),
        ts_ms: 1234,
    })
    .await;
    assert_eq!(
        h.event(|e| matches!(e, SessionEvent::Chat { .. })).await,
        SessionEvent::Chat {
            from: ChatParty::Operator,
            text: "Hello there".into()
        }
    );
    let lines = h.chat.lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "Hello there");
    assert!(
        h.chat
            .visible
            .lock()
            .unwrap()
            .last()
            .copied()
            .unwrap_or(false),
        "chat window shown"
    );
    h.chat.type_line("hi back");
    let reply = next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::Chat { .. })
    })
    .await;
    assert!(
        matches!(reply, ControlMessage::Chat { from: ChatParty::Device, text, .. } if text == "hi back")
    );
    assert_eq!(
        h.event(|e| matches!(
            e,
            SessionEvent::Chat {
                from: ChatParty::Device,
                ..
            }
        ))
        .await,
        SessionEvent::Chat {
            from: ChatParty::Device,
            text: "hi back".into()
        }
    );
    h.end().await;
}

/// Help-me mode: a denied approval ends the session before any peer connection is built.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn help_me_denial_ends_session() {
    let (hub, mut hub_rx) = HubSink::channel();
    let calls = Arc::new(AtomicU64::new(0));
    let deps = SessionDeps {
        media: Arc::new(FakeMedia::default()),
        input: Arc::new(|| panic!("input must not be created when denied")),
        approver: Arc::new(RecordingApprover {
            outcome: remote_agent::approval::ApprovalOutcome::Denied,
            calls: calls.clone(),
        }),
        indicator: Arc::new(NoopIndicator),
        chat: Arc::new(RecordingChat::default()),
        clipboard: Arc::new(FakeClipboard::default()),
        hub,
        annotations: Arc::new(remote_agent::annotate::NoAnnotations),
        config: Arc::new(RwLock::new(AgentConfig {
            mode: DeviceMode::HelpMe,
            ..AgentConfig::default()
        })),
    };
    let sessions = SessionManager::new(deps);
    sessions.start(SessionRequest {
        session_id: "ses_deny".into(),
        operator: OperatorInfo {
            id: "op".into(),
            name: "Nope".into(),
        },
        offer: SessionDescription {
            kind: "offer".into(),
            sdp: "v=0\r\n".into(),
        },
        ice_servers: vec![],
        role: SessionRole::Operator,
        shadow_of: None,
        notify_operator: true,
        privacy_screen_allowed: true,
    });

    let mut saw_awaiting = false;
    let mut saw_denied_result = false;
    let mut saw_ended = false;
    for _ in 0..10 {
        let msg = tokio::time::timeout(Duration::from_secs(5), hub_rx.recv())
            .await
            .expect("timed out")
            .expect("hub closed");
        match msg {
            AgentToConsole::SessionState {
                state: SessionState::AwaitingApproval,
                ..
            } => saw_awaiting = true,
            AgentToConsole::ApprovalResult { approved, .. } => saw_denied_result = !approved,
            AgentToConsole::SessionState {
                state: SessionState::Ended,
                reason,
                ..
            } => {
                assert_eq!(reason, Some(EndReason::Denied));
                saw_ended = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_awaiting, "no awaiting_approval state");
    assert!(saw_denied_result, "no denied approval_result");
    assert!(saw_ended, "session never ended");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[allow(dead_code)]
fn _keep(_: &Path, _: Bytes) {}

/// The person at the device blocks keyboard/mouse mid-session: further injected input is
/// dropped and the browser gets a control-channel notice (a fresh `display_info`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_override_blocks_input_live() {
    use protocol::config::AgentConfig;

    let mut h = Harness::connect(Options {
        config: AgentConfig {
            max_fps: 30,
            allow_input: true,
            ..AgentConfig::default()
        },
        ..Default::default()
    })
    .await;

    // Wait for the input channel to be wired (first display_info means the session is up).
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;

    // Baseline: a mouse-down reaches the injector.
    send_bytes(
        &h.input_dc,
        &serde_json::to_vec(&InputEvent::MouseDown {
            button: MouseButton::Left,
        })
        .unwrap(),
    )
    .await;
    let reached = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !h.input_events.lock().unwrap().is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(reached, "input should reach the injector before the block");

    // The local user blocks input; the effective config now forbids it.
    h.sessions.apply_overrides(AgentConfig {
        max_fps: 30,
        allow_input: false,
        ..AgentConfig::default()
    });
    // Notice to the browser: a fresh display_info arrives after the policy change.
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;

    let before = h.input_events.lock().unwrap().len();
    for _ in 0..5 {
        send_bytes(
            &h.input_dc,
            &serde_json::to_vec(&InputEvent::MouseDown {
                button: MouseButton::Right,
            })
            .unwrap(),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = h.input_events.lock().unwrap().len();
    assert_eq!(
        before, after,
        "input injected after the local block must be dropped ({before} -> {after})"
    );

    h.end().await;
}

/// The session bar's emergency switch: pausing drops remote input immediately, tells the
/// browser (`control_paused`) and the console (`SessionEvent::ControlPaused`); only the device
/// side resumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_pause_control_blocks_input_and_notifies() {
    use protocol::agent::SessionEvent;
    use protocol::config::AgentConfig;

    let mut h = Harness::connect(Options {
        config: AgentConfig {
            max_fps: 30,
            allow_input: true,
            ..AgentConfig::default()
        },
        ..Default::default()
    })
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;

    // Baseline: input flows.
    send_bytes(
        &h.input_dc,
        &serde_json::to_vec(&InputEvent::MouseDown {
            button: MouseButton::Left,
        })
        .unwrap(),
    )
    .await;
    let reached = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !h.input_events.lock().unwrap().is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(reached, "input should reach the injector before the pause");

    // Device user hits "Pause control".
    let releases_before = h.releases.load(Ordering::SeqCst);
    h.sessions.set_control_paused(true);
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::ControlPaused { paused: true })
    })
    .await;
    let ev = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match h.hub_events.recv().await {
                Some(AgentToConsole::SessionEvent {
                    event: SessionEvent::ControlPaused { paused: true },
                    ..
                }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        ev,
        "console must receive SessionEvent::ControlPaused {{ paused: true }}"
    );
    assert!(
        h.releases.load(Ordering::SeqCst) > releases_before,
        "pausing must release held keys/buttons"
    );

    let before = h.input_events.lock().unwrap().len();
    for _ in 0..5 {
        send_bytes(
            &h.input_dc,
            &serde_json::to_vec(&InputEvent::MouseDown {
                button: MouseButton::Right,
            })
            .unwrap(),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        before,
        h.input_events.lock().unwrap().len(),
        "input injected while paused must be dropped"
    );

    // Nothing the operator sends lifts the pause (control messages keep working, input stays off).
    send_json(&h.control_dc, &ControlMessage::RequestKeyframe).await;
    send_json(
        &h.control_dc,
        &ControlMessage::SetQuality {
            max_fps: Some(15),
            max_bitrate_kbps: None,
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    send_bytes(
        &h.input_dc,
        &serde_json::to_vec(&InputEvent::MouseDown {
            button: MouseButton::Middle,
        })
        .unwrap(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(before, h.input_events.lock().unwrap().len());

    // Only the device user resumes.
    h.sessions.set_control_paused(false);
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::ControlPaused { paused: false })
    })
    .await;
    send_bytes(
        &h.input_dc,
        &serde_json::to_vec(&InputEvent::MouseDown {
            button: MouseButton::Left,
        })
        .unwrap(),
    )
    .await;
    let resumed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if h.input_events.lock().unwrap().len() > before {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(resumed, "input must flow again after resume");

    h.end().await;
}

/// "End session" on the window / session bar ends the session locally and reports it as
/// closed by the device user.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_end_button_ends_session() {
    let mut h = Harness::connect(Options::default()).await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;

    h.chat.press_disconnect();

    // The console must learn about the end *before* the peer connection goes away, and the
    // operator must get the `session_ended_by_user` notice over the still-open control channel.
    let mut ended: Option<EndReason> = None;
    let mut closed_first = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while ended.is_none() {
        tokio::select! {
            msg = h.hub_events.recv() => match msg {
                Some(AgentToConsole::SessionState { state: SessionState::Ended, reason, .. }) => ended = reason,
                Some(_) => {}
                None => break,
            },
            st = h.states.recv() => {
                if matches!(st, Some(RTCPeerConnectionState::Closed | RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Failed)) {
                    closed_first = true;
                    break;
                }
            }
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
    assert!(
        !closed_first,
        "peer connection closed before the console was told"
    );
    assert_eq!(ended, Some(EndReason::DeviceUserClosed));
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::SessionEndedByUser)
    })
    .await;
    // The session loop exits on its own (UserEnd), without waiting for the console or peer.
    let started = std::time::Instant::now();
    h.sessions.wait_idle(Duration::from_secs(10)).await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "session task must finish promptly after the device user ended it"
    );
}

/// Annotations flow to the overlay sink independently of the input permission and of the
/// device-side pause; the session removes overlays when it ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn annotations_reach_the_overlay_even_while_control_is_paused() {
    use remote_agent::annotate::AnnotateEvent;

    let mut h = Harness::connect(Options {
        config: AgentConfig {
            max_fps: 30,
            allow_input: false, // guidance must work without input permission
            ..AgentConfig::default()
        },
        ..Default::default()
    })
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;
    // …and while the device user paused control.
    h.sessions.set_control_paused(true);
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::ControlPaused { paused: true })
    })
    .await;

    send_json(
        &h.control_dc,
        &ControlMessage::AnnotateStroke {
            id: 7,
            display: 0,
            color: "#ff0000".into(),
            width: 6.0,
            points: vec![(10.0, 10.0), (20.0, 25.0)],
        },
    )
    .await;
    send_json(
        &h.control_dc,
        &ControlMessage::AnnotateStroke {
            id: 7,
            display: 0,
            color: "#ff0000".into(),
            width: 6.0,
            points: vec![(30.0, 40.0)],
        },
    )
    .await;
    send_json(&h.control_dc, &ControlMessage::AnnotateEnd { id: 7 }).await;
    send_json(
        &h.control_dc,
        &ControlMessage::AnnotatePointer {
            display: 0,
            point: Some((100.0, 120.0)),
            color: "#00ff00".into(),
        },
    )
    .await;
    send_json(&h.control_dc, &ControlMessage::AnnotateClear).await;

    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if h.annotations.events.lock().unwrap().len() >= 5 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(got, "all five annotation messages must reach the sink");
    let events = h.annotations.events.lock().unwrap().clone();
    assert_eq!(
        events[0],
        AnnotateEvent::Stroke {
            id: 7,
            display: 0,
            color: "#ff0000".into(),
            width: 6.0,
            points: vec![(10.0, 10.0), (20.0, 25.0)],
        }
    );
    assert!(
        matches!(&events[1], AnnotateEvent::Stroke { id: 7, points, .. } if points == &vec![(30.0, 40.0)])
    );
    assert_eq!(events[2], AnnotateEvent::End { id: 7 });
    assert!(matches!(
        &events[3],
        AnnotateEvent::Pointer { display: 0, point: Some((x, y)), .. } if *x == 100.0 && *y == 120.0
    ));
    assert_eq!(events[4], AnnotateEvent::Clear);

    // The session ends → overlays are removed.
    h.sessions
        .end_all(protocol::common::EndReason::OperatorClosed);
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if h.annotations.ended.load(Ordering::SeqCst) > 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(ended, "session teardown must remove the overlays");
}

/// Locally disabled (or no UI): the operator is told once, nothing reaches the sink.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn annotations_disabled_locally_reply_once() {
    let mut h = Harness::connect(Options::default()).await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;
    // Device user blocks annotations (restrict-only override applied live).
    h.sessions.apply_overrides(AgentConfig {
        max_fps: 30,
        allow_annotations: false,
        ..AgentConfig::default()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    for i in 0..3u32 {
        send_json(
            &h.control_dc,
            &ControlMessage::AnnotateStroke {
                id: i,
                display: 0,
                color: "#ff0000".into(),
                width: 4.0,
                points: vec![(1.0, 1.0)],
            },
        )
        .await;
    }
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::AnnotationsDisabled)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        h.annotations.events.lock().unwrap().is_empty(),
        "nothing may reach the overlay while annotations are disabled"
    );
    // Only one AnnotationsDisabled per session even for repeated attempts.
    let extra = tokio::time::timeout(Duration::from_millis(500), async {
        next_control(&mut h.control_rx, |m| {
            matches!(m, ControlMessage::AnnotationsDisabled)
        })
        .await
    })
    .await;
    assert!(
        extra.is_err(),
        "AnnotationsDisabled must be sent once per session"
    );
}

/// Headless service mode has no overlay: the operator is told.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn annotations_without_ui_reply_disabled() {
    let mut h = Harness::connect(Options {
        annotations_available: false,
        ..Default::default()
    })
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;
    send_json(
        &h.control_dc,
        &ControlMessage::AnnotatePointer {
            display: 0,
            point: Some((5.0, 5.0)),
            color: "#ff0000".into(),
        },
    )
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::AnnotationsDisabled)
    })
    .await;
    assert!(h.annotations.events.lock().unwrap().is_empty());
}

// ── performance pass ────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_viewport_scales_the_picture_and_forces_a_keyframe() {
    let mut h = Harness::connect(Options::default()).await;
    // Full size first (test displays are 320×240).
    let full = next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::Stats { display: 0, .. })
    })
    .await;
    let ControlMessage::Stats {
        encoded_width: w0,
        encoded_height: h0,
        ..
    } = full
    else {
        unreachable!()
    };
    assert_eq!((w0, h0), (320, 240));

    h.control(&ControlMessage::SetViewport {
        display: 0,
        width: Some(160),
        height: Some(160),
    })
    .await;
    // Debounced (250 ms) encoder rebuild → next stats report the capped size and a keyframe.
    let scaled = tokio::time::timeout(Duration::from_secs(6), async {
        next_control(&mut h.control_rx, |m| {
            matches!(
                m,
                ControlMessage::Stats {
                    display: 0,
                    encoded_width: 160,
                    ..
                }
            )
        })
        .await
    })
    .await
    .expect("stats never reported the viewport-scaled size");
    let ControlMessage::Stats {
        encoded_height,
        keyframes,
        ..
    } = scaled
    else {
        unreachable!()
    };
    assert_eq!(encoded_height, 120, "aspect kept");
    assert!(
        keyframes >= 1,
        "viewport change forces a keyframe (got {keyframes})"
    );

    // A viewport larger than the display means full resolution again.
    h.control(&ControlMessage::SetViewport {
        display: 0,
        width: Some(4000),
        height: Some(4000),
    })
    .await;
    tokio::time::timeout(Duration::from_secs(6), async {
        next_control(&mut h.control_rx, |m| {
            matches!(
                m,
                ControlMessage::Stats {
                    display: 0,
                    encoded_width: 320,
                    ..
                }
            )
        })
        .await
    })
    .await
    .expect("stats never returned to full size");
    h.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cursor_shape_and_positions_arrive_on_the_control_channel() {
    let mut h = Harness::connect(Options {
        media: FakeMedia {
            cursor: true,
            ..FakeMedia::default()
        },
        ..Options::default()
    })
    .await;
    let shape = tokio::time::timeout(Duration::from_secs(5), async {
        next_control(&mut h.control_rx, |m| {
            matches!(m, ControlMessage::CursorShape { .. })
        })
        .await
    })
    .await
    .expect("no cursor_shape");
    let ControlMessage::CursorShape {
        id,
        width,
        height,
        png_base64,
        ..
    } = shape
    else {
        unreachable!()
    };
    assert_eq!((id, width, height), (7, 8, 8));
    assert!(!png_base64.is_empty());
    let mut seen = 0;
    while seen < 3 {
        let m = tokio::time::timeout(Duration::from_secs(5), async {
            next_control(&mut h.control_rx, |m| {
                matches!(
                    m,
                    ControlMessage::CursorPosition {
                        display: 0,
                        shape_id: 7,
                        visible: true,
                        ..
                    }
                )
            })
            .await
        })
        .await
        .expect("no cursor_position");
        if let ControlMessage::CursorPosition { x, y, .. } = m {
            assert!(x > 0 && y > 0);
            seen += 1;
        }
    }
    h.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fast_input_channel_moves_are_applied_and_flushed_before_clicks() {
    let h = Harness::connect(Options {
        fast_input: true,
        ..Options::default()
    })
    .await;
    let fast = h.fast_input_dc.clone().expect("fast channel");
    for i in 1..=5 {
        send_bytes(
            &fast,
            &serde_json::to_vec(&InputEvent::MouseMove {
                x: i * 10,
                y: i * 5,
            })
            .unwrap(),
        )
        .await;
    }
    // Wait for the applier to drain the slot (last position wins).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = h.input_events.lock().unwrap().clone();
        if ev
            .iter()
            .any(|e| matches!(e, InputEvent::MouseMove { x: 50, y: 25 }))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fast moves never applied: {ev:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A fast move that arrived before a click on the reliable channel must be applied
    // first (the two channels are independent SCTP streams, so give the move time to land).
    send_bytes(
        &fast,
        &serde_json::to_vec(&InputEvent::MouseMove { x: 999, y: 888 }).unwrap(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    send_bytes(
        &h.input_dc,
        &serde_json::to_vec(&InputEvent::MouseDown {
            button: MouseButton::Left,
        })
        .unwrap(),
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = h.input_events.lock().unwrap().clone();
        if let Some(down) = ev
            .iter()
            .position(|e| matches!(e, InputEvent::MouseDown { .. }))
        {
            let before = &ev[..down];
            assert!(
                before
                    .iter()
                    .any(|e| matches!(e, InputEvent::MouseMove { x: 999, y: 888 })),
                "move must precede the click: {ev:?}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "click never applied: {ev:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.end().await;
}

// ── chunk compression ─────────────────────────────────────────────────────────────────────

/// Repetitive text: compresses ~20×.
fn compressible(len: usize) -> Vec<u8> {
    let line =
        b"{\"id\": 12345, \"name\": \"remote agent\", \"ok\": true, \"tags\": [\"a\", \"b\"]}\n";
    line.iter().copied().cycle().take(len).collect()
}

fn deflate_raw(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

fn inflate_raw(data: &[u8]) -> Vec<u8> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(data)
        .read_to_end(&mut out)
        .unwrap();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_with_deflate_v2_frames_is_inflated_and_verified() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let mut h = Harness::connect(Options {
        config: AgentConfig {
            max_fps: 30,
            transfer_dir: Some(dir.display().to_string()),
            ..AgentConfig::default()
        },
        ..Default::default()
    })
    .await;
    let data = compressible(4 * 1024 * 1024);
    h.files(&FileMessage::Offer {
        transfer_id: 11,
        token: "deflate-up".into(),
        name: "log.jsonl".into(),
        size: data.len() as u64,
        kind: TransferKind::File,
        direction: TransferDirection::ToDevice,
        dest_dir: None,
        group: None,
        sha256: None,
    })
    .await;
    let mut chunks = Vec::new();
    let accept = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(
            m,
            FileMessage::Accept {
                transfer_id: 11,
                ..
            }
        )
    })
    .await;
    let FileMessage::Accept { codecs, .. } = accept else {
        unreachable!()
    };
    assert_eq!(
        codecs,
        Some(vec![ChunkCodec::Deflate]),
        "the agent advertises DEFLATE"
    );
    // Send version-2 frames: compressed where it pays off, raw otherwise (every 5th chunk raw
    // to exercise both paths).
    let max_payload = 64 * 1024 - CHUNK_HEADER_V2_LEN;
    let mut offset = 0usize;
    let mut wire = 0usize;
    let mut n = 0;
    while offset < data.len() {
        let end = (offset + max_payload).min(data.len());
        let raw = &data[offset..end];
        let (codec, payload) = if n % 5 == 4 {
            (ChunkCodec::Raw, raw.to_vec())
        } else {
            (ChunkCodec::Deflate, deflate_raw(raw))
        };
        let mut f = vec![0u8; CHUNK_HEADER_V2_LEN];
        encode_chunk_header_v2(11, offset as u64, codec, &mut f);
        f.extend_from_slice(&payload);
        assert!(f.len() <= 64 * 1024);
        wire += f.len();
        send_bytes(&h.files_dc, &f).await;
        offset = end;
        n += 1;
    }
    h.files(&FileMessage::Complete {
        transfer_id: 11,
        sha256: sha(&data),
    })
    .await;
    let done = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(
            m,
            FileMessage::Done {
                transfer_id: 11,
                ..
            }
        )
    })
    .await;
    let FileMessage::Done {
        ok, error, path, ..
    } = done
    else {
        unreachable!()
    };
    assert!(ok, "upload failed: {error:?}");
    let stored = std::fs::read(path.unwrap()).unwrap();
    assert_eq!(stored, data, "inflated bytes land on disk");
    assert!(
        wire * 4 < data.len(),
        "wire bytes {wire} vs payload {}",
        data.len()
    );
    h.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_uses_deflate_v2_frames_only_when_advertised() {
    let tmp = tempfile::tempdir().unwrap();
    let data = compressible(2 * 1024 * 1024 + 777);
    let src = tmp.path().join("export.jsonl");
    std::fs::write(&src, &data).unwrap();
    let noise = synthetic(300 * 1024, 3);
    let packed = tmp.path().join("photos.zip");
    std::fs::write(&packed, &noise).unwrap();

    let mut h = Harness::connect(Options::default()).await;
    let mut chunks = Vec::new();

    // ── compressible file, receiver advertises DEFLATE → v2 frames, mostly compressed ──
    h.files(&FileMessage::Request {
        transfer_id: 21,
        path: src.display().to_string(),
    })
    .await;
    next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(
            m,
            FileMessage::Offer {
                transfer_id: 21,
                ..
            }
        )
    })
    .await;
    h.files(&FileMessage::Accept {
        transfer_id: 21,
        offset: 0,
        codecs: Some(vec![ChunkCodec::Deflate]),
    })
    .await;
    let complete = next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(
            m,
            FileMessage::Complete {
                transfer_id: 21,
                ..
            }
        )
    })
    .await;
    let FileMessage::Complete { sha256, .. } = complete else {
        unreachable!()
    };
    let mut out = vec![0u8; data.len()];
    let (mut deflated, mut raw, mut wire) = (0, 0, 0usize);
    for c in chunks.drain(..) {
        assert!(c.len() <= 64 * 1024, "frame within the SCTP limit");
        let f = decode_chunk_any(&c).expect("valid frame");
        assert_eq!(f.transfer_id, 21);
        assert_eq!(c[0], 2, "version-2 frames once DEFLATE was negotiated");
        wire += c.len();
        let bytes = match f.codec {
            ChunkCodec::Deflate => {
                deflated += 1;
                inflate_raw(f.payload)
            }
            ChunkCodec::Raw => {
                raw += 1;
                f.payload.to_vec()
            }
        };
        out[f.offset as usize..f.offset as usize + bytes.len()].copy_from_slice(&bytes);
    }
    assert_eq!(sha(&out), sha256);
    assert_eq!(out, data);
    assert!(
        deflated > 0,
        "compressible data was deflated ({deflated} deflated / {raw} raw)"
    );
    assert!(
        wire * 4 < data.len(),
        "wire {wire} bytes for {} payload",
        data.len()
    );
    h.files(&FileMessage::Done {
        transfer_id: 21,
        ok: true,
        error: None,
        path: None,
    })
    .await;

    // ── incompressible name/magic: v2 frames, all raw ──────────────────────────────
    h.files(&FileMessage::Request {
        transfer_id: 23,
        path: packed.display().to_string(),
    })
    .await;
    next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(
            m,
            FileMessage::Offer {
                transfer_id: 23,
                ..
            }
        )
    })
    .await;
    h.files(&FileMessage::Accept {
        transfer_id: 23,
        offset: 0,
        codecs: Some(vec![ChunkCodec::Deflate]),
    })
    .await;
    next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(
            m,
            FileMessage::Complete {
                transfer_id: 23,
                ..
            }
        )
    })
    .await;
    let mut out = vec![0u8; noise.len()];
    for c in chunks.drain(..) {
        let f = decode_chunk_any(&c).unwrap();
        assert_eq!(f.codec, ChunkCodec::Raw, "noise is never sent deflated");
        assert_eq!(c[0], 2);
        out[f.offset as usize..f.offset as usize + f.payload.len()].copy_from_slice(f.payload);
    }
    assert_eq!(out, noise);
    h.files(&FileMessage::Done {
        transfer_id: 23,
        ok: true,
        error: None,
        path: None,
    })
    .await;

    // ── old receiver (no codecs): version-1 frames only ────────────────────────────
    h.files(&FileMessage::Request {
        transfer_id: 25,
        path: src.display().to_string(),
    })
    .await;
    next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(
            m,
            FileMessage::Offer {
                transfer_id: 25,
                ..
            }
        )
    })
    .await;
    h.files(&FileMessage::Accept {
        transfer_id: 25,
        offset: 0,
        codecs: None,
    })
    .await;
    next_files_msg(&mut h.files_rx, &mut chunks, |m| {
        matches!(
            m,
            FileMessage::Complete {
                transfer_id: 25,
                ..
            }
        )
    })
    .await;
    assert!(!chunks.is_empty());
    for c in chunks.drain(..) {
        assert_eq!(c[0], 1, "version-1 frame for receivers without codecs");
        let (id, _, payload) = decode_chunk(&c).unwrap();
        assert_eq!(id, 25);
        assert!(payload.len() <= MAX_CHUNK_BYTES);
    }
    h.end().await;
}

// ── annotation coordinates ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn annotation_points_scale_from_encoded_picture_to_display() {
    use remote_agent::annotate::AnnotateEvent;

    let mut h = Harness::connect(Options::default()).await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;
    // Shrink the encoded picture to half the 320×240 test display.
    h.control(&ControlMessage::SetViewport {
        display: 0,
        width: Some(160),
        height: Some(160),
    })
    .await;
    tokio::time::timeout(Duration::from_secs(6), async {
        next_control(&mut h.control_rx, |m| {
            matches!(
                m,
                ControlMessage::Stats {
                    display: 0,
                    encoded_width: 160,
                    ..
                }
            )
        })
        .await
    })
    .await
    .expect("viewport-scaled stats");
    // The browser draws against the 160×120 picture: (80, 60) is the centre.
    send_json(
        &h.control_dc,
        &ControlMessage::AnnotateStroke {
            id: 1,
            display: 0,
            color: "#ff0000".into(),
            width: 3.0,
            points: vec![(80.0, 60.0), (160.0, 120.0)],
        },
    )
    .await;
    send_json(
        &h.control_dc,
        &ControlMessage::AnnotatePointer {
            display: 0,
            point: Some((40.0, 30.0)),
            color: "#00ff00".into(),
        },
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if h.annotations.events.lock().unwrap().len() >= 2 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("annotation events");
    let events = h.annotations.events.lock().unwrap().clone();
    match &events[0] {
        AnnotateEvent::Stroke { points, width, .. } => {
            assert_eq!(points, &vec![(160.0, 120.0), (320.0, 240.0)]);
            assert_eq!(*width, 6.0);
        }
        other => panic!("unexpected {other:?}"),
    }
    match &events[1] {
        AnnotateEvent::Pointer { point, .. } => assert_eq!(*point, Some((80.0, 60.0))),
        other => panic!("unexpected {other:?}"),
    }
    h.end().await;
}

// ─── privacy screen ─────────────────────────────────────────────────────────────────────

/// Wait for `SessionEvent::PrivacyScreen { active, reason }` from the agent.
async fn wait_privacy_event(
    rx: &mut mpsc::UnboundedReceiver<AgentToConsole>,
    want_active: bool,
    want_reason: protocol::common::PrivacyScreenReason,
) -> bool {
    use protocol::agent::SessionEvent;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Some(AgentToConsole::SessionEvent {
                    event: SessionEvent::PrivacyScreen { active, reason },
                    ..
                }) if active == want_active && reason == want_reason => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn privacy_screen_is_refused_by_policy_and_by_missing_permission() {
    use protocol::common::PrivacyScreenReason;
    use protocol::config::AgentConfig;

    // Policy off (the default) refuses even an operator the console granted.
    let mut h = Harness::connect(Options::default()).await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;
    send_json(
        &h.control_dc,
        &ControlMessage::SetPrivacyScreen { enabled: true },
    )
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(
            m,
            ControlMessage::PrivacyScreenDenied {
                reason: PrivacyScreenReason::Policy
            }
        )
    })
    .await;
    h.end().await;

    // Policy on, but the console did not grant this operator `manage`.
    let mut h = Harness::connect(Options {
        config: AgentConfig {
            max_fps: 30,
            allow_privacy_screen: true,
            ..AgentConfig::default()
        },
        privacy_screen_allowed: false,
        ..Default::default()
    })
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;
    send_json(
        &h.control_dc,
        &ControlMessage::SetPrivacyScreen { enabled: true },
    )
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(
            m,
            ControlMessage::PrivacyScreenDenied {
                reason: PrivacyScreenReason::Permission
            }
        )
    })
    .await;
    h.end().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn privacy_screen_engages_releases_on_pause_and_can_be_re_engaged_after_a_lift() {
    use protocol::common::PrivacyScreenReason;
    use protocol::config::AgentConfig;

    // Debug-only seam: report support and engage without creating windows.
    std::env::set_var("REMOTE_AGENT_PRIVACY_FAKE", "1");
    let mut h = Harness::connect(Options {
        config: AgentConfig {
            max_fps: 30,
            allow_privacy_screen: true,
            ..AgentConfig::default()
        },
        ..Default::default()
    })
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::DisplayInfo { .. })
    })
    .await;

    // The operator engages it; the viewer and the console both hear about it.
    send_json(
        &h.control_dc,
        &ControlMessage::SetPrivacyScreen { enabled: true },
    )
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(
            m,
            ControlMessage::PrivacyScreen {
                active: true,
                reason: PrivacyScreenReason::Operator,
            }
        )
    })
    .await;
    assert!(wait_privacy_event(&mut h.hub_events, true, PrivacyScreenReason::Operator).await);

    // The emergency stop at the device also gives the desktop back.
    h.sessions.set_control_paused(true);
    next_control(&mut h.control_rx, |m| {
        matches!(
            m,
            ControlMessage::PrivacyScreen {
                active: false,
                reason: PrivacyScreenReason::ControlPaused,
            }
        )
    })
    .await;
    assert!(wait_privacy_event(&mut h.hub_events, false, PrivacyScreenReason::ControlPaused).await);
    h.sessions.set_control_paused(false);
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::ControlPaused { paused: false })
    })
    .await;

    // Engage again, then the person at the device lifts it: a stop, not a session-long lock.
    send_json(
        &h.control_dc,
        &ControlMessage::SetPrivacyScreen { enabled: true },
    )
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(m, ControlMessage::PrivacyScreen { active: true, .. })
    })
    .await;
    h.sessions.lift_privacy_screen();
    next_control(&mut h.control_rx, |m| {
        matches!(
            m,
            ControlMessage::PrivacyScreen {
                active: false,
                reason: PrivacyScreenReason::DeviceUser,
            }
        )
    })
    .await;
    assert!(wait_privacy_event(&mut h.hub_events, false, PrivacyScreenReason::DeviceUser).await);

    // The operator may cover the screen again after a lift.
    send_json(
        &h.control_dc,
        &ControlMessage::SetPrivacyScreen { enabled: true },
    )
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(
            m,
            ControlMessage::PrivacyScreen {
                active: true,
                reason: PrivacyScreenReason::Operator,
            }
        )
    })
    .await;

    // Pausing control at the device is the durable escape: it releases the screen and refuses
    // every further engagement while it lasts.
    h.sessions.set_control_paused(true);
    next_control(&mut h.control_rx, |m| {
        matches!(
            m,
            ControlMessage::PrivacyScreen {
                active: false,
                reason: PrivacyScreenReason::ControlPaused,
            }
        )
    })
    .await;
    send_json(
        &h.control_dc,
        &ControlMessage::SetPrivacyScreen { enabled: true },
    )
    .await;
    next_control(&mut h.control_rx, |m| {
        matches!(
            m,
            ControlMessage::PrivacyScreenDenied {
                reason: PrivacyScreenReason::ControlPaused
            }
        )
    })
    .await;
    h.end().await;
}
