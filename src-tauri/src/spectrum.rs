//! Turns the most recent captured audio into log-spaced frequency bands for display.

use rtrb::Consumer;
use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FFT_SIZE: usize = 4096;
const BANDS: usize = 64;
// The UI's frequency labels assume this range.
const MIN_HZ: f32 = 30.0;
const MAX_HZ: f32 = 18000.0;
const FLOOR_DB: f32 = -75.0;
const CEILING_DB: f32 = -8.0;
/// Music has far less energy in the highs, so tilt them up to fill the display.
const TILT_DB_PER_OCTAVE: f32 = 3.0;
/// The source delivers nothing while nobody is playing, so stale audio is cleared.
const IDLE_AFTER: Duration = Duration::from_millis(150);

pub struct Analyzer {
    consumer: Consumer<f32>,
    fft: Arc<dyn Fft<f32>>,
    samples: Vec<f32>,
    window: Vec<f32>,
    window_sum: f32,
    spectrum: Vec<Complex<f32>>,
    bin_hz: f32,
    last_data: Instant,
}

impl Analyzer {
    pub fn new(consumer: Consumer<f32>, sample_rate: u32) -> Self {
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_SIZE as f32).cos())
            .collect();
        Self {
            consumer,
            fft: FftPlanner::new().plan_fft_forward(FFT_SIZE),
            samples: vec![0.0; FFT_SIZE],
            window_sum: window.iter().sum(),
            window,
            spectrum: vec![Complex::default(); FFT_SIZE],
            bin_hz: sample_rate as f32 / FFT_SIZE as f32,
            last_data: Instant::now(),
        }
    }

    fn drain(&mut self) {
        let available = self.consumer.slots();
        if available == 0 {
            return;
        }
        self.last_data = Instant::now();
        let keep = available.min(FFT_SIZE);
        if let Ok(chunk) = self.consumer.read_chunk(available) {
            let skip = available - keep;
            self.samples.copy_within(keep.., 0);
            let start = FFT_SIZE - keep;
            for (slot, sample) in self.samples[start..].iter_mut().zip(chunk.into_iter().skip(skip)) {
                *slot = sample;
            }
        }
    }

    /// Band levels from 0 to 1, lowest frequency first.
    pub fn bands(&mut self) -> Vec<f32> {
        self.drain();
        if self.last_data.elapsed() > IDLE_AFTER {
            return vec![0.0; BANDS];
        }

        for ((bin, sample), w) in self.spectrum.iter_mut().zip(&self.samples).zip(&self.window) {
            *bin = Complex::new(sample * w, 0.0);
        }
        self.fft.process(&mut self.spectrum);

        // A full-scale sine reads as 1.0.
        let scale = 2.0 / self.window_sum;
        let magnitude = |bin: usize| self.spectrum[bin.min(FFT_SIZE / 2)].norm() * scale;
        let step = (MAX_HZ / MIN_HZ).powf(1.0 / BANDS as f32);

        (0..BANDS)
            .map(|band| {
                let low = MIN_HZ * step.powi(band as i32);
                let high = low * step;
                let (low_bin, high_bin) = (low / self.bin_hz, high / self.bin_hz);
                let level = if high_bin - low_bin < 1.0 {
                    // Narrower than one FFT bin: interpolate so low bands don't look blocky.
                    let center = (low_bin + high_bin) / 2.0;
                    let below = center as usize;
                    let blend = center - below as f32;
                    magnitude(below) * (1.0 - blend) + magnitude(below + 1) * blend
                } else {
                    (low_bin.ceil() as usize..=high_bin.floor() as usize)
                        .map(magnitude)
                        .fold(0.0, f32::max)
                };
                let center_hz = (low * high).sqrt();
                let db = 20.0 * level.max(1e-7).log10() + TILT_DB_PER_OCTAVE * (center_hz / 1000.0).log2();
                ((db - FLOOR_DB) / (CEILING_DB - FLOOR_DB)).clamp(0.0, 1.0)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    #[test]
    fn a_tone_peaks_in_its_own_band() {
        let rate = 48000.0;
        let (mut producer, consumer) = RingBuffer::new(FFT_SIZE * 2);
        let mut analyzer = Analyzer::new(consumer, rate as u32);
        for hz in [100.0f32, 1000.0, 8000.0] {
            for i in 0..FFT_SIZE {
                let t = i as f32 / rate;
                producer.push((std::f32::consts::TAU * hz * t).sin() * 0.25).unwrap();
            }
            let bands = analyzer.bands();
            let loudest = (0..BANDS).max_by(|a, b| bands[*a].total_cmp(&bands[*b])).unwrap();
            let step = (MAX_HZ / MIN_HZ).powf(1.0 / BANDS as f32);
            let center = MIN_HZ * step.powf(loudest as f32 + 0.5);
            assert!((center / hz).log2().abs() < 0.3, "{hz} Hz showed at {center} Hz");
            assert!(bands[loudest] > 0.5);
        }
    }

    #[test]
    fn clears_when_the_source_goes_quiet() {
        let (mut producer, consumer) = RingBuffer::new(FFT_SIZE);
        let mut analyzer = Analyzer::new(consumer, 48000);
        for i in 0..FFT_SIZE {
            producer.push((i as f32 * 0.1).sin()).unwrap();
        }
        assert!(analyzer.bands().iter().any(|b| *b > 0.0));
        analyzer.last_data -= IDLE_AFTER * 2;
        assert!(analyzer.bands().iter().all(|b| *b == 0.0));
    }

    #[test]
    fn keeps_only_the_newest_samples_in_order() {
        let (mut producer, consumer) = RingBuffer::new(FFT_SIZE * 2);
        let mut analyzer = Analyzer::new(consumer, 48000);
        for i in 0..FFT_SIZE + 10 {
            producer.push(i as f32).unwrap();
        }
        analyzer.drain();
        assert_eq!(analyzer.samples[0], 10.0);
        assert_eq!(analyzer.samples[FFT_SIZE - 1], (FFT_SIZE + 9) as f32);
        producer.push(-1.0).unwrap();
        analyzer.drain();
        assert_eq!(analyzer.samples[0], 11.0);
        assert_eq!(analyzer.samples[FFT_SIZE - 1], -1.0);
    }
}
