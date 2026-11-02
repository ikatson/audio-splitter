//! Captures what a playback device is playing (WASAPI loopback) and repeats it
//! to other playback devices, each behind its own ring buffer and resampler.

use crate::resampler::Resampler;
use crate::spectrum::Analyzer;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, ErrorKind, Stream};
use rtrb::{Consumer, Producer, RingBuffer};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const INITIAL_LATENCY_S: f64 = 0.04;
const MAX_LATENCY_S: f64 = 0.2;
const LATENCY_STEP_S: f64 = 0.01;
/// Buffer this far above the target is thrown away instead of slowly caught up on.
const MAX_EXCESS_S: f64 = 0.15;
/// A gap shorter than this is a glitch; a longer one means the source went quiet.
const GLITCH_GAP_S: f64 = 0.2;
/// Rate correction per second of buffer error, and its limit (0.5% is inaudible).
const DRIFT_GAIN: f64 = 0.1;
const MAX_DRIFT: f64 = 0.005;
const FILL_SMOOTHING_S: f64 = 1.0;
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const ANALYZER_BUFFER_FRAMES: usize = 16384;
/// Gain changes glide over about this long, so moving a slider doesn't click.
const GAIN_SMOOTHING_S: f64 = 0.01;

#[derive(Deserialize)]
pub struct OutputConfig {
    pub id: String,
    /// Linear: 1.0 leaves the level unchanged.
    pub gain: f32,
}

#[derive(Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Serialize)]
pub struct DeviceList {
    pub outputs: Vec<DeviceInfo>,
}

#[derive(Serialize)]
pub struct EndpointStatus {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub latency_ms: f32,
    pub glitches: u64,
    pub dropped_frames: u64,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct Status {
    pub source: EndpointStatus,
    pub outputs: Vec<EndpointStatus>,
}

fn device_id(device: &Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

pub fn list_devices() -> Result<DeviceList, String> {
    let host = cpal::default_host();
    let default_id = host.default_output_device().as_ref().and_then(device_id);
    let devices = host
        .output_devices()
        .map_err(|e| format!("Could not list audio devices: {e}"))?;
    let mut outputs: Vec<DeviceInfo> = devices
        .filter_map(|device| {
            let id = device_id(&device)?;
            Some(DeviceInfo {
                is_default: default_id.as_ref() == Some(&id),
                name: device.to_string(),
                id,
            })
        })
        .collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(DeviceList { outputs })
}

/// Counters shared between an audio callback and the status poll.
#[derive(Default)]
struct Shared {
    gain_bits: AtomicU32,
    latency_us: AtomicU32,
    glitches: AtomicU64,
    dropped_frames: AtomicU64,
    error: Mutex<Option<String>>,
}

impl Shared {
    fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Relaxed))
    }

    fn set_gain(&self, gain: f32) {
        self.gain_bits.store(gain.max(0.0).to_bits(), Relaxed);
    }

    fn on_stream_error(&self, error: cpal::Error) {
        match error.kind() {
            ErrorKind::Xrun => {
                self.glitches.fetch_add(1, Relaxed);
            }
            ErrorKind::DeviceChanged => {}
            _ => *self.error.lock().unwrap() = Some(error.to_string()),
        }
    }
}

/// Converts one frame between channel layouts, assuming standard WAVE channel order.
fn mix_frame(src: &[f32], dst: &mut [f32]) {
    if src.len() == dst.len() {
        dst.copy_from_slice(src);
    } else if src.len() == 1 {
        dst.fill(src[0]);
    } else if dst.len() == 1 {
        dst[0] = src.iter().sum::<f32>() / src.len() as f32;
    } else if dst.len() == 2 && src.len() >= 6 {
        // FL FR C LFE then surround pairs. The LFE channel is left out, as is usual for downmixes.
        const SURROUND: f32 = std::f32::consts::FRAC_1_SQRT_2;
        const GAIN: f32 = 0.7;
        let center = src[2] * SURROUND;
        let (mut left, mut right) = (src[0] + center, src[1] + center);
        for pair in src[4..].chunks_exact(2) {
            left += pair[0] * SURROUND;
            right += pair[1] * SURROUND;
        }
        dst[0] = (left * GAIN).clamp(-1.0, 1.0);
        dst[1] = (right * GAIN).clamp(-1.0, 1.0);
    } else {
        for (i, d) in dst.iter_mut().enumerate() {
            *d = src.get(i).copied().unwrap_or(0.0);
        }
    }
}

