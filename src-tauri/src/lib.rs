pub mod engine;
pub mod follower;
pub mod mpv;
pub mod resampler;
pub mod spectrum;

use engine::{DeviceList, Engine, OutputConfig, Status};
use std::sync::Mutex;
use tauri::Manager;

#[derive(Default)]
struct AppState(Mutex<Option<Engine>>);

/// Hidden mpvs playing other tracks of the main mpv's video.
#[derive(Default)]
struct FollowState(Mutex<Option<follower::Session>>);

#[tauri::command(async)]
fn list_devices() -> Result<DeviceList, String> {
    engine::list_devices()
}

#[tauri::command(async)]
fn start(
    state: tauri::State<'_, AppState>,
    source: String,
    outputs: Vec<OutputConfig>,
) -> Result<(), String> {
    let mut current = state.0.lock().unwrap();
    // Release the devices before opening them again.
    *current = None;
    *current = Some(Engine::start(&source, &outputs)?);
    Ok(())
}

#[tauri::command(async)]
fn stop(state: tauri::State<'_, AppState>) {
    *state.0.lock().unwrap() = None;
}

#[tauri::command(async)]
fn status(state: tauri::State<'_, AppState>) -> Option<Status> {
    state.0.lock().unwrap().as_ref().map(Engine::status)
}

#[tauri::command(async)]
fn set_gain(state: tauri::State<'_, AppState>, id: String, gain: f32) {
    if let Some(engine) = state.0.lock().unwrap().as_ref() {
        engine.set_gain(&id, gain);
    }
}

#[tauri::command(async)]
fn spectrum(state: tauri::State<'_, AppState>) -> Option<Vec<f32>> {
    state.0.lock().unwrap().as_mut().map(Engine::spectrum)
}

#[tauri::command(async)]
fn mpv_status() -> Option<mpv::Status> {
    mpv::status()
}

#[tauri::command(async)]
fn mpv_select_track(id: i64) -> Result<(), String> {
    mpv::select_track(id)
}

#[tauri::command(async)]
fn follow_start(
    state: tauri::State<'_, FollowState>,
    followers: Vec<follower::FollowerConfig>,
) -> Result<(), String> {
    let mut session = state.0.lock().unwrap();
    // Close the previous followers before starting new ones on the same devices.
    *session = None;
    *session = Some(follower::Session::start(&followers)?);
    Ok(())
}

#[tauri::command(async)]
fn follow_stop(state: tauri::State<'_, FollowState>) {
    *state.0.lock().unwrap() = None;
}

#[tauri::command(async)]
fn follow_status(state: tauri::State<'_, FollowState>) -> Option<Vec<follower::FollowerStatus>> {
    state
        .0
        .lock()
        .unwrap()
        .as_ref()
        .map(follower::Session::status)
}

#[tauri::command(async)]
fn follow_set_gain(state: tauri::State<'_, FollowState>, device: String, gain_db: Option<f64>) {
    if let Some(session) = state.0.lock().unwrap().as_ref() {
        session.set_gain(&device, gain_db);
    }
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .manage(FollowState::default())
        .invoke_handler(tauri::generate_handler![
            list_devices,
            start,
            stop,
            status,
            set_gain,
            spectrum,
            mpv_status,
            mpv_select_track,
            follow_start,
            follow_stop,
            follow_status,
            follow_set_gain
        ])
        .build(tauri::generate_context!())
        .expect("error while starting Audio Splitter")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // Closes the hidden mpvs.
                app.state::<FollowState>().0.lock().unwrap().take();
            }
        });
}
