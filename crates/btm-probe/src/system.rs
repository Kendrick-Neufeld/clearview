//! System-wide counters: CPU time, memory, and pressure stall information.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;

/// Cumulative CPU jiffies from one line of `/proc/stat`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimes {
    /// Everything the CPU did, busy or not.
    pub fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }

    /// Time not available for work. `iowait` counts as idle here: the CPU was free,
    /// it just had nothing runnable. Treating it as busy is a common way to make a
    /// healthy machine look saturated.
    pub fn idle_total(&self) -> u64 {
        self.idle + self.iowait
    }

    fn parse_fields(rest: &str) -> Self {
        let mut f = rest.split_ascii_whitespace().map(|v| v.parse().unwrap_or(0));
        CpuTimes {
            user: f.next().unwrap_or(0),
            nice: f.next().unwrap_or(0),
            system: f.next().unwrap_or(0),
            idle: f.next().unwrap_or(0),
            iowait: f.next().unwrap_or(0),
            irq: f.next().unwrap_or(0),
            softirq: f.next().unwrap_or(0),
            steal: f.next().unwrap_or(0),
        }
    }
}

/// `/proc/stat`: aggregate CPU line plus one line per logical CPU.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CpuStat {
    pub total: CpuTimes,
    pub per_cpu: Vec<CpuTimes>,
    /// Cumulative context switches, useful as a thrash indicator.
    pub ctxt: u64,
    /// Processes currently runnable, i.e. competing for CPU right now.
    pub procs_running: u64,
    /// Processes blocked on I/O.
    pub procs_blocked: u64,
}

pub fn read_cpu_stat() -> io::Result<CpuStat> {
    let text = fs::read_to_string("/proc/stat")?;
    let mut out = CpuStat::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "cpu" => out.total = CpuTimes::parse_fields(rest.trim_start()),
            k if k.starts_with("cpu") => out.per_cpu.push(CpuTimes::parse_fields(rest.trim_start())),
            "ctxt" => out.ctxt = rest.trim().parse().unwrap_or(0),
            "procs_running" => out.procs_running = rest.trim().parse().unwrap_or(0),
            "procs_blocked" => out.procs_blocked = rest.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    Ok(out)
}

/// `/proc/meminfo`, in bytes. Only the fields we can explain to a user.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct MemInfo {
    pub total: u64,
    pub free: u64,
    /// The kernel's own estimate of what a new allocation could get without swapping.
    /// This, not `free`, is the number that answers "am I running out of memory".
    pub available: u64,
    pub buffers: u64,
    pub cached: u64,
    pub sreclaimable: u64,
    pub shmem: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl MemInfo {
    /// Memory genuinely committed to running work. Cache and reclaimable slab are
    /// excluded because the kernel will hand them back on demand — counting them as
    /// "used" is what makes Linux look permanently out of memory.
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }

    pub fn swap_used(&self) -> u64 {
        self.swap_total.saturating_sub(self.swap_free)
    }
}

pub fn read_mem_info() -> io::Result<MemInfo> {
    let text = fs::read_to_string("/proc/meminfo")?;
    let mut m = MemInfo::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        // Values are in kB unless stated otherwise; normalise to bytes.
        let kb: u64 = rest
            .split_ascii_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let bytes = kb * 1024;
        match key {
            "MemTotal" => m.total = bytes,
            "MemFree" => m.free = bytes,
            "MemAvailable" => m.available = bytes,
            "Buffers" => m.buffers = bytes,
            "Cached" => m.cached = bytes,
            "SReclaimable" => m.sreclaimable = bytes,
            "Shmem" => m.shmem = bytes,
            "SwapTotal" => m.swap_total = bytes,
            "SwapFree" => m.swap_free = bytes,
            _ => {}
        }
    }
    Ok(m)
}

/// One `some`/`full` line of a pressure file.
///
/// `some` = at least one task was stalled on this resource. `full` = *every*
/// runnable task was stalled, i.e. the machine achieved nothing. The gap between a
/// high utilisation number and a low pressure number is the difference between a
/// machine working hard and a machine in trouble.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PressureLine {
    pub avg10: f32,
    pub avg60: f32,
    pub avg300: f32,
    /// Cumulative stall time in microseconds; the only field safe to difference.
    pub total_us: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Pressure {
    pub some: PressureLine,
    pub full: PressureLine,
}

/// Parses the shared PSI format used by both `/proc/pressure/*` and cgroup
/// `*.pressure` files.
pub fn parse_pressure(text: &str) -> Pressure {
    let mut p = Pressure::default();
    for line in text.lines() {
        let mut parts = line.split_ascii_whitespace();
        let kind = parts.next().unwrap_or("");
        let mut l = PressureLine::default();
        for kv in parts {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            match k {
                "avg10" => l.avg10 = v.parse().unwrap_or(0.0),
                "avg60" => l.avg60 = v.parse().unwrap_or(0.0),
                "avg300" => l.avg300 = v.parse().unwrap_or(0.0),
                "total" => l.total_us = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        match kind {
            "some" => p.some = l,
            "full" => p.full = l,
            _ => {}
        }
    }
    p
}

/// System-wide PSI. Any field may be absent if the kernel was built without
/// `CONFIG_PSI`, so each is optional rather than defaulted to a misleading zero.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SystemPressure {
    pub cpu: Option<Pressure>,
    pub memory: Option<Pressure>,
    pub io: Option<Pressure>,
    pub irq: Option<Pressure>,
}

pub fn read_system_pressure() -> SystemPressure {
    let read = |n: &str| {
        fs::read_to_string(format!("/proc/pressure/{n}"))
            .ok()
            .map(|t| parse_pressure(&t))
    };
    SystemPressure {
        cpu: read("cpu"),
        memory: read("memory"),
        io: read("io"),
        irq: read("irq"),
    }
}

/// Seconds since boot, from `/proc/uptime`. Used to turn a process `starttime` into
/// a wall-clock age.
pub fn read_uptime_secs() -> io::Result<f64> {
    let t = fs::read_to_string("/proc/uptime")?;
    Ok(t.split_ascii_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_parses_both_lines() {
        let text = "some avg10=5.10 avg60=5.17 avg300=5.44 total=433433798\n\
                    full avg10=4.21 avg60=4.43 avg300=4.72 total=391302320\n";
        let p = parse_pressure(text);
        assert_eq!(p.some.avg10, 5.10);
        assert_eq!(p.some.total_us, 433433798);
        assert_eq!(p.full.avg300, 4.72);
    }

    /// The CPU-only pressure file has no `full` line; its absence must read as zero
    /// rather than as garbage.
    #[test]
    fn pressure_tolerates_a_missing_full_line() {
        let p = parse_pressure("some avg10=1.00 avg60=2.00 avg300=3.00 total=10\n");
        assert_eq!(p.some.avg60, 2.0);
        assert_eq!(p.full.total_us, 0);
    }

    /// iowait counts as idle: the CPU was available, nothing was runnable.
    #[test]
    fn iowait_is_not_counted_as_busy() {
        let t = CpuTimes { user: 10, system: 10, idle: 60, iowait: 20, ..Default::default() };
        assert_eq!(t.total(), 100);
        assert_eq!(t.idle_total(), 80);
    }

    #[test]
    fn used_memory_excludes_reclaimable_cache() {
        let m = MemInfo { total: 32_000, available: 20_000, cached: 8_000, ..Default::default() };
        assert_eq!(m.used(), 12_000);
    }
}
