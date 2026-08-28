//! Synthetic capture source for the latency / bandwidth test rig.
//!
//! Enabled with `REMOTE_AGENT_SYNTHETIC_SOURCE=1` (see [`crate::session::media::SystemMedia`]).
//! Renders 1920×1080 BGRA frames at 60 Hz carrying machine-readable timing:
//!
//! * a **binary strip** along the top edge, starting at x = 0, y = 0: [`STRIP_CELLS`] solid
//!   cells of [`CELL`]×[`CELL`] px. Cell 0 is always white (marker), cells 1–12 carry the
//!   **low 12 bits of the Unix epoch capture time in milliseconds** (MSB first, white = 1),
//!   cell 13 is even parity over the 12 data bits. The browser rig samples rows 8–56 of each
//!   cell, so the cells are painted solid;
//! * a large **frame counter** rendered as 7-segment digits below the strip;
//! * scenario-specific motion selected with `REMOTE_AGENT_SYNTHETIC_SCENARIO`:
//!   `static` (strip updates once per second, nothing else moves), `typing` (a small
//!   region changes 10×/s), `drag` (a 600×400 window moves at full rate), `video`
//!   (full-frame noisy motion at 30 fps).
//!
//! The browser-side rig (`remote-console/web/perf/`) decodes the strip from the received
//! video and computes glass-to-glass latency; bandwidth comes from `getStats()`.

use super::{CaptureConfig, Capturer, Frame, FrameData};
use anyhow::Result;
use std::time::{Duration, Instant};

pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;
/// Cell size of the binary strip in pixels.
pub const CELL: u32 = 64;
/// Marker cell + 12 timestamp bits + 1 parity cell.
pub const STRIP_CELLS: u32 = 14;
/// Timestamp bits carried by the strip (low bits of the epoch millisecond).
pub const STRIP_BITS: u32 = 12;
pub const ENV_ENABLE: &str = "REMOTE_AGENT_SYNTHETIC_SOURCE";
pub const ENV_SCENARIO: &str = "REMOTE_AGENT_SYNTHETIC_SCENARIO";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    Static,
    Typing,
    Drag,
    Video,
}

impl Scenario {
    pub fn from_env() -> Self {
        match std::env::var(ENV_SCENARIO).unwrap_or_default().as_str() {
            "typing" => Scenario::Typing,
            "drag" => Scenario::Drag,
            "video" => Scenario::Video,
            _ => Scenario::Static,
        }
    }
}

/// Whether the synthetic source is requested through the environment.
pub fn enabled() -> bool {
    std::env::var_os(ENV_ENABLE).is_some_and(|v| v == "1" || v == "true")
}

pub struct SyntheticCapturer {
    scenario: Scenario,
    epoch: Instant,
    fps: u32,
    frame_no: u64,
    last_frame_at: Option<Instant>,
    last_strip_second: u64,
    /// Persistent canvas so unchanged regions stay bit-identical (P-frames stay tiny).
    canvas: Vec<u8>,
    rng: u64,
    /// Last drag-window rectangle, restored before painting the next one.
    last_window: Option<(u32, u32, u32, u32)>,
}

impl SyntheticCapturer {
    pub fn new(cfg: &CaptureConfig) -> Self {
        let mut me = Self {
            scenario: Scenario::from_env(),
            epoch: Instant::now(),
            fps: cfg.max_fps.clamp(1, 60),
            frame_no: 0,
            last_frame_at: None,
            last_strip_second: u64::MAX,
            canvas: vec![0; (WIDTH * HEIGHT * 4) as usize],
            rng: 0x9E37_79B9_7F4A_7C15,
            last_window: None,
        };
        me.paint_background();
        me
    }

    fn background_at(y: u32) -> [u8; 4] {
        let v = 40 + ((y * 60) / HEIGHT) as u8;
        [v, v, v + 10, 255]
    }

    fn paint_background(&mut self) {
        for (i, px) in self.canvas.chunks_exact_mut(4).enumerate() {
            let y = (i as u32) / WIDTH;
            px.copy_from_slice(&Self::background_at(y));
        }
    }

    /// Repaint the background gradient inside a rectangle only.
    fn restore_background(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let x1 = (x + w).min(WIDTH);
        let y1 = (y + h).min(HEIGHT);
        for yy in y..y1 {
            let px = Self::background_at(yy);
            let row = (yy * WIDTH) as usize * 4;
            for xx in x..x1 {
                let o = row + xx as usize * 4;
                self.canvas[o..o + 4].copy_from_slice(&px);
            }
        }
    }

