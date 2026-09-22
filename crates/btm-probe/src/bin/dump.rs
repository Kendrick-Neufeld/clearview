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
