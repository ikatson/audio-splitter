//! End-to-end check on real devices: plays a quiet (-60 dB) tone into the
//! source, runs the engine, and listens to each output through loopback.
//!
//! The tone can be audible: choose outputs that aren't routed to speakers.
//!
//!   cargo run --release --example e2e -- "Line 1" "Voicemeeter In 4" "Voicemeeter In 5"

use audio_splitter_lib::engine::{list_devices, Engine, OutputConfig};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::f64::consts::TAU;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TONE_HZ: f64 = 1000.0;
const AMPLITUDE: f64 = 0.001;
const SECONDS: u64 = 12;
const SETTLE_S: usize = 2;

fn device(host: &cpal::Host, id: &str) -> cpal::Device {
    host.output_devices()
        .unwrap()
        .find(|d| d.id().unwrap().to_string() == id)
        .unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let devices = list_devices().unwrap().outputs;
    let find = |needle: &String| -> String {
        let found = devices.iter().find(|d| d.name.contains(needle.as_str()));
        found
            .unwrap_or_else(|| panic!("no device matching {needle}"))
            .id
            .clone()
    };
    let source = find(&args[0]);
    let outputs: Vec<String> = args[1..].iter().map(find).collect();
    let host = cpal::default_host();

    let source_device = device(&host, &source);
    let config = source_device.default_output_config().unwrap().config();
    let (rate, channels) = (config.sample_rate as f64, config.channels as usize);

    // Other apps may be playing into the source, which lowers the SNR measured below.
    let background = Arc::new(Mutex::new(0.0f32));
    let background_peak = background.clone();
    let probe = source_device
        .build_input_stream::<f32, _, _>(
            config,
            move |data: &[f32], _| {
                let mut peak = background_peak.lock().unwrap();
                *peak = data.iter().fold(*peak, |p, s| p.max(s.abs()));
            },
            |e| eprintln!("probe: {e}"),
            None,
        )
        .unwrap();
    probe.play().unwrap();
    std::thread::sleep(Duration::from_secs(3));
    drop(probe);
    println!(
        "background peak before tone: {:.5}",
        background.lock().unwrap()
    );

    let mut n = 0u64;
    let tone = source_device
        .build_output_stream::<f32, _, _>(
            config,
            move |out, _| {
                for frame in out.chunks_exact_mut(channels) {
                    frame.fill(((TAU * TONE_HZ * n as f64 / rate).sin() * AMPLITUDE) as f32);
                    n += 1;
                }
            },
            |e| eprintln!("tone: {e}"),
            None,
        )
        .unwrap();
    tone.play().unwrap();

    // Output 0 at half level, so the amplitudes below also confirm per-device gain.
    let configs: Vec<OutputConfig> = outputs
        .iter()
        .enumerate()
        .map(|(i, id)| OutputConfig {
            id: id.clone(),
            gain: if i == 0 { 0.5 } else { 1.0 },
        })
        .collect();
    let mut engine = Engine::start(&source, &configs).unwrap();

    let mut listeners = Vec::new();
    for id in &outputs {
        let d = device(&host, id);
        let config = d.default_output_config().unwrap().config();
        let (rate, channels) = (config.sample_rate as usize, config.channels as usize);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        let stream = d
            .build_input_stream::<f32, _, _>(
                config,
                move |data: &[f32], _| {
                    let mut sink = sink.lock().unwrap();
                    sink.extend(data.chunks_exact(channels).map(|f| f[0]));
                },
                |e| eprintln!("listener: {e}"),
                None,
            )
            .unwrap();
        stream.play().unwrap();
        listeners.push((stream, captured, rate));
    }

    std::thread::sleep(Duration::from_secs(SECONDS));
    let status = engine.status();
    let bands = engine.spectrum();
    let loudest = (0..bands.len())
        .max_by(|a, b| bands[*a].total_cmp(&bands[*b]))
        .unwrap();
    println!(
        "spectrum: {} bands, loudest is #{loudest} at {:.2}",
        bands.len(),
        bands[loudest]
    );
    // Snapshot before stopping, so the silence after the engine stops isn't analysed.
    let recordings: Vec<(Vec<f32>, usize)> = listeners
        .iter()
        .map(|(_, captured, rate)| (captured.lock().unwrap().clone(), *rate))
        .collect();
    drop(engine);
    println!("{}", serde_json::to_string_pretty(&status).unwrap());

    for (i, (samples, rate)) in recordings.iter().enumerate() {
        let block = rate / 10;
        let mut snrs = Vec::new();
        let mut levels = Vec::new();
        for chunk in samples[(SETTLE_S * rate).min(samples.len())..].chunks_exact(block) {
            let total: f64 = chunk.iter().map(|s| (*s as f64).powi(2)).sum();
            // Clock-drift correction legitimately shifts the pitch slightly, so search around the tone.
            let tone_energy = (-60..=60)
                .map(|step| {
                    let freq = TONE_HZ + step as f64 * 0.1;
                    let (mut re, mut im) = (0.0, 0.0);
                    for (j, s) in chunk.iter().enumerate() {
                        let a = TAU * freq * j as f64 / *rate as f64;
                        re += *s as f64 * a.cos();
                        im += *s as f64 * a.sin();
                    }
                    2.0 * (re * re + im * im) / block as f64
                })
                .fold(0.0, f64::max);
            snrs.push(10.0 * (tone_energy / (total - tone_energy).max(1e-30)).log10());
            levels.push((2.0 * total / block as f64).sqrt());
        }
        snrs.sort_by(f64::total_cmp);
        levels.sort_by(f64::total_cmp);
        println!(
            "output {i}: {} blocks, amplitude median {:.5} (sent {AMPLITUDE}), SNR median {:.1} dB, worst {:.1} dB",
            snrs.len(),
            levels.get(levels.len() / 2).unwrap_or(&0.0),
            snrs.get(snrs.len() / 2).unwrap_or(&f64::NAN),
            snrs.first().unwrap_or(&f64::NAN),
        );
    }
}
