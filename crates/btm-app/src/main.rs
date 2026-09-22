//! The desktop application.
//!
//! The backend does one thing on a loop: sample, build the model, and push it to
//! the interface. All the interpretation happened in `btm-model`; nothing here
//! decides what anything means.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use btm_model::SystemModel;
use btm_probe::desktop::DesktopDb;
use btm_probe::{Sampler, conf, sampler::SamplerConfig, wm};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Emitter;

/// How often the foreground view refreshes. Fast enough to feel live, slow enough
/// that the monitor is not itself a notable process.
const REFRESH: Duration = Duration::from_millis(1000);

/// The application database changes rarely; rescanning it every second would cost
/// far more than it is worth.
const DESKTOP_RESCAN: Duration = Duration::from_secs(120);

struct AppState {
    /// The most recent model, so a window that opens mid-stream has something to
    /// draw immediately instead of a blank screen.
    latest: Mutex<Option<SystemModel>>,
}

/// The current model, for a view that has just loaded.
#[tauri::command]
fn latest_snapshot(state: tauri::State<'_, Arc<AppState>>) -> Option<SystemModel> {
    state.latest.lock().ok()?.clone()
}

/// The full command line for one process, fetched on demand.
///
/// Command lines are long and mostly noise, and sending every one of them sixty
/// times a minute would dwarf the rest of the payload — so the stream carries the
/// interpreted role instead, and the raw truth is a click away.
#[tauri::command]
fn process_cmdline(pid: i32) -> Vec<String> {
    btm_probe::process::read_cmdline(pid)
}

/// Static facts the interface needs to state its units honestly.
#[tauri::command]
fn machine_info() -> serde_json::Value {
    serde_json::json!({
        "cpuCount": conf::cpu_count(),
        "clockTicks": conf::clock_ticks(),
    })
}

fn main() {
    let state = Arc::new(AppState { latest: Mutex::new(None) });

    tauri::Builder::default()
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![latest_snapshot, process_cmdline, machine_info])
        .setup(move |app| {
            let handle = app.handle().clone();
            let state = state.clone();
            std::thread::spawn(move || sample_loop(handle, state));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to start");
}

fn sample_loop(handle: tauri::AppHandle, state: Arc<AppState>) {
    let mut sampler = Sampler::new(SamplerConfig::default());
    let mut db = DesktopDb::load();
    let mut db_loaded = Instant::now();

    // The first sample only establishes a baseline — no rates exist yet.
    let _ = sampler.sample();

    loop {
        std::thread::sleep(REFRESH);

        if db_loaded.elapsed() > DESKTOP_RESCAN {
            db = DesktopDb::load();
            db_loaded = Instant::now();
        }

        let Ok(sample) = sampler.sample() else { continue };
        let model = SystemModel::build(&sample, &wm::windows(), &db);

        if let Ok(mut slot) = state.latest.lock() {
            *slot = Some(model.clone());
        }
        // A failed emit means the window has gone; the loop keeps the model fresh
        // for whenever one opens again.
        let _ = handle.emit("snapshot", &model);
    }
}
