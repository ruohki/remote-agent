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
    decode_chunk, encode_chunk_header, FileMessage, TransferDirection, TransferKind,
    CHUNK_HEADER_LEN, MAX_CHUNK_BYTES,
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
        }
    }
}

/// A connected agent session + browser peer with all three data channels open.
struct Harness {
    sessions: Arc<SessionManager>,
    browser: Arc<dyn PeerConnection>,
    session_id: String,
    hub_events: mpsc::UnboundedReceiver<AgentToConsole>,
    tracks: mpsc::UnboundedReceiver<Arc<dyn TrackRemote>>,
    control_dc: Arc<dyn DataChannel>,
    control_rx: ControlRx,
    input_dc: Arc<dyn DataChannel>,
    files_dc: Arc<dyn DataChannel>,
    files_rx: FilesRx,
    input_events: Arc<std::sync::Mutex<Vec<InputEvent>>>,
    releases: Arc<AtomicU64>,
    chat: RecordingChat,
    clipboard: FakeClipboard,
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

        Self {
            sessions,
            browser,
            session_id,
            hub_events,
            tracks,
            control_dc,
            control_rx,
            input_dc,
            files_dc,
            files_rx,
            input_events,
            releases,
            chat,
            clipboard,
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
            offset: 0
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
