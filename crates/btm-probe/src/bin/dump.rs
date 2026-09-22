//! A plain-text dump of what the probe layer sees.
//!
//! This exists to check the arithmetic against `top`, `btop` and Mission Center
//! before any of it is drawn. Getting the numbers right comes first.

use btm_probe::conf;
use btm_probe::process;
use btm_probe::{Sample, Sampler, sampler::SamplerConfig};
use std::thread;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let opt = |name: &str, default: f64| -> f64 {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let interval = opt("--interval", 1.0);
    let top = opt("--top", 15.0) as usize;
    let filter = args
        .iter()
        .position(|a| a == "--match")
        .and_then(|i| args.get(i + 1))
        .cloned();

    if args.iter().any(|a| a == "--devices") {
        return devices(interval);
    }

    let mut sampler = Sampler::new(SamplerConfig {
        // The dump is a one-shot, so take the full PSS cost immediately rather
        // than waiting for the periodic refresh.
        pss_interval: Duration::from_millis(0),
        pss_top_n: 256,
        ..Default::default()
    });

    // First sample is the baseline; rates only exist after the second.
    sampler.sample()?;
    thread::sleep(Duration::from_secs_f64(interval));
    let s = sampler.sample()?;

    print_system(&s);
    match filter {
        Some(pat) => print_group(&s, &pat),
        None => print_top(&s, top),
    }
    Ok(())
}

/// Reads the hardware sensors, GPUs and network interfaces once, so the values
/// can be checked against whatever else the machine reports.
fn devices(interval: f64) -> std::io::Result<()> {
    use btm_probe::{gpu, net, sensors};

    let mut power = sensors::PowerMeter::new();
    let _ = power.read_watts();
    let before_drm = gpu::read_drm_clients();
    let before_net = net::read_interfaces();
    let t0 = std::time::Instant::now();
    thread::sleep(Duration::from_secs_f64(interval));
    let dt = t0.elapsed().as_secs_f64();

    let t = sensors::read_thermals();
    println!("── thermals ──────────────────────────────");
    println!("  cpu package  {}", opt_c(t.cpu_package_c));
    for (i, c) in t.cores_c.iter().enumerate() {
        println!("  core {i:<7}  {}", opt_c(*c));
    }
    println!("  nvme         {}", opt_c(t.nvme_c));
    println!("  wifi         {}", opt_c(t.wifi_c));
    println!("  ambient      {}", opt_c(t.ambient_c));

    println!();
    println!("── cpu ───────────────────────────────────");
    let freqs = sensors::core_frequencies_mhz();
    let map = sensors::logical_to_physical_core();
    println!("  {} logical cpus on {} physical cores",
             freqs.len(), map.iter().collect::<std::collections::BTreeSet<_>>().len());
    println!("  frequency    {} MHz avg, {} MHz peak",
             freqs.iter().sum::<u32>() / freqs.len().max(1) as u32,
             freqs.iter().max().copied().unwrap_or(0));
    match power.read_watts() {
        Some(w) => println!("  package draw {w:.1} W"),
        None => println!("  package draw unavailable"),
    }

    println!();
    println!("── gpus ──────────────────────────────────");
    for (card, driver) in gpu::list_devices() {
        println!("  {card} ({driver})");
    }
    let after_drm = gpu::read_drm_clients();
    let mut busy_ns = 0u64;
    let mut per_pid: std::collections::HashMap<i32, u64> = std::collections::HashMap::new();
    for (id, now) in &after_drm {
        let was = before_drm.get(id).map(|c| c.total_ns).unwrap_or(0);
        let delta = now.total_ns.saturating_sub(was);
        busy_ns += delta;
        *per_pid.entry(now.pid).or_default() += delta;
    }
    println!("  integrated   {:.1}% busy (summed from {} drm clients)",
             busy_ns as f64 / 1e9 / dt * 100.0, after_drm.len());
    let mut top: Vec<_> = per_pid.into_iter().filter(|(_, ns)| *ns > 0).collect();
    top.sort_unstable_by_key(|(_, ns)| std::cmp::Reverse(*ns));
    for (pid, ns) in top.iter().take(5) {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        println!("    pid {pid:<7} {:.1}%  {}", *ns as f64 / 1e9 / dt * 100.0, comm.trim());
    }
    for g in gpu::read_nvidia() {
        println!("  {}", g.name);
        println!("    {:.0}% busy · {} · {} MiB used · {:.0} W · {} MHz",
                 g.busy.unwrap_or(0.0) * 100.0,
                 opt_c(g.temp_c),
                 g.mem_used_bytes.unwrap_or(0) / 1024 / 1024,
                 g.power_w.unwrap_or(0.0),
                 g.clock_mhz.unwrap_or(0));
    }

    println!();
    println!("── network ───────────────────────────────");
    let after_net = net::read_interfaces();
    for now in after_net.iter().filter(|i| i.is_real()) {
        let was = before_net.iter().find(|i| i.name == now.name);
        let (rx, tx) = match was {
            Some(w) => (
                (now.rx_bytes.saturating_sub(w.rx_bytes)) as f64 / dt,
                (now.tx_bytes.saturating_sub(w.tx_bytes)) as f64 / dt,
            ),
            None => (0.0, 0.0),
        };
        println!("  {:<12} down {:>8}/s   up {:>8}/s", now.name, rate(rx), rate(tx));
    }
    let traffic = net::read_process_traffic();
    let mut by_bytes: Vec<_> = traffic.into_iter().collect();
    by_bytes.sort_unstable_by_key(|(_, t)| std::cmp::Reverse(t.rx_bytes + t.tx_bytes));
    println!("  busiest processes by total tcp bytes this connection lifetime:");
    for (pid, t) in by_bytes.iter().take(5) {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        println!("    pid {pid:<7} {:>9} down {:>9} up  {} sockets  {}",
                 bytes(t.rx_bytes), bytes(t.tx_bytes), t.sockets, comm.trim());
    }
    Ok(())
}

