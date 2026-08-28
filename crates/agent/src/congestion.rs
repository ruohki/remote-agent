//! Sender-side congestion control driven by the browser's RTCP feedback.
//!
//! Inputs, whichever the receiver provides:
//! * **REMB** (`goog-remb`, advertised in our SDP): the browser's receiver-side bandwidth
//!   estimate in bit/s — the most direct signal, used as a ceiling.
//! * **Receiver reports**: `fraction_lost` (loss since the last report) and interarrival
//!   jitter growth stand in for the queueing signal; TWCC feedback is consumed by the
//!   interceptor and not exposed per packet in webrtc-rs 0.20.
//!
//! Output: a target bitrate (kbit/s) re-evaluated every [`AimdController::INTERVAL`] plus an
//! fps ladder (60 → 30 → 15) that kicks in before quality drops below the floor. Pure logic,
//! no I/O, unit-tested.

use std::time::{Duration, Instant};

/// Additive-increase / multiplicative-decrease bitrate controller.
#[derive(Debug, Clone)]
pub struct AimdController {
    /// Configured cap (the operator's/console's `max_bitrate_kbps`).
    cap_kbps: u32,
    floor_kbps: u32,
    target_kbps: u32,
    /// Latest REMB estimate (kbps), if any.
    remb_kbps: Option<u32>,
    /// Loss fraction (0..=1) accumulated since the last evaluation.
    loss: Option<f32>,
    /// Jitter (RTP timestamp units at 90 kHz) of the previous report, to detect growth.
    last_jitter: Option<u32>,
    jitter_growth: bool,
    last_eval: Instant,
    last_increase: Instant,
    last_decrease: Option<Instant>,
}

impl AimdController {
    /// How often [`AimdController::evaluate`] produces a new decision.
    pub const INTERVAL: Duration = Duration::from_millis(200);
    /// Loss above this triggers a decrease.
    pub const LOSS_THRESHOLD: f32 = 0.02;
    /// Additive increase step (fraction of the current target) every `INCREASE_EVERY`.
    pub const INCREASE_STEP: f32 = 0.05;
    pub const INCREASE_EVERY: Duration = Duration::from_secs(2);
    /// Multiplicative decrease factor.
    pub const DECREASE_FACTOR: f32 = 0.8;
    /// Jitter growth (90 kHz units ≈ 50 ms) treated as queue build-up.
    pub const JITTER_GROWTH_UNITS: u32 = 4500;
    pub const DEFAULT_FLOOR_KBPS: u32 = 300;

    pub fn new(cap_kbps: u32, now: Instant) -> Self {
        let cap = cap_kbps.max(Self::DEFAULT_FLOOR_KBPS);
        Self {
            cap_kbps: cap,
            floor_kbps: Self::DEFAULT_FLOOR_KBPS.min(cap),
            target_kbps: cap,
            remb_kbps: None,
            loss: None,
            last_jitter: None,
            jitter_growth: false,
            last_eval: now,
            last_increase: now,
            last_decrease: None,
        }
    }

    pub fn target_kbps(&self) -> u32 {
        self.target_kbps
    }

    pub fn cap_kbps(&self) -> u32 {
        self.cap_kbps
    }

    /// The operator changed the cap (quality preset).
    pub fn set_cap(&mut self, cap_kbps: u32) {
        self.cap_kbps = cap_kbps.max(Self::DEFAULT_FLOOR_KBPS);
        self.floor_kbps = Self::DEFAULT_FLOOR_KBPS.min(self.cap_kbps);
        self.target_kbps = self.target_kbps.min(self.cap_kbps).max(self.floor_kbps);
    }

    /// Receiver-estimated maximum bitrate (bit/s).
    pub fn on_remb(&mut self, bitrate_bps: f32) {
        if bitrate_bps.is_finite() && bitrate_bps > 0.0 {
            self.remb_kbps = Some((bitrate_bps / 1000.0) as u32);
        }
    }

    /// One reception report block: `fraction_lost` is the RTCP 8-bit fraction, `jitter`
    /// the interarrival jitter in RTP clock units.
    pub fn on_receiver_report(&mut self, fraction_lost: u8, jitter: u32) {
        let loss = fraction_lost as f32 / 256.0;
        self.loss = Some(self.loss.map_or(loss, |l| l.max(loss)));
        if let Some(prev) = self.last_jitter {
            if jitter > prev.saturating_add(Self::JITTER_GROWTH_UNITS) {
                self.jitter_growth = true;
            }
        }
        self.last_jitter = Some(jitter);
    }

