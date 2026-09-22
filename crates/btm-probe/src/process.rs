//! Per-process acquisition.
//!
//! Every read here can race with the process exiting, so a missing file is a normal
//! outcome and never an error worth reporting.

use crate::conf;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Identity that survives PID reuse.
///
/// PIDs wrap. A long-lived sampler that keys on the PID alone will eventually
/// subtract a dead process's CPU counter from a brand-new one's and report an
/// absurd spike, so the start time is part of the key everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProcKey {
    pub pid: i32,
    /// Field 22 of `/proc/PID/stat`: start time in clock ticks since boot.
    pub start_time: u64,
}

/// The fields of `/proc/PID/stat` we actually use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcStat {
    pub pid: i32,
    /// `comm`, truncated by the kernel to 15 characters. This truncation is exactly
    /// why so many unrelated processes appear to share a name.
    pub comm: String,
    pub state: char,
    pub ppid: i32,
    pub utime: u64,
    pub stime: u64,
    pub num_threads: i64,
    pub start_time: u64,
    pub vsize: u64,
    /// Resident pages. Shared pages are counted in full for every process that maps
    /// them, so summing this across an app over-reports badly; see `smaps_rollup`.
    pub rss_bytes: u64,
}

impl ProcStat {
    pub fn key(&self) -> ProcKey {
        ProcKey { pid: self.pid, start_time: self.start_time }
    }

    /// Total CPU jiffies this process has consumed since it started.
    pub fn cpu_ticks(&self) -> u64 {
        self.utime + self.stime
    }
}

/// Parses `/proc/PID/stat`.
///
/// `comm` is wrapped in parentheses and may itself contain spaces *and*
/// parentheses, so the only safe split is at the final `)`. Splitting on
/// whitespace, as many parsers do, silently corrupts every field for any process
/// with a space in its name.
pub fn parse_stat(text: &str) -> Option<ProcStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close < open {
        return None;
    }
    let pid: i32 = text[..open].trim().parse().ok()?;
    let comm = text[open + 1..close].to_string();

    // Fields after `comm` start at field 3, so field N is at index N - 3.
    let f: Vec<&str> = text[close + 1..].split_ascii_whitespace().collect();
    let get = |n: usize| -> u64 { f.get(n - 3).and_then(|v| v.parse().ok()).unwrap_or(0) };

    Some(ProcStat {
        pid,
        comm,
        state: f.first().and_then(|s| s.chars().next()).unwrap_or('?'),
        ppid: f.get(1).and_then(|v| v.parse().ok()).unwrap_or(0),
        utime: get(14),
        stime: get(15),
        num_threads: get(20) as i64,
        start_time: get(22),
        vsize: get(23),
        rss_bytes: get(24) * conf::page_size(),
    })
}

pub fn read_stat(pid: i32) -> Option<ProcStat> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Proportional set size, from `/proc/PID/smaps_rollup`.
///
/// PSS divides each shared page by the number of processes mapping it, so summing
/// PSS across an app's processes gives its true memory cost. Summing RSS instead can
/// overstate a multi-process app by more than double.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct MemRollup {
    pub pss_bytes: u64,
    pub rss_bytes: u64,
    /// Memory private to this process — freed for certain when it exits.
    pub private_bytes: u64,
    pub swap_bytes: u64,
}

/// Reads PSS. Costs roughly a millisecond per process because the kernel walks the
/// page tables, so callers should sample it less often than the cheap counters.
pub fn read_mem_rollup(pid: i32) -> Option<MemRollup> {
    let text = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    let mut m = MemRollup::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let kb: u64 = rest
            .split_ascii_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let bytes = kb * 1024;
        match key {
            "Pss" => m.pss_bytes = bytes,
            "Rss" => m.rss_bytes = bytes,
            "Private_Clean" | "Private_Dirty" => m.private_bytes += bytes,
            "Swap" => m.swap_bytes = bytes,
            _ => {}
        }
    }
    Some(m)
}

/// `/proc/PID/io`. Cumulative byte counters.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ProcIo {
    /// Bytes requested through syscalls, including reads served from page cache.
    pub rchar: u64,
    pub wchar: u64,
    /// Bytes that actually reached the block layer. This is the one that means
    /// "this process is hitting your disk".
    pub read_bytes: u64,
    pub write_bytes: u64,
}

pub fn read_io(pid: i32) -> Option<ProcIo> {
    let text = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let mut io = ProcIo::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let v: u64 = rest.trim().parse().unwrap_or(0);
        match key {
            "rchar" => io.rchar = v,
            "wchar" => io.wchar = v,
            "read_bytes" => io.read_bytes = v,
            "write_bytes" => io.write_bytes = v,
            _ => {}
        }
    }
    Some(io)
}