/// The capture side's handle on one output.
struct Sink {
    producer: Producer<f32>,
    scratch: Vec<f32>,
    shared: Arc<Shared>,
}

impl Sink {
    fn push_frame(&mut self, frame: &[f32]) {
        mix_frame(frame, &mut self.scratch);
        if self.producer.slots() < self.scratch.len() {
            self.shared.dropped_frames.fetch_add(1, Relaxed);
            return;
        }
        for &sample in &self.scratch {
            // Capacity was checked above and this is the only producer.
            let _ = self.producer.push(sample);
        }
    }
}

/// The output callback's state: drains the ring buffer, resampling to the device's
/// rate and nudging the ratio to hold the buffer at the target level.
struct Pump {
    consumer: Consumer<f32>,
    resampler: Resampler,
    channels: usize,
    src_rate: f64,
    out_rate: f64,
    target: f64,
    fill: f64,
    buffering: bool,
    has_played: bool,
    gap_frames: u64,
    gain: f32,
    gain_step: f32,
    shared: Arc<Shared>,
}

impl Pump {
    fn new(consumer: Consumer<f32>, channels: usize, src_rate: u32, out_rate: u32, shared: Arc<Shared>) -> Self {
        let (src_rate, out_rate) = (src_rate as f64, out_rate as f64);
        Self {
            consumer,
            resampler: Resampler::new(channels, src_rate / out_rate),
            channels,
            src_rate,
            out_rate,
            target: INITIAL_LATENCY_S * src_rate,
            fill: 0.0,
            buffering: true,
            has_played: false,
            gap_frames: 0,
            gain: shared.gain(),
            gain_step: (1.0 - (-1.0 / (GAIN_SMOOTHING_S * out_rate)).exp()) as f32,
            shared,
        }
    }

    fn fill_output(&mut self, out: &mut [f32]) {
        let ch = self.channels;
        let frames = out.len() / ch;
        let available = (self.consumer.slots() / ch) as f64;

        if self.buffering {
            if available < self.target {
                out.fill(0.0);
                self.gap_frames += frames as u64;
                return;
            }
            self.buffering = false;
            if self.has_played && (self.gap_frames as f64) < GLITCH_GAP_S * self.out_rate {
                self.shared.glitches.fetch_add(1, Relaxed);
                self.target = (self.target + LATENCY_STEP_S * self.src_rate).min(MAX_LATENCY_S * self.src_rate);
            }
            self.has_played = true;
            self.fill = available;
        }

        let excess = available - self.target;
        if excess > MAX_EXCESS_S * self.src_rate {
            let skip = excess as usize;
            if let Ok(chunk) = self.consumer.read_chunk(skip * ch) {
                chunk.commit_all();
                self.shared.dropped_frames.fetch_add(skip as u64, Relaxed);
            }
            self.fill = self.target;
        } else {
            let alpha = 1.0 - (-(frames as f64) / (self.out_rate * FILL_SMOOTHING_S)).exp();
            self.fill += (available - self.fill) * alpha;
        }

        let error_s = (self.fill - self.target) / self.src_rate;
        let drift = (error_s * DRIFT_GAIN).clamp(-MAX_DRIFT, MAX_DRIFT);
        let ratio = self.src_rate / self.out_rate * (1.0 + drift);

        let consumer = &mut self.consumer;
        let mut pull = |slot: &mut [f32]| {
            if consumer.slots() < slot.len() {
                return false;
            }
            for sample in slot.iter_mut() {
                *sample = consumer.pop().unwrap_or(0.0);
            }
            true
        };
        let target_gain = self.shared.gain();
        for (i, frame) in out.chunks_exact_mut(ch).enumerate() {
            if !self.resampler.next_frame(ratio, &mut pull, frame) {
                out[i * ch..].fill(0.0);
                self.buffering = true;
                self.gap_frames = (frames - i) as u64;
                break;
            }
            self.gain += (target_gain - self.gain) * self.gain_step;
            for sample in frame.iter_mut() {
                // Boosting can exceed full scale; clip here rather than wrap in the device.
                *sample = (*sample * self.gain).clamp(-1.0, 1.0);
            }
        }

        let latency_s = self.fill / self.src_rate;
        self.shared.latency_us.store((latency_s * 1e6) as u32, Relaxed);
    }
}

struct Endpoint {
    id: String,
    name: String,
    sample_rate: u32,
    channels: u16,
    shared: Arc<Shared>,
}

