//! Graphics hardware, both the built-in one and the discrete one.
//!
//! There is no single interface for this. The open drivers publish per-client
//! engine busy-time through DRM fdinfo, which is the only way to attribute GPU
//! work to a process at all; NVIDIA's proprietary stack answers through
//! `nvidia-smi` instead. Both are handled, and either may be absent.

use crate::process;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

/// One GPU's whole-device state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    /// Busy fraction 0.0–1.0, where it can be determined.
    pub busy: Option<f32>,
    pub temp_c: Option<f32>,
    pub mem_used_bytes: Option<u64>,
    pub mem_total_bytes: Option<u64>,
    pub power_w: Option<f32>,
    pub clock_mhz: Option<u32>,
    /// True when the figures came from summing per-process engine time rather
    /// than from a hardware counter — an underestimate, since work not
    /// attributable to a client is not counted.
    pub busy_from_clients: bool,
}

/// Cumulative engine nanoseconds for one DRM client.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientEngineTime {
    pub pid: i32,
    pub total_ns: u64,
    pub memory_bytes: u64,
}

/// Scans DRM clients, remembering exactly which file descriptors carry them.
///
/// A full walk of every process's `fdinfo` costs about 85 ms on a busy machine.
/// Narrowing it to the processes that use the GPU only gets to ~34 ms, because
/// a browser holds hundreds of descriptors and almost none of them are DRM
/// ones. So the scanner remembers the individual `fdinfo` *paths* that carried
/// a client id — around a hundred files — and re-reads just those,
/// rediscovering the full set occasionally to catch new programs.
#[derive(Default)]
pub struct DrmScanner {
    known_paths: Vec<std::path::PathBuf>,
    since_full_scan: u32,
}

impl DrmScanner {
    /// One sweep. Every `rediscover_every` calls it walks the whole process
    /// table again so newly started programs are picked up.
    pub fn sample(&mut self, rediscover_every: u32) -> HashMap<u64, ClientEngineTime> {
        if self.known_paths.is_empty() || self.since_full_scan >= rediscover_every {
            self.since_full_scan = 0;
            let (clients, paths) = full_scan();
            self.known_paths = paths;
            return clients;
        }

        self.since_full_scan += 1;
        let mut out: HashMap<u64, ClientEngineTime> = HashMap::new();
        // A descriptor that has gone is a process that has exited; drop it and
        // let the next full scan re-establish the truth.
        self.known_paths.retain(|path| {
            let Ok(text) = fs::read_to_string(path) else { return false };
            let pid = pid_of_fdinfo(path).unwrap_or(0);
            merge_client(&mut out, pid, &text);
            true
        });
        out
    }
}

fn full_scan() -> (HashMap<u64, ClientEngineTime>, Vec<std::path::PathBuf>) {
    let mut out: HashMap<u64, ClientEngineTime> = HashMap::new();
    let mut paths = Vec::new();
    for pid in process::list_pids() {
        let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fdinfo")) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(text) = fs::read_to_string(&path) else { continue };
            if !text.contains("drm-client-id") {
                continue;
            }
            merge_client(&mut out, pid, &text);
            paths.push(path);
        }
    }
    (out, paths)
}

fn pid_of_fdinfo(path: &std::path::Path) -> Option<i32> {
    // `/proc/<pid>/fdinfo/<fd>`
    path.components().nth(2)?.as_os_str().to_str()?.parse().ok()
}

/// Folds one `fdinfo` into the client map, keyed by the driver's client id.
///
/// Keying on the client id rather than the pid matters: a process can hold
/// several DRM file descriptors pointing at the same client, and each reports
/// the same totals, so summing per descriptor would multiply a browser's GPU
/// use by its tab count.
fn merge_client(out: &mut HashMap<u64, ClientEngineTime>, pid: i32, text: &str) {
    let mut client_id = None;
    let mut engine_ns = 0u64;
    let mut memory = 0u64;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key {
            "drm-client-id" => client_id = value.parse::<u64>().ok(),
            // Every `drm-engine-<name>` is nanoseconds busy on that engine.
            // `drm-engine-capacity-*` is a count, not a time, and must not be
            // added in.
            k if k.starts_with("drm-engine-") && !k.starts_with("drm-engine-capacity") => {
                engine_ns += value.trim_end_matches(" ns").trim().parse::<u64>().unwrap_or(0);
            }
            k if k.starts_with("drm-total-") || k.starts_with("drm-resident-") => {
                memory = memory.max(parse_drm_size(value));
            }
            _ => {}
        }
    }
    if let Some(id) = client_id {
        let slot = out.entry(id).or_default();
        slot.pid = pid;
        slot.total_ns = slot.total_ns.max(engine_ns);
        slot.memory_bytes = slot.memory_bytes.max(memory);
    }
}

/// Convenience for a one-off full sweep.
pub fn read_drm_clients() -> HashMap<u64, ClientEngineTime> {
    full_scan().0
}

/// DRM sizes come as `371148 KiB` or a bare byte count.
fn parse_drm_size(value: &str) -> u64 {
    let mut parts = value.split_ascii_whitespace();
    let n: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    match parts.next() {
        Some("KiB") => n * 1024,
        Some("MiB") => n * 1024 * 1024,
        Some("GiB") => n * 1024 * 1024 * 1024,
        _ => n,
    }
}

/// Discovers the GPUs present, from the DRM device list.
pub fn list_devices() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/sys/class/drm") else { return out };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // `card0`, not `card0-HDMI-A-1`.
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let driver = fs::read_link(entry.path().join("device/driver"))
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_default();
        out.push((name, driver));
    }
    out.sort();
    out
}

/// Queries NVIDIA's proprietary stack.
///
/// Shelling out is not elegant, but the alternative is linking NVML, and this
/// runs at most once every few seconds. Absent hardware simply yields nothing.
pub fn read_nvidia() -> Vec<GpuInfo> {
    let Ok(out) = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,temperature.gpu,memory.used,memory.total,power.draw,clocks.sm",
            "--format=csv,noheader,nounits",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            if f.len() < 7 {
                return None;
            }
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<f32>().ok());
            Some(GpuInfo {
                name: f[0].to_string(),
                busy: num(1).map(|v| v / 100.0),
                temp_c: num(2),
                mem_used_bytes: num(3).map(|v| (v * 1024.0 * 1024.0) as u64),
                mem_total_bytes: num(4).map(|v| (v * 1024.0 * 1024.0) as u64),
                power_w: num(5),
                clock_mhz: num(6).map(|v| v as u32),
                busy_from_clients: false,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_sizes_carry_units() {
        assert_eq!(parse_drm_size("371148 KiB"), 371148 * 1024);
        assert_eq!(parse_drm_size("2 MiB"), 2 * 1024 * 1024);
        assert_eq!(parse_drm_size("4096"), 4096);
    }
}
