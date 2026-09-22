//! Turns the cumulative counters in `/proc` into rates.
//!
//! Everything in this module exists because a single read of `/proc` cannot tell you
//! what a process is doing *now* — only what it has done since it started. Rates
//! need two samples and the discipline to compare the right pairs.

use crate::conf;
use crate::gpu::{self, DrmScanner, GpuInfo};
use crate::net::{self, Interface};
use crate::process::{self, MemRollup, ProcIo, ProcKey, ProcStat};
use crate::sensors::{self, PowerMeter, Thermals};
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
    /// Per-process graphics work, from DRM fdinfo. Cheap once warmed up.
    pub collect_gpu: bool,
    /// How often the GPU scanner re-walks every process to find new clients.
    pub gpu_rediscover_every: u32,
    /// Temperatures, frequencies and package power.
    pub collect_sensors: bool,
    /// Per-process network. Costs a `ss` invocation, so it has its own cadence.
    pub collect_network: bool,
    pub network_interval: Duration,
    /// `nvidia-smi` costs about 40 ms, so it is not run every tick either.
    pub nvidia_interval: Duration,
}

impl Default for SamplerConfig {
    fn default() -> Self {
        Self {
            collect_pss: true,
            pss_interval: Duration::from_secs(10),
            pss_top_n: 64,
            collect_io: true,
            collect_cmdline: true,
            collect_gpu: true,
            gpu_rediscover_every: 15,
            collect_sensors: true,
            collect_network: true,
            network_interval: Duration::from_secs(4),
            nvidia_interval: Duration::from_secs(5),
        }
    }
}

/// Throughput on one interface, in bytes per second.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceRate {
    pub name: String,
    pub rx_bps: f64,
    pub tx_bps: f64,
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

    /// Share of one GPU's engine time, 0.0–1.0. `None` when this process does
    /// no graphics work or the driver reports nothing for it.
    pub gpu_busy: Option<f64>,
    pub gpu_mem_bytes: u64,

    /// TCP throughput, bytes per second. `None` until two network samples
    /// exist, and always an underestimate — see `net::read_process_traffic`.
    pub net_rx_bps: Option<f64>,
    pub net_tx_bps: Option<f64>,
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

    pub thermals: Thermals,
    /// Package power draw in watts, where the hardware exposes it.
    pub power_w: Option<f32>,
    /// Current frequency per logical CPU, in MHz.
    pub core_mhz: Vec<u32>,
    /// Which physical core each logical CPU belongs to, for pairing a
    /// hyper-thread with the temperature it shares.
    pub core_of_cpu: Vec<usize>,
    pub gpus: Vec<GpuInfo>,
    pub interfaces: Vec<InterfaceRate>,
}

struct PrevProc {
    cpu_ticks: u64,
    io: Option<ProcIo>,
}

/// Everything that is sampled on a slower cadence than the main tick.
struct Slow<T> {
    value: T,
    at: Option<Instant>,
}

impl<T: Default> Default for Slow<T> {
    fn default() -> Self {
        Slow { value: T::default(), at: None }
    }
}

impl<T> Slow<T> {
    fn due(&self, now: Instant, every: Duration) -> bool {
        self.at.is_none_or(|t| now.duration_since(t) >= every)
    }
}

pub struct Sampler {
    config: SamplerConfig,
    prev: HashMap<ProcKey, PrevProc>,
    prev_cpu: Option<CpuStat>,
    prev_at: Option<Instant>,
    mem_cache: HashMap<ProcKey, MemRollup>,
    mem_refreshed_at: Option<Instant>,

    drm: DrmScanner,
    prev_drm: HashMap<u64, u64>,
    power: PowerMeter,
    core_of_cpu: Vec<usize>,

    net_prev: Slow<HashMap<i32, net::ProcessTraffic>>,
    net_rates: HashMap<i32, (f64, f64)>,
    iface_prev: Slow<Vec<Interface>>,
    iface_rates: Vec<InterfaceRate>,
    nvidia: Slow<Vec<GpuInfo>>,
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
            drm: DrmScanner::default(),
            prev_drm: HashMap::new(),
            power: PowerMeter::new(),
            core_of_cpu: sensors::logical_to_physical_core(),
            net_prev: Slow::default(),
            net_rates: HashMap::new(),
            iface_prev: Slow::default(),
            iface_rates: Vec::new(),
            nvidia: Slow::default(),
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

        // Graphics: engine nanoseconds per client, differenced into a busy
        // fraction and then attributed to whichever process owns the client.
        let mut gpu_by_pid: HashMap<i32, (f64, u64)> = HashMap::new();
        if self.config.collect_gpu {
            let clients = self.drm.sample(self.config.gpu_rediscover_every);
            let mut next: HashMap<u64, u64> = HashMap::with_capacity(clients.len());
            for (id, client) in &clients {
                next.insert(*id, client.total_ns);
                let before = self.prev_drm.get(id).copied().unwrap_or(client.total_ns);
                let delta_ns = client.total_ns.saturating_sub(before);
                if let Some(dt) = interval.filter(|d| *d > 0.0) {
                    let entry = gpu_by_pid.entry(client.pid).or_insert((0.0, 0));
                    entry.0 += delta_ns as f64 / 1e9 / dt;
                    entry.1 += client.memory_bytes;
                }
            }
            self.prev_drm = next;
        }

