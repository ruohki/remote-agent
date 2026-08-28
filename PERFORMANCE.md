# Performance pass — plan and targets

Goal: lowest practical glass-to-glass latency and bandwidth for remote control, with every
trick that does not compromise correctness. Codec P-frames already skip unchanged blocks, so
the wins are elsewhere.

## Targets (LAN, 5120×2160 source, Apple Silicon)

| Metric | Target |
|--------|--------|
| Glass-to-glass latency (capture → browser paint) | ≤ 60 ms median, ≤ 90 ms p95 |
| Static desktop bandwidth | < 20 kbit/s |
| Typing / cursor movement | < 300 kbit/s |
| Window drag / scrolling | ≤ configured cap, no queueing |
| Mouse-to-photon (pointer move → cursor moves on device) | ≤ 1 frame + network RTT |

## Agent

1. **Infinite GOP** — no periodic keyframes; keyframe only on session start, display switch,
   resolution change, PLI/FIR from the browser. VideoToolbox: `MaxKeyFrameInterval` = huge,
   `MaxKeyFrameIntervalDuration` = 0. Media Foundation: `CODECAPI_AVEncMPVGOPSize` = 0xFFFFFFFF.
   OpenH264: `intra period` 0 + `force_intra_frame` on demand.
2. **Encode only on change** — ScreenCaptureKit/DXGI already deliver frames only on change;
   make sure nothing re-encodes identical frames; add a 1 fps idle refresh (re-send the last
   encoded P-frame is wrong — instead encode a frame every 1 s with "skip" hint so the browser
   jitter buffer keeps ticking). Use `SCStreamFrameInfoDirtyRects` / DXGI dirty rects to skip
   cursor-only updates when the cursor is composited separately.
3. **Rate control** — VBR with a cap: VideoToolbox `AverageBitRate` + `DataRateLimits` (cap at
   1.5× average), quality-mode floor; MF `eAVEncCommonRateControlMode_PeakConstrainedVBR`.
4. **Viewer-size encoding** — browser reports its rendered tile size
   (`ControlMessage::SetViewport { display, width, height }`); the agent encodes at
   `min(display, viewport × dpr)` (VideoToolbox scales for free via `VTCompressionSession`
   destination size; MF via the video processor); full resolution when the tile is fullscreen.
5. **Congestion control** — read TWCC/REMB feedback via the interceptor registry; adapt bitrate
   every 200 ms (AIMD-style: −20 % on loss/queue growth, +5 % every 2 s otherwise), fps ladder
   60 → 30 → 15 before quality drops below the floor.
6. **Pipeline** — capture/encode thread at real-time priority (`pthread_set_qos_class_self_np`
   user-interactive on macOS, `THREAD_PRIORITY_TIME_CRITICAL` on Windows), single frame in
   flight, per-frame timestamps logged (`capture_at`, `encoded_at`, `sent_at`) behind
   `--log perf` and reported in `Stats`.
7. **Input path** — the browser sends pointer moves on an unordered/unreliable data channel
   (`input-fast`, `maxRetransmits: 0`) and buttons/keys on the reliable one; the agent applies
   the latest position only.
8. **Cursor** — agent sends cursor shape (PNG + hotspot) and position at 60 Hz on the control
   channel (`CursorShape`, `CursorPosition`); the browser draws it locally, capture excludes
   the system cursor (`showsCursor = false` / DXGI pointer skipped). Removes cursor lag entirely.

## Browser

* `receiver.playoutDelayHint = 0`, `jitterBufferTarget = 0`; render with `<video>` only, no canvas
  copies; stats overlay shows the end-to-end number from the test rig, `framesDecoded/s`,
  `jitterBufferDelay`, RTT, bitrate.
* Send `set_viewport` on resize/fullscreen (debounced 250 ms).

## Test rig

Agent side (implemented): set `REMOTE_AGENT_SYNTHETIC_SOURCE=1` on any agent command that
captures (`run`, app mode, the ignored `macos_perf` tests) to replace the screen capturer
with `capture::synthetic::SyntheticCapturer` — 1920×1080 BGRA at 60 Hz with:

* a strip of 14 solid 64×64 px cells at the top-left: cell 0 white (marker), cells 1–12 the
  low 12 bits of the Unix epoch capture time in ms (MSB first, white = 1), cell 13 even
  parity over the 12 data bits (`capture::synthetic::decode_strip` / `latency_ms` are the
  reference decoder; the browser rig averages rows 8–56 of each cell);
* a six-digit 7-segment frame counter below the strip;
* `REMOTE_AGENT_SYNTHETIC_SCENARIO=static|typing|drag|video` — `static` repaints the strip
  once per second and produces no other frames, `typing` changes a small region 10×/s,
  `drag` moves a 600×400 window every frame, `video` is full-frame noisy motion at 30 fps.

Every per-second `perf` log line (`--log perf=info`) carries `last_capture_epoch_ms` so
agent-side timings can be correlated with the browser's decoded stamps. Stats sent to the
viewer (`ControlMessage::Stats`) include `encoded_width/height`, `capture_to_encoded_ms`,
`encode_ms`, `keyframes` and `frames_skipped_idle` (idle refreshes) per window.

Browser side: `remote-console/web/perf/` (see the console repo) connects, samples
`requestVideoFrameCallback`, decodes the strip from the decoded frame and computes
glass-to-glass latency; bandwidth comes from `getStats()`.


## Results

Agent-side measurements on this Mac (Apple Silicon, VideoToolbox H.265, 2026-08-28,
`cargo test -p remote-agent --test macos_perf -- --ignored --nocapture`):

| Scenario | Frames / 10 s | Keyframes | Bandwidth | capture→encoded | encode |
|----------|---------------|-----------|-----------|-----------------|--------|
| Synthetic `static` (strip repaints 1×/s) | 10 | 0 | **4.4 kbit/s** | 4.6 ms | 4.2 ms |
| Live desktop (terminal scrolling, 3024×1964) | 70 | 0 | 307 kbit/s | 10.6 ms | 10.6 ms |
| Synthetic `drag` (600×400 window, 1080p) | 133 / 3 s (44 fps) | 1 | — | 5.6 ms | 3.9 ms |

First keyframe: 2.2 KB (synthetic 1080p) / 75 KB (live 3024×1964). Glass-to-glass numbers
come from the browser rig (`remote-console/web/perf/`).

Browser rig on the same Mac (headless Chromium, software H.264 decode, `npm run perf:latency`,
15 s per scenario, 2026-08-28, agent build c34e01f):

| Scenario | Glass-to-glass median | p95 | Bandwidth | Decoded fps | Samples |
|----------|----------------------|-----|-----------|-------------|---------|
| static (strip repaint 1×/s) | 244 ms | 1338 ms | 4 kbit/s | 1.0 | 15 |
| typing | **31 ms** | 39 ms | 81 kbit/s | 29.3 | 439 |
| drag (600×400 window) | **32 ms** | 38 ms | 284 kbit/s | 29.2 | 437 |
| video (full-frame motion) | 67 ms | 74 ms | 8018 kbit/s (cap) | 14.7 | 219 |

Notes: the `static` number is not pipeline latency — at 1 frame/s the browser holds a frame
until the next one arrives; a follow-up frame ~50 ms after every change frame (planned) will
bring it in line. `video` is bitrate-limited by the 8 Mb/s cap (fps ladder engaged), not by
latency. Hardware decode (headed Chrome/Safari) should shave a further ~10 ms off all rows.
