//! One operator session = one WebRTC peer connection.
//!
//! [`SessionManager`] owns at most one active session and is driven by the hub
//! (`session_request`, `ice_candidate`, `session_end`). Each session runs as a tokio task
//! (see [`run_session`]) that:
//!
//! 1. in help-me mode asks the local user for approval (auto-deny on timeout);
//! 2. picks the codec (browser preference ∩ what we can encode), builds the peer
//!    connection with only that codec, answers the offer and reports `session_answer`;
//! 3. relays trickle ICE in both directions;
//! 4. once connected starts the capture→encode [`video::VideoPipeline`] and feeds the video
//!    track, honours PLI/FIR keyframe requests, and serves the `input` / `control` data
//!    channels;
//! 5. tears everything down (including releasing pressed keys) on any end condition.

pub mod media;
pub mod peer;
pub mod sdp;
pub mod video;

use crate::approval::{ApprovalOutcome, Approver, Indicator, IndicatorHandle};
use crate::hub::HubSink;
use crate::input::InputHandler;
use anyhow::Result;
use bytes::BytesMut;
use media::{choose_codec, MediaFactory};
use parking_lot::{Mutex, RwLock};
use peer::{is_keyframe_request, Peer, PeerEvent};
use protocol::agent::AgentToConsole;
use protocol::channel::{ControlMessage, InputEvent};
use protocol::common::{
    DeviceMode, DisplayInfo, EndReason, IceCandidate, IceServer, OperatorInfo, SessionDescription,
    SessionState,
};
use protocol::config::AgentConfig;
use protocol::{CONTROL_CHANNEL_LABEL, INPUT_CHANNEL_LABEL};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use video::{PipelineConfig, PipelineEvent, VideoPipeline};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::RTCPeerConnectionState;

/// Builds an input handler; called once per session (may prompt for permissions).
pub type InputFactory = Arc<dyn Fn() -> Result<Box<dyn InputHandler>> + Send + Sync>;

/// Everything a session needs from the outside world.
#[derive(Clone)]
pub struct SessionDeps {
    pub media: Arc<dyn MediaFactory>,
    pub input: InputFactory,
    pub approver: Arc<dyn Approver>,
    pub indicator: Arc<dyn Indicator>,
    pub hub: HubSink,
    pub config: Arc<RwLock<AgentConfig>>,
}

/// Incoming `session_request`.
#[derive(Debug, Clone)]
pub struct SessionRequest {
    pub session_id: String,
    pub operator: OperatorInfo,
    pub offer: SessionDescription,
    pub ice_servers: Vec<IceServer>,
}

enum SessionCommand {
    AddIceCandidate(IceCandidate),
    End(EndReason),
}

struct ActiveSession {
    id: String,
    cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    task: JoinHandle<()>,
}

/// Owns the (single) active session.
pub struct SessionManager {
    deps: SessionDeps,
    active: Mutex<Option<ActiveSession>>,
}

impl SessionManager {
    pub fn new(deps: SessionDeps) -> Arc<Self> {
        Arc::new(Self {
            deps,
            active: Mutex::new(None),
        })
    }

    pub fn active_session_id(&self) -> Option<String> {
        self.active.lock().as_ref().map(|s| s.id.clone())
    }

    /// Handle a `session_request`. Rejected (with `session_state: ended`) when a session is
    /// already active — the console enforces this too, this is defence in depth.
    pub fn start(self: &Arc<Self>, req: SessionRequest) {
        let mut active = self.active.lock();
        if let Some(existing) = active.as_ref() {
            if existing.task.is_finished() {
                *active = None;
            } else {
                tracing::warn!(
                    session = %req.session_id,
                    active = %existing.id,
                    "rejecting session request: another session is active"
                );
                self.deps.hub.send(AgentToConsole::SessionState {
                    session_id: req.session_id,
                    state: SessionState::Ended,
                    reason: Some(EndReason::Error),
                });
                return;
            }
        }
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let id = req.session_id.clone();
        let deps = self.deps.clone();
        let me = Arc::downgrade(self);
        let task_id = id.clone();
        let task = tokio::spawn(async move {
            run_session(deps, req, cmd_rx).await;
            if let Some(m) = me.upgrade() {
                m.clear_if(&task_id);
            }
        });
        *active = Some(ActiveSession { id, cmd_tx, task });
    }

