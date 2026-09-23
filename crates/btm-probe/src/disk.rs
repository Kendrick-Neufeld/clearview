//! Whole-device disk throughput.
//!
//! Per-process byte counters come from `/proc/PID/io`; this is the other half,
//! what the hardware actually did. The two rarely match: a read served from
//! page cache never reaches the device, and writeback reaches it long after the
//! process that asked for it moved on.

use serde::{Deserialize, Serialize};
use std::fs;

/// Sector size assumed by `/proc/diskstats`, regardless of the device's own.
const SECTOR_BYTES: u64 = 512;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiskDevice {
    pub name: String,
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// Milliseconds spent with at least one request in flight. Saturation shows
    /// up here long before throughput looks unusual.
    pub io_ms: u64,
}

/// Reads `/proc/diskstats`, keeping only whole devices.
///
/// Partitions are listed alongside their parent device and carry overlapping
/// counters, so including both would roughly double every figure. `/sys/block`
/// lists exactly the whole devices, which is the distinction that matters.
pub fn read_devices() -> Vec<DiskDevice> {
    let whole: Vec<String> = fs::read_dir("/sys/block")
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();

    let Ok(text) = fs::read_to_string("/proc/diskstats") else { return Vec::new() };
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_ascii_whitespace().collect();
            if f.len() < 14 {
                return None;
            }
            let name = f[2].to_string();
            if !whole.contains(&name) {
                return None;
            }
            // Loop and ram devices are not storage anyone is asking about.
            if name.starts_with("loop") || name.starts_with("ram") {
                return None;
            }
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            Some(DiskDevice {
                name,
                read_bytes: num(5) * SECTOR_BYTES,
                write_bytes: num(9) * SECTOR_BYTES,
                io_ms: num(12),
            })
        })
        .collect()
}