impl Endpoint {
    fn status(&self) -> EndpointStatus {
        let shared = &self.shared;
        EndpointStatus {
            name: self.name.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            latency_ms: shared.latency_us.load(Relaxed) as f32 / 1000.0,
            glitches: shared.glitches.load(Relaxed),
            dropped_frames: shared.dropped_frames.load(Relaxed),
            error: shared.error.lock().unwrap().clone(),
        }
    }
}

/// Audio flows for as long as this value is alive.
pub struct Engine {
    // Capture first, so it stops before the outputs it feeds.
    _streams: Vec<Stream>,
    source: Endpoint,
    outputs: Vec<Endpoint>,
    analyzer: Analyzer,
}

fn find_device(host: &cpal::Host, id: &str) -> Result<Device, String> {
    let mut devices = host
        .output_devices()
        .map_err(|e| format!("Could not list audio devices: {e}"))?;
    devices
        .find(|d| device_id(d).as_deref() == Some(id))
        .ok_or_else(|| format!("Device is no longer available: {id}"))
}

impl Engine {
    pub fn start(source_id: &str, output_configs: &[OutputConfig]) -> Result<Self, String> {
        if output_configs.is_empty() {
            return Err("Choose at least one output.".into());
        }
        if output_configs.iter().any(|output| output.id == source_id) {
            return Err("An output can't be the same as the source: it would feed back into itself.".into());
        }
        if output_configs.len() == 2 && output_configs[0].id == output_configs[1].id {
            return Err("Choose two different outputs.".into());
        }

        let host = cpal::default_host();
        let source_device = find_device(&host, source_id)?;
        let source_config = source_device
            .default_output_config()
            .map_err(|e| format!("{source_device}: {e}"))?
            .config();
        let src_rate = source_config.sample_rate;
        let src_channels = source_config.channels as usize;

        let mut sinks = Vec::new();
        let mut outputs = Vec::new();
        let mut output_streams = Vec::new();
        for output in output_configs {
            let device = find_device(&host, &output.id)?;
            let config = device
                .default_output_config()
                .map_err(|e| format!("{device}: {e}"))?
                .config();
            let channels = config.channels as usize;
            let shared = Arc::new(Shared::default());
            shared.set_gain(output.gain);
            let (producer, consumer) = RingBuffer::new(src_rate as usize * channels);
            let mut pump = Pump::new(consumer, channels, src_rate, config.sample_rate, shared.clone());
            let on_error = shared.clone();
            let stream = device
                .build_output_stream::<f32, _, _>(
                    config,
                    move |out, _| pump.fill_output(out),
                    move |e| on_error.on_stream_error(e),
                    Some(OPEN_TIMEOUT),
                )
                .map_err(|e| format!("{device}: {e}"))?;
            stream.play().map_err(|e| format!("{device}: {e}"))?;
            output_streams.push(stream);
            sinks.push(Sink {
                producer,
                scratch: vec![0.0; channels],
                shared: shared.clone(),
            });
            outputs.push(Endpoint {
                id: output.id.clone(),
                name: device.to_string(),
                sample_rate: config.sample_rate,
                channels: config.channels,
                shared,
            });
        }

        let source_shared = Arc::new(Shared::default());
        let on_error = source_shared.clone();
        let (mut analyzer_input, analyzer_output) = RingBuffer::new(ANALYZER_BUFFER_FRAMES);
        // cpal opens a loopback capture when an input stream is built on a playback device.
        let capture = source_device
            .build_input_stream::<f32, _, _>(
                source_config,
                move |data: &[f32], _| {
                    for frame in data.chunks_exact(src_channels) {
                        for sink in &mut sinks {
                            sink.push_frame(frame);
                        }
                        // When nobody is reading (hidden window), the analyzer simply misses audio.
                        let _ = analyzer_input.push(frame.iter().sum::<f32>() / src_channels as f32);
                    }
                },
                move |e| on_error.on_stream_error(e),
                Some(OPEN_TIMEOUT),
            )
            .map_err(|e| format!("{source_device}: {e}"))?;
        capture.play().map_err(|e| format!("{source_device}: {e}"))?;

        let mut streams = vec![capture];
        streams.append(&mut output_streams);
        Ok(Self {
            _streams: streams,
            source: Endpoint {
                id: source_id.to_string(),
                name: source_device.to_string(),
                sample_rate: src_rate,
                channels: source_config.channels,
                shared: source_shared,
            },
            outputs,
            analyzer: Analyzer::new(analyzer_output, src_rate),
        })
    }