    pub fn add_ice_candidate(&self, session_id: &str, candidate: IceCandidate) {
        let active = self.active.lock();
        match active.as_ref() {
            Some(s) if s.id == session_id => {
                let _ = s.cmd_tx.send(SessionCommand::AddIceCandidate(candidate));
            }
            _ => tracing::debug!(session = session_id, "ICE candidate for unknown session"),
        }
    }

    pub fn end(&self, session_id: &str, reason: EndReason) {
        let active = self.active.lock();
        if let Some(s) = active.as_ref() {
            if s.id == session_id {
                let _ = s.cmd_tx.send(SessionCommand::End(reason));
            }
        }
    }

    /// End whatever is running (used on config changes that forbid sessions, shutdown, …).
    pub fn end_all(&self, reason: EndReason) {
        let active = self.active.lock();
        if let Some(s) = active.as_ref() {
            let _ = s.cmd_tx.send(SessionCommand::End(reason));
        }
    }

    /// Wait for the active session task to finish (bounded).
    pub async fn wait_idle(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let finished = self
                .active
                .lock()
                .as_ref()
                .map(|s| s.task.is_finished())
                .unwrap_or(true);
            if finished || tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn clear_if(&self, session_id: &str) {
        let mut active = self.active.lock();
        if active.as_ref().map(|s| s.id == session_id).unwrap_or(false) {
            *active = None;
        }
    }
}

// ─── the session task ───────────────────────────────────────────────────────────────────

/// Events from the data channel reader tasks.
enum ChannelEvent {
    ControlOpen(Arc<dyn DataChannel>),
    Control(ControlMessage),
    ControlClosed,
}

struct Media {
    pipeline: Arc<VideoPipeline>,
    events: mpsc::UnboundedReceiver<PipelineEvent>,
    writer: JoinHandle<()>,
    rtcp: JoinHandle<()>,
    displays: Vec<DisplayInfo>,
    current_display: u32,
    /// Encoded picture size (browser mouse coordinates are in this space).
    video_size: (u32, u32),
}

struct Session {
    deps: SessionDeps,
    id: String,
    operator: OperatorInfo,
    cfg: AgentConfig,
    peer: Arc<Peer>,
    input: Arc<Mutex<Option<Box<dyn InputHandler>>>>,
    control: Option<Arc<dyn DataChannel>>,
    media: Option<Media>,
    channel_tx: mpsc::UnboundedSender<ChannelEvent>,
    readers: Vec<JoinHandle<()>>,
    clipboard: Option<ClipboardWatch>,
    indicator: Option<Box<dyn IndicatorHandle>>,
    /// Set when the local user pressed "Disconnect" on the indicator.
    user_ended: Arc<std::sync::atomic::AtomicBool>,
}

struct ClipboardWatch {
    rx: mpsc::UnboundedReceiver<String>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

/// How long a `disconnected` connection state may last before the session is ended.
const DISCONNECT_GRACE: Duration = Duration::from_secs(12);

async fn run_session(
    deps: SessionDeps,
    req: SessionRequest,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
) {
    let session_id = req.session_id.clone();
    let hub = deps.hub.clone();
    let cfg = deps.config.read().clone();
    let report = |state: SessionState, reason: Option<EndReason>| {
        hub.send(AgentToConsole::SessionState {
            session_id: session_id.clone(),
            state,
            reason,
        });
    };
    tracing::info!(session = %session_id, operator = %req.operator.name, mode = ?cfg.mode, "session requested");

    // Candidates that arrive before the peer connection exists.
    let mut pending: Vec<IceCandidate> = Vec::new();

    // ── approval ──────────────────────────────────────────────────────────────────
    if cfg.mode == DeviceMode::HelpMe {
        report(SessionState::AwaitingApproval, None);
        let timeout = Duration::from_secs(cfg.approval_timeout_s.max(5) as u64);
        let outcome = tokio::select! {
            r = deps.approver.ask(&req.operator, timeout) => r,
            end = drain_until_end(&mut cmd_rx, &mut pending) => {
                tracing::info!(session = %session_id, "ended while awaiting approval");
                report(SessionState::Ended, Some(end));
                return;
            }
        };
        let approved = matches!(outcome, Ok(ApprovalOutcome::Approved));
        hub.send(AgentToConsole::ApprovalResult {
            session_id: session_id.clone(),
            approved,
        });
        match outcome {
            Ok(ApprovalOutcome::Approved) => {}
            Ok(ApprovalOutcome::Denied) => {
                report(SessionState::Ended, Some(EndReason::Denied));
                return;
            }
            Ok(ApprovalOutcome::TimedOut) => {
                report(SessionState::Ended, Some(EndReason::ApprovalTimeout));
                return;
            }
            Err(e) => {
                tracing::error!(session = %session_id, "approval prompt failed: {e:#}");
                report(SessionState::Ended, Some(EndReason::Error));
                return;
            }
        }
    }

    // ── codec + peer connection ───────────────────────────────────────────────────
    let offered = sdp::offered_video_codecs(&req.offer.sdp);
    let available = deps.media.available_codecs();
    let Some(codec) = choose_codec(&offered, &available, cfg.preferred_codec) else {
        tracing::error!(session = %session_id, ?offered, ?available, "no common video codec");
        report(SessionState::Ended, Some(EndReason::Error));
        return;
    };
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
    let peer = match Peer::new(codec, &req.ice_servers, peer_tx).await {
        Ok(p) => Arc::new(p),
        Err(e) => {
            tracing::error!(session = %session_id, "creating peer connection: {e:#}");
            report(SessionState::Ended, Some(EndReason::Error));
            return;
        }
    };
    let answer_sdp = match peer.answer(req.offer.sdp.clone()).await {
        Ok(sdp) => sdp,
        Err(e) => {
            tracing::error!(session = %session_id, "answering offer: {e:#}");
            peer.close().await;
            report(SessionState::Ended, Some(EndReason::Error));
            return;
        }
    };
    hub.send(AgentToConsole::SessionAnswer {
        session_id: session_id.clone(),
        answer: SessionDescription {
            kind: "answer".into(),
            sdp: answer_sdp,
        },
        codec,
    });
    report(SessionState::Connecting, None);
    tracing::info!(session = %session_id, ?codec, "answer sent");

    let (channel_tx, mut channel_rx) = mpsc::unbounded_channel();
    let mut session = Session {
        deps: deps.clone(),
        id: session_id.clone(),
        operator: req.operator.clone(),
        cfg,
        peer,
        input: Arc::new(Mutex::new(None)),
        control: None,
        media: None,
        channel_tx,
        readers: Vec::new(),
        clipboard: None,
        indicator: None,
        user_ended: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    for c in pending.drain(..) {
        session.add_candidate(&c).await;
    }

    // ── main loop ─────────────────────────────────────────────────────────────────
    let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut disconnect_deadline: Option<tokio::time::Instant> = None;
    let mut connected_once = false;

    let reason = loop {
        let disconnect_sleep = async {
            match disconnect_deadline {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending::<()>().await,
            }
        };
        let pipeline_event = async {
            match session.media.as_mut() {
                Some(m) => m.events.recv().await,
                None => std::future::pending().await,
            }
        };
        let clipboard_event = async {
            match session.clipboard.as_mut() {
                Some(c) => c.rx.recv().await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(SessionCommand::AddIceCandidate(c)) => session.add_candidate(&c).await,
                Some(SessionCommand::End(reason)) => break reason,
                None => break EndReason::Error,
            },
            ev = peer_rx.recv() => match ev {
                Some(PeerEvent::IceCandidate(c)) => {
                    session.deps.hub.send(AgentToConsole::IceCandidate { session_id: session.id.clone(), candidate: c });
                }
                Some(PeerEvent::ConnectionState(state)) => {
                    tracing::info!(session = %session.id, ?state, "peer connection state");
                    match state {
                        RTCPeerConnectionState::Connected => {
                            disconnect_deadline = None;
                            if !connected_once {
                                connected_once = true;
                                if let Err(e) = session.on_connected().await {
                                    tracing::error!(session = %session.id, "starting media: {e:#}");
                                    break EndReason::Error;
                                }
                                report(SessionState::Connected, None);
                            }
                        }
                        RTCPeerConnectionState::Disconnected => {
                            disconnect_deadline = Some(tokio::time::Instant::now() + DISCONNECT_GRACE);
                        }
                        RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                            break if session.user_ended.load(std::sync::atomic::Ordering::Relaxed) {
                                EndReason::DeviceUserClosed
                            } else {
                                EndReason::ConnectionFailed
                            };
                        }
                        _ => {}
                    }
                }
                Some(PeerEvent::DataChannel(dc)) => session.on_data_channel(dc).await,
                None => break EndReason::Error,
            },
            ev = channel_rx.recv() => match ev {
                Some(ChannelEvent::ControlOpen(dc)) => {
                    session.control = Some(dc);
                    session.send_display_info().await;
                }
                Some(ChannelEvent::Control(msg)) => session.on_control(msg).await,
                Some(ChannelEvent::ControlClosed) => session.control = None,
                None => {}
            },
            Some(ev) = pipeline_event => match ev {
                PipelineEvent::Failed(msg) => {
                    tracing::error!(session = %session.id, "video pipeline failed: {msg}");
                    break EndReason::Error;
                }
                PipelineEvent::Started { display_index, width, height, encoded_width, encoded_height, codec, hardware } => {
                    tracing::info!(session = %session.id, display_index, width, height, encoded_width, encoded_height, ?codec, hardware, "pipeline started");
                    session.update_input_display(display_index, Some((encoded_width, encoded_height)));
                    session.send_display_info().await;
                }
            },
            Some(text) = clipboard_event => {
                session.send_control(ControlMessage::ClipboardChanged { text }).await;
            }
            _ = stats_tick.tick() => session.send_stats().await,
            _ = disconnect_sleep => {
                tracing::warn!(session = %session.id, "connection stayed disconnected; ending");
                break EndReason::ConnectionFailed;
            }
        }
    };

    if session
        .user_ended
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        session
            .send_control(ControlMessage::SessionEndedByUser)
            .await;
    }
    let reason = if session
        .user_ended
        .load(std::sync::atomic::Ordering::Relaxed)
        && reason == EndReason::OperatorClosed
    {
        EndReason::DeviceUserClosed
    } else {
        reason
    };
    session.teardown().await;
    tracing::info!(session = %session_id, ?reason, "session ended");
    report(SessionState::Ended, Some(reason));
}

/// While waiting for approval: buffer ICE candidates, resolve on `End`.
async fn drain_until_end(
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCommand>,
    pending: &mut Vec<IceCandidate>,
) -> EndReason {
    loop {
        match cmd_rx.recv().await {
            Some(SessionCommand::AddIceCandidate(c)) => pending.push(c),
            Some(SessionCommand::End(reason)) => return reason,
            None => return EndReason::Error,
        }
    }
}

impl Session {
    async fn add_candidate(&self, c: &IceCandidate) {
        if let Err(e) = self.peer.add_ice_candidate(c).await {
            tracing::debug!(session = %self.id, "add_ice_candidate: {e:#}");
        }
    }

