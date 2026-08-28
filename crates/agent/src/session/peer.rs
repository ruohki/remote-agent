//! Thin wrapper around a webrtc-rs 0.20 `PeerConnection` for one operator session.
//!
//! The browser is always the offerer. We register exactly one video codec (the one chosen
//! for the session) so the negotiated codec is known before the encoder is created, answer
//! the offer, and forward events (ICE candidates, connection state, incoming data channels)
//! to the session task over a channel.

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use protocol::common::{IceCandidate, IceServer, VideoCodec};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_HEVC};
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

/// One peer connection with a single outbound video track.
pub struct Peer {
    pc: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    codec: VideoCodec,
    ssrc: u32,
}

impl Peer {
    /// Build the peer connection and add the video track for `codec`.
    pub async fn new(
        codec: VideoCodec,
        ice_servers: &[IceServer],
        events: mpsc::UnboundedSender<PeerEvent>,
    ) -> Result<Self> {
        let codec_params = video_codec_params(codec);
        let mut media_engine = MediaEngine::default();
        media_engine
            .register_codec(codec_params.clone(), RtpCodecKind::Video)
            .map_err(|e| anyhow!("registering codec: {e}"))?;
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
            .build()
            .await
            .map_err(|e| anyhow!("building peer connection: {e}"))?;
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);

        let ssrc: u32 = rand::random();
        let track = Arc::new(
            TrackLocalStaticSample::new(MediaStreamTrack::new(
                "remote-agent-stream".to_owned(),
                "remote-agent-video".to_owned(),
                "screen".to_owned(),
                RtpCodecKind::Video,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: codec_params.rtp_codec.clone(),
                    ..Default::default()
                }],
            ))
            .map_err(|e| anyhow!("creating video track: {e}"))?,
        );
        let sender = pc
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .map_err(|e| anyhow!("adding video track: {e}"))?;

        Ok(Self {
            pc,
            track,
            sender,
            codec,
            ssrc,
        })
    }

    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// Apply the browser's offer and produce our answer SDP.
    pub async fn answer(&self, offer_sdp: String) -> Result<String> {
        let offer =
            RTCSessionDescription::offer(offer_sdp).map_err(|e| anyhow!("parsing offer: {e}"))?;
        self.pc
            .set_remote_description(offer)
            .await
            .map_err(|e| anyhow!("set_remote_description: {e}"))?;
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
        Ok(local.sdp)
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

    /// Payload type negotiated for our codec (falls back to the locally registered one).
    pub async fn negotiated_payload_type(&self) -> PayloadType {
        match self.sender.get_parameters().await {
            Ok(p) => p
                .rtp_parameters
                .codecs
                .first()
                .map(|c| c.payload_type)
                .unwrap_or_else(|| video_codec_params(self.codec).payload_type),
            Err(e) => {
                tracing::warn!("get_parameters failed, using local payload type: {e}");
                video_codec_params(self.codec).payload_type
            }
        }
    }

    pub async fn ssrc(&self) -> u32 {
        self.track
            .ssrcs()
            .await
            .first()
            .copied()
            .unwrap_or(self.ssrc)
    }

    /// Write one Annex-B access unit as a sample.
    pub async fn write_frame(
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

    /// Next RTCP feedback event for our track; `None` once the track is unbound.
    pub async fn poll_rtcp(&self) -> Option<TrackLocalEvent> {
        self.track.poll().await
    }

    pub async fn close(&self) {
        if let Err(e) = self.pc.close().await {
            tracing::debug!("closing peer connection: {e}");
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
