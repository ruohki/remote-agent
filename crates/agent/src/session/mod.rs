//! One operator session = one WebRTC peer connection.
//!
//! TODO(builder-core): implement, modelled on the webrtc-rs 0.20 examples
//! `play-from-disk-h26x` (video track) and `data-channels` (channels):
//!
//! * `MediaEngine`: register H265 (`MIME_TYPE_HEVC`, pt 98) only when
//!   `encode::available_codecs()` contains it, and H264 (`MIME_TYPE_H264`,
//!   `packetization-mode=1;profile-level-id=42e01f`, pt 102) always. The browser is the
//!   offerer with `setCodecPreferences([H265, H264])`; after `set_remote_description`
//!   + `create_answer`, read the negotiated codec from the sender's parameters and
//!   create the matching encoder.
//! * `TrackLocalStaticSample` fed from a capture+encode std thread through a bounded
//!   channel (drop frames when the network is behind; never queue more than 1 frame).
//!   Each Annex-B access unit becomes one `Sample`.
//! * RTCP PLI/FIR from the browser → `force_keyframe`.
//! * Data channels are created by the browser: `input` → `input::Injector`,
//!   `control` → display switching, quality, clipboard, stats.
//! * Trickle ICE both ways through the hub; ICE servers from `SessionRequest`.
//! * Connection state → `AgentToConsole::SessionState`; teardown releases all keys.

pub mod video;

pub struct SessionManager;
