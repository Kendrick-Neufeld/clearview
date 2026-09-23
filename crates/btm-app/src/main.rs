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

/// One notable moment, with whatever caused it.
#[derive(serde::Serialize)]
struct Spike {
    t: i64,
    /// The machine-wide value at that moment, in the metric's own stored unit.
    value: u32,
    /// The app that accounts for it.
    app: Option<String>,
    app_value: u32,
    /// Seconds the per-app figure is averaged over. A five-second spike matched
    /// against a one-minute average is an attribution, not a measurement, and
    /// the interface says so rather than implying more precision than exists.
    app_window: u32,
}

/// Finds the moments worth pointing at in one metric, and names the likely cause.
///
/// Marking every bump would be noise; a chart littered with labels is one
/// nobody reads. Only clear outliers survive, and only the largest handful.
///
/// Memory is treated differently from the rest. Processor, graphics and network
/// are rates, where a high value *is* the event. Memory is a level: flagging
/// its outliers would just point at "a lot is in use", which the graph already
/// shows. So for memory the events are the largest *increases*, and the culprit
/// is whichever app grew most.
#[tauri::command]
async fn spikes(
    span_secs: i64,
    metric: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Vec<Spike>, ()> {
    let metric = btm_store::Metric::parse(&metric);
    let res = Store::resolution_for(span_secs);
    let series = with_history(&state, |store, now| {
        store.system_series(res, now - span_secs, now)
    });
    if series.len() < 8 {
        return Ok(Vec::new());
    }

    let value_of = |p: &btm_store::SystemPoint| -> u32 {
        match metric {
            btm_store::Metric::Cpu => p.cpu_pm as u32,
            btm_store::Metric::Memory => p.mem_mb,
            btm_store::Metric::Gpu => p.gpu_pm as u32,
            btm_store::Metric::Network => p.net_rx_kbps + p.net_tx_kbps,
            btm_store::Metric::Disk => p.disk_rd_kbps + p.disk_wr_kbps,
        }
    };

    let app_res = res.max(btm_store::RES_MINUTE);
    let bucket = app_res as i64;

    let mut out: Vec<Spike> = if metric == btm_store::Metric::Memory {
        growth_events(&series, value_of)
            .into_iter()
            .map(|(prev_t, p, growth)| {
                let aligned = (p.t / bucket) * bucket;
                let before = (prev_t / bucket) * bucket;
                let found = with_history(&state, |store, _| {
                    store.biggest_grower(app_res, before, aligned).map(|o| o.into_iter().collect())
                });
                match found.into_iter().next() {
                    Some((name, grew)) => Spike {
                        t: p.t,
                        value: growth,
                        app: Some(name),
                        app_value: grew,
                        app_window: app_res,
                    },
                    None => Spike {
                        t: p.t,
                        value: growth,
                        app: None,
                        app_value: 0,
                        app_window: app_res,
                    },
                }
            })
            .collect()
    } else {
        outlier_events(&series, value_of)
            .into_iter()
            .map(|p| {
                let aligned = (p.t / bucket) * bucket;
                let found = with_history(&state, |store, _| {
                    store
                        .top_app_in_bucket(app_res, aligned, metric)
                        .map(|o| o.into_iter().collect())
                });
                let (app, app_value) = match found.into_iter().next() {
                    Some((name, value)) => (Some(name), value),
                    None => (None, 0),
                };
                Spike { t: p.t, value: value_of(&p), app, app_value, app_window: app_res }
            })
            .collect()
    };

    out.sort_unstable_by_key(|s| s.t);
    Ok(out)
}

/// Peaks that stand clearly above the usual level.
fn outlier_events(
    series: &[btm_store::SystemPoint],
    value_of: impl Fn(&btm_store::SystemPoint) -> u32,
) -> Vec<btm_store::SystemPoint> {
    // A median baseline rather than a mean: the spikes themselves would drag a
    // mean upward and hide the smaller ones.
    let mut sorted: Vec<u32> = series.iter().map(&value_of).collect();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    // Distance from the median to the upper quartile stands in for spread, and
    // unlike a standard deviation it is not inflated by the outliers.
    let q3 = sorted[sorted.len() * 3 / 4];
    let spread = q3.saturating_sub(median).max(sorted.last().copied().unwrap_or(0) / 20).max(1);
    let threshold = median + spread * 3;

    // Group runs of consecutive points over the threshold, keeping each run's
    // peak: one burst should produce one mark, not fifteen.
    let mut peaks = Vec::new();
    let mut run: Option<btm_store::SystemPoint> = None;
    for p in series {
        if value_of(p) >= threshold {
            if run.is_none_or(|best| value_of(p) > value_of(&best)) {
                run = Some(*p);
            }
        } else if let Some(best) = run.take() {
            peaks.push(best);
        }
    }
    if let Some(best) = run.take() {
        peaks.push(best);
    }

    peaks.sort_unstable_by_key(|p| std::cmp::Reverse(value_of(p)));
    peaks.truncate(6);
    peaks
}

/// The largest jumps upward, for quantities where the level is uninteresting
/// but the moment it changed is not.
fn growth_events(
    series: &[btm_store::SystemPoint],
    value_of: impl Fn(&btm_store::SystemPoint) -> u32,
) -> Vec<(i64, btm_store::SystemPoint, u32)> {
    let mut jumps: Vec<(i64, btm_store::SystemPoint, u32)> = series
        .windows(2)
        .filter_map(|w| {
            let growth = value_of(&w[1]).saturating_sub(value_of(&w[0]));
            (growth > 0).then_some((w[0].t, w[1], growth))
        })
        .collect();
    if jumps.is_empty() {
        return Vec::new();
    }

    // Only jumps that are large relative to the biggest one seen; on a quiet
    // machine nothing qualifies, which is the correct answer.
    let largest = jumps.iter().map(|(_, _, g)| *g).max().unwrap_or(0);
    let floor = (largest / 3).max(64); // at least 64 MB to be worth a mark
    jumps.retain(|(_, _, g)| *g >= floor);
    jumps.sort_unstable_by_key(|(_, _, g)| std::cmp::Reverse(*g));
    jumps.truncate(5);
    jumps
}

/// Whether any history exists yet/// Whether any history exists yet, so the interface can explain an empty graph
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

/// One process to signal, addressed by identity rather than by number alone.
#[derive(serde::Deserialize)]
struct Target {
    pid: i32,
    start_time: u64,
}

#[derive(serde::Serialize, Default)]
struct TerminationResult {
    /// How many were asked to close.
    requested: usize,
    /// How many actually received the signal.
    signalled: usize,
    /// How many had already exited — not a failure, and not reported as one.
    already_gone: usize,
    /// Human-readable reasons, one per genuine failure.
    refused: Vec<String>,
}

/// Asks processes to close, or forces them to.
///
/// Every target is addressed by pid *and* start time. Pids are reused, and
/// between the interface drawing a row and someone clicking it the process can
/// exit and its number be handed to something else — so this is the difference
/// between closing what was asked for and killing a bystander. The check lives
/// in `btm_probe::control`, immediately before the signal.
#[tauri::command]
async fn terminate(targets: Vec<Target>, force: bool) -> Result<TerminationResult, ()> {
    use btm_probe::control::{Signal, SignalError, send_signal};

    let signal = if force { Signal::Force } else { Signal::Terminate };
    let mut out = TerminationResult { requested: targets.len(), ..Default::default() };

    for t in targets {
        match send_signal(t.pid, t.start_time, signal) {
            Ok(()) => out.signalled += 1,
            Err(SignalError::Vanished) => out.already_gone += 1,
            Err(e) => out.refused.push(format!("pid {}: {e}", t.pid)),
        }
    }
    Ok(out)
}

/// Which of these are still running, so the interface can tell whether asking
/// politely worked before offering to force the issue.
#[tauri::command]
async fn still_running(targets: Vec<Target>) -> Result<Vec<i32>, ()> {
    Ok(targets
        .into_iter()
        .filter(|t| btm_probe::control::still_running(t.pid, t.start_time))
        .map(|t| t.pid)
        .collect())
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
            history_status,
            spikes,
            terminate,
            still_running
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
