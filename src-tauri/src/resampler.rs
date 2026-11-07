//! Windowed-sinc resampler whose ratio can change on every frame. The ratio has
//! to be adjustable because every sound device runs on its own clock, so a fixed
//! ratio would slowly drain or overflow the buffer in front of each output.

const TAPS: usize = 32;
const PHASES: usize = 256;
const KAISER_BETA: f64 = 8.0;
/// History is stored linearly and compacted when it reaches this many frames.
const HISTORY_FRAMES: usize = TAPS * 8;

pub struct Resampler {
    channels: usize,
    /// `PHASES + 1` rows of `TAPS` coefficients, indexed by fractional position.
    table: Vec<f32>,
    history: Vec<f32>,
    end: usize,
    frac: f64,
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..40 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
    }
    sum
}

fn build_table(cutoff: f64) -> Vec<f32> {
    let half = TAPS as f64 / 2.0;
    let mut table = Vec::with_capacity((PHASES + 1) * TAPS);
    for phase in 0..=PHASES {
        let offset = phase as f64 / PHASES as f64;
        let row: Vec<f64> = (0..TAPS)
            .map(|tap| {
                let x = tap as f64 - (half - 1.0) - offset;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    let a = std::f64::consts::PI * cutoff * x;
                    a.sin() / a
                };
                let w = (1.0 - (x / half).powi(2)).max(0.0).sqrt();
                sinc * bessel_i0(KAISER_BETA * w)
            })
            .collect();
        let sum: f64 = row.iter().sum();
        table.extend(row.iter().map(|c| (c / sum) as f32));
    }
    table
}

impl Resampler {
    /// `ratio` is the nominal input frames consumed per output frame.
    pub fn new(channels: usize, ratio: f64) -> Self {
        // When downsampling, cut below the output's Nyquist frequency to avoid aliasing.
        let cutoff = (1.0 / ratio).min(1.0) * 0.97;
        Self {
            channels,
            table: build_table(cutoff),
            history: vec![0.0; HISTORY_FRAMES * channels],
            end: TAPS,
            frac: 0.0,
        }
    }

    /// Produces one output frame. `pull` fills a slice with the next input frame,
    /// or returns false when none is available; this then returns false as well
    /// and leaves `out` untouched, and can be resumed once more input arrives.
    pub fn next_frame(
        &mut self,
        ratio: f64,
        mut pull: impl FnMut(&mut [f32]) -> bool,
        out: &mut [f32],
    ) -> bool {
        let ch = self.channels;
        while self.frac >= 1.0 {
            if self.end == HISTORY_FRAMES {
                self.history
                    .copy_within((HISTORY_FRAMES - TAPS) * ch..HISTORY_FRAMES * ch, 0);
                self.end = TAPS;
            }
            let slot = &mut self.history[self.end * ch..(self.end + 1) * ch];
            if !pull(slot) {
                return false;
            }
            self.end += 1;
            self.frac -= 1.0;
        }

        let scaled = self.frac as f32 * PHASES as f32;
        let phase = (scaled as usize).min(PHASES - 1);
        let blend = scaled - phase as f32;
        let row_a = &self.table[phase * TAPS..(phase + 1) * TAPS];
        let row_b = &self.table[(phase + 1) * TAPS..(phase + 2) * TAPS];
        let window = &self.history[(self.end - TAPS) * ch..self.end * ch];

        out.fill(0.0);
        for (tap, frame) in window.chunks_exact(ch).enumerate() {
            let coeff = row_a[tap] + (row_b[tap] - row_a[tap]) * blend;
            for (o, s) in out.iter_mut().zip(frame) {
                *o += coeff * s;
            }
        }
        self.frac += ratio;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    /// Resamples a sine and returns the signal-to-residual ratio of the result in dB.
    fn snr_db(src_rate: f64, out_rate: f64, freq: f64) -> f64 {
        let ratio = src_rate / out_rate;
        let mut resampler = Resampler::new(1, ratio);
        let mut n = 0u64;
        let mut output = Vec::new();
        for _ in 0..out_rate as usize {
            let mut frame = [0.0f32];
            let mut pull = |s: &mut [f32]| {
                s[0] = (TAU * freq * n as f64 / src_rate).sin() as f32 * 0.5;
                n += 1;
                true
            };
            assert!(resampler.next_frame(ratio, &mut pull, &mut frame));
            output.push(frame[0] as f64);
        }
        // Skip the filter's start-up transient, then remove the best-fitting sine.
        let tail = &output[4096..];
        let (mut re, mut im) = (0.0, 0.0);
        for (i, s) in tail.iter().enumerate() {
            let a = TAU * freq * i as f64 / out_rate;
            re += s * a.cos();
            im += s * a.sin();
        }
        let len = tail.len() as f64;
        let (re, im) = (2.0 * re / len, 2.0 * im / len);
        let (mut signal, mut noise) = (0.0, 0.0);
        for (i, s) in tail.iter().enumerate() {
            let a = TAU * freq * i as f64 / out_rate;
            let fit = re * a.cos() + im * a.sin();
            signal += fit * fit;
            noise += (s - fit).powi(2);
        }
        10.0 * (signal / noise).log10()
    }

    #[test]
    fn converts_common_rates_cleanly() {
        for (from, to) in [
            (48000.0, 48000.0),
            (44100.0, 48000.0),
            (48000.0, 44100.0),
            (96000.0, 48000.0),
        ] {
            let snr = snr_db(from, to, 1000.0);
            assert!(snr > 70.0, "{from} -> {to}: {snr:.1} dB");
        }
    }

    #[test]
    fn stops_without_input_and_resumes() {
        let mut resampler = Resampler::new(2, 1.0);
        let mut out = [0.0f32; 2];
        assert!(resampler.next_frame(1.0, |_| false, &mut out));
        assert!(!resampler.next_frame(1.0, |_| false, &mut out));
        let refill = |s: &mut [f32]| {
            s.fill(1.0);
            true
        };
        assert!(resampler.next_frame(1.0, refill, &mut out));
    }
}
