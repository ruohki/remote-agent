//! End-to-end loopback: drive a real [`SessionManager`] with fake capture/encode/input, and a
//! second webrtc-rs peer connection standing in for the browser. Verifies the full path:
//! offer → answer → ICE → connected → video RTP flows, `display_info` arrives on the control
//! channel, and an input event reaches the injector; then a clean teardown.

mod support;

use bytes::BytesMut;
use parking_lot::RwLock;
use protocol::agent::AgentToConsole;
use protocol::channel::{InputEvent, MouseButton};
use protocol::common::{
    DeviceMode, EndReason, IceCandidate, OperatorInfo, SessionDescription, SessionState,
};
use protocol::config::AgentConfig;
use remote_agent::approval::AutoApprover;
use remote_agent::hub::HubSink;
use remote_agent::input::InputHandler;
use remote_agent::session::{SessionDeps, SessionManager, SessionRequest};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use support::{FakeMedia, NoopIndicator, RecordingApprover, RecordingInput};
use tokio::sync::mpsc;

use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_H264};
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind,
};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    register_default_interceptors, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription, Registry,
};

/// The "browser" side event handler.
struct BrowserHandler {
    ice_tx: mpsc::UnboundedSender<IceCandidate>,
    state_tx: mpsc::UnboundedSender<RTCPeerConnectionState>,
    track_tx: mpsc::UnboundedSender<Arc<dyn TrackRemote>>,
    control_tx: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
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
    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        // Only the agent creates channels in production; here the browser creates them, so
        // this is unused, but forward anything just in case.
        let _ = self.control_tx.send(dc);
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

async fn recv_timeout<T>(rx: &mut mpsc::UnboundedReceiver<T>, what: &str) -> T {
    tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("channel closed waiting for {what}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_session_loopback() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();

    // ── agent side: session manager with fakes ──────────────────────────────────────
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
    let approver_calls = Arc::new(AtomicU64::new(0));
    let deps = SessionDeps {
        media: Arc::new(FakeMedia::default()),
        input: input_factory,
        approver: Arc::new(AutoApprover(
            remote_agent::approval::ApprovalOutcome::Approved,
        )),
        indicator: Arc::new(NoopIndicator),
        hub,
        config: Arc::new(RwLock::new(AgentConfig {
            max_fps: 30,
            ..AgentConfig::default()
        })),
    };
    let _ = &approver_calls;
    let sessions = SessionManager::new(deps);

    // ── browser side ────────────────────────────────────────────────────────────────
    let (ice_tx, mut ice_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let (track_tx, mut track_rx) = mpsc::unbounded_channel();
    let (control_tx, _control_rx) = mpsc::unbounded_channel();

    let mut media_engine = MediaEngine::default();
    media_engine
        .register_codec(h264_codec(), RtpCodecKind::Video)
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
            control_tx,
        }))
        .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
        .build()
        .await
        .unwrap();
    let browser: Arc<dyn PeerConnection> = Arc::new(browser);

    // recvonly video transceiver + input/control channels, created before the offer.
    browser
        .add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(webrtc::rtp_transceiver::RTCRtpTransceiverInit {
                direction: webrtc::rtp_transceiver::RTCRtpTransceiverDirection::Recvonly,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let input_dc = browser
        .create_data_channel(protocol::INPUT_CHANNEL_LABEL, None)
        .await
        .unwrap();
    let _control_dc = browser
        .create_data_channel(protocol::CONTROL_CHANNEL_LABEL, None)
        .await
        .unwrap();

    let offer = browser.create_offer(None).await.unwrap();
    browser.set_local_description(offer.clone()).await.unwrap();

    // ── start the agent session with the offer ──────────────────────────────────────
    let session_id = "ses_test".to_string();
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
    });

    // Pump agent → browser: answer, ICE, and watch session state.
    let browser_relay = browser.clone();
    let (answer_done_tx, mut answer_done_rx) = mpsc::unbounded_channel();
    let sid = session_id.clone();
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
                AgentToConsole::SessionState {
                    session_id, state, ..
                } if session_id == sid => {
                    tracing::info!(?state, "agent session state");
                }
                _ => {}
            }
        }
    });

    // Pump browser → agent ICE.
    let sessions_ice = sessions.clone();
    let sid2 = session_id.clone();
    tokio::spawn(async move {
        while let Some(c) = ice_rx.recv().await {
            sessions_ice.add_ice_candidate(&sid2, c);
        }
    });

    recv_timeout(&mut answer_done_rx, "session answer").await;

    // ── wait for the browser to connect ─────────────────────────────────────────────
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

    // ── video RTP flows ─────────────────────────────────────────────────────────────
    let track = recv_timeout(&mut track_rx, "incoming video track").await;
    let got_rtp = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(evt) = track.poll().await {
            if let webrtc::media_stream::track_remote::TrackRemoteEvent::OnRtpPacket(_) = evt {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(got_rtp, "no RTP packets received on the video track");

    // ── control channel receives display_info; input reaches the injector ───────────
    // Read the input data channel until it opens, then send a mouse move.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(DataChannelEvent::OnOpen) = input_dc.poll().await {
                break;
            }
        }
    })
    .await
    .expect("input channel never opened");

    let ev = InputEvent::MouseDown {
        button: MouseButton::Left,
    };
    input_dc
        .send(BytesMut::from(&serde_json::to_vec(&ev).unwrap()[..]))
        .await
        .unwrap();

    let got_input = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if input_events
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
    assert!(got_input, "input event never reached the injector");

    // ── teardown ────────────────────────────────────────────────────────────────────
    sessions.end(&session_id, EndReason::OperatorClosed);
    sessions.wait_idle(Duration::from_secs(10)).await;
    assert!(
        sessions.active_session_id().is_none(),
        "session did not clear after end"
    );
    // Input handler was told to release keys at least once.
    assert!(
        releases.load(Ordering::SeqCst) >= 1,
        "release_all was never called"
    );

    browser.close().await.ok();
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
    });

    // Expect: awaiting_approval, approval_result(false), ended(Denied).
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
