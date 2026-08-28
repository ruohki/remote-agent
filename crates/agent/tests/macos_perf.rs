//! Real-hardware performance checks on macOS (ScreenCaptureKit + VideoToolbox).
//!
//! Ignored by default: they need the Screen Recording permission for the test runner.
//! Run with `cargo test -p remote-agent --test macos_perf -- --ignored --nocapture`.
#![cfg(target_os = "macos")]

use remote_agent::session::media::{MediaFactory, SystemMedia};
use remote_agent::session::video::{PipelineConfig, VideoPipeline};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Run one pipeline on the hardware encoder and return
/// `(first keyframe bytes, frames, keyframes, bytes, seconds, stats)` after `window`.
async fn measure(
    window: Duration,
) -> (
    usize,
    u32,
    u32,
    u64,
    f64,
    remote_agent::session::video::PipelineStats,
) {
    let media: Arc<dyn MediaFactory> = Arc::new(SystemMedia);
    let codec = media.available_codecs()[0];
    let (frame_tx, mut frame_rx) = mpsc::channel(4);
    let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
    let cfg = PipelineConfig {
        display_index: 0,
        codec,
        max_fps: 60,
        max_bitrate_kbps: 8000,
        show_cursor: false,
        viewport: None,
    };
    let pipeline =
        tokio::task::spawn_blocking(move || VideoPipeline::start(media, cfg, frame_tx, ev_tx))
            .await
            .unwrap()
            .expect("pipeline (Screen Recording permission?)");

    // Wait for the first keyframe (session start), then measure 10 s.
    let first = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let f = frame_rx.recv().await.expect("pipeline closed");
            if f.keyframe {
                break f;
            }
        }
    })
    .await
    .expect("no keyframe within 10 s");
    println!("first keyframe: {} bytes ({codec:?})", first.data.len());

    let start = Instant::now();
    let (mut bytes, mut frames, mut keyframes) = (0u64, 0u32, 0u32);
    while start.elapsed() < window {
        match tokio::time::timeout(window - start.elapsed(), frame_rx.recv()).await {
            Ok(Some(f)) => {
                bytes += f.data.len() as u64;
                frames += 1;
                keyframes += f.keyframe as u32;
            }
            _ => break,
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let stats = pipeline.stats().borrow().clone();
    drop(pipeline);
    (first.data.len(), frames, keyframes, bytes, secs, stats)
}

/// Static content through the real hardware encoder (synthetic `static` scenario: only the
/// timestamp strip repaints, once per second) must stay far below 20 kbit/s with zero
/// periodic keyframes (infinite GOP).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn static_content_bandwidth_is_tiny() {
    std::env::set_var(remote_agent::capture::synthetic::ENV_ENABLE, "1");
    std::env::set_var(remote_agent::capture::synthetic::ENV_SCENARIO, "static");
    let (first, frames, keyframes, bytes, secs, stats) = measure(Duration::from_secs(10)).await;
    std::env::remove_var(remote_agent::capture::synthetic::ENV_ENABLE);
    let kbps = bytes as f64 * 8.0 / secs / 1000.0;
    println!(
        "static (synthetic, hardware {:?}): first keyframe {first} bytes; over {secs:.1} s {frames} frames, \
         {keyframes} keyframes, {bytes} bytes = {kbps:.1} kbit/s; capture→encoded {:.1} ms, encode {:.1} ms",
        stats.codec, stats.capture_to_encoded_ms, stats.encode_ms
    );
    assert_eq!(keyframes, 0, "no periodic keyframes with an infinite GOP");
    assert!(
        kbps < 20.0,
        "static content must stay below 20 kbit/s, got {kbps:.1}"
    );
}

/// Informational: the live desktop (whatever is on screen right now) — prints the numbers
/// and only asserts the infinite GOP.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn live_screen_bandwidth_report() {
    std::env::remove_var(remote_agent::capture::synthetic::ENV_ENABLE);
    let (first, frames, keyframes, bytes, secs, stats) = measure(Duration::from_secs(10)).await;
    let kbps = bytes as f64 * 8.0 / secs / 1000.0;
    println!(
        "live screen ({}x{} {:?}): first keyframe {first} bytes; over {secs:.1} s {frames} frames, {keyframes} keyframes, \
         {bytes} bytes = {kbps:.1} kbit/s; capture→encoded {:.1} ms, encode {:.1} ms, idle refreshes {}",
        stats.width, stats.height, stats.codec, stats.capture_to_encoded_ms, stats.encode_ms, stats.idle_refreshes
    );
    assert_eq!(keyframes, 0, "no periodic keyframes with an infinite GOP");
}

/// The synthetic source renders decodable timestamps at 60 Hz and its static scenario
/// produces (almost) no frames.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn synthetic_source_drives_the_hardware_encoder() {
    std::env::set_var(remote_agent::capture::synthetic::ENV_ENABLE, "1");
    std::env::set_var(remote_agent::capture::synthetic::ENV_SCENARIO, "drag");
    let media: Arc<dyn MediaFactory> = Arc::new(SystemMedia);
    let codec = media.available_codecs()[0];
    let (frame_tx, mut frame_rx) = mpsc::channel(4);
    let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
    let cfg = PipelineConfig {
        display_index: 0,
        codec,
        max_fps: 60,
        max_bitrate_kbps: 8000,
        show_cursor: false,
        viewport: None,
    };
    let pipeline =
        tokio::task::spawn_blocking(move || VideoPipeline::start(media, cfg, frame_tx, ev_tx))
            .await
            .unwrap()
            .expect("pipeline");
    let start = Instant::now();
    let mut frames = 0;
    while start.elapsed() < Duration::from_secs(3) {
        if tokio::time::timeout(Duration::from_millis(500), frame_rx.recv())
            .await
            .is_ok()
        {
            frames += 1;
        }
    }
    let stats = pipeline.stats().borrow().clone();
    println!(
        "synthetic drag: {frames} frames in 3 s, {}x{}, capture→encoded {:.1} ms, encode {:.1} ms",
        stats.encoded_width, stats.encoded_height, stats.capture_to_encoded_ms, stats.encode_ms
    );
    std::env::remove_var(remote_agent::capture::synthetic::ENV_ENABLE);
    assert!(
        frames >= 120,
        "expected ≥ 40 fps from the synthetic drag scenario, got {frames}/3s"
    );
}

