//! Consumer-side stream conditioning between the IPC ring and WASAPI.
//!
//! * **Clock-drift compensation**: the DAW's audio interface and the WASAPI output device
//!   run on independent clocks. A PI controller watches the ring fill level and nudges a
//!   4-point (Catmull-Rom) resampler by at most ±0.2 %, so the cushion neither drains
//!   (dropouts) nor grows until an overrun reset (skips).
//! * **De-clicking**: underruns, overrun jumps and resumes are smoothed (decaying offset /
//!   short fade-in) instead of hard steps.
//! * **Smooth volume / mute** with a soft clipper when boosting above 100 %.
//!
//! Real-time safe after construction: no allocations in `render`.

use dawstream_ipc::{apply_gain_staged, SharedAudioRing};

/// Maximum deviation from the nominal rate (±0.2 % ≈ ±3.5 cents, inaudible).
const MAX_RATIO_DEVIATION: f64 = 0.002;
/// Proportional gain: ratio change per frame of (smoothed) fill error.
const KP: f64 = 2e-6;
/// Integral gain: ratio change per frame-second of accumulated fill error.
const KI: f64 = 1e-7;
/// Cap on the integral term's contribution (covers ±1000 ppm of clock mismatch).
const MAX_INTEGRAL_RATIO: f64 = 0.001;
/// Time constant of the fill-level low-pass (smooths out bursty DAW block delivery).
const FILL_EMA_SECONDS: f64 = 1.0;
/// Fade-in length after (re)starting playback.
const FADE_IN_SECONDS: f32 = 0.005;
/// Time constant of the decaying offset that removes steps at discontinuities.
const DECLICK_SECONDS: f32 = 0.002;
/// Time constant of the output volume / mute smoothing.
const GAIN_SMOOTH_SECONDS: f32 = 0.01;

pub struct StreamConditioner {
    sample_rate: f64,
    /// Last three input frames before the current chunk (resampler history).
    hist: [[f32; 2]; 3],
    /// Fractional read position into the extended input (history + chunk).
    frac: f64,
    /// Input frames consumed per output frame (1.0 = nominal).
    ratio: f64,
    fill_ema: f64,
    integral: f64,
    controller_primed: bool,
    /// Interleaved staging buffer for frames pulled from the ring (preallocated).
    in_buf: Vec<f32>,
    playing: bool,
    /// Output (pre-gain) of the last rendered frame, for de-click residuals.
    last_y: [f32; 2],
    declick: [f32; 2],
    declick_coef: f32,
    fade_in_len: usize,
    gain: f32,
    gain_coef: f32,
}

impl StreamConditioner {
    /// `max_output_frames` is the largest chunk `render` will be asked for
    /// (the WASAPI buffer size); the staging buffer is sized from it up front.
    pub fn new(sample_rate: u32, max_output_frames: usize, initial_gain: f32) -> Self {
        let sr = sample_rate.max(1) as f32;
        let max_in = (max_output_frames as f64 * (1.0 + MAX_RATIO_DEVIATION)).ceil() as usize + 4;
        Self {
            sample_rate: sr as f64,
            hist: [[0.0; 2]; 3],
            frac: 0.0,
            ratio: 1.0,
            fill_ema: 0.0,
            integral: 0.0,
            controller_primed: false,
            in_buf: vec![0.0; max_in * 2],
            playing: false,
            last_y: [0.0; 2],
            declick: [0.0; 2],
            declick_coef: (-1.0 / (DECLICK_SECONDS * sr)).exp(),
            fade_in_len: ((FADE_IN_SECONDS * sr) as usize).max(1),
            gain: initial_gain,
            gain_coef: 1.0 - (-1.0 / (GAIN_SMOOTH_SECONDS * sr)).exp(),
        }
    }

    #[cfg(test)]
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    fn reset_stream(&mut self) {
        self.hist = [[0.0; 2]; 3];
        self.frac = 0.0;
        self.controller_primed = false;
        self.playing = false;
    }