fn opt_c(v: Option<f32>) -> String {
    v.map(|v| format!("{v:.0}°C")).unwrap_or_else(|| "—".into())
}

fn rate(bps: f64) -> String {
    if bps >= 1024.0 * 1024.0 { format!("{:.1} MB", bps / 1024.0 / 1024.0) }
    else if bps >= 1024.0 { format!("{:.0} KB", bps / 1024.0) }
    else { format!("{bps:.0} B") }
}

fn bytes(b: u64) -> String {
    if b >= 1024 * 1024 * 1024 { format!("{:.1} GB", b as f64 / 1024.0f64.powi(3)) }
    else if b >= 1024 * 1024 { format!("{:.0} MB", b as f64 / 1024.0 / 1024.0) }
    else { format!("{:.0} KB", b as f64 / 1024.0) }
}

fn print_system(s: &Sample) {
    let cpus = conf::cpu_count();
    println!("── system ──────────────────────────────────────────────");
    println!(
        "cpu        {:>6.1}% of machine ({} threads){}",
        s.cpu_busy.unwrap_or(0.0) * 100.0,
        cpus,
        s.interval_secs.map(|d| format!("   over {d:.2}s")).unwrap_or_default()
    );
    if !s.per_cpu_busy.is_empty() {
        let bars: String = s.per_cpu_busy.iter().map(|b| block(*b)).collect();
        println!("per-core   {bars}");
    }
    println!(
        "memory     {} used of {}   ({:.1}%)   swap {} of {}",
        gib(s.mem.used()),
        gib(s.mem.total),
        s.mem.used() as f64 / s.mem.total.max(1) as f64 * 100.0,
        gib(s.mem.swap_used()),
        gib(s.mem.swap_total),
    );
    println!("cache      {} reclaimable", gib(s.mem.cached + s.mem.sreclaimable));

    // Utilisation says how hard the machine is working; pressure says whether that
    // work is costing anything. Both are needed to tell "busy" from "struggling".
    let psi = |name: &str, p: Option<btm_probe::system::Pressure>| {
        if let Some(p) = p {
            println!(
                "psi {name:<6} some {:>5.1}% / full {:>5.1}%   (10s avg){}",
                p.some.avg10,
                p.full.avg10,
                if p.some.avg10 > 10.0 { "   << contended" } else { "" }
            );
        }
    };
    psi("cpu", s.pressure.cpu);
    psi("memory", s.pressure.memory);
    psi("io", s.pressure.io);
    println!(
        "runnable   {} running, {} blocked on io",
        s.cpu.procs_running, s.cpu.procs_blocked
    );
    println!();
}

fn print_top(s: &Sample, n: usize) {
    let mut procs: Vec<_> = s.procs.iter().filter(|p| !p.is_kernel_thread()).collect();
    procs.sort_by(|a, b| {
        b.cpu_cores.unwrap_or(0.0).partial_cmp(&a.cpu_cores.unwrap_or(0.0)).unwrap()
    });

    println!("── top {n} processes by cpu (self only, not subtree) ────");
    println!(
        "{:>7} {:>7} {:>7} {:>9} {:>9}  {:<18} ROLE HINT",
        "PID", "PPID", "CPU%", "RSS", "PSS", "COMM"
    );
    for p in procs.iter().take(n) {
        println!(
            "{:>7} {:>7} {:>6.1}% {:>9} {:>9}  {:<18} {}",
            p.key.pid,
            p.ppid,
            p.cpu_percent_of_machine().unwrap_or(0.0),
            mib(p.rss_bytes),
            p.mem.map(|m| mib(m.pss_bytes)).unwrap_or_else(|| "-".into()),
            truncate(&p.comm, 18),
            role_hint(&p.cmdline),
        );
    }
    println!();
    println!("note: a launcher reading ~0% here is normal — its children do the work.");
    println!("      run with --match <name> to see a whole app added up.");
}