    pub fn set_gain(&self, id: &str, gain: f32) {
        if let Some(output) = self.outputs.iter().find(|output| output.id == id) {
            output.shared.set_gain(gain);
        }
    }

    pub fn spectrum(&mut self) -> Vec<f32> {
        self.analyzer.bands()
    }

    pub fn status(&self) -> Status {
        Status {
            source: self.source.status(),
            outputs: self.outputs.iter().map(Endpoint::status).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pump_with(src_rate: u32, out_rate: u32) -> (Producer<f32>, Pump) {
        let (producer, consumer) = RingBuffer::new(src_rate as usize);
        let shared = Arc::new(Shared::default());
        shared.set_gain(1.0);
        let pump = Pump::new(consumer, 1, src_rate, out_rate, shared);
        (producer, pump)
    }

    /// Plays a steady 0.5 signal at `gain` and returns the level once settled.
    fn settled_level(gain: f32) -> f32 {
        let (mut producer, mut pump) = pump_with(48000, 48000);
        let mut out = [0.0f32; 480];
        feed(&mut producer, 2400);
        pump.fill_output(&mut out);
        pump.shared.set_gain(gain);
        for _ in 0..20 {
            feed(&mut producer, 480);
            pump.fill_output(&mut out);
        }
        out[479]
    }

    #[test]
    fn applies_gain_and_clips_boosts_at_full_scale() {
        assert!((settled_level(1.0) - 0.5).abs() < 0.01);
        assert!((settled_level(0.5) - 0.25).abs() < 0.01);
        assert!(settled_level(0.0).abs() < 0.001);
        assert_eq!(settled_level(4.0), 1.0);
    }

    fn feed(producer: &mut Producer<f32>, frames: usize) {
        for _ in 0..frames {
            producer.push(0.5).unwrap();
        }
    }

    #[test]
    fn mixes_between_layouts() {
        let mut stereo = [0.0; 2];
        mix_frame(&[0.25], &mut stereo);
        assert_eq!(stereo, [0.25, 0.25]);
        let mut mono = [0.0; 1];
        mix_frame(&[0.5, 1.0], &mut mono);
        assert_eq!(mono, [0.75]);
        // Hard-left content in a 7.1 stream stays on the left.
        mix_frame(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], &mut stereo);
        assert!(stereo[0] > 0.5 && stereo[1] == 0.0);
        let mut surround = [1.0; 6];
        mix_frame(&[0.5, 0.25], &mut surround);
        assert_eq!(surround, [0.5, 0.25, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn stays_silent_until_buffered_then_plays() {
        let (mut producer, mut pump) = pump_with(48000, 48000);
        let mut out = [1.0f32; 480];
        feed(&mut producer, 480);
        pump.fill_output(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
        feed(&mut producer, 4800);
        pump.fill_output(&mut out);
        pump.fill_output(&mut out);
        assert!(out.iter().any(|s| *s != 0.0));
    }

    #[test]
    fn counts_a_short_gap_as_a_glitch_but_not_a_long_one() {
        let run = |gap_callbacks: usize| {
            let (mut producer, mut pump) = pump_with(48000, 48000);
            let mut out = [0.0f32; 480];
            feed(&mut producer, 2400);
            while !pump.buffering || !pump.has_played {
                pump.fill_output(&mut out);
            }
            for _ in 0..gap_callbacks {
                pump.fill_output(&mut out);
            }
            feed(&mut producer, 2400);
            pump.fill_output(&mut out);
            pump.shared.glitches.load(Relaxed)
        };
        assert_eq!(run(1), 1);
        assert_eq!(run(100), 0);
    }

    #[test]
    fn tracks_a_faster_source_clock_without_dropping() {
        // The source runs 200 ppm fast; the fill level must settle instead of growing.
        let (mut producer, mut pump) = pump_with(48000, 48000);
        let mut out = [0.0f32; 480];
        feed(&mut producer, 2400);
        let mut owed = 0.0;
        for _ in 0..30_000 {
            pump.fill_output(&mut out);
            owed += 480.0 * 1.0002;
            let whole = owed as usize;
            feed(&mut producer, whole);
            owed -= whole as f64;
        }
        assert_eq!(pump.shared.dropped_frames.load(Relaxed), 0);
        assert_eq!(pump.shared.glitches.load(Relaxed), 0);
        let settled = pump.fill / pump.src_rate;
        assert!((settled - INITIAL_LATENCY_S).abs() < 0.01, "settled at {settled}");
    }
}
