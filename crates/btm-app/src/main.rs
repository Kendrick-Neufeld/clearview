//! The desktop application.
//!
//! The backend does one thing on a loop: sample, build the model, and push it to
//! the interface. All the interpretation happened in `btm-model`; nothing here
//! decides what anything means.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use btm_model::SystemModel;
use btm_probe::desktop::DesktopDb;
use btm_probe::{Sampler, conf, sampler::SamplerConfig, wm};
use btm_store::{AppSeriesRow, Store, SystemPoint};
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
    /// Read-only handle on the collector's history. `None` when the collector
    /// has never run, which is a normal state and not an error — the live view
    /// works without it.
    history: Mutex<Option<Store>>,
}

/// The current model, for a view that has just loaded.
#[tauri::command]
async fn latest_snapshot(state: tauri::State<'_, Arc<AppState>>) -> Result<Option<SystemModel>, ()> {
    Ok(state.latest.lock().ok().and_then(|g| g.clone()))
}

/// The full command line for one process, fetched on demand.
///
/// Command lines are long and mostly noise, and sending every one of them sixty
/// times a minute would dwarf the rest of the payload — so the stream carries the
/// interpreted role instead, and the raw truth is a click away.
#[tauri::command]
async fn process_cmdline(pid: i32) -> Vec<String> {
    btm_probe::process::read_cmdline(pid)
}

/// Machine history over the last `span_secs`, at whichever resolution suits
/// that span.
#[tauri::command]
async fn system_history(
    span_secs: i64,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Vec<SystemPoint>, ()> {
    Ok(with_history(&state, |store, now| {
        store.system_series(Store::resolution_for(span_secs), now - span_secs, now)
    }))
}

/// One app's history over the same span.
#[tauri::command]
async fn app_history(
    key: String,
    span_secs: i64,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Vec<AppSeriesRow>, ()> {
    Ok(with_history(&state, |store, now| {
        // Per-app rows are never written at the finest resolution, so a short
        // span still has to read the minute series.
        let res = Store::resolution_for(span_secs).max(btm_store::RES_MINUTE);
        store.app_series(&key, res, now - span_secs, now)
    }))
}

/// Whether any history exists yet, so the interface can explain an empty graph
/// instead of just showing one.
#[tauri::command]
async fn history_status(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<serde_json::Value, ()> {
    // Everything that touches the store goes through `with_history`, which takes
    // the lock itself. An earlier version checked the handle here *and* called
    // `with_history`, which took the same non-reentrant mutex twice and
    // deadlocked the command — and because it was a synchronous command it did
    // so on the main thread, freezing the window outright.
    let points = with_history(&state, |store, now| {
        store.system_series(btm_store::RES_FINE, now - 3600, now)
    });
    Ok(serde_json::json!({
        "available": btm_store::default_path().exists(),
        "recentPoints": points.len(),
        "oldest": points.first().map(|p| p.t),
    }))
}

/// Runs a history query, re-opening the database if the collector has appeared
/// since the last attempt. Any failure reads as "no history", because a missing
/// graph is a far better outcome than a broken window.
///
/// This function takes the `history` lock. Nothing that calls it may already
/// hold it.
fn with_history<T>(
    state: &Arc<AppState>,
    query: impl FnOnce(&Store, i64) -> btm_store::Result<Vec<T>>,
) -> Vec<T> {
    let Ok(mut guard) = state.history.lock() else { return Vec::new() };
    if guard.is_none() {
        *guard = Store::open_read_only(&btm_store::default_path()).ok();
    }
    let Some(store) = guard.as_ref() else { return Vec::new() };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    query(store, now).unwrap_or_default()
}

/// Static facts the interface needs to state its units honestly.
#[tauri::command]
async fn machine_info() -> serde_json::Value {
    serde_json::json!({
        "cpuCount": conf::cpu_count(),
        "clockTicks": conf::clock_ticks(),
    })
}

fn main() {
    let state = Arc::new(AppState {
        latest: Mutex::new(None),
        // Opened lazily: the collector may be installed after the window is.
        history: Mutex::new(Store::open_read_only(&btm_store::default_path()).ok()),
    });

    tauri::Builder::default()
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            latest_snapshot,
            process_cmdline,
            machine_info,
            system_history,
            app_history,
            history_status
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let state = state.clone();
            std::thread::spawn(move || sample_loop(handle, state));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to start");
}

/// Drops command lines before the model crosses the IPC boundary.
///
/// They are roughly a third of the payload, sixty times a minute, and the
/// interface never reads them from the stream — it asks for the single one it
/// is about to display. The roles they were parsed into have already been
/// computed here.
fn strip_cmdlines(model: &mut SystemModel) {
    fn walk(nodes: &mut [btm_model::ProcNode]) {
        for n in nodes {
            n.cmdline = Vec::new();
            walk(&mut n.children);
        }
    }
    for app in &mut model.apps {
        walk(&mut app.roots);
    }
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
        let mut model = SystemModel::build(&sample, &wm::windows(), &db);
        strip_cmdlines(&mut model);

        if let Ok(mut slot) = state.latest.lock() {
            *slot = Some(model.clone());
        }
            // A failed emit means the window has gone; the loop keeps the model
        // fresh for whenever one opens again.
        if let Err(e) = handle.emit("snapshot", &model) {
            eprintln!("clearview: could not push a snapshot: {e}");
        }
    }
}