/// Adds up every process whose command mentions `pat`, which is the comparison that
/// exposes how badly summed RSS overstates a multi-process app.
fn print_group(s: &Sample, pat: &str) {
    let needle = pat.to_lowercase();
    let matched: Vec<_> = s
        .procs
        .iter()
        .filter(|p| {
            p.comm.to_lowercase().contains(&needle)
                || p.cmdline.first().is_some_and(|c| c.to_lowercase().contains(&needle))
                || p.cgroup.as_deref().is_some_and(|c| c.to_lowercase().contains(&needle))
        })
        .collect();

    if matched.is_empty() {
        println!("no processes matching {pat:?}");
        return;
    }

    println!("── processes matching {pat:?} ──────────────────────────");
    println!("{:>7} {:>7} {:>7} {:>9} {:>9}  {:<22} CGROUP LEAF", "PID", "PPID", "CPU%", "RSS", "PSS", "ROLE");
    let mut by_cpu = matched.clone();
    by_cpu.sort_by(|a, b| {
        b.cpu_cores.unwrap_or(0.0).partial_cmp(&a.cpu_cores.unwrap_or(0.0)).unwrap()
    });
    for p in &by_cpu {
        println!(
            "{:>7} {:>7} {:>6.1}% {:>9} {:>9}  {:<22} {}",
            p.key.pid,
            p.ppid,
            p.cpu_percent_of_machine().unwrap_or(0.0),
            mib(p.rss_bytes),
            p.mem.map(|m| mib(m.pss_bytes)).unwrap_or_else(|| "-".into()),
            truncate(&role_hint(&p.cmdline), 22),
            cgroup_leaf(p.cgroup.as_deref()),
        );
    }

    let cpu: f64 = matched.iter().filter_map(|p| p.cpu_percent_of_machine()).sum();
    let rss: u64 = matched.iter().map(|p| p.rss_bytes).sum();
    let pss: u64 = matched.iter().filter_map(|p| p.mem.map(|m| m.pss_bytes)).sum();
    let scopes: std::collections::BTreeSet<_> =
        matched.iter().map(|p| cgroup_leaf(p.cgroup.as_deref())).collect();

    println!();
    println!("processes    {}", matched.len());
    println!("cpu total    {cpu:.1}% of machine");
    println!("summed rss   {}   << what naive tools report", gib(rss));
    println!("summed pss   {}   << actual memory cost", gib(pss));
    if rss > pss {
        println!(
            "over-count   {}   ({:.1}x)",
            gib(rss - pss),
            rss as f64 / pss.max(1) as f64
        );
    }
    println!("cgroups      {} distinct: {:?}", scopes.len(), scopes);
    if scopes.len() > 1 {
        println!("             ^ one app split across scopes — cgroup alone cannot group it");
    }
}

/// A first approximation of why a process exists, read straight from its arguments.
/// The real labelling, with human sentences, belongs in `btm-model`.
fn role_hint(cmdline: &[String]) -> String {
    if cmdline.is_empty() {
        return "kernel thread".into();
    }
    let ty = process::flag_value(cmdline, "--type=");
    let sub = process::flag_value(cmdline, "--utility-sub-type=")
        .map(|v| v.rsplit('.').next().unwrap_or(v));
    match (ty, sub) {
        (Some(t), Some(s)) => format!("{t}: {s}"),
        (Some(t), None) => t.to_string(),
        (None, _) if process::has_flag(cmdline, "-contentproc") => "content process".into(),
        _ => "main process".into(),
    }
}

fn cgroup_leaf(cgroup: Option<&str>) -> String {
    cgroup
        .and_then(|c| c.rsplit('/').next())
        .unwrap_or("-")
        .to_string()
}

fn block(frac: f64) -> char {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    LEVELS[((frac.clamp(0.0, 1.0) * 7.0).round() as usize).min(7)]
}

fn mib(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / 1024.0 / 1024.0)
}

fn gib(bytes: u64) -> String {
    let mb = bytes as f64 / 1024.0 / 1024.0;
    if mb >= 1024.0 { format!("{:.2} GB", mb / 1024.0) } else { format!("{mb:.0} MB") }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}
