//! Talks to a running mpv through its JSON IPC pipe.
//!
//! The mpv the user plays with only opens the pipe when started with
//! `input-ipc-server` set to [`MAIN_PIPE`].

use serde::Serialize;
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

pub const MAIN_PIPE: &str = "mpv-audio-splitter";

#[derive(Serialize, Clone, PartialEq, Debug)]
pub struct Track {
    pub id: i64,
    pub label: String,
}

#[derive(Serialize)]
pub struct Status {
    pub title: String,
    pub path: String,
    pub tracks: Vec<Track>,
    /// The audio track mpv is playing, if any.
    pub selected: Option<i64>,
}

/// Sends commands to the mpv listening on `pipe` and returns their results in
/// order. Fails if no such mpv is running, or if it rejects a command.
pub fn request_on(pipe: &str, commands: &[Value]) -> Result<Vec<Value>, String> {
    let pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!(r"\\.\pipe\{pipe}"))
        .map_err(|e| format!("mpv isn't reachable: {e}"))?;
    let mut message = String::new();
    for (i, command) in commands.iter().enumerate() {
        message += &json!({ "command": command, "request_id": i + 1 }).to_string();
        message.push('\n');
    }
    (&pipe)
        .write_all(message.as_bytes())
        .map_err(|e| format!("Could not send to mpv: {e}"))?;

    let mut results = vec![Value::Null; commands.len()];
    let mut pending = commands.len();
    // Events are interleaved with replies; replies carry the request id.
    for line in BufReader::new(&pipe).lines() {
        let line = line.map_err(|e| format!("Could not read from mpv: {e}"))?;
        let Ok(reply) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = reply["request_id"]
            .as_u64()
            .filter(|id| (1..=commands.len() as u64).contains(id))
        else {
            continue;
        };
        let index = id as usize - 1;
        // A property that has no value right now (such as time-pos while loading) is not a failure.
        if reply["error"] != "success" && reply["error"] != "property unavailable" {
            return Err(format!(
                "mpv rejected {}: {}",
                commands[index], reply["error"]
            ));
        }
        results[index] = reply["data"].clone();
        pending -= 1;
        if pending == 0 {
            return Ok(results);
        }
    }
    Err("mpv closed the connection".into())
}

pub fn request(commands: &[Value]) -> Result<Vec<Value>, String> {
    request_on(MAIN_PIPE, commands)
}

pub fn get(property: &str) -> Value {
    json!(["get_property", property])
}

pub fn set(property: &str, value: impl Into<Value>) -> Value {
    json!(["set_property", property, value.into()])
}

fn track_label(track: &Value) -> String {
    let mut parts = vec![track["id"].to_string()];
    if let Some(lang) = track["lang"].as_str() {
        parts.push(lang.to_string());
    }
    if let Some(title) = track["title"].as_str() {
        parts.push(title.to_string());
    }
    let channels = match track["demux-channel-count"].as_u64() {
        Some(1) => Some("mono".to_string()),
        Some(2) => Some("stereo".to_string()),
        Some(6) => Some("5.1".to_string()),
        Some(8) => Some("7.1".to_string()),
        Some(n) => Some(format!("{n} ch")),
        None => None,
    };
    parts.extend(channels);
    parts.join(" · ")
}

/// What mpv is playing, or `None` when no mpv with the pipe is running.
pub fn status() -> Option<Status> {
    let [title, path, tracks] = request(&[get("media-title"), get("path"), get("track-list")])
        .ok()?
        .try_into()
        .ok()?;
    let audio: Vec<&Value> = tracks
        .as_array()?
        .iter()
        .filter(|t| t["type"] == "audio")
        .collect();
    Some(Status {
        title: title.as_str().unwrap_or_default().to_string(),
        path: path.as_str().unwrap_or_default().to_string(),
        selected: audio
            .iter()
            .find(|t| t["selected"] == true)
            .and_then(|t| t["id"].as_i64()),
        tracks: audio
            .iter()
            .filter_map(|t| {
                Some(Track {
                    id: t["id"].as_i64()?,
                    label: track_label(t),
                })
            })
            .collect(),
    })
}

/// The file mpv is playing, made absolute so another mpv can open it.
pub fn open_file() -> Result<String, String> {
    let [path, dir] = request(&[get("path"), get("working-directory")])?
        .try_into()
        .unwrap();
    let path = path.as_str().ok_or("mpv isn't playing a file")?;
    Ok(resolve(path, dir.as_str().unwrap_or_default()))
}

fn resolve(path: &str, working_dir: &str) -> String {
    if path.contains("://") || Path::new(path).is_absolute() {
        path.to_string()
    } else {
        Path::new(working_dir)
            .join(path)
            .to_string_lossy()
            .into_owned()
    }
}

/// mpv's name for the audio device Windows calls `description`.
pub fn device_name(description: &str) -> Result<String, String> {
    let [devices] = request(&[get("audio-device-list")])?.try_into().unwrap();
    devices
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["description"] == description)
        .and_then(|d| d["name"].as_str().map(str::to_string))
        .ok_or_else(|| format!("mpv can't find the audio device {description}"))
}

/// Switches mpv's audio track.
pub fn select_track(id: i64) -> Result<(), String> {
    request(&[set("aid", id)]).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_tracks_with_what_mpv_knows() {
        let track = json!({ "id": 2, "lang": "rus", "title": "Dub", "demux-channel-count": 6 });
        assert_eq!(track_label(&track), "2 · rus · Dub · 5.1");
        assert_eq!(track_label(&json!({ "id": 1 })), "1");
    }

    #[test]
    fn makes_relative_paths_absolute() {
        assert_eq!(resolve("movie.mkv", r"C:\films"), r"C:\films\movie.mkv");
        assert_eq!(resolve(r"D:\a.mkv", r"C:\films"), r"D:\a.mkv");
        assert_eq!(resolve("https://x/y.mkv", r"C:\films"), "https://x/y.mkv");
    }
}