    fn rand(&mut self) -> u32 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, bgra: [u8; 4]) {
        let x1 = (x + w).min(WIDTH);
        let y1 = (y + h).min(HEIGHT);
        for yy in y..y1 {
            let row = (yy * WIDTH) as usize * 4;
            for xx in x..x1 {
                let o = row + xx as usize * 4;
                self.canvas[o..o + 4].copy_from_slice(&bgra);
            }
        }
    }

    /// Paint the strip for `epoch_ms`: marker, 12 data bits (MSB first), even parity.
    fn paint_strip(&mut self, epoch_ms: u64) {
        let bits = (epoch_ms & ((1 << STRIP_BITS) - 1)) as u32;
        self.fill_rect(0, 0, CELL, CELL, [255, 255, 255, 255]);
        let mut ones = 0u32;
        for bit in 0..STRIP_BITS {
            let on = (bits >> (STRIP_BITS - 1 - bit)) & 1 == 1;
            ones += on as u32;
            let v = if on { 255 } else { 0 };
            self.fill_rect((1 + bit) * CELL, 0, CELL, CELL, [v, v, v, 255]);
        }
        let parity = if ones % 2 == 1 { 255 } else { 0 };
        self.fill_rect(
            (1 + STRIP_BITS) * CELL,
            0,
            CELL,
            CELL,
            [parity, parity, parity, 255],
        );
    }

    /// 7-segment digits, 48×80 px each, at (x, y).
    fn paint_counter(&mut self, mut x: u32, y: u32, value: u64) {
        const SEGS: [u8; 10] = [
            0b0111111, 0b0000110, 0b1011011, 0b1001111, 0b1100110, 0b1101101, 0b1111101, 0b0000111,
            0b1111111, 0b1101111,
        ];
        let digits = format!("{value:06}");
        for ch in digits.bytes() {
            let d = (ch - b'0') as usize;
            let s = SEGS[d];
            // clear cell
            self.fill_rect(x, y, 56, 88, [40, 40, 50, 255]);
            let on = [255, 255, 255, 255];
            let (w, h, t) = (40, 36, 6);
            if s & 0b0000001 != 0 {
                self.fill_rect(x + 4, y, w, t, on);
            }
            if s & 0b0000010 != 0 {
                self.fill_rect(x + 4 + w - t, y, t, h, on);
            }
            if s & 0b0000100 != 0 {
                self.fill_rect(x + 4 + w - t, y + h, t, h, on);
            }
            if s & 0b0001000 != 0 {
                self.fill_rect(x + 4, y + 2 * h - t, w, t, on);
            }
            if s & 0b0010000 != 0 {
                self.fill_rect(x + 4, y + h, t, h, on);
            }
            if s & 0b0100000 != 0 {
                self.fill_rect(x + 4, y, t, h, on);
            }
            if s & 0b1000000 != 0 {
                self.fill_rect(x + 4, y + h - t / 2, w, t, on);
            }
            x += 56;
        }
    }

    fn paint_scenario(&mut self, now_ms: u64) {
        match self.scenario {
            Scenario::Static => {}
            Scenario::Typing => {
                // A "caret" region 10×/s: alternate glyph blocks.
                let step = (now_ms / 100) as u32;
                let x = 200 + (step % 40) * 24;
                self.fill_rect(200, 400, 40 * 24, 40, [30, 30, 30, 255]);
                self.fill_rect(x, 404, 18, 32, [230, 230, 230, 255]);
            }
            Scenario::Drag => {
                // A 600×400 "window" bouncing around at frame rate.
                let t = now_ms as f64 / 1000.0;
                let x = ((t * 0.5).sin() * 0.5 + 0.5) * (WIDTH - 600) as f64;
                let y = 200.0 + ((t * 0.7).cos() * 0.5 + 0.5) * (HEIGHT - 600) as f64;
                if let Some((px, py, pw, ph)) = self.last_window.take() {
                    self.restore_background(px, py, pw, ph);
                }
                let (x, y) = (x as u32, y as u32);
                self.last_window = Some((x, y, 600, 400));
                self.fill_rect(x, y, 600, 400, [80, 120, 200, 255]);
                self.fill_rect(x, y, 600, 28, [200, 200, 210, 255]);
                for i in 0..12u32 {
                    self.fill_rect(x + 20, y + 50 + i * 26, 500, 14, [120, 150, 220, 255]);
                }
            }
            Scenario::Video => {
                // Full-frame noisy motion at 30 fps (every other 60 Hz frame).
                if self.frame_no.is_multiple_of(2) {
                    let phase = (now_ms / 33) as u32;
                    for by in (CELL + 100..HEIGHT).step_by(16) {
                        for bx in (0..WIDTH).step_by(16) {
                            let n = self.rand();
                            let base = ((bx / 16 + by / 16 + phase) % 64) as u8 * 3;
                            let c = [
                                base.wrapping_add((n & 0x3f) as u8),
                                base.wrapping_add(((n >> 8) & 0x3f) as u8),
                                base.wrapping_add(((n >> 16) & 0x3f) as u8),
                                255,
                            ];
                            self.fill_rect(bx, by, 16, 16, c);
                        }
                    }
                }
            }
        }
    }
}

