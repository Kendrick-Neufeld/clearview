//! Turns the cumulative counters in `/proc` into rates.
//!
//! Everything in this module exists because a single read of `/proc` cannot tell you
//! what a process is doing *now* — only what it has done since it started. Rates
//! need two samples and the discipline to compare the right pairs.

use crate::conf;
use crate::process::{self, MemRollup, ProcIo, ProcKey, ProcStat};
use crate::system::{self, CpuStat, MemInfo, SystemPressure};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// What to collect. The expensive sources are opt-in and rate-limited because a
/// monitor that shows up in its own process list has failed at its job.
#[derive(Debug, Clone)]
pub struct SamplerConfig {
    /// Collect proportional set size. Requires a page-table walk per process.
    pub collect_pss: bool,
    /// How often to refresh PSS. Between refreshes the last value is reused.
    pub pss_interval: Duration,
    /// Only the N largest processes by RSS get a PSS reading each refresh. A full
    /// system scan costs ~380 ms here, which is far too much to do every second.
    pub pss_top_n: usize,
    pub collect_io: bool,
    pub collect_cmdline: bool,
}

impl Default for SamplerConfig {
    fn default() -> Self {
        Self {
            collect_pss: true,
            pss_interval: Duration::from_secs(10),
            pss_top_n: 64,
            collect_io: true,
            collect_cmdline: true,
        }
    }
}

/// One process at one instant, with rates where two samples allowed them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcSample {
    pub key: ProcKey,
    pub ppid: i32,
    pub comm: String,
    pub state: char,
    pub num_threads: i64,
    pub uid: Option<u32>,

    /// CPU consumed by this process alone, expressed as a fraction of one core:
    /// 1.0 means it kept one core fully busy. `None` on the first sample, when no
    /// rate can honestly be reported yet.
    pub cpu_cores: Option<f64>,
    /// Cumulative CPU seconds since the process started.
    pub cpu_seconds_total: f64,

    pub rss_bytes: u64,
    /// True memory cost, shared pages divided among their users. `None` when not
    /// sampled this round.
    pub mem: Option<MemRollup>,

    pub io: Option<ProcIo>,
    /// Bytes per second actually reaching the disk, if two samples allowed it.
    pub disk_read_bps: Option<f64>,
    pub disk_write_bps: Option<f64>,

    pub cgroup: Option<String>,
    pub cmdline: Vec<String>,
}

impl ProcSample {
    /// Percentage of the entire machine, the unit this tool reports by default.
    pub fn cpu_percent_of_machine(&self) -> Option<f64> {
        self.cpu_cores.map(|c| c / conf::cpu_count() as f64 * 100.0)
    }

    /// Percentage of a single core — the `top` convention, where values above 100
    /// are normal and expected.
    pub fn cpu_percent_of_core(&self) -> Option<f64> {
        self.cpu_cores.map(|c| c * 100.0)
    }

    /// Kernel threads have no argument vector and are children of kthreadd.
    pub fn is_kernel_thread(&self) -> bool {
        self.cmdline.is_empty() && (self.key.pid == 2 || self.ppid == 2)
    }
}

/// A complete reading of the system at one instant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// Seconds since the previous sample; `None` for the first one.
    pub interval_secs: Option<f64>,
    pub cpu: CpuStat,
    /// Machine-wide CPU busy fraction, 0.0 to 1.0.
    pub cpu_busy: Option<f64>,
    /// Per-logical-CPU busy fraction, in `/proc/stat` order.
    pub per_cpu_busy: Vec<f64>,
    pub mem: MemInfo,
    pub pressure: SystemPressure,
    pub procs: Vec<ProcSample>,
}

struct PrevProc {
    cpu_ticks: u64,
    io: Option<ProcIo>,
}

pub struct Sampler {
    config: SamplerConfig,
    prev: HashMap<ProcKey, PrevProc>,
    prev_cpu: Option<CpuStat>,
    prev_at: Option<Instant>,
    mem_cache: HashMap<ProcKey, MemRollup>,
    mem_refreshed_at: Option<Instant>,
}

impl Sampler {
    pub fn new(config: SamplerConfig) -> Self {
        Self {
            config,
            prev: HashMap::new(),
            prev_cpu: None,
            prev_at: None,
            mem_cache: HashMap::new(),
            mem_refreshed_at: None,
        }
    }

    pub fn config(&self) -> &SamplerConfig {
        &self.config
    }

