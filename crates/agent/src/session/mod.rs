//! One operator session = one WebRTC peer connection.
//!
//! [`SessionManager`] owns at most one active session and is driven by the hub
//! (`session_request`, `ice_candidate`, `session_end`). Each session runs as a tokio task
//! (see [`run_session`]) that:
//!
//! 1. in help-me mode asks the local user for approval (auto-deny on timeout);
//! 2. picks the codec (browser preference ∩ what we can encode), builds the peer
//!    connection, applies the offer, binds one video track per display the browser asked
//!    for (m-line order = display order, see [`peer`]) plus an Opus track when offered,
//!    answers and reports `session_answer`;
//! 3. relays trickle ICE in both directions;
//! 4. once connected starts a capture→encode [`video::VideoPipeline`] per *active* display
//!    (default: the primary one), honours PLI/FIR keyframe requests, serves the `input`,
//!    `control` and `files` data channels (file transfer, remote browser, clipboard, chat,
//!    display selection, audio on/off), and reports [`SessionEvent`]s to the console;
//! 5. tears everything down (including releasing pressed keys) on any end condition.

pub mod audio;
pub mod files;
pub mod media;
pub mod peer;
pub mod sdp;
pub mod video;

use crate::approval::{ApprovalOutcome, Approver, Indicator, IndicatorHandle};
use crate::chat::{ChatHandle, ChatModel, ChatUi};
use crate::clipboard::{ClipboardBackend, ClipboardContent, ClipboardWatch};
use crate::congestion::AimdController;
use crate::cursor::CursorUpdate;
use crate::hub::HubSink;
use crate::input::{InputHandler, LatestMove};
use crate::transfer::{TransferConfig, TransferManager, TransferNotice};
use anyhow::{anyhow, Context, Result};
use audio::{AudioPacket, AudioPipeline};
use bytes::{Bytes, BytesMut};
use media::{choose_codec, MediaFactory};
use parking_lot::{Mutex, RwLock};
use peer::{is_keyframe_request, Peer, PeerEvent};
use protocol::agent::{AgentToConsole, SessionEvent};
use protocol::channel::{ChatParty, ClipboardKind, ControlMessage, InputEvent};
use protocol::common::{
    DeviceMode, DisplayInfo, EndReason, IceCandidate, IceServer, OperatorInfo, SessionDescription,
    SessionRole, SessionState,
};
use protocol::config::AgentConfig;
use protocol::files::{FileMessage, TransferDirection, TransferKind};
use protocol::{
    CONTROL_CHANNEL_LABEL, FAST_INPUT_CHANNEL_LABEL, FILES_CHANNEL_LABEL, INPUT_CHANNEL_LABEL,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub chat: Arc<dyn ChatUi>,
    pub clipboard: Arc<dyn ClipboardBackend>,
    pub hub: HubSink,
    pub config: Arc<RwLock<AgentConfig>>,
    /// Where operator screen annotations are drawn (overlay windows in app mode).
    pub annotations: crate::annotate::SharedSink,
}

/// Incoming `session_request`.
#[derive(Debug, Clone)]
pub struct SessionRequest {
    pub session_id: String,
    pub operator: OperatorInfo,
    pub offer: SessionDescription,
    pub ice_servers: Vec<IceServer>,
    pub role: SessionRole,
    pub shadow_of: Option<String>,
    pub notify_operator: bool,
}

enum SessionCommand {
    AddIceCandidate(IceCandidate),
    /// Live-apply a new effective config (console policy tightened by local overrides).
    UpdateConfig(AgentConfig),
    /// The person at the device paused / resumed remote control (session bar switch).
    SetControlPaused(bool),
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

    /// Emergency switch from the session bar: drop all remote input until the device user
    /// resumes. Only the device side can call this; nothing the operator sends lifts it.
    pub fn set_control_paused(&self, paused: bool) {
        let active = self.active.lock();
        if let Some(s) = active.as_ref() {
            let _ = s.cmd_tx.send(SessionCommand::SetControlPaused(paused));
        }
    }

