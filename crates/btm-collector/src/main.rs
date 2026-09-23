//! The background sampler.
//!
//! Runs as a `systemd --user` service so the history covers the whole day rather
//! than only the minutes the window happened to be open. It is built to be
//! unnoticeable: a five-second tick, per-app figures averaged in memory for a
//! whole minute before a single row is written, and a database measured in
//! single-digit megabytes.

use btm_model::SystemModel;
use btm_probe::desktop::DesktopDb;
use btm_probe::{Sampler, conf, sampler::SamplerConfig, wm};
use btm_store::{AppPoint, RES_COARSE, RES_FINE, RES_MINUTE, Store, SystemPoint};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TICK: Duration = Duration::from_secs(5);
/// Rolling up and pruning every minute keeps each pass tiny, rather than doing
/// one large and noticeable pass occasionally.
const MAINTENANCE: Duration = Duration::from_secs(60);

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Per-app sums for the minute currently being accumulated.
///
/// This is the main reason the database stays small: five-second per-app rows
/// would be twelve times the volume and tell a person nothing extra.
#[derive(Default)]
struct Bucket {
    minute: i64,
    ticks: u32,
    apps: HashMap<String, Accum>,
}

struct Accum {
    name: String,
    cpu_pm_sum: u64,
    mem_mb_sum: u64,
    gpu_pm_sum: u64,
    net_kbps_sum: u64,
    disk_kbps_sum: u64,
    ticks: u32,
}

impl Bucket {
    fn add(&mut self, key: &str, name: &str, point: AppPoint) {
        let e = self.apps.entry(key.to_string()).or_insert_with(|| Accum {
            name: name.to_string(),
            cpu_pm_sum: 0,
            mem_mb_sum: 0,
            gpu_pm_sum: 0,
            net_kbps_sum: 0,
            disk_kbps_sum: 0,
            ticks: 0,
        });
        e.cpu_pm_sum += point.cpu_pm as u64;
        e.mem_mb_sum += point.mem_mb as u64;
        e.gpu_pm_sum += point.gpu_pm as u64;
        e.net_kbps_sum += point.net_kbps as u64;
        e.disk_kbps_sum += point.disk_kbps as u64;
        e.ticks += 1;
        if e.name != name {
            e.name = name.to_string();
        }
    }