    /// Takes one reading. The first call establishes a baseline and reports no
    /// rates; every call after that reports rates over the elapsed interval.
    pub fn sample(&mut self) -> std::io::Result<Sample> {
        let now = Instant::now();
        let interval = self.prev_at.map(|p| now.duration_since(p).as_secs_f64());

        let cpu = system::read_cpu_stat()?;
        let mem = system::read_mem_info()?;
        let pressure = system::read_system_pressure();

        let (cpu_busy, per_cpu_busy) = match &self.prev_cpu {
            Some(prev) => (busy_fraction(&prev.total, &cpu.total), per_cpu_busy(prev, &cpu)),
            None => (None, Vec::new()),
        };

        let stats: Vec<ProcStat> = process::list_pids()
            .into_iter()
            .filter_map(process::read_stat)
            .collect();

        let refresh_pss = self.config.collect_pss
            && self
                .mem_refreshed_at
                .is_none_or(|t| now.duration_since(t) >= self.config.pss_interval);
        let pss_targets = if refresh_pss { self.pss_targets(&stats) } else { Vec::new() };

        let ticks = conf::clock_ticks() as f64;
        let mut procs = Vec::with_capacity(stats.len());
        let mut next_prev = HashMap::with_capacity(stats.len());

        for st in &stats {
            let key = st.key();
            let pid = key.pid;
            let cpu_ticks = st.cpu_ticks();

            // The start-time half of the key is what stops a recycled PID from
            // being diffed against its predecessor.
            let previous = self.prev.get(&key);

            let cpu_cores = match (previous, interval) {
                (Some(p), Some(dt)) if dt > 0.0 => {
                    Some(cpu_ticks.saturating_sub(p.cpu_ticks) as f64 / ticks / dt)
                }
                _ => None,
            };

            let io = if self.config.collect_io { process::read_io(pid) } else { None };
            let (disk_read_bps, disk_write_bps) = match (previous.and_then(|p| p.io), io, interval) {
                (Some(prev_io), Some(cur), Some(dt)) if dt > 0.0 => (
                    Some(cur.read_bytes.saturating_sub(prev_io.read_bytes) as f64 / dt),
                    Some(cur.write_bytes.saturating_sub(prev_io.write_bytes) as f64 / dt),
                ),
                _ => (None, None),
            };

            if refresh_pss && pss_targets.binary_search(&pid).is_ok()
                && let Some(rollup) = process::read_mem_rollup(pid) {
                    self.mem_cache.insert(key, rollup);
                }

            next_prev.insert(key, PrevProc { cpu_ticks, io });

            procs.push(ProcSample {
                key,
                ppid: st.ppid,
                comm: st.comm.clone(),
                state: st.state,
                num_threads: st.num_threads,
                uid: process::read_uid(pid),
                cpu_cores,
                cpu_seconds_total: cpu_ticks as f64 / ticks,
                rss_bytes: st.rss_bytes,
                mem: self.mem_cache.get(&key).copied(),
                io,
                disk_read_bps,
                disk_write_bps,
                cgroup: process::read_cgroup(pid),
                cmdline: if self.config.collect_cmdline {
                    process::read_cmdline(pid)
                } else {
                    Vec::new()
                },
            });
        }

        if refresh_pss {
            self.mem_refreshed_at = Some(now);
            // Drop cached memory for processes that have gone, so the map cannot
            // grow without bound over a long run.
            self.mem_cache.retain(|k, _| next_prev.contains_key(k));
        }

        self.prev = next_prev;
        self.prev_cpu = Some(cpu.clone());
        self.prev_at = Some(now);

        Ok(Sample { interval_secs: interval, cpu, cpu_busy, per_cpu_busy, mem, pressure, procs })
    }

    /// The PIDs worth paying for a page-table walk on, chosen by RSS as a cheap
    /// stand-in for PSS. Returned sorted so lookup during the main loop is binary.
    fn pss_targets(&self, stats: &[ProcStat]) -> Vec<i32> {
        let mut by_size: Vec<&ProcStat> = stats.iter().filter(|s| s.rss_bytes > 0).collect();
        by_size.sort_unstable_by_key(|s| std::cmp::Reverse(s.rss_bytes));
        let mut pids: Vec<i32> =
            by_size.iter().take(self.config.pss_top_n).map(|s| s.pid).collect();
        pids.sort_unstable();
        pids
    }
}

fn busy_fraction(prev: &system::CpuTimes, cur: &system::CpuTimes) -> Option<f64> {
    let total = cur.total().checked_sub(prev.total())?;
    if total == 0 {
        return None;
    }
    let idle = cur.idle_total().saturating_sub(prev.idle_total());
    Some((total.saturating_sub(idle)) as f64 / total as f64)
}

fn per_cpu_busy(prev: &CpuStat, cur: &CpuStat) -> Vec<f64> {
    cur.per_cpu
        .iter()
        .zip(prev.per_cpu.iter())
        .map(|(c, p)| busy_fraction(p, c).unwrap_or(0.0))
        .collect()
}