    /// Fills `out` (interleaved stereo) with conditioned audio from the ring.
    /// `connected`: whether the plugin is alive (otherwise silence is rendered without
    /// touching the ring). `gain_target`: output volume (0 = muted); the ring keeps being
    /// drained while muted so it doesn't overflow.
    pub fn render(
        &mut self,
        ring: &SharedAudioRing,
        out: &mut [f32],
        target_latency_frames: u64,
        connected: bool,
        gain_target: f32,
    ) {
        let n = out.len() / 2;
        if n == 0 {
            return;
        }

        let mut started = false;
        if !connected {
            out[..n * 2].fill(0.0);
            self.end_playback();
        } else {
            let max_in = self.in_buf.len() / 2;
            let m = ((self.frac + n as f64 * self.ratio).floor() as usize).clamp(1, max_in);
            let got = ring.read_interleaved(&mut self.in_buf[..m * 2], target_latency_frames);
            let jumped = ring.take_discontinuity();

            if got == 0 {
                // Buffering (startup or underrun): silence, de-clicked from the last sample.
                out[..n * 2].fill(0.0);
                self.end_playback();
            } else {
                if !self.playing {
                    // Resume from silence: short fade-in on the incoming audio.
                    started = true;
                    let len = self.fade_in_len.min(m);
                    for i in 0..len {
                        let g = i as f32 / len as f32;
                        self.in_buf[i * 2] *= g;
                        self.in_buf[i * 2 + 1] *= g;
                    }
                } else if jumped {
                    // Overrun reset skipped audio: seed resampler history with the first
                    // post-jump frame so Catmull-Rom does not ring across the jump and
                    // out[0] equals the exact post-jump sample for the de-clicker.
                    let first = [self.in_buf[0], self.in_buf[1]];
                    self.hist = [first; 3];
                    self.frac = 0.0;
                }
                self.resample(m, &mut out[..n * 2]);
                if jumped && !started {
                    // Remove the step between the previous output and the first post-jump sample.
                    self.declick[0] += self.last_y[0] - out[0];
                    self.declick[1] += self.last_y[1] - out[1];
                }
                self.playing = got == m;
                if got == m {
                    self.update_controller(ring.available_frames(), target_latency_frames, n);
                } else {
                    // Partial read: the ring just underran; start over cleanly next time.
                    self.reset_stream();
                }
            }
        }

        // De-click, then smoothed gain + gain staging.
        for f in 0..n {
            for c in 0..2 {
                let y = out[f * 2 + c] + self.declick[c];
                self.declick[c] *= self.declick_coef;
                if f == n - 1 {
                    self.last_y[c] = y;
                }
                out[f * 2 + c] = y;
            }
            self.gain += (gain_target - self.gain) * self.gain_coef;
            for c in 0..2 {
                out[f * 2 + c] = apply_gain_staged(out[f * 2 + c], self.gain);
            }
        }
    }

    /// Transition to silence: the decaying residual takes the output from the last
    /// sample to zero instead of stepping.
    fn end_playback(&mut self) {
        if self.playing {
            self.declick[0] += self.last_y[0];
            self.declick[1] += self.last_y[1];
        }
        self.reset_stream();
    }

    /// Resamples `in_buf[..m]` into `out` (n frames) at the current ratio.
    fn resample(&mut self, m: usize, out: &mut [f32]) {
        let n = out.len() / 2;
        let hist = self.hist;
        let inb = &self.in_buf;
        let ext = |k: usize, c: usize| -> f32 {
            if k < 3 {
                hist[k][c]
            } else {
                inb[(k - 3) * 2 + c]
            }
        };
        for j in 0..n {
            let p = self.frac + j as f64 * self.ratio;
            let mut i = p.floor() as usize;
            let mut t = (p - i as f64) as f32;
            if i > m - 1 {
                // Only reachable by < 1 sample when ratio < 1; hold the newest frame.
                i = m - 1;
                t = 1.0;
            }
            for c in 0..2 {
                let (y0, y1, y2, y3) = (ext(i, c), ext(i + 1, c), ext(i + 2, c), ext(i + 3, c));
                // Catmull-Rom between y1 and y2.
                let a = -0.5 * y0 + 1.5 * y1 - 1.5 * y2 + 0.5 * y3;
                let b = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
                let cc = -0.5 * y0 + 0.5 * y2;
                out[j * 2 + c] = ((a * t + b) * t + cc) * t + y1;
            }
        }
        let next = [
            [ext(m, 0), ext(m, 1)],
            [ext(m + 1, 0), ext(m + 1, 1)],
            [ext(m + 2, 0), ext(m + 2, 1)],
        ];
        self.hist = next;
        self.frac = (self.frac + n as f64 * self.ratio - m as f64).clamp(0.0, 1.0);
    }