        // Network is sampled on its own slower clock: reading it costs an
        // external command, and throughput averaged over a few seconds is what
        // a person can actually read anyway.
        if self.config.collect_network && self.net_prev.due(now, self.config.network_interval) {
            let current = net::read_process_traffic();
            if let Some(last) = self.net_prev.at {
                let dt = now.duration_since(last).as_secs_f64();
                if dt > 0.0 {
                    self.net_rates = current
                        .iter()
                        .map(|(pid, t)| {
                            let before = self.net_prev.value.get(pid).copied().unwrap_or_default();
                            (
                                *pid,
                                (
                                    t.rx_bytes.saturating_sub(before.rx_bytes) as f64 / dt,
                                    t.tx_bytes.saturating_sub(before.tx_bytes) as f64 / dt,
                                ),
                            )
                        })
                        .collect();
                }
            }
            self.net_prev = Slow { value: current, at: Some(now) };
        }

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
                gpu_busy: gpu_by_pid.get(&pid).map(|(busy, _)| *busy).filter(|b| *b > 0.0),
                gpu_mem_bytes: gpu_by_pid.get(&pid).map(|(_, mem)| *mem).unwrap_or(0),
                net_rx_bps: self.net_rates.get(&pid).map(|(rx, _)| *rx),
                net_tx_bps: self.net_rates.get(&pid).map(|(_, tx)| *tx),
            });
        }

        if refresh_pss {
            self.mem_refreshed_at = Some(now);
            // Drop cached memory for processes that have gone, so the map cannot
            // grow without bound over a long run.
            self.mem_cache.retain(|k, _| next_prev.contains_key(k));
        }

        // Whole-interface throughput, which unlike the per-process figures
        // includes UDP and so is always the larger, truer number.
        let interfaces = net::read_interfaces();
        if let Some(last) = self.iface_prev.at {
            let dt = now.duration_since(last).as_secs_f64();
            if dt > 0.0 {
                self.iface_rates = interfaces
                    .iter()
                    .filter(|i| i.is_real())
                    .map(|i| {
                        let before = self.iface_prev.value.iter().find(|p| p.name == i.name);
                        let (rx, tx) = match before {
                            Some(b) => (
                                i.rx_bytes.saturating_sub(b.rx_bytes) as f64 / dt,
                                i.tx_bytes.saturating_sub(b.tx_bytes) as f64 / dt,
                            ),
                            None => (0.0, 0.0),
                        };
                        InterfaceRate { name: i.name.clone(), rx_bps: rx, tx_bps: tx }
                    })
                    .collect();
            }
        }
        self.iface_prev = Slow { value: interfaces, at: Some(now) };

        let (thermals, power_w, core_mhz) = if self.config.collect_sensors {
            (sensors::read_thermals(), self.power.read_watts(), sensors::core_frequencies_mhz())
        } else {
            (Thermals::default(), None, Vec::new())
        };

        // The integrated GPU has no whole-device busy counter available without
        // privilege, so its utilisation is the sum of what its clients report.
        // That is an underestimate — work the driver cannot attribute to a
        // client is invisible — and the flag on the struct says so.
        let mut gpus = Vec::new();
        if self.config.collect_gpu {
            let client_busy: f64 = gpu_by_pid.values().map(|(b, _)| *b).sum();
            if let Some((card, driver)) =
                gpu::list_devices().into_iter().find(|(_, d)| d != "nvidia")
            {
                gpus.push(GpuInfo {
                    name: format!("{card} ({driver})"),
                    busy: interval.map(|_| client_busy.min(1.0) as f32),
                    temp_c: None,
                    mem_used_bytes: Some(gpu_by_pid.values().map(|(_, m)| *m).sum()),
                    busy_from_clients: true,
                    ..Default::default()
                });
            }
            if self.nvidia.due(now, self.config.nvidia_interval) {
                self.nvidia = Slow { value: gpu::read_nvidia(), at: Some(now) };
            }
            gpus.extend(self.nvidia.value.iter().cloned());
        }

        self.prev = next_prev;
        self.prev_cpu = Some(cpu.clone());
        self.prev_at = Some(now);

        Ok(Sample {
            interval_secs: interval,
            cpu,
            cpu_busy,
            per_cpu_busy,
            mem,
            pressure,
            procs,
            thermals,
            power_w,
            core_mhz,
            core_of_cpu: self.core_of_cpu.clone(),
            gpus,
            interfaces: self.iface_rates.clone(),
        })
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
