//! Thin wrapper around a webrtc-rs 0.20 `PeerConnection` for one operator session.
//!
//! The browser is always the offerer. We register exactly one video codec (the one chosen
//! for the session) plus Opus, apply the offer, then add one video track per display the
//! browser asked for and — when the offer carries a `recvonly` audio m-line — one audio
//! track, answer, and forward events (ICE candidates, connection state, incoming data
//! channels) to the session task over a channel.
//!
//! **m-line ↔ display binding.** After `set_remote_description(offer)` the peer connection
//! holds one local transceiver per remote m-line, in m-line order. `add_track` fills the
//! first free transceiver of the track's kind, so the i-th video track added is bound to the
//! i-th `m=video` line of the offer. The browser therefore MUST create its `recvonly` video
//! transceivers in `DisplayInfo.index` order; track `i` carries display `i`.

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use protocol::common::{IceCandidate, IceServer, VideoCodec};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_H264, MIME_TYPE_HEVC, MIME_TYPE_OPUS,
};
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::rtp_transceiver::PayloadType;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::data_channel::DataChannel;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::Track;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription, Registry,
};
use webrtc::rtp_transceiver::RtpSender;

/// Events forwarded from the peer connection to the session task.
pub enum PeerEvent {
    IceCandidate(IceCandidate),
    ConnectionState(RTCPeerConnectionState),
    DataChannel(Arc<dyn DataChannel>),
}

/// Local payload types used when registering codecs; the answer adopts the browser's.
pub const PT_H265: PayloadType = 98;
pub const PT_H264: PayloadType = 102;
pub const PT_OPUS: PayloadType = 111;

/// Codec parameters for the single video codec of a session.
pub fn video_codec_params(codec: VideoCodec) -> RTCRtpCodecParameters {
    let feedback = ["nack", "nack pli", "ccm fir", "goog-remb"]
        .iter()
        .map(|s| {
            let (typ, parameter) = s.split_once(' ').unwrap_or((s, ""));
            RTCPFeedback {
                typ: typ.to_owned(),
                parameter: parameter.to_owned(),
            }
        })
        .collect();
    match codec {
        VideoCodec::H265 => RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_HEVC.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: feedback,
            },
            payload_type: PT_H265,
        },
        VideoCodec::H264 => RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_owned(),
                rtcp_feedback: feedback,
            },
            payload_type: PT_H264,
        },
    }
}

pub fn opus_codec_params() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: PT_OPUS,
    }
}

struct Handler {
    tx: mpsc::UnboundedSender<PeerEvent>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        match event.candidate.to_json() {
            Ok(init) => {
                let _ = self.tx.send(PeerEvent::IceCandidate(IceCandidate {
                    candidate: init.candidate,
                    sdp_mid: init.sdp_mid,
                    sdp_mline_index: init.sdp_mline_index,
                    username_fragment: init.username_fragment,
                }));
            }
            Err(e) => tracing::warn!("serializing ICE candidate: {e}"),
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.tx.send(PeerEvent::ConnectionState(state));
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let _ = self.tx.send(PeerEvent::DataChannel(data_channel));
    }
}

/// One outbound track (video for one display, or the audio track).
pub struct LocalTrack {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    fallback_pt: PayloadType,
    fallback_ssrc: u32,
}

impl LocalTrack {
    async fn new(
        pc: &Arc<dyn PeerConnection>,
        kind: RtpCodecKind,
        label: String,
        codec: &RTCRtpCodecParameters,
    ) -> Result<Self> {
        let ssrc: u32 = rand::random();
        let track = Arc::new(
            TrackLocalStaticSample::new(MediaStreamTrack::new(
                "remote-agent-stream".to_owned(),
                label.clone(),
                label,
                kind,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: codec.rtp_codec.clone(),
                    ..Default::default()
                }],
            ))
            .map_err(|e| anyhow!("creating track: {e}"))?,
        );
        let sender = pc
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|e| anyhow!("adding track: {e}"))?;
        Ok(Self {
            track,
            sender,
            fallback_pt: codec.payload_type,
            fallback_ssrc: ssrc,
        })
    }

    /// Payload type negotiated for this track (falls back to the locally registered one).
    pub async fn payload_type(&self) -> PayloadType {
        match self.sender.get_parameters().await {
            Ok(p) => p
                .rtp_parameters
                .codecs
                .first()
                .map(|c| c.payload_type)
                .unwrap_or(self.fallback_pt),
            Err(e) => {
                tracing::warn!("get_parameters failed, using local payload type: {e}");
                self.fallback_pt
            }
        }
    }

    pub async fn ssrc(&self) -> u32 {
        self.track
            .ssrcs()
            .await
            .first()
            .copied()
            .unwrap_or(self.fallback_ssrc)
    }

    /// Write one sample (an Annex-B access unit or an Opus packet).
    pub async fn write(
        &self,
        payload_type: PayloadType,
        ssrc: u32,
        data: Bytes,
        duration: Duration,
    ) -> Result<()> {
        self.track
            .sample_writer(ssrc, payload_type)
            .write_sample(&Sample {
                data,
                duration,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow!("write_sample: {e}"))
    }

    /// Next RTCP feedback event for this track; `None` once the track is unbound.
    pub async fn poll_rtcp(&self) -> Option<TrackLocalEvent> {
        self.track.poll().await
    }
}

/// Result of answering the offer.
#[derive(Debug, Clone)]
pub struct Answer {
    pub sdp: String,
    /// Number of video tracks bound (≤ requested, ≤ video m-lines in the offer).
    pub video_tracks: usize,
    pub audio: bool,
}