/// Full argument vector. Empty for kernel threads, which have no userspace command.
pub fn read_cmdline(pid: i32) -> Vec<String> {
    let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    raw.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Yields command-line tokens, tolerating a flattened argv.
///
/// `/proc/PID/cmdline` is meant to be NUL-separated, but Chromium and Electron
/// rewrite their argv in place to set the process title, leaving every argument in a
/// single space-separated blob. A parser that only splits on NUL sees one giant
/// "argument" and silently fails to find any flag — which is why so many tools show
/// these processes as unexplained duplicates. Splitting on whitespace as well costs
/// nothing for well-behaved processes and recovers the flags for the rest.
pub fn cmdline_tokens(cmdline: &[String]) -> impl Iterator<Item = &str> {
    cmdline.iter().flat_map(|a| a.split_ascii_whitespace())
}

/// The value of a `--flag=value` style argument, wherever it appears.
pub fn flag_value<'a>(cmdline: &'a [String], prefix: &str) -> Option<&'a str> {
    cmdline_tokens(cmdline).find_map(|t| t.strip_prefix(prefix))
}

/// Whether a bare flag is present.
pub fn has_flag(cmdline: &[String], flag: &str) -> bool {
    cmdline_tokens(cmdline).any(|t| t == flag)
}

/// Resolved path of the running executable. Requires ownership or privilege, so
/// absent for other users' processes.
pub fn read_exe(pid: i32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// The cgroup v2 path, e.g. `/user.slice/.../app-vesktop-3929.scope`.
///
/// This is the kernel's own opinion of what a process belongs to, and the strongest
/// grouping signal available — though not a complete one, since processes that
/// daemonise or are spawned outside their launcher's scope end up in the bare
/// session scope instead.
pub fn read_cgroup(pid: i32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    // Unified hierarchy lines look like `0::/path`.
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Real user id, from `/proc/PID/status`.
pub fn read_uid(pid: i32) -> Option<u32> {
    let text = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            return rest.split_ascii_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// Every PID currently present in `/proc`, unsorted.
pub fn list_pids() -> Vec<i32> {
    let Ok(dir) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classic `/proc/PID/stat` bug: `comm` is unquoted and unescaped, so a
    /// process named `foo (bar) baz` shifts every subsequent field for any parser
    /// that splits on whitespace or the first `)`.
    #[test]
    fn stat_handles_parens_and_spaces_in_comm() {
        let line = "1234 (evil ) name) S 42 1234 1234 0 -1 4194304 100 0 0 0 \
                    11 22 0 0 20 0 7 0 99999 1000 500 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let s = parse_stat(line).expect("parses");
        assert_eq!(s.pid, 1234);
        assert_eq!(s.comm, "evil ) name");
        assert_eq!(s.state, 'S');
        assert_eq!(s.ppid, 42);
        assert_eq!(s.utime, 11);
        assert_eq!(s.stime, 22);
        assert_eq!(s.num_threads, 7);
        assert_eq!(s.start_time, 99999);
    }

    #[test]
    fn stat_field_offsets_match_a_real_line() {
        let line = "1 (systemd) S 0 1 1 0 -1 4194560 30000 500 100 0 \
                    250 800 10 20 20 0 1 0 25 170000000 3000 18446744073709551615 \
                    1 1 1 1 1 1 1 1 1 1 1 1 1 1";
        let s = parse_stat(line).expect("parses");
        assert_eq!(s.comm, "systemd");
        assert_eq!(s.utime, 250);
        assert_eq!(s.stime, 800);
        assert_eq!(s.cpu_ticks(), 1050);
        assert_eq!(s.num_threads, 1);
        assert_eq!(s.start_time, 25);
    }

    /// Electron and Chromium children arrive as one space-separated blob rather
    /// than a NUL-separated vector; flags must still be found.
    #[test]
    fn flags_are_found_in_a_flattened_argv() {
        let flat = vec![
            "/usr/lib/vesktop/vesktop --type=utility \
             --utility-sub-type=network.mojom.NetworkService --enable-sandbox"
                .to_string(),
        ];
        assert_eq!(flag_value(&flat, "--type="), Some("utility"));
        assert_eq!(
            flag_value(&flat, "--utility-sub-type="),
            Some("network.mojom.NetworkService")
        );
        assert!(has_flag(&flat, "--enable-sandbox"));
    }

    #[test]
    fn flags_are_found_in_a_normal_argv() {
        let argv = vec!["firefox".to_string(), "-contentproc".to_string(), "-childID".to_string()];
        assert!(has_flag(&argv, "-contentproc"));
        assert_eq!(flag_value(&argv, "--type="), None);
    }

    /// A recycled PID must never be diffed against its predecessor.
    #[test]
    fn proc_key_distinguishes_a_recycled_pid() {
        let a = ProcKey { pid: 4160, start_time: 100 };
        let b = ProcKey { pid: 4160, start_time: 900 };
        assert_ne!(a, b);
    }
}