    /// Averages over the ticks an app was actually present for, not over the
    /// whole minute — otherwise an app that launched halfway through looks half
    /// as busy as it was.
    fn drain(&mut self) -> Vec<AppPoint> {
        self.ticks = 0;
        self.apps
            .drain()
            .map(|(key, a)| {
                let n = a.ticks.max(1) as u64;
                AppPoint {
                    key,
                    name: a.name,
                    cpu_pm: (a.cpu_pm_sum / n) as u16,
                    mem_mb: (a.mem_mb_sum / n) as u32,
                    gpu_pm: (a.gpu_pm_sum / n) as u16,
                    net_kbps: (a.net_kbps_sum / n) as u32,
                    disk_kbps: (a.disk_kbps_sum / n) as u32,
                }
            })
            .collect()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = btm_store::default_path();

    if args.iter().any(|a| a == "--report") {
        return report(&path);
    }

    let mut store = match Store::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("clearview-collector: cannot open {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    eprintln!("clearview-collector: writing {}", path.display());

    // Cheap by construction: no command lines are needed for grouping totals,
    // no per-process I/O is stored, and PSS — the expensive read — is taken at a
    // third of the tick rate, which is still far finer than the megabyte
    // resolution anything is stored at.
    let mut sampler = Sampler::new(SamplerConfig {
        collect_pss: true,
        pss_interval: Duration::from_secs(30),
        pss_top_n: 48,
        collect_io: true,
        collect_cmdline: true,
        // The collector records graphics, sensors and network too, but on
        // slower clocks than the foreground view — it is storing minute
        // averages, so finer sampling would buy nothing and cost power.
        collect_gpu: true,
        gpu_rediscover_every: 12,
        collect_sensors: true,
        collect_network: true,
        network_interval: Duration::from_secs(10),
        nvidia_interval: Duration::from_secs(15),
    });
    let mut db = DesktopDb::load();
    let mut db_age = 0u32;

    let _ = sampler.sample(); // baseline; no rates exist yet
    let mut bucket = Bucket { minute: now_secs() / 60, ..Default::default() };
    let mut last_maintenance = SystemTime::now();

    loop {
        std::thread::sleep(TICK);
        let t = now_secs();

        let Ok(sample) = sampler.sample() else { continue };
        let model = SystemModel::build(&sample, &wm::windows(), &db);

        // The integrated GPU comes from summed client engine time; the
        // discrete one, when present, reports its own counter.
        let integrated = sample.gpus.iter().find(|g| g.busy_from_clients);
        let discrete = sample.gpus.iter().find(|g| !g.busy_from_clients);
        let net_rx: f64 = sample.interfaces.iter().map(|i| i.rx_bps).sum();
        let net_tx: f64 = sample.interfaces.iter().map(|i| i.tx_bps).sum();
        let disk_rd: f64 = sample.disks.iter().map(|d| d.read_bps).sum();
        let disk_wr: f64 = sample.disks.iter().map(|d| d.write_bps).sum();
        // The busiest single device, not a sum: two drives each half busy is
        // not one drive fully busy.
        let disk_busy = sample.disks.iter().map(|d| d.busy).fold(0.0f64, f64::max);

        let point = SystemPoint {
            // Align to the tick so re-runs overwrite rather than interleave.
            t: (t / RES_FINE as i64) * RES_FINE as i64,
            cpu_pm: per_mille(sample.cpu_busy.unwrap_or(0.0)),
            mem_mb: mb(sample.mem.total.saturating_sub(sample.mem.available)),
            swap_mb: mb(sample.mem.swap_total.saturating_sub(sample.mem.swap_free)),
            psi_cpu_pm: psi(sample.pressure.cpu),
            psi_mem_pm: psi(sample.pressure.memory),
            psi_io_pm: psi(sample.pressure.io),
            gpu_pm: integrated.and_then(|g| g.busy).map(|b| per_mille(b as f64)).unwrap_or(0),
            gpu2_pm: discrete.and_then(|g| g.busy).map(|b| per_mille(b as f64)).unwrap_or(0),
            cpu_temp_c: celsius(sample.thermals.cpu_package_c),
            gpu_temp_c: celsius(discrete.and_then(|g| g.temp_c)),
            power_w: sample.power_w.map(|w| w.round().max(0.0) as u16).unwrap_or(0),
            net_rx_kbps: (net_rx / 1024.0) as u32,
            net_tx_kbps: (net_tx / 1024.0) as u32,
            disk_rd_kbps: (disk_rd / 1024.0) as u32,
            disk_wr_kbps: (disk_wr / 1024.0) as u32,
            disk_busy_pm: per_mille(disk_busy),
        };
        if let Err(e) = store.record_system(RES_FINE, &point) {
            eprintln!("clearview-collector: write failed: {e}");
        }

        let cpus = conf::cpu_count() as f64;
        for app in &model.apps {
            bucket.add(
                &app.id,
                &app.name,
                AppPoint {
                    key: String::new(),
                    name: String::new(),
                    cpu_pm: per_mille(app.totals.cpu_cores / cpus),
                    mem_mb: mb(app.totals.mem_pss),
                    gpu_pm: per_mille(app.totals.gpu_busy),
                    net_kbps: (app.totals.net_bps() / 1024.0) as u32,
                    disk_kbps: ((app.totals.disk_read_bps + app.totals.disk_write_bps) / 1024.0)
                        as u32,
                },
            );
        }
        bucket.ticks += 1;

        // A closed minute is written once, then forgotten.
        let minute = t / 60;
        if minute != bucket.minute {
            let closed = bucket.minute * 60;
            let points = bucket.drain();
            if let Err(e) = store.record_apps(RES_MINUTE, closed, &points) {
                eprintln!("clearview-collector: app write failed: {e}");
            }
            bucket.minute = minute;
        }

        if last_maintenance.elapsed().unwrap_or_default() >= MAINTENANCE {
            last_maintenance = SystemTime::now();
            maintain(&store, t);

            // The application database changes rarely; every ten minutes is
            // generous.
            db_age += 1;
            if db_age >= 10 {
                db = DesktopDb::load();
                db_age = 0;
            }
        }
    }
}

fn maintain(store: &Store, now: i64) {
    let steps: [(&str, btm_store::Result<usize>); 4] = [
        ("system fine→minute", store.rollup_system(RES_FINE, RES_MINUTE, now)),
        ("system minute→coarse", store.rollup_system(RES_MINUTE, RES_COARSE, now)),
        ("apps minute→coarse", store.rollup_apps(RES_MINUTE, RES_COARSE, now)),
        ("prune", store.prune(now)),
    ];
    for (what, result) in steps {
        if let Err(e) = result {
            eprintln!("clearview-collector: {what} failed: {e}");
        }
    }
}

fn report(path: &std::path::Path) {
    match Store::open(path) {
        Ok(store) => {
            let (sys, apps, names) = store.stats().unwrap_or((0, 0, 0));
            let bytes = Store::size_bytes(path);
            println!("history   {}", path.display());
            println!("rows      {sys} machine, {apps} per-app, {names} app names");
            println!("on disk   {:.2} MB", bytes as f64 / 1024.0 / 1024.0);
            let day = now_secs() - 86_400;
            let recent = store.system_series(RES_MINUTE, day, now_secs()).unwrap_or_default();
            println!("coverage  {} minutes recorded in the last day", recent.len());
        }
        Err(e) => eprintln!("cannot read {}: {e}", path.display()),
    }
}

/// 0.0–1.0 to 0–1000, which is finer than any graph can show and costs one or
/// two bytes per value instead of eight.
fn per_mille(fraction: f64) -> u16 {
    (fraction.clamp(0.0, 1.0) * 1000.0).round() as u16
}

/// Whole degrees, clamped into a byte. Nothing sensible reads above 255°C.
fn celsius(v: Option<f32>) -> u8 {
    v.map(|c| c.round().clamp(0.0, 255.0) as u8).unwrap_or(0)
}

fn mb(bytes: u64) -> u32 {
    (bytes / (1024 * 1024)) as u32
}

fn psi(p: Option<btm_probe::system::Pressure>) -> u16 {
    p.map(|p| (p.some.avg10.clamp(0.0, 100.0) * 10.0).round() as u16).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    /// Two unit files exist because a from-source install puts the collector in
    /// `~/.local/bin` and a package puts it in `/usr/bin`, and systemd will not
    /// resolve one path from the other. Two files that differ by one line are
    /// exactly the kind of pair that drifts silently, so this pins them
    /// together: change anything but `ExecStart=` in one and this fails.
    #[test]
    fn the_two_service_units_differ_only_in_their_exec_path() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        let read = |name: &str| {
            std::fs::read_to_string(format!("{root}/packaging/{name}"))
                .unwrap_or_else(|e| panic!("{name}: {e}"))
        };
        let strip_exec = |text: String| {
            text.lines()
                .filter(|l| !l.starts_with("ExecStart="))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };

        let user = read("clearview-collector.service");
        let system = read("clearview-collector.system.service");

        assert!(user.contains("ExecStart=%h/.local/bin/clearview-collector"));
        assert!(system.contains("ExecStart=/usr/bin/clearview-collector"));
        assert_eq!(
            strip_exec(user),
            strip_exec(system),
            "the two units have drifted apart outside ExecStart"
        );
    }
}