    /// Re-evaluate; returns the new target when it changed.
    pub fn evaluate(&mut self, now: Instant) -> Option<u32> {
        if now.duration_since(self.last_eval) < Self::INTERVAL {
            return None;
        }
        self.last_eval = now;
        let before = self.target_kbps;
        let congested = self.loss.is_some_and(|l| l > Self::LOSS_THRESHOLD) || self.jitter_growth;
        if congested {
            // Back off once per interval at most; keep the decrease timestamp so the
            // increase timer restarts from here.
            let decreased = (self.target_kbps as f32 * Self::DECREASE_FACTOR) as u32;
            self.target_kbps = decreased.max(self.floor_kbps);
            self.last_decrease = Some(now);
            self.last_increase = now;
        } else if now.duration_since(self.last_increase) >= Self::INCREASE_EVERY {
            let increased = (self.target_kbps as f32 * (1.0 + Self::INCREASE_STEP)) as u32;
            self.target_kbps = increased.min(self.cap_kbps).max(self.target_kbps + 1);
            self.last_increase = now;
        }
        if let Some(remb) = self.remb_kbps {
            // Never exceed what the receiver says it can absorb (with a little headroom).
            let ceiling = ((remb as f32) * 1.1) as u32;
            self.target_kbps = self.target_kbps.min(ceiling.max(self.floor_kbps));
        }
        self.target_kbps = self.target_kbps.clamp(self.floor_kbps, self.cap_kbps);
        self.loss = None;
        self.jitter_growth = false;
        (self.target_kbps != before).then_some(self.target_kbps)
    }

    /// Frame-rate ladder: full fps while the target is healthy, 30 below 25 % of the cap,
    /// 15 below 12.5 %.
    pub fn fps_for(&self, max_fps: u32) -> u32 {
        let ratio = self.target_kbps as f32 / self.cap_kbps.max(1) as f32;
        if ratio < 0.125 {
            max_fps.min(15)
        } else if ratio < 0.25 {
            max_fps.min(30)
        } else {
            max_fps
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn starts_at_cap_and_holds_without_feedback() {
        let t0 = Instant::now();
        let mut c = AimdController::new(8000, t0);
        assert_eq!(c.target_kbps(), 8000);
        assert_eq!(c.evaluate(at(t0, 250)), None);
        assert_eq!(c.evaluate(at(t0, 2500)), None, "already at the cap");
    }

    #[test]
    fn loss_backs_off_multiplicatively_and_recovers_additively() {
        let t0 = Instant::now();
        let mut c = AimdController::new(8000, t0);
        c.on_receiver_report(26, 0); // ~10 % loss
        assert_eq!(c.evaluate(at(t0, 250)), Some(6400));
        c.on_receiver_report(26, 0);
        assert_eq!(c.evaluate(at(t0, 500)), Some(5120));
        // no loss: nothing for 2 s, then +5 %
        assert_eq!(c.evaluate(at(t0, 1000)), None);
        assert_eq!(c.evaluate(at(t0, 2600)), Some(5376));
    }

    #[test]
    fn remb_is_a_ceiling_and_floor_holds() {
        let t0 = Instant::now();
        let mut c = AimdController::new(8000, t0);
        c.on_remb(2_000_000.0);
        assert_eq!(c.evaluate(at(t0, 250)), Some(2200));
        for i in 1..40 {
            c.on_receiver_report(255, 0);
            c.evaluate(at(t0, 250 + i * 250));
        }
        assert_eq!(c.target_kbps(), AimdController::DEFAULT_FLOOR_KBPS);
    }

    #[test]
    fn jitter_growth_counts_as_congestion() {
        let t0 = Instant::now();
        let mut c = AimdController::new(4000, t0);
        c.on_receiver_report(0, 100);
        assert_eq!(c.evaluate(at(t0, 250)), None);
        c.on_receiver_report(0, 100 + AimdController::JITTER_GROWTH_UNITS + 1);
        assert_eq!(c.evaluate(at(t0, 500)), Some(3200));
    }

    #[test]
    fn fps_ladder_follows_the_target() {
        let t0 = Instant::now();
        let mut c = AimdController::new(8000, t0);
        assert_eq!(c.fps_for(60), 60);
        c.on_remb(1_500_000.0);
        c.evaluate(at(t0, 250));
        assert_eq!(c.target_kbps(), 1650);
        assert_eq!(c.fps_for(60), 30);
        c.on_remb(500_000.0);
        c.evaluate(at(t0, 500));
        assert_eq!(c.fps_for(60), 15);
        c.set_cap(1000);
        assert_eq!(c.target_kbps(), 550);
        assert_eq!(c.fps_for(60), 60);
    }
}