impl Capturer for SyntheticCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let interval = Duration::from_secs_f64(1.0 / self.fps as f64);
        let due = self
            .last_frame_at
            .map(|t| t + interval)
            .unwrap_or_else(Instant::now);
        let now = Instant::now();
        if due > now {
            let wait = due - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        let now = Instant::now();
        let now_ms = now.duration_since(self.epoch).as_millis() as u64;
        self.frame_no += 1;
        let second = now_ms / 1000;
        let changed = match self.scenario {
            // Static: only the strip changes, once per second.
            Scenario::Static => second != self.last_strip_second,
            _ => true,
        };
        if !changed {
            self.last_frame_at = Some(now);
            std::thread::sleep(interval.min(timeout));
            return Ok(None);
        }
        self.last_strip_second = second;
        self.paint_scenario(now_ms);
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.paint_strip(epoch_ms);
        self.paint_counter(0, CELL + 8, self.frame_no);
        self.last_frame_at = Some(now);
        Ok(Some(Frame {
            width: WIDTH,
            height: HEIGHT,
            captured_at: now,
            data: FrameData::Bgra {
                data: self.canvas.clone(),
                stride: (WIDTH * 4) as usize,
            },
        }))
    }

    fn size(&self) -> (u32, u32) {
        (WIDTH, HEIGHT)
    }

    fn stop(&mut self) {}
}

/// Decode the strip from a BGRA picture of the synthetic source: the low 12 bits of the
/// epoch millisecond (reference implementation for the browser rig). `None` when the marker
/// is missing or the parity does not match.
pub fn decode_strip(bgra: &[u8], stride: usize) -> Option<u32> {
    let sample = |cell: u32| -> bool {
        let x = (cell * CELL + CELL / 2) as usize;
        let y = (CELL / 2) as usize;
        bgra[y * stride + x * 4 + 1] > 127
    };
    if !sample(0) {
        return None;
    }
    let mut v = 0u32;
    let mut ones = 0u32;
    for bit in 0..STRIP_BITS {
        let on = sample(1 + bit);
        ones += on as u32;
        v = (v << 1) | on as u32;
    }
    let parity = sample(1 + STRIP_BITS);
    (parity == (ones % 2 == 1)).then_some(v)
}

/// Glass-to-glass latency from a decoded 12-bit stamp and the observer's epoch millisecond
/// (wraps every 4.096 s, so only sane for latencies below ~2 s).
pub fn latency_ms(decoded_low_bits: u32, observer_epoch_ms: u64) -> u32 {
    let modulus = 1u64 << STRIP_BITS;
    let observed = observer_epoch_ms & (modulus - 1);
    ((observed + modulus - decoded_low_bits as u64) % modulus) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_roundtrips_timestamps() {
        let cfg = CaptureConfig {
            display_index: 0,
            max_fps: 60,
            show_cursor: false,
        };
        let mut c = SyntheticCapturer::new(&cfg);
        for ts in [0u64, 1, 12_345, 0xdead_beef, u64::MAX] {
            c.paint_strip(ts);
            assert_eq!(
                decode_strip(&c.canvas, (WIDTH * 4) as usize),
                Some((ts & 0xfff) as u32),
                "ts {ts:#x}"
            );
        }
        assert_eq!(
            latency_ms(0xff0, 0x1_0010),
            32,
            "wraps across the 12-bit boundary"
        );
        assert_eq!(latency_ms(100, 4096 + 150), 50);
    }

    #[test]
    fn static_scenario_only_changes_once_per_second() {
        std::env::remove_var(ENV_SCENARIO);
        let cfg = CaptureConfig {
            display_index: 0,
            max_fps: 60,
            show_cursor: false,
        };
        let mut c = SyntheticCapturer::new(&cfg);
        let first = c.next_frame(Duration::from_millis(50)).unwrap();
        assert!(first.is_some(), "first frame paints the strip");
        let mut produced = 0;
        for _ in 0..10 {
            if c.next_frame(Duration::from_millis(20)).unwrap().is_some() {
                produced += 1;
            }
        }
        assert!(
            produced <= 1,
            "static scene must not produce frames every tick"
        );
    }
}