/// One peer connection with one video track per display and an optional audio track.
pub struct Peer {
    pc: Arc<dyn PeerConnection>,
    video: Vec<LocalTrack>,
    audio: Option<LocalTrack>,
    codec: VideoCodec,
}

impl Peer {
    /// Build the peer connection (no tracks yet; see [`Peer::answer`]).
    pub async fn new(
        codec: VideoCodec,
        ice_servers: &[IceServer],
        events: mpsc::UnboundedSender<PeerEvent>,
    ) -> Result<Self> {
        let codec_params = video_codec_params(codec);
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_codec(codec_params, RtpCodecKind::Video)
            .map_err(|e| anyhow!("registering video codec: {e}"))?;
        media_engine
            .register_codec(opus_codec_params(), RtpCodecKind::Audio)
            .map_err(|e| anyhow!("registering Opus: {e}"))?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)
            .map_err(|e| anyhow!("configuring interceptors: {e}"))?;

        let servers: Vec<RTCIceServer> = ice_servers
            .iter()
            .filter(|s| !s.urls.is_empty())
            .map(|s| RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone().unwrap_or_default(),
                credential: s.credential.clone().unwrap_or_default(),
            })
            .collect();
        let config = RTCConfigurationBuilder::new()
            .with_ice_servers(servers)
            .build();

        let pc = PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(Handler { tx: events }))
            .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
            // Bounded per-channel send buffer: `DataChannel::send` blocks when a file
            // transfer fills it, which is the back-pressure the sender task relies on.
            .with_data_channel_send_buffer_limit(protocol::files::BUFFERED_HIGH_WATER as usize)
            .build()
            .await
            .map_err(|e| anyhow!("building peer connection: {e}"))?;
        Ok(Self {
            pc: Arc::new(pc),
            video: Vec::new(),
            audio: None,
            codec,
        })
    }

    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// Apply the browser's offer, bind `video_tracks` video tracks (display order) and an
    /// audio track when `want_audio` and the offer has an audio m-line, then answer.
    pub async fn answer(
        &mut self,
        offer_sdp: String,
        video_tracks: usize,
        want_audio: bool,
    ) -> Result<Answer> {
        let video_mlines = super::sdp::count_media(&offer_sdp, "video");
        let audio_mlines = super::sdp::count_media(&offer_sdp, "audio");
        let offer =
            RTCSessionDescription::offer(offer_sdp).map_err(|e| anyhow!("parsing offer: {e}"))?;
        self.pc
            .set_remote_description(offer)
            .await
            .map_err(|e| anyhow!("set_remote_description: {e}"))?;

        let n = video_tracks.min(video_mlines);
        if n == 0 {
            bail!("offer has no video m-line");
        }
        let video_codec = video_codec_params(self.codec);
        for i in 0..n {
            let t = LocalTrack::new(
                &self.pc,
                RtpCodecKind::Video,
                format!("screen-{i}"),
                &video_codec,
            )
            .await
            .with_context(|| format!("video track {i}"))?;
            self.video.push(t);
        }
        let audio = want_audio && audio_mlines > 0;
        if audio {
            let t = LocalTrack::new(
                &self.pc,
                RtpCodecKind::Audio,
                "system-audio".to_owned(),
                &opus_codec_params(),
            )
            .await
            .context("audio track")?;
            self.audio = Some(t);
        }

        let answer = self
            .pc
            .create_answer(None)
            .await
            .map_err(|e| anyhow!("create_answer: {e}"))?;
        self.pc
            .set_local_description(answer)
            .await
            .map_err(|e| anyhow!("set_local_description: {e}"))?;
        let local = self
            .pc
            .local_description()
            .await
            .context("no local description after answering")?;
        Ok(Answer {
            sdp: local.sdp,
            video_tracks: n,
            audio,
        })
    }

    pub fn video_tracks(&self) -> usize {
        self.video.len()
    }

    pub fn video(&self, index: usize) -> Option<&LocalTrack> {
        self.video.get(index)
    }

    pub fn audio(&self) -> Option<&LocalTrack> {
        self.audio.as_ref()
    }

    pub async fn add_ice_candidate(&self, c: &IceCandidate) -> Result<()> {
        self.pc
            .add_ice_candidate(RTCIceCandidateInit {
                candidate: c.candidate.clone(),
                sdp_mid: c.sdp_mid.clone(),
                sdp_mline_index: c.sdp_mline_index,
                username_fragment: c.username_fragment.clone(),
                url: None,
            })
            .await
            .map_err(|e| anyhow!("add_ice_candidate: {e}"))
    }

    pub async fn close(&self) {
        if let Err(e) = self.pc.close().await {
            tracing::debug!("closing peer connection: {e}");
        }
    }
}

/// Feed REMB / receiver-report feedback from an RTCP batch into the congestion controller.
pub fn feed_congestion(event: &TrackLocalEvent, cc: &mut crate::congestion::AimdController) {
    use rtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
    use rtc::rtcp::receiver_report::ReceiverReport;
    let TrackLocalEvent::OnRtcpPacket(packets) = event;
    for p in packets {
        let any = p.as_any();
        if let Some(remb) = any.downcast_ref::<ReceiverEstimatedMaximumBitrate>() {
            cc.on_remb(remb.bitrate);
        } else if let Some(rr) = any.downcast_ref::<ReceiverReport>() {
            for r in &rr.reports {
                cc.on_receiver_report(r.fraction_lost, r.jitter);
            }
        }
    }
}

/// True when the RTCP batch contains a keyframe request (PLI or FIR).
pub fn is_keyframe_request(event: &TrackLocalEvent) -> bool {
    let TrackLocalEvent::OnRtcpPacket(packets) = event;
    packets.iter().any(|p| {
        let any = p.as_any();
        any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>()
    })
}
