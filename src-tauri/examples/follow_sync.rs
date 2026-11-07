//! Measures how closely a follower tracks the main mpv, silently. Start the main
//! mpv first with `--ao=null --input-ipc-server=mpv-audio-splitter`, then:
//!
//!   AUDIO_SPLITTER_SILENT_FOLLOWERS=1 cargo run --release --example follow_sync -- "<device name>" <track>

use audio_splitter_lib::follower::{FollowerConfig, Session};
use audio_splitter_lib::mpv;
use serde_json::json;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let session = Session::start(&[FollowerConfig {
        device: args[0].clone(),
        track: args[1].parse().unwrap(),
        gain_db: Some(0.0),
    }])
    .unwrap();
    let start = Instant::now();
    let mut log = |label: &str| {
        let offset = session.status()[0].offset_ms;
        println!(
            "{:5.1}s {label:<22} offset {offset:?}",
            start.elapsed().as_secs_f64()
        );
    };
    let wait = |seconds: f64, log: &mut dyn FnMut(&str), label: &str| {
        let until = Instant::now() + Duration::from_secs_f64(seconds);
        while Instant::now() < until {
            std::thread::sleep(Duration::from_millis(500));
            log(label);
        }
    };
    wait(5.0, &mut log, "playing");
    mpv::request(&[json!(["seek", 60, "relative"])]).unwrap();
    wait(4.0, &mut log, "after seek +60 s");
    mpv::request(&[mpv::set("pause", true)]).unwrap();
    wait(2.0, &mut log, "paused");
    mpv::request(&[mpv::set("pause", false)]).unwrap();
    wait(3.0, &mut log, "resumed");
    mpv::request(&[mpv::set("speed", 1.5)]).unwrap();
    wait(4.0, &mut log, "main at 1.5x");
    mpv::request(&[mpv::set("speed", 1.0)]).unwrap();
    wait(10.0, &mut log, "back to 1x");
    println!(
        "final: {:?}",
        session.status()[0]
            .offset_ms
            .zip(session.status()[0].error.clone().or(Some(String::new())))
    );
}