    fn update_controller(&mut self, fill: u64, target: u64, frames: usize) {
        let fill = fill as f64;
        let dt = frames as f64 / self.sample_rate;
        if !self.controller_primed {
            self.fill_ema = fill;
            self.controller_primed = true;
        } else {
            let alpha = (dt / FILL_EMA_SECONDS).min(1.0);
            self.fill_ema += alpha * (fill - self.fill_ema);
        }
        // Positive error: the producer runs fast, consume slightly faster.
        let err = self.fill_ema - target as f64;
        let lim = MAX_INTEGRAL_RATIO / KI;
        self.integral = (self.integral + err * dt).clamp(-lim, lim);
        let dev = (KP * err + KI * self.integral).clamp(-MAX_RATIO_DEVIATION, MAX_RATIO_DEVIATION);
        self.ratio = 1.0 + dev;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(tag: &str) -> SharedAudioRing {
        let name = format!(
            "Local\\DAWStream_DspTest_{tag}_{}_{:?}",
            std::process::id(),
            std::time::Instant::now()
        );
        SharedAudioRing::open_or_create_named(&name).unwrap()
    }

    /// Producer and consumer on clocks that differ by `ppm`; returns (gap chunks after
    /// the first sound, final ratio).
    fn run_drift(ppm: f64, block: usize, seconds: f64) -> (usize, f64) {
        let sr = 48_000u32;
        let period = 480usize;
        let ring = ring(&format!("drift{}", ppm as i64));
        ring.set_sample_rate(sr);
        ring.set_producer_block_frames(block as u32);
        let base = 960u64;
        let target = ring.effective_target_latency(base, period as u64);
        ring.resync_consumer(target);

        let mut cond = StreamConditioner::new(sr, period * 2, 1.0);
        let src = vec![0.25f32; block];
        let mut out = vec![0.0f32; period * 2];
        let producer_rate = 1.0 + ppm * 1e-6;
        let (mut produced, mut consumed_time) = (0.0f64, 0.0f64);
        let (mut started, mut gaps) = (false, 0usize);
        let chunks = (seconds * sr as f64 / period as f64) as usize;
        for _ in 0..chunks {
            ring.bump_app_heartbeat();
            consumed_time += period as f64 / sr as f64;
            while produced < consumed_time * sr as f64 * producer_rate {
                ring.write_planar(&src, &src, 1.0, false);
                produced += block as f64;
            }
            cond.render(&ring, &mut out, target, true, 1.0);
            let silent = out.iter().all(|&s| s.abs() < 1e-6);
            if !silent {
                started = true;
            } else if started {
                gaps += 1;
            }
        }
        (gaps, cond.ratio())
    }

    #[test]
    fn tracks_fast_producer_clock_without_gaps() {
        let (gaps, ratio) = run_drift(150.0, 256, 300.0);
        assert_eq!(gaps, 0);
        assert!(ratio > 1.0, "ratio {ratio}");
    }

    #[test]
    fn tracks_slow_producer_clock_without_gaps() {
        let (gaps, ratio) = run_drift(-150.0, 256, 300.0);
        assert_eq!(gaps, 0);
        assert!(ratio < 1.0, "ratio {ratio}");
    }

    #[test]
    fn large_blocks_with_drift_play_without_gaps() {
        let (gaps, _) = run_drift(100.0, 4096, 300.0);
        assert_eq!(gaps, 0);
    }

    #[test]
    fn unity_ratio_reproduces_input_delayed() {
        let ring = ring("identity");
        ring.bump_app_heartbeat();
        ring.resync_consumer(0);
        let mut cond = StreamConditioner::new(48_000, 64, 1.0);
        let left: Vec<f32> = (0..64).map(|i| (i as f32) / 128.0).collect();
        ring.write_planar(&left, &left, 1.0, false);
        // Already "playing" so no fade-in shapes the samples.
        cond.playing = true;
        let mut out = vec![0.0f32; 64];
        cond.render(&ring, &mut out, 0, true, 1.0);
        // Two frames of resampler latency; samples land exactly at ratio 1.0.
        for j in 2..32 {
            assert!((out[j * 2] - left[j - 2]).abs() < 1e-6, "frame {j}");
        }
    }

    #[test]
    fn mute_ramps_instead_of_stepping() {
        let ring = ring("muteramp");
        ring.bump_app_heartbeat();
        ring.resync_consumer(0);
        let mut cond = StreamConditioner::new(48_000, 256, 1.0);
        cond.playing = true;
        ring.write_planar(&[0.5; 256], &[0.5; 256], 1.0, false);
        let mut out = vec![0.0f32; 512];
        cond.render(&ring, &mut out, 0, true, 0.0);
        // First frames still near full level, decaying monotonically toward 0.
        assert!(out[2 * 4] > 0.4);
        assert!(out[2 * 255] < out[2 * 4]);
    }

    #[test]
    fn overrun_declick_removes_boundary_step() {
        let ring = ring("overrun_declick");
        ring.bump_app_heartbeat();
        let target = 64u64;
        ring.resync_consumer(target);

        let mut cond = StreamConditioner::new(48_000, 64, 1.0);
        // Prime steady playback at +0.8.
        ring.write_planar(&[0.8; 128], &[0.8; 128], 1.0, false);
        let mut out = vec![0.0f32; 64]; // 32 frames
        cond.playing = true;
        cond.render(&ring, &mut out, target, true, 1.0);
        let prev_end = out[out.len() - 2];
        assert!((prev_end - 0.8).abs() < 1e-4);

        // Trigger an overrun jump into -0.8 (1.6-amplitude step).
        ring.write_planar(&[-0.8; 512], &[-0.8; 512], 1.0, false);
        cond.render(&ring, &mut out, target, true, 1.0);

        // First sample must stay continuous with prev_end, and no huge step should appear.
        assert!(
            (out[0] - prev_end).abs() < 0.05,
            "boundary step: {prev_end} -> {}",
            out[0]
        );
        for f in 1..32 {
            let delta = (out[f * 2] - out[(f - 1) * 2]).abs();
            assert!(delta < 0.05, "click at frame {f}: delta={delta}");
        }
    }
}
