//! Capture → encode pipeline running on a dedicated OS thread per session.
//!
//! TODO(builder-core): implement `VideoPipeline`:
//! * owns a `Box<dyn Capturer>` and a `Box<dyn Encoder>`;
//! * loop: `next_frame(timeout=1/fps)` → `encode(frame, force_keyframe.take())` →
//!   send `EncodedFrame`s on a `tokio::sync::mpsc::Sender` with capacity 2 using
//!   `try_send` (drop on full, request keyframe on next iteration if a keyframe was dropped);
//! * commands via `crossbeam_channel`: `SelectDisplay`, `SetQuality`, `RequestKeyframe`, `Stop`;
//! * recreate capturer + encoder on display switch / resolution change;
//! * maintain rolling stats (fps, kbps, pipeline latency) for `ControlMessage::Stats`.

pub struct VideoPipeline;
