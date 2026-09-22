//! Values the kernel fixes at boot and we need for every rate calculation.

use std::sync::OnceLock;

/// `USER_HZ` — the unit of the CPU time fields in `/proc/PID/stat`.
pub fn clock_ticks() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
        n if n > 0 => n as u64,
        // Every Linux ABI in practice uses 100; only reachable if sysconf fails.
        _ => 100,
    })
}

/// Page size in bytes, for the page-counted fields (`rss` in `/proc/PID/stat`).
pub fn page_size() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        n if n > 0 => n as u64,
        _ => 4096,
    })
}

/// Number of schedulable CPUs. This is the divisor that turns "cores busy" into
/// "percent of this machine", so it is stated everywhere rather than assumed.
pub fn cpu_count() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| match unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } {
        n if n > 0 => n as u64,
        _ => 1,
    })
}