    /// Connection established: start capture/encode and the writer/RTCP tasks.
    async fn on_connected(&mut self) -> Result<()> {
        let media = Arc::clone(&self.deps.media);
        let displays = tokio::task::spawn_blocking({
            let media = Arc::clone(&media);
            move || media.list_displays()
        })
        .await??;
        let current_display = displays
            .iter()
            .find(|d| d.primary)
            .or_else(|| displays.first())
            .map(|d| d.index)
            .unwrap_or(0);

        let (frame_tx, mut frame_rx) = mpsc::channel(2);
        let (pev_tx, pev_rx) = mpsc::unbounded_channel();
        let pipeline_cfg = PipelineConfig {
            display_index: current_display,
            codec: self.peer.codec(),
            max_fps: self.cfg.max_fps.clamp(1, 240),
            max_bitrate_kbps: self.cfg.max_bitrate_kbps.max(100),
            show_cursor: true,
        };
        let pipeline = tokio::task::spawn_blocking({
            let media = Arc::clone(&media);
            move || VideoPipeline::start(media, pipeline_cfg, frame_tx, pev_tx)
        })
        .await??;
        let pipeline = Arc::new(pipeline);

        let payload_type = self.peer.negotiated_payload_type().await;
        let ssrc = self.peer.ssrc().await;
        let fps = self.cfg.max_fps.clamp(1, 240) as f64;
        tracing::info!(session = %self.id, payload_type, ssrc, "video track ready");

        let writer = tokio::spawn({
            let peer = Arc::clone(&self.peer);
            let sid = self.id.clone();
            async move {
                let default_duration = Duration::from_secs_f64(1.0 / fps);
                let mut last_pts: Option<Duration> = None;
                while let Some(frame) = frame_rx.recv().await {
                    let duration = match last_pts {
                        Some(prev) if frame.pts > prev => (frame.pts - prev)
                            .clamp(Duration::from_millis(1), Duration::from_secs(1)),
                        _ => default_duration,
                    };
                    last_pts = Some(frame.pts);
                    if let Err(e) = peer
                        .write_frame(payload_type, ssrc, frame.data, duration)
                        .await
                    {
                        tracing::debug!(session = %sid, "write_frame: {e:#}");
                    }
                }
            }
        });

        let rtcp = tokio::spawn({
            let peer = Arc::clone(&self.peer);
            let keyframe = pipeline.keyframe_requester();
            async move {
                loop {
                    match peer.poll_rtcp().await {
                        Some(ev) => {
                            if is_keyframe_request(&ev) {
                                keyframe.request();
                            }
                        }
                        // Not bound yet (or unbound): back off and retry until aborted.
                        None => tokio::time::sleep(Duration::from_millis(200)).await,
                    }
                }
            }
        });

        let video_size = {
            let s = pipeline.stats().borrow().clone();
            (s.encoded_width, s.encoded_height)
        };
        self.media = Some(Media {
            pipeline,
            events: pev_rx,
            writer,
            rtcp,
            displays,
            current_display,
            video_size,
        });
        self.update_input_display(current_display, None);

        if self.cfg.allow_clipboard {
            self.clipboard = Some(start_clipboard_watch());
        }
        if self.cfg.show_session_indicator {
            let user_ended = Arc::clone(&self.user_ended);
            let hub = self.deps.hub.clone();
            let sid = self.id.clone();
            let on_disconnect: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                user_ended.store(true, std::sync::atomic::Ordering::Relaxed);
                // Route through the console so its bookkeeping (and the browser) see the end;
                // the session loop also exits when the peer connection closes.
                hub.send(AgentToConsole::SessionState {
                    session_id: sid.clone(),
                    state: SessionState::Ended,
                    reason: Some(EndReason::DeviceUserClosed),
                });
            });
            match self.deps.indicator.show(&self.operator, on_disconnect) {
                Ok(handle) => self.indicator = Some(handle),
                Err(e) => tracing::warn!("session indicator unavailable: {e:#}"),
            }
        }
        Ok(())
    }

    /// Point the input handler at `index`; `video_size` is the encoded picture size the
    /// browser's mouse coordinates refer to (`None` = same as the display).
    fn update_input_display(&mut self, index: u32, video_size: Option<(u32, u32)>) {
        let Some(media) = self.media.as_mut() else {
            return;
        };
        media.current_display = index;
        if let Some(size) = video_size {
            media.video_size = size;
        }
        if let Some(d) = media.displays.iter().find(|d| d.index == index).cloned() {
            if let Some(handler) = self.input.lock().as_mut() {
                handler.set_display(&d, media.video_size);
            }
        }
    }

    async fn on_data_channel(&mut self, dc: Arc<dyn DataChannel>) {
        let label = match dc.label().await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("data channel without label: {e}");
                return;
            }
        };
        tracing::info!(session = %self.id, label, "data channel");
        match label.as_str() {
            INPUT_CHANNEL_LABEL => {
                if self.input.lock().is_none() {
                    match (self.deps.input)() {
                        Ok(mut h) => {
                            if let Some(m) = self.media.as_ref() {
                                if let Some(d) =
                                    m.displays.iter().find(|d| d.index == m.current_display)
                                {
                                    h.set_display(d, m.video_size);
                                }
                            }
                            *self.input.lock() = Some(h);
                        }
                        Err(e) => tracing::error!("input injection unavailable: {e:#}"),
                    }
                }
                let input = Arc::clone(&self.input);
                let allow = self.cfg.allow_input;
                let sid = self.id.clone();
                self.readers.push(tokio::spawn(async move {
                    while let Some(ev) = dc.poll().await {
                        match ev {
                            DataChannelEvent::OnMessage(msg) => {
                                if !allow {
                                    continue;
                                }
                                match serde_json::from_slice::<InputEvent>(&msg.data) {
                                    Ok(event) => {
                                        if let Some(h) = input.lock().as_mut() {
                                            if let Err(e) = h.handle(event) {
                                                tracing::debug!(session = %sid, "input: {e:#}");
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::debug!(session = %sid, "bad input event: {e}")
                                    }
                                }
                            }
                            DataChannelEvent::OnClose => break,
                            _ => {}
                        }
                    }
                    if let Some(h) = input.lock().as_mut() {
                        h.release_all();
                    }
                }));
            }
            CONTROL_CHANNEL_LABEL => {
                let tx = self.channel_tx.clone();
                let sid = self.id.clone();
                self.readers.push(tokio::spawn(async move {
                    while let Some(ev) = dc.poll().await {
                        match ev {
                            DataChannelEvent::OnOpen => {
                                let _ = tx.send(ChannelEvent::ControlOpen(Arc::clone(&dc)));
                            }
                            DataChannelEvent::OnMessage(msg) => {
                                match serde_json::from_slice::<ControlMessage>(&msg.data) {
                                    Ok(m) => {
                                        let _ = tx.send(ChannelEvent::Control(m));
                                    }
                                    Err(e) => {
                                        tracing::debug!(session = %sid, "bad control message: {e}")
                                    }
                                }
                            }
                            DataChannelEvent::OnClose => {
                                let _ = tx.send(ChannelEvent::ControlClosed);
                                break;
                            }
                            _ => {}
                        }
                    }
                }));
            }
            other => tracing::warn!(session = %self.id, "ignoring unknown data channel {other}"),
        }
    }

    async fn on_control(&mut self, msg: ControlMessage) {
        match msg {
            ControlMessage::SelectDisplay { index } => {
                if let Some(m) = self.media.as_ref() {
                    if m.displays.iter().any(|d| d.index == index) {
                        m.pipeline.select_display(index);
                    } else {
                        tracing::warn!(session = %self.id, index, "unknown display");
                    }
                }
            }
            ControlMessage::SetQuality {
                max_fps,
                max_bitrate_kbps,
            } => {
                if let Some(m) = self.media.as_ref() {
                    let fps = max_fps.map(|f| f.clamp(1, self.cfg.max_fps.max(1)));
                    let kbps =
                        max_bitrate_kbps.map(|b| b.clamp(100, self.cfg.max_bitrate_kbps.max(100)));
                    m.pipeline.set_quality(fps, kbps);
                }
            }
            ControlMessage::RequestKeyframe => {
                if let Some(m) = self.media.as_ref() {
                    m.pipeline.request_keyframe();
                }
            }
            ControlMessage::SecureAttention => {
                if !self.cfg.allow_input {
                    return;
                }
                if let Err(e) = tokio::task::spawn_blocking(crate::platform::secure_attention).await
                {
                    tracing::warn!("secure attention: {e}");
                }
            }
            ControlMessage::ClipboardSet { text } => {
                if !self.cfg.allow_clipboard {
                    return;
                }
                let res = tokio::task::spawn_blocking(move || -> Result<()> {
                    let mut cb = arboard::Clipboard::new()?;
                    cb.set_text(text)?;
                    Ok(())
                })
                .await;
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("setting clipboard: {e:#}"),
                    Err(e) => tracing::warn!("clipboard task: {e}"),
                }
            }
            // agent → browser messages are never expected inbound
            ControlMessage::DisplayInfo { .. }
            | ControlMessage::ClipboardChanged { .. }
            | ControlMessage::Stats { .. }
            | ControlMessage::SessionEndedByUser => {}
        }
    }

    async fn send_control(&self, msg: ControlMessage) {
        let Some(dc) = self.control.as_ref() else {
            return;
        };
        match serde_json::to_vec(&msg) {
            Ok(bytes) => {
                if let Err(e) = dc.send(BytesMut::from(&bytes[..])).await {
                    tracing::debug!(session = %self.id, "control send: {e}");
                }
            }
            Err(e) => tracing::warn!("serializing control message: {e}"),
        }
    }

    async fn send_display_info(&self) {
        let Some(m) = self.media.as_ref() else { return };
        self.send_control(ControlMessage::DisplayInfo {
            displays: m.displays.clone(),
            current: m.current_display,
        })
        .await;
    }

    async fn send_stats(&self) {
        let Some(m) = self.media.as_ref() else { return };
        if self.control.is_none() {
            return;
        }
        let s = m.pipeline.stats().borrow().clone();
        self.send_control(ControlMessage::Stats {
            codec: s.codec,
            fps: s.fps,
            bitrate_kbps: s.bitrate_kbps,
            width: s.encoded_width,
            height: s.encoded_height,
            pipeline_ms: s.pipeline_ms,
            hardware: s.hardware,
        })
        .await;
    }

    async fn teardown(&mut self) {
        if let Some(h) = self.input.lock().as_mut() {
            h.release_all();
        }
        self.indicator = None;
        if let Some(c) = self.clipboard.take() {
            c.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(m) = self.media.take() {
            m.rtcp.abort();
            m.writer.abort();
            let pipeline = m.pipeline;
            let _ = tokio::task::spawn_blocking(move || drop(pipeline)).await;
        }
        for r in self.readers.drain(..) {
            r.abort();
        }
        self.peer.close().await;
    }
}

/// Poll the system clipboard on a background thread; changes are sent as text.
fn start_clipboard_watch() -> ClipboardWatch {
    let (tx, rx) = mpsc::unbounded_channel();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let spawned = std::thread::Builder::new()
        .name("clipboard-watch".into())
        .spawn(move || {
            let mut clipboard = match arboard::Clipboard::new() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("clipboard unavailable: {e}");
                    return;
                }
            };
            // Don't replay whatever is on the clipboard when the session starts.
            let mut last = clipboard.get_text().ok();
            while !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if let Ok(text) = clipboard.get_text() {
                    if last.as_deref() != Some(text.as_str()) {
                        last = Some(text.clone());
                        if tx.send(text).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("clipboard watcher thread: {e}");
    }
    ClipboardWatch { rx, stop }
}