    /// Apply a new effective config to the running session (live policy change).
    pub fn apply_overrides(&self, effective: AgentConfig) {
        let active = self.active.lock();
        if let Some(s) = active.as_ref() {
            let _ = s.cmd_tx.send(SessionCommand::UpdateConfig(effective));
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

/// Events from the data channel reader tasks and the native UI.
enum ChannelEvent {
    ControlOpen(Arc<dyn DataChannel>),
    Control(ControlMessage),
    ControlClosed,
    FilesOpen(Arc<dyn DataChannel>),
    FilesText(FileMessage),
    FilesBinary(Bytes),
    FilesClosed,
    /// The local user typed a chat line.
    ChatFromDevice(String),
    /// The person at the device ended the session (banner / app / session bar).
    UserEnd,
    /// A control message produced by a helper thread (cursor source) to forward.
    Outbound(ControlMessage),
}

/// One streaming display: its pipeline and the tasks feeding its video track.
struct DisplayStream {
    pipeline: Arc<VideoPipeline>,
    writer: JoinHandle<()>,
    rtcp: JoinHandle<()>,
    forwarder: JoinHandle<()>,
    /// Encoded picture size (browser mouse coordinates are in this space).
    video_size: (u32, u32),
}

struct AudioStream {
    pipeline: AudioPipeline,
    writer: JoinHandle<()>,
}

struct Media {
    displays: Vec<DisplayInfo>,
    streams: BTreeMap<u32, DisplayStream>,
    /// Merged pipeline events: `(display index, event)`.
    events_tx: mpsc::UnboundedSender<(u32, PipelineEvent)>,
    events_rx: mpsc::UnboundedReceiver<(u32, PipelineEvent)>,
    /// Display the operator's pointer is on (input coordinates refer to it).
    current_display: u32,
    /// Video tracks bound in the answer (display `i` ↔ track `i`).
    video_tracks: usize,
    audio: Option<AudioStream>,
}

/// Media start deferred until the OS grants screen capture (macOS TCC).
struct PendingMedia {
    video_tracks: usize,
    deadline: tokio::time::Instant,
    /// Whether the browser was told over the control channel.
    notified: bool,
}

/// Outcome of one `poll_pending_media` tick.
enum PendingPoll {
    Idle,
    Started,
    TimedOut,
    Failed,
}

/// How long a connected session waits for the Screen Recording permission before giving up.
const PERMISSION_WAIT: Duration = Duration::from_secs(60);

struct Session {
    deps: SessionDeps,
    id: String,
    /// Set while waiting for the screen-capture permission after the peer connected.
    pending_media: Option<PendingMedia>,
    operator: OperatorInfo,
    cfg: AgentConfig,
    peer: Arc<Peer>,
    input: Arc<Mutex<Option<Box<dyn InputHandler>>>>,
    /// Live input gate (`cfg.allow_input && !control_paused`; the reader task checks it every event).
    input_allowed: Arc<AtomicBool>,
    /// Device user pressed "Pause control" on the session bar.
    control_paused: bool,
    /// `AnnotationsDisabled` was already sent to this operator (once per session).
    annotations_disabled_sent: bool,
    /// An annotation was forwarded to the overlay (so teardown must remove overlays).
    annotations_used: bool,
    control: Option<Arc<dyn DataChannel>>,
    media: Option<Media>,
    channel_tx: mpsc::UnboundedSender<ChannelEvent>,
    readers: Vec<JoinHandle<()>>,
    /// Newest pointer position from the unreliable `input-fast` channel.
    fast_move: Arc<LatestMove>,
    /// Set while a cursor source thread streams shape/position updates.
    cursor_stop: Option<Arc<AtomicBool>>,
    /// The capture omits the system cursor (client-side cursor active).
    client_cursor: bool,
    clipboard: Option<ClipboardWatch>,
    /// Rich clipboard content (image / files) announced to the operator but not yet pulled.
    pending_clipboard: Option<ClipboardContent>,
    transfers: Option<TransferManager>,
    transfer_notices_tx: mpsc::UnboundedSender<TransferNotice>,
    transfer_notices_rx: mpsc::UnboundedReceiver<TransferNotice>,
    chat: ChatModel,
    chat_ui: Option<Box<dyn ChatHandle>>,
    indicator: Option<Box<dyn IndicatorHandle>>,
    /// Set when the local user pressed "Disconnect" on the indicator / chat window.
    user_ended: Arc<AtomicBool>,
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
    tracing::info!(session = %session_id, operator = %req.operator.name, mode = ?cfg.mode, role = ?req.role, "session requested");

    if req.role == SessionRole::Observer {
        // Observer fan-out is not implemented yet: decline cleanly.
        tracing::warn!(session = %session_id, shadow_of = ?req.shadow_of, "observer sessions are not supported yet");
        report(SessionState::Ended, Some(EndReason::Error));
        return;
    }

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
    let display_count = deps
        .media
        .list_displays()
        .map(|d| d.len())
        .unwrap_or(1)
        .max(1);
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
    let mut peer = match Peer::new(codec, &req.ice_servers, peer_tx).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(session = %session_id, "creating peer connection: {e:#}");
            report(SessionState::Ended, Some(EndReason::Error));
            return;
        }
    };
    let want_audio = cfg.allow_audio && deps.media.audio_available();
    let answer = match peer
        .answer(req.offer.sdp.clone(), display_count, want_audio)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(session = %session_id, "answering offer: {e:#}");
            peer.close().await;
            report(SessionState::Ended, Some(EndReason::Error));
            return;
        }
    };
    let peer = Arc::new(peer);
    hub.send(AgentToConsole::SessionAnswer {
        session_id: session_id.clone(),
        answer: SessionDescription {
            kind: "answer".into(),
            sdp: answer.sdp,
        },
        codec,
    });
    report(SessionState::Connecting, None);
    tracing::info!(
        session = %session_id,
        ?codec,
        video_tracks = answer.video_tracks,
        audio = answer.audio,
        "answer sent"
    );

    let (channel_tx, mut channel_rx) = mpsc::unbounded_channel();
    let (notices_tx, notices_rx) = mpsc::unbounded_channel();
    let cfg_allow_input = cfg.allow_input;
    let mut session = Session {
        deps: deps.clone(),
        id: session_id.clone(),
        pending_media: None,
        operator: req.operator.clone(),
        cfg,
        peer,
        input: Arc::new(Mutex::new(None)),
        input_allowed: Arc::new(AtomicBool::new(cfg_allow_input)),
        control_paused: false,
        annotations_disabled_sent: false,
        annotations_used: false,
        control: None,
        media: None,
        channel_tx,
        readers: Vec::new(),
        fast_move: Arc::new(LatestMove::default()),
        cursor_stop: None,
        client_cursor: false,
        clipboard: None,
        pending_clipboard: None,
        transfers: None,
        transfer_notices_tx: notices_tx,
        transfer_notices_rx: notices_rx,
        chat: ChatModel::new(req.operator.clone()),
        chat_ui: None,
        indicator: None,
        user_ended: Arc::new(AtomicBool::new(false)),
    };
    for c in pending.drain(..) {
        session.add_candidate(&c).await;
    }

    // ── main loop ─────────────────────────────────────────────────────────────────
    let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut transfer_tick = tokio::time::interval(Duration::from_secs(5));
    transfer_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                Some(m) => m.events_rx.recv().await,
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
                Some(SessionCommand::UpdateConfig(new)) => session.apply_config(new).await,
                Some(SessionCommand::SetControlPaused(p)) => session.set_control_paused(p).await,
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
                                if let Err(e) = session.on_connected(answer.video_tracks).await {
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
                            break if session.user_ended.load(Ordering::Relaxed) {
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
                    session.notify_pending_permission().await;
                    if session.control_paused {
                        // Survives an operator reconnect within the same session.
                        session.send_control(ControlMessage::ControlPaused { paused: true }).await;
                    }
                }
                Some(ChannelEvent::Control(msg)) => session.on_control(msg).await,
                Some(ChannelEvent::ControlClosed) => session.control = None,
                Some(ChannelEvent::FilesOpen(dc)) => session.on_files_open(dc),
                Some(ChannelEvent::FilesText(msg)) => session.on_files_message(msg).await,
                Some(ChannelEvent::FilesBinary(bytes)) => {
                    if let Some(t) = session.transfers.as_mut() {
                        t.handle_chunk(&bytes).await;
                    }
                }
                Some(ChannelEvent::FilesClosed) => {
                    if let Some(mut t) = session.transfers.take() {
                        t.cancel_all().await;
                    }
                }
                Some(ChannelEvent::ChatFromDevice(text)) => session.on_chat_from_device(text).await,
                Some(ChannelEvent::UserEnd) => break EndReason::DeviceUserClosed,
                Some(ChannelEvent::Outbound(msg)) => session.send_control(msg).await,
                None => {}
            },
            Some(notice) = session.transfer_notices_rx.recv() => session.on_transfer_notice(notice).await,
            Some((display_idx, ev)) = pipeline_event => match ev {
                PipelineEvent::Failed(msg) => {
                    tracing::error!(session = %session.id, display = display_idx, "video pipeline failed: {msg}");
                    break EndReason::Error;
                }
                PipelineEvent::Started { display_index, width, height, encoded_width, encoded_height, codec, hardware } => {
                    tracing::info!(session = %session.id, display_index, width, height, encoded_width, encoded_height, ?codec, hardware, "pipeline started");
                    if let Some(m) = session.media.as_mut() {
                        if let Some(s) = m.streams.get_mut(&display_idx) {
                            s.video_size = (encoded_width, encoded_height);
                        }
                    }
                    session.update_input_display();
                    session.send_display_info().await;
                }
            },
            Some(content) = clipboard_event => session.on_clipboard_change(content).await,
            _ = stats_tick.tick() => {
                session.send_stats().await;
                match session.poll_pending_media().await {
                    PendingPoll::Idle | PendingPoll::Started => {}
                    PendingPoll::TimedOut => {
                        tracing::error!(session = %session.id, "screen recording permission not granted within {}s", PERMISSION_WAIT.as_secs());
                        break EndReason::Error;
                    }
                    PendingPoll::Failed => break EndReason::Error,
                }
            }
            _ = transfer_tick.tick() => {
                if let Some(t) = session.transfers.as_mut() {
                    t.tick();
                }
            }
            _ = disconnect_sleep => {
                tracing::warn!(session = %session.id, "connection stayed disconnected; ending");
                break EndReason::ConnectionFailed;
            }
        }
    };

    if session.user_ended.load(Ordering::Relaxed) {
        session
            .send_control(ControlMessage::SessionEndedByUser)
            .await;
    }
    let reason =
        if session.user_ended.load(Ordering::Relaxed) && reason == EndReason::OperatorClosed {
            EndReason::DeviceUserClosed
        } else {
            reason
        };
    session.teardown().await;
    let (total, visible) = crate::platform::window_counts();
    tracing::info!(session = %session_id, ?reason, total, visible, "session ended");
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
            // A policy change before the session is up is picked up from the shared config when
            // the session builds; ignore it here.
            Some(SessionCommand::UpdateConfig(_)) => {}
            // No input yet before the session is up; the pause applies once it exists.
            Some(SessionCommand::SetControlPaused(_)) => {}
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

    /// Live-apply a new effective config: tighten input/audio/clipboard immediately.
    async fn apply_config(&mut self, new: AgentConfig) {
        let old = self.cfg.clone();
        self.cfg = new.clone();
        self.input_allowed
            .store(new.allow_input && !self.control_paused, Ordering::Relaxed);
        if old.allow_input && !new.allow_input {
            tracing::info!(session = %self.id, "input disabled by the device user");
            if let Some(h) = self.input.lock().as_mut() {
                h.release_all();
            }
            // Notice to the browser: refresh display info (also carries the audio state). The
            // operator's injected input now has no effect.
            self.send_display_info().await;
        }
        if !new.allow_audio {
            self.set_audio(false).await;
        }
        if old.allow_clipboard && !new.allow_clipboard {
            if let Some(c) = self.clipboard.take() {
                c.stop();
            }
            self.pending_clipboard = None;
        }
        if old.allow_annotations && !new.allow_annotations {
            tracing::info!(session = %self.id, "annotations disabled by the device user");
            self.deps.annotations.session_ended();
            self.annotations_used = false;
            self.annotations_disabled_sent = false;
            self.reject_annotation().await;
        }
        self.send_display_info().await;
    }

    /// Screen annotations are independent of the input permission and of the pause switch:
    /// they only need `allow_annotations` and a UI able to draw.
    async fn on_annotation(&mut self, ev: crate::annotate::AnnotateEvent) {
        if !self.cfg.allow_annotations || !self.deps.annotations.available() {
            self.reject_annotation().await;
            return;
        }
        self.annotations_used = true;
        self.deps.annotations.apply(ev);
    }

    /// Tell the operator once that annotations cannot be shown here.
    async fn reject_annotation(&mut self) {
        if self.annotations_disabled_sent {
            return;
        }
        self.annotations_disabled_sent = true;
        tracing::info!(
            session = %self.id,
            allowed = self.cfg.allow_annotations,
            ui = self.deps.annotations.available(),
            "annotations not available; telling the operator"
        );
        self.send_control(ControlMessage::AnnotationsDisabled).await;
    }

    /// Device-side emergency switch: pause / resume remote keyboard & mouse control. Screen
    /// sharing and chat continue; the operator is told and cannot lift the pause.
    async fn set_control_paused(&mut self, paused: bool) {
        if self.control_paused == paused {
            return;
        }
        self.control_paused = paused;
        self.input_allowed
            .store(self.cfg.allow_input && !paused, Ordering::Relaxed);
        if paused {
            tracing::info!(session = %self.id, "remote control paused by the device user");
            if let Some(h) = self.input.lock().as_mut() {
                h.release_all();
            }
        } else {
            tracing::info!(session = %self.id, "remote control resumed by the device user");
        }
        self.send_control(ControlMessage::ControlPaused { paused })
            .await;
        self.event(SessionEvent::ControlPaused { paused });
        crate::app::set_control_paused_state(paused);
    }

    /// Report an in-session event to the console.
    fn event(&self, event: SessionEvent) {
        self.deps.hub.send(AgentToConsole::SessionEvent {
            session_id: self.id.clone(),
            event,
            ts_ms: crate::chat::now_ms(),
        });
    }

    fn disconnect_callback(&self) -> Arc<dyn Fn() + Send + Sync> {
        let user_ended = Arc::clone(&self.user_ended);
        let hub = self.deps.hub.clone();
        let sid = self.id.clone();
        let tx = self.channel_tx.clone();
        Arc::new(move || {
            user_ended.store(true, Ordering::Relaxed);
            // End locally too: the session loop exits right away instead of waiting for the
            // console / peer to react.
            let _ = tx.send(ChannelEvent::UserEnd);
            // Route through the console so its bookkeeping (and the browser) see the end;
            // the session loop also exits when the peer connection closes.
            hub.send(AgentToConsole::SessionState {
                session_id: sid.clone(),
                state: SessionState::Ended,
                reason: Some(EndReason::DeviceUserClosed),
            });
        })
    }

    /// Connection established: start the primary display's pipeline, the clipboard watcher,
    /// the indicator and the (hidden) chat window.
    async fn on_connected(&mut self, video_tracks: usize) -> Result<()> {
        // macOS: without the Screen Recording permission capture cannot start. Rather than
        // failing the session, keep it connected, ask the OS / the person at the device, and
        // start media as soon as the permission arrives (see `poll_pending_media`).
        if !self.deps.media.capture_permission_granted() {
            tracing::warn!(session = %self.id, "screen recording permission missing; waiting up to {}s", PERMISSION_WAIT.as_secs());
            crate::platform::request_screen_capture();
            crate::app::permission_needed();
            self.deps.hub.send(AgentToConsole::Log {
                level: "warn".into(),
                message:
                    "screen recording permission not granted on the device; waiting for the user"
                        .into(),
            });
            self.pending_media = Some(PendingMedia {
                video_tracks,
                deadline: tokio::time::Instant::now() + PERMISSION_WAIT,
                notified: false,
            });
            self.notify_pending_permission().await;
            return Ok(());
        }
        let media = Arc::clone(&self.deps.media);
        let displays = tokio::task::spawn_blocking({
            let media = Arc::clone(&media);
            move || media.list_displays()
        })
        .await??;
        let primary = displays
            .iter()
            .find(|d| d.primary)
            .or_else(|| displays.first())
            .map(|d| d.index)
            .unwrap_or(0);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        self.start_cursor_source();
        self.media = Some(Media {
            displays,
            streams: BTreeMap::new(),
            events_tx,
            events_rx,
            current_display: primary,
            video_tracks,
            audio: None,
        });
        let first = if (primary as usize) < video_tracks {
            primary
        } else {
            0
        };
        self.start_display(first).await?;
        self.update_input_display();

        if self.cfg.allow_clipboard {
            self.clipboard = Some(
                self.deps
                    .clipboard
                    .start_watch(self.cfg.allow_file_transfer),
            );
        }
        // The branded session banner is always shown (the person at the device must always see
        // and be able to end the session).
        match self
            .deps
            .indicator
            .show(&self.operator, self.disconnect_callback())
        {
            Ok(handle) => self.indicator = Some(handle),
            Err(e) => tracing::warn!("session indicator unavailable: {e:#}"),
        }
        // Chat window: created now, shown on the first message.
        let on_send: Arc<dyn Fn(String) + Send + Sync> = {
            let tx = self.channel_tx.clone();
            Arc::new(move |text| {
                let _ = tx.send(ChannelEvent::ChatFromDevice(text));
            })
        };
        match self
            .deps
            .chat
            .open(&self.operator, on_send, self.disconnect_callback())
        {
            Ok(handle) => {
                handle.set_visible(false);
                self.chat_ui = Some(handle);
            }
            Err(e) => tracing::warn!("chat window unavailable: {e:#}"),
        }
        let (total, visible) = crate::platform::window_counts();
        tracing::info!(total, visible, "session ui opened");
        Ok(())
    }

    // ── displays ─────────────────────────────────────────────────────────────────

    async fn start_display(&mut self, index: u32) -> Result<()> {
        let media = self.media.as_mut().context("media not started")?;
        if media.streams.contains_key(&index) {
            return Ok(());
        }
        if !media.displays.iter().any(|d| d.index == index) {
            anyhow::bail!("unknown display {index}");
        }
        if index as usize >= media.video_tracks {
            anyhow::bail!(
                "display {index} has no video track (browser offered {} video m-lines)",
                media.video_tracks
            );
        }
        let track_index = index as usize;
        let factory = Arc::clone(&self.deps.media);
        let (frame_tx, mut frame_rx) = mpsc::channel(2);
        let (pev_tx, mut pev_rx) = mpsc::unbounded_channel();
        let pipeline_cfg = PipelineConfig {
            display_index: index,
            codec: self.peer.codec(),
            max_fps: self.cfg.max_fps.clamp(1, 240),
            max_bitrate_kbps: self.cfg.max_bitrate_kbps.max(100),
            // With a client-side cursor the capture omits the system cursor: cursor-only
            // changes then produce no frames at all and the browser draws it lag-free.
            show_cursor: !self.client_cursor,
            viewport: None,
        };
        let pipeline = tokio::task::spawn_blocking({
            let factory = Arc::clone(&factory);
            move || VideoPipeline::start(factory, pipeline_cfg, frame_tx, pev_tx)
        })
        .await??;
        let pipeline = Arc::new(pipeline);

        let merged = media.events_tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(ev) = pev_rx.recv().await {
                if merged.send((index, ev)).is_err() {
                    break;
                }
            }
        });

        let track = self
            .peer
            .video(track_index)
            .context("video track missing")?;
        let payload_type = track.payload_type().await;
        let ssrc = track.ssrc().await;
        let fps = self.cfg.max_fps.clamp(1, 240) as f64;
        tracing::info!(session = %self.id, display = index, track = track_index, payload_type, ssrc, "video track ready");

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
                    let Some(track) = peer.video(track_index) else {
                        break;
                    };
                    if let Err(e) = track.write(payload_type, ssrc, frame.data, duration).await {
                        tracing::debug!(session = %sid, display = index, "write_frame: {e:#}");
                    }
                }
            }
        });

        let rtcp = tokio::spawn({
            let peer = Arc::clone(&self.peer);
            let keyframe = pipeline.keyframe_requester();
            let pipeline = Arc::clone(&pipeline);
            let cap = self.cfg.max_bitrate_kbps.max(100);
            let max_fps = self.cfg.max_fps.clamp(1, 240);
            let sid = self.id.clone();
            async move {
                let mut cc = AimdController::new(cap, std::time::Instant::now());
                let mut fps = max_fps;
                let mut ticker = tokio::time::interval(AimdController::INTERVAL);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    let Some(track) = peer.video(track_index) else {
                        break;
                    };
                    tokio::select! {
                        ev = track.poll_rtcp() => match ev {
                            Some(ev) => {
                                if is_keyframe_request(&ev) {
                                    keyframe.request();
                                }
                                peer::feed_congestion(&ev, &mut cc);
                            }
                            None => tokio::time::sleep(Duration::from_millis(200)).await,
                        },
                        _ = ticker.tick() => {
                            if let Some(target) = cc.evaluate(std::time::Instant::now()) {
                                tracing::debug!(session = %sid, display = index, target, "congestion control: new target bitrate");
                                pipeline.set_target_bitrate(target);
                            }
                            let want = cc.fps_for(max_fps);
                            if want != fps {
                                fps = want;
                                tracing::info!(session = %sid, display = index, fps, "congestion control: frame rate");
                                pipeline.set_target_fps(fps);
                            }
                        }
                    }
                }
            }
        });

        let video_size = {
            let s = pipeline.stats().borrow().clone();
            (s.encoded_width, s.encoded_height)
        };
        let media = self.media.as_mut().context("media not started")?;
        media.streams.insert(
            index,
            DisplayStream {
                pipeline,
                writer,
                rtcp,
                forwarder,
                video_size,
            },
        );
        Ok(())
    }

    async fn stop_display(&mut self, index: u32) {
        let Some(media) = self.media.as_mut() else {
            return;
        };
        if let Some(s) = media.streams.remove(&index) {
            s.writer.abort();
            s.rtcp.abort();
            s.forwarder.abort();
            let pipeline = s.pipeline;
            let _ = tokio::task::spawn_blocking(move || drop(pipeline)).await;
        }
    }

    /// Stream exactly the given displays (unknown / unbindable ones are skipped).
    async fn set_active_displays(&mut self, indices: &[u32]) {
        let Some(media) = self.media.as_ref() else {
            return;
        };
        let wanted: Vec<u32> = indices
            .iter()
            .copied()
            .filter(|i| media.displays.iter().any(|d| d.index == *i))
            .filter(|i| (*i as usize) < media.video_tracks)
            .collect();
        if wanted.is_empty() {
            tracing::warn!(session = %self.id, ?indices, "no valid display in set_active_displays");
            return;
        }
        let current: Vec<u32> = media.streams.keys().copied().collect();
        for idx in current {
            if !wanted.contains(&idx) {
                self.stop_display(idx).await;
            }
        }
        for idx in &wanted {
            if let Err(e) = self.start_display(*idx).await {
                tracing::warn!(session = %self.id, display = idx, "starting display: {e:#}");
            }
        }
        let active = self.active_displays();
        self.event(SessionEvent::DisplaysChanged {
            active: active.clone(),
        });
        self.send_display_info().await;
    }

    fn active_displays(&self) -> Vec<u32> {
        self.media
            .as_ref()
            .map(|m| m.streams.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Point the input handler at the current display and its encoded picture size.
    fn update_input_display(&mut self) {
        let Some(media) = self.media.as_ref() else {
            return;
        };
        let index = media.current_display;
        let Some(d) = media.displays.iter().find(|d| d.index == index).cloned() else {
            return;
        };
        let video_size = media
            .streams
            .get(&index)
            .map(|s| s.video_size)
            .filter(|(w, h)| *w > 0 && *h > 0)
            .unwrap_or((d.width, d.height));
        if let Some(handler) = self.input.lock().as_mut() {
            handler.set_display(&d, video_size);
        }
    }

    // ── audio ────────────────────────────────────────────────────────────────────

    async fn set_audio(&mut self, enabled: bool) {
        let Some(media) = self.media.as_mut() else {
            return;
        };
        if !enabled {
            if let Some(a) = media.audio.take() {
                a.writer.abort();
                let _ = tokio::task::spawn_blocking(move || a.pipeline.stop()).await;
                self.event(SessionEvent::AudioChanged { enabled: false });
                self.send_display_info().await;
            }
            return;
        }
        if media.audio.is_some() {
            return;
        }
        if !self.cfg.allow_audio {
            tracing::info!(session = %self.id, "audio requested but disabled by configuration");
            return;
        }
        let Some(track) = self.peer.audio() else {
            tracing::info!(session = %self.id, "audio requested but no audio track was negotiated");
            return;
        };
        let payload_type = track.payload_type().await;
        let ssrc = track.ssrc().await;
        let factory = Arc::clone(&self.deps.media);
        let (tx, mut rx) = mpsc::channel::<AudioPacket>(8);
        let started = tokio::task::spawn_blocking(move || -> Result<AudioPipeline> {
            let source = factory.create_audio_source()?;
            AudioPipeline::start(source, tx)
        })
        .await;
        let pipeline = match started {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                tracing::warn!(session = %self.id, "audio capture unavailable: {e:#}");
                return;
            }
            Err(e) => {
                tracing::warn!(session = %self.id, "audio task: {e}");
                return;
            }
        };
        let writer = tokio::spawn({
            let peer = Arc::clone(&self.peer);
            async move {
                while let Some(p) = rx.recv().await {
                    let Some(track) = peer.audio() else { break };
                    if let Err(e) = track.write(payload_type, ssrc, p.data, p.duration).await {
                        tracing::debug!("write audio: {e:#}");
                    }
                }
            }
        });
        if let Some(media) = self.media.as_mut() {
            media.audio = Some(AudioStream { pipeline, writer });
        }
        tracing::info!(session = %self.id, payload_type, ssrc, "audio streaming");
        self.event(SessionEvent::AudioChanged { enabled: true });
        self.send_display_info().await;
    }

    // ── data channels ────────────────────────────────────────────────────────────

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
            FAST_INPUT_CHANNEL_LABEL => {
                self.ensure_input_handler();
                let input = Arc::clone(&self.input);
                let allow = Arc::clone(&self.input_allowed);
                let fast = Arc::clone(&self.fast_move);
                let sid = self.id.clone();
                // Reader: keep only the newest position.
                self.readers.push(tokio::spawn({
                    let fast = Arc::clone(&fast);
                    async move {
                        while let Some(ev) = dc.poll().await {
                            match ev {
                                DataChannelEvent::OnMessage(msg) => {
                                    if let Ok(InputEvent::MouseMove { x, y }) =
                                        serde_json::from_slice::<InputEvent>(&msg.data)
                                    {
                                        fast.push(x, y);
                                    }
                                }
                                DataChannelEvent::OnClose => break,
                                _ => {}
                            }
                        }
                    }
                }));
                // Applier: drain the slot at the injector rate (coalesces bursts).
                let notify = fast.notify();
                self.readers.push(tokio::spawn(async move {
                    loop {
                        notify.notified().await;
                        while let Some((x, y)) = fast.take() {
                            if allow.load(Ordering::Relaxed) {
                                if let Some(h) = input.lock().as_mut() {
                                    if let Err(e) = h.handle(InputEvent::MouseMove { x, y }) {
                                        tracing::debug!(session = %sid, "fast input: {e:#}");
                                    }
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(4)).await;
                        }
                    }
                }));
            }
            INPUT_CHANNEL_LABEL => {
                self.ensure_input_handler();
                let input = Arc::clone(&self.input);
                let allow = Arc::clone(&self.input_allowed);
                let fast = Arc::clone(&self.fast_move);
                let sid = self.id.clone();
                self.readers.push(tokio::spawn(async move {
                    while let Some(ev) = dc.poll().await {
                        match ev {
                            DataChannelEvent::OnMessage(msg) => {
                                if !allow.load(Ordering::Relaxed) {
                                    continue;
                                }
                                match serde_json::from_slice::<InputEvent>(&msg.data) {
                                    Ok(event) => {
                                        if let Some(h) = input.lock().as_mut() {
                                            // A click must land where the operator last
                                            // saw the pointer: flush the fast channel first.
                                            if !matches!(event, InputEvent::MouseMove { .. }) {
                                                if let Some((x, y)) = fast.take() {
                                                    let _ =
                                                        h.handle(InputEvent::MouseMove { x, y });
                                                }
                                            }
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
            FILES_CHANNEL_LABEL => {
                let tx = self.channel_tx.clone();
                let sid = self.id.clone();
                self.readers.push(tokio::spawn(async move {
                    while let Some(ev) = dc.poll().await {
                        match ev {
                            DataChannelEvent::OnOpen => {
                                let _ = tx.send(ChannelEvent::FilesOpen(Arc::clone(&dc)));
                            }
                            DataChannelEvent::OnMessage(msg) => {
                                // Text frames are JSON control messages; binary frames are
                                // chunks (version byte 1 first). A JSON frame always starts
                                // with `{`, so the first byte disambiguates when the
                                // transport does not flag string-ness.
                                let data = msg.data;
                                if data.first() == Some(&b'{') {
                                    match serde_json::from_slice::<FileMessage>(&data) {
                                        Ok(m) => {
                                            let _ = tx.send(ChannelEvent::FilesText(m));
                                        }
                                        Err(e) => {
                                            tracing::debug!(session = %sid, "bad files message: {e}")
                                        }
                                    }
                                } else {
                                    let _ = tx.send(ChannelEvent::FilesBinary(data.freeze()));
                                }
                            }
                            DataChannelEvent::OnClose => {
                                let _ = tx.send(ChannelEvent::FilesClosed);
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

    fn on_files_open(&mut self, dc: Arc<dyn DataChannel>) {
        let dir = self
            .cfg
            .transfer_dir
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(TransferConfig::default_dir);
        let cfg = TransferConfig {
            allow_files: self.cfg.allow_file_transfer,
            allow_clipboard: self.cfg.allow_clipboard,
            dir,
        };
        let sink = files::DataChannelSink::shared(dc);
        self.transfers = Some(TransferManager::new(
            cfg,
            sink,
            self.transfer_notices_tx.clone(),
        ));
    }

    async fn on_files_message(&mut self, msg: FileMessage) {
        if matches!(msg, FileMessage::RequestClipboard) {
            self.offer_clipboard().await;
            return;
        }
        if let Some(t) = self.transfers.as_mut() {
            t.handle_message(msg).await;
        }
    }

    async fn on_transfer_notice(&mut self, notice: TransferNotice) {
        match notice {
            TransferNotice::Started {
                token,
                name,
                size,
                kind,
                direction,
                offset,
            } => self.event(SessionEvent::TransferStarted {
                token,
                name,
                size,
                kind,
                direction,
                offset,
            }),
            TransferNotice::Completed {
                token,
                name,
                size,
                direction,
                path,
                ..
            } => self.event(SessionEvent::TransferCompleted {
                token,
                name,
                size,
                direction,
                path: path.map(|p| p.display().to_string()),
            }),
            TransferNotice::Failed {
                token,
                name,
                reason,
            } => self.event(SessionEvent::TransferFailed {
                token,
                name,
                reason,
            }),
            TransferNotice::ClipboardImage(path) => {
                let p = path.clone();
                let backend = Arc::clone(&self.deps.clipboard);
                let res = tokio::task::spawn_blocking(move || backend.set_image_from_png(&p)).await;
                match res {
                    Ok(Ok((w, h))) => {
                        if let (Some(watch), Ok(png)) =
                            (self.clipboard.as_ref(), std::fs::read(&path))
                        {
                            watch.mark_own(&ClipboardContent::Image {
                                png,
                                width: w,
                                height: h,
                            });
                        }
                        self.event(SessionEvent::ClipboardSync {
                            direction: TransferDirection::ToDevice,
                            summary: format!("image {w}×{h}"),
                        });
                    }
                    Ok(Err(e)) => tracing::warn!("placing clipboard image: {e:#}"),
                    Err(e) => tracing::warn!("clipboard task: {e}"),
                }
            }
            TransferNotice::ClipboardFiles(paths) => {
                let count = paths.len();
                let p = paths.clone();
                let backend = Arc::clone(&self.deps.clipboard);
                let res = tokio::task::spawn_blocking(move || backend.set_files(&p)).await;
                match res {
                    Ok(Ok(())) => {
                        if let Some(watch) = self.clipboard.as_ref() {
                            watch.mark_own(&ClipboardContent::Files(paths));
                        }
                        self.event(SessionEvent::ClipboardSync {
                            direction: TransferDirection::ToDevice,
                            summary: format!("{count} file(s)"),
                        });
                    }
                    Ok(Err(e)) => tracing::warn!("placing clipboard files: {e:#}"),
                    Err(e) => tracing::warn!("clipboard task: {e}"),
                }
            }
        }
    }

    // ── clipboard ────────────────────────────────────────────────────────────────

    async fn on_clipboard_change(&mut self, content: ClipboardContent) {
        match content {
            ClipboardContent::Text(text) => {
                self.send_control(ControlMessage::ClipboardChanged { text })
                    .await;
            }
            ClipboardContent::Image { width, height, .. } => {
                let total = content.total_bytes();
                let name = format!("clipboard-{}.png", crate::chat::now_ms());
                self.pending_clipboard = Some(content);
                self.send_control(ControlMessage::ClipboardAvailable {
                    kind: ClipboardKind::Image,
                    names: vec![name],
                    total_bytes: total,
                })
                .await;
                tracing::debug!(session = %self.id, width, height, "clipboard image available");
            }
            ClipboardContent::Files(ref paths) => {
                if !self.cfg.allow_file_transfer {
                    return;
                }
                let names: Vec<String> = paths
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .collect();
                if names.is_empty() {
                    return;
                }
                let total = content.total_bytes();
                self.pending_clipboard = Some(content);
                self.send_control(ControlMessage::ClipboardAvailable {
                    kind: ClipboardKind::Files,
                    names,
                    total_bytes: total,
                })
                .await;
            }
        }
    }

    /// Operator asked for the announced clipboard image / files.
    async fn offer_clipboard(&mut self) {
        let Some(content) = self.pending_clipboard.clone() else {
            return;
        };
        let Some(transfers) = self.transfers.as_mut() else {
            return;
        };
        match content {
            ClipboardContent::Image { png, width, height } => {
                let name = format!("clipboard-{}.png", crate::chat::now_ms());
                match transfers
                    .offer_bytes(name, Bytes::from(png), TransferKind::ClipboardImage, None)
                    .await
                {
                    Ok(_) => self.event(SessionEvent::ClipboardSync {
                        direction: TransferDirection::ToOperator,
                        summary: format!("image {width}×{height}"),
                    }),
                    Err(e) => tracing::warn!("offering clipboard image: {e:#}"),
                }
            }
            ClipboardContent::Files(paths) => {
                let group = uuid::Uuid::new_v4().simple().to_string();
                let mut offered = 0usize;
                for p in paths.iter().filter(|p| p.is_file()) {
                    match transfers
                        .offer_file(p, TransferKind::ClipboardFiles, Some(group.clone()), None)
                        .await
                    {
                        Ok(_) => offered += 1,
                        Err(e) => tracing::warn!("offering {}: {e:#}", p.display()),
                    }
                }
                if offered > 0 {
                    self.event(SessionEvent::ClipboardSync {
                        direction: TransferDirection::ToOperator,
                        summary: format!("{offered} file(s)"),
                    });
                }
            }
            ClipboardContent::Text(_) => {}
        }
    }

    // ── chat ─────────────────────────────────────────────────────────────────────

    async fn on_chat_from_operator(&mut self, text: String, ts_ms: u64) {
        let Some(line) = self.chat.push(ChatParty::Operator, &text, Some(ts_ms)) else {
            return;
        };
        if let Some(ui) = self.chat_ui.as_ref() {
            ui.push_line(&line);
            ui.set_visible(true);
        }
        self.event(SessionEvent::Chat {
            from: ChatParty::Operator,
            text: line.text,
        });
    }

    async fn on_chat_from_device(&mut self, text: String) {
        let Some(line) = self.chat.push(ChatParty::Device, &text, None) else {
            return;
        };
        if let Some(ui) = self.chat_ui.as_ref() {
            ui.push_line(&line);
        }
        self.send_control(ControlMessage::Chat {
            from: ChatParty::Device,
            text: line.text.clone(),
            ts_ms: line.ts_ms,
        })
        .await;
        self.event(SessionEvent::Chat {
            from: ChatParty::Device,
            text: line.text,
        });
    }

    // ── control channel ──────────────────────────────────────────────────────────

    async fn on_control(&mut self, msg: ControlMessage) {
        match msg {
            ControlMessage::SelectDisplay { index } => {
                let known = self
                    .media
                    .as_ref()
                    .map(|m| m.displays.iter().any(|d| d.index == index))
                    .unwrap_or(false);
                if !known {
                    tracing::warn!(session = %self.id, index, "unknown display");
                    return;
                }
                if let Some(m) = self.media.as_mut() {
                    m.current_display = index;
                }
                let streaming = self.active_displays().contains(&index);
                if !streaming {
                    // Single-tile viewer semantics: switch the stream to this display.
                    self.set_active_displays(&[index]).await;
                } else {
                    // Multi-tile: the pointer moved to another streaming tile.
                    self.send_display_info().await;
                }
                self.update_input_display();
            }
            ControlMessage::SetActiveDisplays { indices } => {
                self.set_active_displays(&indices).await;
                if let Some(m) = self.media.as_mut() {
                    if !m.streams.contains_key(&m.current_display) {
                        if let Some(first) = m.streams.keys().next().copied() {
                            m.current_display = first;
                        }
                    }
                }
                self.update_input_display();
            }
            ControlMessage::SetAudio { enabled } => self.set_audio(enabled).await,
            ControlMessage::Chat { from, text, ts_ms } => {
                if from == ChatParty::Operator {
                    self.on_chat_from_operator(text, ts_ms).await;
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
                    for s in m.streams.values() {
                        s.pipeline.set_quality(fps, kbps);
                    }
                }
            }
            ControlMessage::RequestKeyframe => {
                if let Some(m) = self.media.as_ref() {
                    for s in m.streams.values() {
                        s.pipeline.request_keyframe();
                    }
                }
            }
            ControlMessage::SetViewport {
                display,
                width,
                height,
            } => {
                if let Some(m) = self.media.as_ref() {
                    if let Some(s) = m.streams.get(&display) {
                        let viewport = match (width, height) {
                            (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
                            _ => None,
                        };
                        s.pipeline.set_viewport(viewport);
                    }
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
                if let Some(watch) = self.clipboard.as_ref() {
                    watch.mark_own(&ClipboardContent::Text(text.clone()));
                }
                let backend = Arc::clone(&self.deps.clipboard);
                let res = tokio::task::spawn_blocking(move || backend.set_text(&text)).await;
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("setting clipboard: {e:#}"),
                    Err(e) => tracing::warn!("clipboard task: {e}"),
                }
            }
            ControlMessage::AnnotateStroke {
                id,
                display,
                color,
                width,
                points,
            } => {
                self.on_annotation(crate::annotate::AnnotateEvent::Stroke {
                    id,
                    display,
                    color,
                    width,
                    points,
                })
                .await
            }
            ControlMessage::AnnotateEnd { id } => {
                self.on_annotation(crate::annotate::AnnotateEvent::End { id })
                    .await
            }
            ControlMessage::AnnotatePointer {
                display,
                point,
                color,
            } => {
                self.on_annotation(crate::annotate::AnnotateEvent::Pointer {
                    display,
                    point,
                    color,
                })
                .await
            }
            ControlMessage::AnnotateClear => {
                self.on_annotation(crate::annotate::AnnotateEvent::Clear)
                    .await
            }
            // agent → browser messages are never expected inbound
            ControlMessage::DisplayInfo { .. }
            | ControlMessage::ClipboardChanged { .. }
            | ControlMessage::ClipboardAvailable { .. }
            | ControlMessage::Stats { .. }
            | ControlMessage::SessionEndedByUser => {}
            // observer notifications are console → browser only; ignore anything else
            other => tracing::debug!(session = %self.id, ?other, "ignoring control message"),
        }
    }

    /// Tell the browser (once, over the control channel) that media waits for a permission.
    async fn notify_pending_permission(&mut self) {
        let Some(p) = self.pending_media.as_mut() else {
            return;
        };
        if p.notified || self.control.is_none() {
            return;
        }
        p.notified = true;
        let text = format!(
            "[{}] This computer has not granted Screen Recording permission yet. The person at the device is being asked; the stream starts automatically once granted (waiting up to {} s).",
            crate::branding::product_name(),
            PERMISSION_WAIT.as_secs()
        );
        self.send_control(ControlMessage::Chat {
            from: ChatParty::Device,
            text,
            ts_ms: crate::chat::now_ms(),
        })
        .await;
    }

    /// Called every second: start media once the permission arrived, or give up at the deadline.
    async fn poll_pending_media(&mut self) -> PendingPoll {
        let Some(p) = self.pending_media.as_ref() else {
            return PendingPoll::Idle;
        };
        if self.deps.media.capture_permission_granted() {
            let video_tracks = p.video_tracks;
            self.pending_media = None;
            tracing::info!(session = %self.id, "screen recording permission granted; starting media");
            match self.on_connected(video_tracks).await {
                Ok(()) => {
                    self.send_control(ControlMessage::Chat {
                        from: ChatParty::Device,
                        text: format!(
                            "[{}] Screen Recording permission granted — starting the stream.",
                            crate::branding::product_name()
                        ),
                        ts_ms: crate::chat::now_ms(),
                    })
                    .await;
                    PendingPoll::Started
                }
                Err(e) => {
                    tracing::error!(session = %self.id, "starting media: {e:#}");
                    PendingPoll::Failed
                }
            }
        } else if tokio::time::Instant::now() >= p.deadline {
            self.send_control(ControlMessage::Chat {
                from: ChatParty::Device,
                text: format!(
                    "[{}] Screen Recording permission was not granted in time. Ending the session.",
                    crate::branding::product_name()
                ),
                ts_ms: crate::chat::now_ms(),
            })
            .await;
            PendingPoll::TimedOut
        } else {
            PendingPoll::Idle
        }
    }

    fn ensure_input_handler(&mut self) {
        if self.input.lock().is_none() {
            match (self.deps.input)() {
                Ok(h) => {
                    *self.input.lock() = Some(h);
                    self.update_input_display();
                }
                Err(e) => tracing::error!("input injection unavailable: {e:#}"),
            }
        }
    }

    /// Stream cursor shape/position updates on the control channel from a dedicated thread
    /// (client-side cursor); the capture then omits the system cursor.
    fn start_cursor_source(&mut self) {
        if self.cursor_stop.is_some() {
            return;
        }
        let Some(mut source) = self.deps.media.create_cursor_source() else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let tx = self.channel_tx.clone();
        let sid = self.id.clone();
        let stop2 = Arc::clone(&stop);
        if let Err(e) = std::thread::Builder::new()
            .name("cursor-source".into())
            .spawn(move || {
                while !stop2.load(Ordering::Relaxed) {
                    let Some(update) = source.next(Duration::from_millis(50)) else {
                        continue;
                    };
                    let msg = match update {
                        CursorUpdate::Shape {
                            id,
                            png,
                            hotspot_x,
                            hotspot_y,
                            width,
                            height,
                        } => ControlMessage::CursorShape {
                            id,
                            png_base64: {
                                use base64::Engine;
                                base64::engine::general_purpose::STANDARD.encode(png)
                            },
                            hotspot_x,
                            hotspot_y,
                            width,
                            height,
                        },
                        CursorUpdate::Position {
                            display,
                            x,
                            y,
                            shape_id,
                            visible,
                        } => ControlMessage::CursorPosition {
                            display,
                            x,
                            y,
                            shape_id,
                            visible,
                        },
                    };
                    if tx.send(ChannelEvent::Outbound(msg)).is_err() {
                        break;
                    }
                }
                tracing::debug!(session = %sid, "cursor source stopped");
            })
        {
            tracing::warn!("cursor source thread: {e:#}");
            return;
        }
        self.cursor_stop = Some(stop);
        self.client_cursor = true;
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
            active: m.streams.keys().copied().collect(),
            audio: m.audio.is_some(),
        })
        .await;
    }

    async fn send_stats(&self) {
        let Some(m) = self.media.as_ref() else { return };
        if self.control.is_none() {
            return;
        }
        for (idx, s) in &m.streams {
            let st = s.pipeline.stats().borrow().clone();
            self.send_control(ControlMessage::Stats {
                display: *idx,
                codec: st.codec,
                encoded_width: st.encoded_width,
                encoded_height: st.encoded_height,
                capture_to_encoded_ms: st.capture_to_encoded_ms,
                encode_ms: st.encode_ms,
                keyframes: st.keyframes,
                frames_skipped_idle: st.idle_refreshes,
                fps: st.fps,
                bitrate_kbps: st.bitrate_kbps,
                width: st.encoded_width,
                height: st.encoded_height,
                pipeline_ms: st.pipeline_ms,
                hardware: st.hardware,
            })
            .await;
        }
    }

    async fn teardown(&mut self) {
        if let Some(stop) = self.cursor_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(h) = self.input.lock().as_mut() {
            h.release_all();
        }
        if self.annotations_used {
            self.deps.annotations.session_ended();
        }
        self.indicator = None;
        self.chat_ui = None;
        if let Some(c) = self.clipboard.take() {
            c.stop();
        }
        if let Some(mut t) = self.transfers.take() {
            t.cancel_all().await;
        }
        if let Some(mut m) = self.media.take() {
            if let Some(a) = m.audio.take() {
                a.writer.abort();
                let _ = tokio::task::spawn_blocking(move || a.pipeline.stop()).await;
            }
            for (_, s) in std::mem::take(&mut m.streams) {
                s.writer.abort();
                s.rtcp.abort();
                s.forwarder.abort();
                let pipeline = s.pipeline;
                let _ = tokio::task::spawn_blocking(move || drop(pipeline)).await;
            }
        }
        for r in self.readers.drain(..) {
            r.abort();
        }
        self.peer.close().await;
    }
}

#[allow(dead_code)]
fn _unused(e: anyhow::Error) -> anyhow::Error {
    anyhow!("{e}")
}
