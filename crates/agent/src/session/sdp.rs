//! Minimal SDP inspection: which video codecs does the browser's offer accept, in its
//! preference order? We only need the `m=video` payload list plus `a=rtpmap`/`a=fmtp`.

use protocol::common::VideoCodec;
use std::collections::HashMap;

/// Video codecs the offer can receive, ordered by the offerer's preference (payload type
/// order on the `m=video` line). H.264 entries without `packetization-mode=1` are skipped
/// because the RTP payloader only produces non-interleaved mode 1 packets.
pub fn offered_video_codecs(sdp: &str) -> Vec<VideoCodec> {
    let mut payload_order: Vec<String> = Vec::new();
    let mut rtpmap: HashMap<String, String> = HashMap::new();
    let mut fmtp: HashMap<String, String> = HashMap::new();
    let mut in_video = false;

    for raw in sdp.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(m) = line.strip_prefix("m=") {
            in_video = m.starts_with("video ");
            if in_video {
                // m=video 9 UDP/TLS/RTP/SAVPF 96 97 98 ...
                payload_order.extend(m.split_whitespace().skip(3).map(str::to_string));
            }
            continue;
        }
        if !in_video {
            continue;
        }
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if let Some((pt, codec)) = rest.split_once(' ') {
                rtpmap.insert(pt.to_string(), codec.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            if let Some((pt, params)) = rest.split_once(' ') {
                fmtp.insert(pt.to_string(), params.to_string());
            }
        }
    }

    let mut out = Vec::new();
    for pt in payload_order {
        let Some(codec) = rtpmap.get(&pt) else {
            continue;
        };
        let name = codec.split('/').next().unwrap_or("").to_ascii_uppercase();
        let candidate = match name.as_str() {
            "H265" | "HEVC" => VideoCodec::H265,
            "H264" => {
                let mode_ok = fmtp
                    .get(&pt)
                    .map(|p| p.split(';').any(|kv| kv.trim() == "packetization-mode=1"))
                    .unwrap_or(false);
                if !mode_ok {
                    continue;
                }
                VideoCodec::H264
            }
            _ => continue,
        };
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\n\
o=- 1 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 49 50 102 103 96\r\n\
a=rtpmap:49 H265/90000\r\n\
a=fmtp:49 level-id=93;profile-id=1;tier-flag=0;tx-mode=SRST\r\n\
a=rtpmap:50 rtx/90000\r\n\
a=rtpmap:102 H264/90000\r\n\
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n\
a=rtpmap:103 H264/90000\r\n\
a=fmtp:103 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f\r\n\
a=rtpmap:96 VP8/90000\r\n";

    #[test]
    fn parses_browser_preference_order() {
        assert_eq!(
            offered_video_codecs(OFFER),
            vec![VideoCodec::H265, VideoCodec::H264]
        );
    }

    #[test]
    fn h264_only_when_h265_missing() {
        let sdp = OFFER.replace("49 50 102", "102 103 49 50");
        assert_eq!(
            offered_video_codecs(&sdp),
            vec![VideoCodec::H264, VideoCodec::H265]
        );
        let sdp = OFFER.replace("a=rtpmap:49 H265/90000", "a=rtpmap:49 VP9/90000");
        assert_eq!(offered_video_codecs(&sdp), vec![VideoCodec::H264]);
    }

    #[test]
    fn packetization_mode_0_only_is_rejected() {
        let sdp = OFFER.replace("packetization-mode=1", "packetization-mode=0");
        assert_eq!(offered_video_codecs(&sdp), vec![VideoCodec::H265]);
    }
}
