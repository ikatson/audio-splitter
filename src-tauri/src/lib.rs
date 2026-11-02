pub mod engine;
pub mod resampler;
pub mod spectrum;

use engine::{DeviceList, Engine, OutputConfig, Status};
use std::sync::Mutex;

#[derive(Default)]
struct AppState(Mutex<Option<Engine>>);

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

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![list_devices, start, stop, status, set_gain, spectrum])
        .run(tauri::generate_context!())
        .expect("error while running Audio Splitter");
}
