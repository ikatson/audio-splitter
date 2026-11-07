//! Plays other audio tracks of the video open in mpv on other devices. Each
//! device gets a hidden mpv that follows the main one: pause and seeks are
//! copied, and drift is corrected by playing slightly faster or slower.

use crate::mpv::{self, get, request_on, set};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Larger offsets are fixed by seeking, smaller ones by changing speed.
const SEEK_THRESHOLD_S: f64 = 0.3;
/// Offsets below this are left alone.
const DEADBAND_S: f64 = 0.01;
/// Speed change per second of offset, and its limit. mpv keeps the pitch.
const SPEED_GAIN: f64 = 0.5;
const MAX_SPEED_CHANGE: f64 = 0.03;
const OFFSET_SMOOTHING: f64 = 0.3;
/// How long after a seek playback takes to settle before measuring again.
const SEEK_SETTLE: Duration = Duration::from_millis(400);
/// Seeking to the main mpv's position lands behind it by the time playback
/// resumes; aim this far ahead to start with, then learn from each seek.
const INITIAL_SEEK_LEAD_S: f64 = 0.1;

#[derive(Deserialize)]
pub struct FollowerConfig {
    /// The device as Windows names it.
    pub device: String,
    pub track: i64,
    /// `None` mutes.
    pub gain_db: Option<f64>,
}

#[derive(Serialize, Clone)]
pub struct FollowerStatus {
    pub device: String,
    /// How far behind the main mpv this one plays, in milliseconds.
    pub offset_ms: Option<f64>,
    pub error: Option<String>,
}

struct Follower {
    device: String,
    pipe: String,
    child: Child,
    offset: Option<f64>,
    seek_lead: f64,
    seeked_at: Option<Instant>,
    speed: f64,
    error: Option<String>,
}

fn volume_filter(gain_db: Option<f64>) -> String {
    match gain_db {
        Some(db) => format!("lavfi=[volume={db}dB]"),
        None => "lavfi=[volume=0]".into(),
    }
}

/// Followers are killed with the app, even if it crashes, by putting them in a job.
fn kill_on_exit(child: &Child) {
    use windows_sys::Win32::System::JobObjects::*;
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        job as usize
    });
    unsafe {
        AssignProcessToJobObject(job as _, child.as_raw_handle() as _);
    }
}

