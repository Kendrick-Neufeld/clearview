//! Network throughput, for the machine and for individual processes.
//!
//! Whole-interface counters are easy. Attributing bytes to a *process* is not:
//! the kernel keeps no per-process byte count, so the only route without
//! elevated privileges is to read every TCP socket's own counters and group
//! them by their owning process.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Interface {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

impl Interface {
    /// Loopback is not network traffic in any sense a person means.
    pub fn is_real(&self) -> bool {
        self.name != "lo" && !self.name.starts_with("veth") && !self.name.starts_with("docker")
    }
}

/// `/proc/net/dev` — cumulative bytes per interface.
pub fn read_interfaces() -> Vec<Interface> {
    let Ok(text) = fs::read_to_string("/proc/net/dev") else { return Vec::new() };
    text.lines()
        .skip(2)
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let f: Vec<u64> =
                rest.split_ascii_whitespace().map(|v| v.parse().unwrap_or(0)).collect();
            Some(Interface {
                name: name.trim().to_string(),
                rx_bytes: *f.first()?,
                // Receive occupies eight columns before transmit begins.
                tx_bytes: *f.get(8)?,
            })
        })
        .collect()
}

/// Cumulative bytes over all of one process's TCP sockets.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessTraffic {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub sockets: u32,
}

/// Per-process TCP byte counters, via `ss`.
///
/// Honest limits, which the interface states rather than hides:
///
/// - **TCP only.** UDP sockets keep no equivalent counter, so QUIC — which is
///   most modern browser traffic — is invisible here. Whole-interface totals
///   still include it, so the per-app figures can sum to less than the total.
/// - **Live sockets only.** A connection that opens and closes between two
///   samples is never seen.
/// - Other users' processes are not attributable without privilege.
///
/// This shells out rather than speaking netlink directly, which costs about
/// twenty milliseconds. That is why it is sampled on its own slower cadence
/// instead of on every tick.
pub fn read_process_traffic() -> HashMap<i32, ProcessTraffic> {
    let mut out: HashMap<i32, ProcessTraffic> = HashMap::new();
    let Ok(result) = std::process::Command::new("ss").args(["-tinp", "--no-header"]).output() else {
        return out;
    };
    if !result.status.success() {
        return out;
    }

    let text = String::from_utf8_lossy(&result.stdout);
    // Output alternates: a socket line carrying `users:(("name",pid=N,fd=M))`,
    // then an indented line of tcp_info containing the byte counters.
    let mut current_pid: Option<i32> = None;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            current_pid = line
                .split_once("pid=")
                .and_then(|(_, rest)| rest.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|d| d.parse().ok());
            if let Some(pid) = current_pid {
                out.entry(pid).or_default().sockets += 1;
            }
            continue;
        }
        let Some(pid) = current_pid else { continue };
        let entry = out.entry(pid).or_default();
        for token in line.split_ascii_whitespace() {
            if let Some(v) = token.strip_prefix("bytes_received:") {
                entry.rx_bytes += v.parse::<u64>().unwrap_or(0);
            } else if let Some(v) = token.strip_prefix("bytes_sent:") {
                entry.tx_bytes += v.parse::<u64>().unwrap_or(0);
            }
        }
    }
    out
}

/// Whether per-process traffic can be read at all.
pub fn process_traffic_available() -> bool {
    std::process::Command::new("ss")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}
