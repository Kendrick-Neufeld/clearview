//! Temperature, frequency and power.
//!
//! None of this is in `/proc`; it lives in `/sys` under names that differ by
//! machine, so everything here is discovered rather than assumed, and every
//! field is optional.

use serde::{Deserialize, Serialize};
use std::fs;
use std::time::Instant;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Thermals {
    /// Whole-package temperature — the number that matters for throttling.
    pub cpu_package_c: Option<f32>,
    /// Per *physical* core, indexed by the kernel's `core_id`.
    pub cores_c: Vec<Option<f32>>,
    pub nvme_c: Option<f32>,
    pub wifi_c: Option<f32>,
    /// Chassis/acpi zone, where one is exposed.
    pub ambient_c: Option<f32>,
}

fn read_milli(path: &std::path::Path) -> Option<f32> {
    fs::read_to_string(path).ok()?.trim().parse::<f32>().ok().map(|v| v / 1000.0)
}

/// Walks `/sys/class/hwmon`, matching sensors by the labels the driver
/// publishes rather than by index, which is not stable between boots.
pub fn read_thermals() -> Thermals {
    let mut t = Thermals::default();
    let Ok(dir) = fs::read_dir("/sys/class/hwmon") else { return t };

    for entry in dir.flatten() {
        let base = entry.path();
        let name = fs::read_to_string(base.join("name")).unwrap_or_default().trim().to_string();

        let Ok(files) = fs::read_dir(&base) else { continue };
        for f in files.flatten() {
            let path = f.path();
            let file = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file.starts_with("temp") || !file.ends_with("_input") {
                continue;
            }
            let Some(value) = read_milli(&path) else { continue };
            let label = fs::read_to_string(path.with_file_name(
                format!("{}_label", file.trim_end_matches("_input")),
            ))
            .unwrap_or_default()
            .trim()
            .to_string();

            match name.as_str() {
                "coretemp" => {
                    if label.starts_with("Package") {
                        t.cpu_package_c = Some(value);
                    } else if let Some(n) = label.strip_prefix("Core ")
                        && let Ok(idx) = n.trim().parse::<usize>() {
                            if t.cores_c.len() <= idx {
                                t.cores_c.resize(idx + 1, None);
                            }
                            t.cores_c[idx] = Some(value);
                        }
                }
                // AMD exposes the package as Tctl/Tdie instead.
                "k10temp" | "zenpower" if label.starts_with("Tctl") || label.starts_with("Tdie") => {
                    t.cpu_package_c = Some(value)
                }
                "nvme" if label.is_empty() || label.starts_with("Composite") => {
                    t.nvme_c = Some(value)
                }
                n if n.starts_with("iwlwifi") || n.starts_with("mt79") => t.wifi_c = Some(value),
                "acpitz" => t.ambient_c = Some(value),
                _ => {}
            }
        }
    }
    t
}

/// Maps each logical CPU to the physical core it shares a temperature with.
///
/// Hyper-threads report no temperature of their own, so a per-core view has to
/// know which siblings to show the same reading for.
pub fn logical_to_physical_core() -> Vec<usize> {
    let mut map = Vec::new();
    for cpu in 0..crate::conf::cpu_count() {
        let id = fs::read_to_string(format!(
            "/sys/devices/system/cpu/cpu{cpu}/topology/core_id"
        ))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(cpu as usize);
        map.push(id);
    }
    map
}

/// Current frequency per logical CPU, in MHz.
pub fn core_frequencies_mhz() -> Vec<u32> {
    (0..crate::conf::cpu_count())
        .map(|cpu| {
            fs::read_to_string(format!(
                "/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_cur_freq"
            ))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|khz| (khz / 1000) as u32)
            .unwrap_or(0)
        })
        .collect()
}

/// Turns the RAPL energy counter into watts.
///
/// `energy_uj` is a free-running microjoule counter that wraps, so power is
/// only meaningful as a difference between two readings — and the wrap has to
/// be handled or the reading goes hugely negative once a minute or so.
pub struct PowerMeter {
    zone: Option<std::path::PathBuf>,
    max_range_uj: u64,
    prev: Option<(u64, Instant)>,
}

impl Default for PowerMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl PowerMeter {
    pub fn new() -> Self {
        // `intel-rapl:0` is the package domain on every machine that has one.
        let zone = ["/sys/class/powercap/intel-rapl:0", "/sys/class/powercap/intel-rapl-mmio:0"]
            .iter()
            .map(std::path::PathBuf::from)
            .find(|p| p.join("energy_uj").exists() && fs::read_to_string(p.join("energy_uj")).is_ok());

        let max_range_uj = zone
            .as_ref()
            .and_then(|z| fs::read_to_string(z.join("max_energy_range_uj")).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(u64::MAX);

        PowerMeter { zone, max_range_uj, prev: None }
    }

    pub fn available(&self) -> bool {
        self.zone.is_some()
    }

    /// Average watts since the previous call; `None` on the first one.
    pub fn read_watts(&mut self) -> Option<f32> {
        let zone = self.zone.as_ref()?;
        let uj: u64 = fs::read_to_string(zone.join("energy_uj")).ok()?.trim().parse().ok()?;
        let now = Instant::now();

        let watts = match self.prev {
            Some((prev_uj, prev_at)) => {
                let dt = now.duration_since(prev_at).as_secs_f64();
                let delta = if uj >= prev_uj {
                    uj - prev_uj
                } else {
                    // Counter wrapped.
                    self.max_range_uj.saturating_sub(prev_uj).saturating_add(uj)
                };
                (dt > 0.0).then(|| (delta as f64 / 1_000_000.0 / dt) as f32)
            }
            None => None,
        };
        self.prev = Some((uj, now));
        watts
    }
}