impl Follower {
    fn spawn(
        index: usize,
        config: &FollowerConfig,
        file: &str,
        start: f64,
    ) -> Result<Self, String> {
        let device = mpv::device_name(&config.device)?;
        let pipe = format!("{}-follower-{index}", mpv::MAIN_PIPE);
        let mut command = Command::new("mpv");
        // Lets sync be tested without playing sound.
        if std::env::var_os("AUDIO_SPLITTER_SILENT_FOLLOWERS").is_some() {
            command.arg("--ao=null");
        }
        let child = command
            .args([
                "--no-config".to_string(),
                "--vid=no".into(),
                "--sid=no".into(),
                "--force-window=no".into(),
                "--keep-open=yes".into(),
                "--pause".into(),
                format!("--aid={}", config.track),
                format!("--audio-device={device}"),
                format!("--af={}", volume_filter(config.gain_db)),
                format!("--start={start}"),
                format!("--input-ipc-server={pipe}"),
                "--".into(),
                file.into(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("Could not start mpv: {e}"))?;
        kill_on_exit(&child);
        Ok(Self {
            device: config.device.clone(),
            pipe,
            child,
            offset: None,
            seek_lead: INITIAL_SEEK_LEAD_S,
            seeked_at: None,
            speed: 1.0,
            error: None,
        })
    }

    fn seek(&mut self, position: f64) {
        let _ = request_on(
            &self.pipe,
            &[serde_json::json!(["seek", position, "absolute+exact"])],
        );
        self.offset = None;
    }

    /// One step of following the main mpv, which was at `main_pos` when this was called.
    fn follow(&mut self, main_pos: f64, main_paused: bool, main_speed: f64, measured_at: Instant) {
        if let Ok(Some(status)) = self.child.try_wait() {
            self.error = Some(format!("Its mpv stopped ({status})"));
            return;
        }
        // Not answering yet means it's still starting.
        let Ok(reply) = request_on(&self.pipe, &[get("time-pos"), get("pause"), get("seeking")])
        else {
            return;
        };
        let [pos, paused, seeking]: [Value; 3] = reply.try_into().unwrap();
        if paused.as_bool() != Some(main_paused) {
            let _ = request_on(&self.pipe, &[set("pause", main_paused)]);
        }
        let (Some(pos), false) = (pos.as_f64(), seeking == true) else {
            return;
        };
        // The main mpv kept playing while this one was being asked.
        let main_now = if main_paused {
            main_pos
        } else {
            main_pos + measured_at.elapsed().as_secs_f64() * main_speed
        };
        let offset = main_now - pos;

        if main_paused {
            self.seeked_at = None;
            if offset.abs() > DEADBAND_S {
                self.seek(main_now);
            }
            return;
        }
        if let Some(at) = self.seeked_at {
            if at.elapsed() < SEEK_SETTLE {
                return;
            }
            self.seeked_at = None;
            self.seek_lead = (self.seek_lead + offset).clamp(0.0, 1.0);
        }

        let smoothed = match self.offset {
            Some(previous) => previous + (offset - previous) * OFFSET_SMOOTHING,
            None => offset,
        };
        self.offset = Some(smoothed);
        if smoothed.abs() > SEEK_THRESHOLD_S {
            self.seek(main_now + self.seek_lead);
            self.seeked_at = Some(Instant::now());
            return;
        }
        let speed = if smoothed.abs() < DEADBAND_S {
            main_speed
        } else {
            main_speed * (1.0 + (smoothed * SPEED_GAIN).clamp(-MAX_SPEED_CHANGE, MAX_SPEED_CHANGE))
        };
        if (speed - self.speed).abs() > 1e-4 {
            let _ = request_on(&self.pipe, &[set("speed", speed)]);
            self.speed = speed;
        }
    }

    fn status(&self) -> FollowerStatus {
        FollowerStatus {
            device: self.device.clone(),
            offset_ms: self.offset.map(|o| o * 1000.0),
            error: self.error.clone(),
        }
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Followers run, and stay in sync, for as long as this value is alive.
pub struct Session {
    followers: Arc<Mutex<Vec<Follower>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    pub fn start(configs: &[FollowerConfig]) -> Result<Self, String> {
        let file = mpv::open_file()?;
        let [pos] = mpv::request(&[get("time-pos")])?.try_into().unwrap();
        let start = pos.as_f64().unwrap_or(0.0);
        let followers = configs
            .iter()
            .enumerate()
            .map(|(i, config)| Follower::spawn(i, config, &file, start))
            .collect::<Result<Vec<_>, _>>()?;

        let followers = Arc::new(Mutex::new(followers));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (followers, stop) = (followers.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let measured_at = Instant::now();
                    let reply = mpv::request(&[
                        get("time-pos"),
                        get("pause"),
                        get("speed"),
                        get("seeking"),
                    ]);
                    let Ok([pos, paused, speed, seeking]) =
                        reply.map(|r| <[Value; 4]>::try_from(r).unwrap())
                    else {
                        // The main mpv is gone, so the followers have nothing to follow.
                        followers.lock().unwrap().clear();
                        return;
                    };
                    if let (Some(pos), false) = (pos.as_f64(), seeking == true) {
                        let (paused, speed) = (paused == true, speed.as_f64().unwrap_or(1.0));
                        for follower in followers.lock().unwrap().iter_mut() {
                            follower.follow(pos, paused, speed, measured_at);
                        }
                    }
                    std::thread::sleep(POLL.saturating_sub(measured_at.elapsed()));
                }
            })
        };
        Ok(Self {
            followers,
            stop,
            thread: Some(thread),
        })
    }

    pub fn set_gain(&self, device: &str, gain_db: Option<f64>) {
        for follower in self.followers.lock().unwrap().iter() {
            if follower.device == device {
                let _ = request_on(&follower.pipe, &[set("af", volume_filter(gain_db))]);
            }
        }
    }

    pub fn status(&self) -> Vec<FollowerStatus> {
        self.followers
            .lock()
            .unwrap()
            .iter()
            .map(Follower::status)
            .collect()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_volume_filters() {
        assert_eq!(volume_filter(Some(-6.0)), "lavfi=[volume=-6dB]");
        assert_eq!(volume_filter(None), "lavfi=[volume=0]");
    }
}
