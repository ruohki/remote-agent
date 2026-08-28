//! Resample/downmix to 48 kHz stereo and encode 20 ms Opus frames.

use super::AudioFormat;
use anyhow::{anyhow, Result};

pub const OPUS_RATE: u32 = 48_000;
pub const OPUS_CHANNELS: usize = 2;
/// 20 ms at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;
const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * OPUS_CHANNELS;
const MAX_PACKET: usize = 1500;

/// Converts arbitrary interleaved f32 PCM into 20 ms Opus packets.
pub struct FrameEncoder {
    encoder: opus::Encoder,
    input: AudioFormat,
    /// Pending 48 kHz stereo samples (interleaved) not yet forming a full frame.
    pending: Vec<f32>,
    /// Fractional read position for the linear resampler (in input frames).
    phase: f64,
    /// Last input frame (L, R) kept for interpolation across calls.
    carry: Option<(f32, f32)>,
    scratch: Vec<u8>,
}

impl FrameEncoder {
    pub fn new(input: AudioFormat, bitrate_bps: i32) -> Result<Self> {
        if input.sample_rate == 0 || input.channels == 0 {
            return Err(anyhow!("invalid audio format {input:?}"));
        }
        let mut encoder =
            opus::Encoder::new(OPUS_RATE, opus::Channels::Stereo, opus::Application::Audio)
                .map_err(|e| anyhow!("creating Opus encoder: {e}"))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(bitrate_bps.max(16_000)))
            .map_err(|e| anyhow!("Opus bitrate: {e}"))?;
        encoder
            .set_inband_fec(true)
            .map_err(|e| anyhow!("Opus FEC: {e}"))?;
        encoder
            .set_complexity(5)
            .map_err(|e| anyhow!("Opus complexity: {e}"))?;
        Ok(Self {
            encoder,
            input,
            pending: Vec::with_capacity(FRAME_INTERLEAVED * 4),
            phase: 0.0,
            carry: None,
            scratch: vec![0u8; MAX_PACKET],
        })
    }

    pub fn set_bitrate(&mut self, bitrate_bps: i32) -> Result<()> {
        self.encoder
            .set_bitrate(opus::Bitrate::Bits(bitrate_bps.max(16_000)))
            .map_err(|e| anyhow!("Opus bitrate: {e}"))
    }

    /// Feed interleaved input samples; returns zero or more encoded 20 ms packets.
    pub fn push(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>> {
        self.convert(pcm);
        let mut out = Vec::new();
        while self.pending.len() >= FRAME_INTERLEAVED {
            let frame: Vec<f32> = self.pending.drain(..FRAME_INTERLEAVED).collect();
            let n = self
                .encoder
                .encode_float(&frame, &mut self.scratch)
                .map_err(|e| anyhow!("Opus encode: {e}"))?;
            out.push(self.scratch[..n].to_vec());
        }
        Ok(out)
    }

    /// Downmix to stereo and resample to 48 kHz into `pending`.
    fn convert(&mut self, pcm: &[f32]) {
        let ch = self.input.channels as usize;
        let frames = pcm.len() / ch;
        if frames == 0 {
            return;
        }
        // Stereo view of the input.
        let stereo = |i: usize| -> (f32, f32) {
            let base = i * ch;
            match ch {
                1 => (pcm[base], pcm[base]),
                _ => (pcm[base], pcm[base + 1]),
            }
        };
        if self.input.sample_rate == OPUS_RATE {
            self.pending.reserve(frames * 2);
            for i in 0..frames {
                let (l, r) = stereo(i);
                self.pending.push(l);
                self.pending.push(r);
            }
            return;
        }
        // Linear interpolation resampler. `phase` indexes into a virtual buffer that is
        // [carry, pcm...]; index 0 is the carried frame from the previous call.
        let step = self.input.sample_rate as f64 / OPUS_RATE as f64;
        let get = |idx: usize, carry: Option<(f32, f32)>| -> (f32, f32) {
            match carry {
                Some(c) => {
                    if idx == 0 {
                        c
                    } else {
                        stereo(idx - 1)
                    }
                }
                None => stereo(idx),
            }
        };
        let total = frames + usize::from(self.carry.is_some());
        let mut pos = self.phase;
        while pos + 1.0 < total as f64 {
            let i = pos.floor() as usize;
            let t = (pos - i as f64) as f32;
            let (l0, r0) = get(i, self.carry);
            let (l1, r1) = get(i + 1, self.carry);
            self.pending.push(l0 + (l1 - l0) * t);
            self.pending.push(r0 + (r1 - r0) * t);
            pos += step;
        }
        // Keep the last input frame for the next call and rebase the phase onto it.
        let last = total - 1;
        self.carry = Some(get(last, self.carry));
        self.phase = pos - last as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_20ms_frames_at_48k_stereo() {
        let mut enc = FrameEncoder::new(
            AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            96_000,
        )
        .unwrap();
        let sine: Vec<f32> = (0..FRAME_INTERLEAVED * 3)
            .map(|i| ((i / 2) as f32 * 0.05).sin() * 0.3)
            .collect();
        let packets = enc.push(&sine).unwrap();
        assert_eq!(packets.len(), 3);
        assert!(packets
            .iter()
            .all(|p| !p.is_empty() && p.len() < MAX_PACKET));
    }

    #[test]
    fn resamples_mono_44k_to_stereo_48k() {
        let mut enc = FrameEncoder::new(
            AudioFormat {
                sample_rate: 44_100,
                channels: 1,
            },
            64_000,
        )
        .unwrap();
        // 1 s of mono 44.1 kHz should yield ~50 frames of 20 ms.
        let input: Vec<f32> = (0..44_100).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut n = 0;
        for chunk in input.chunks(441) {
            n += enc.push(chunk).unwrap().len();
        }
        assert!((48..=50).contains(&n), "got {n} frames");
    }
}
