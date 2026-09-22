//! The numbers, with the rules that keep them honest.

use btm_probe::ProcSample;
use btm_probe::conf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// CPU as a fraction of one core: 2.0 means two cores' worth of work.
    pub cpu_cores: f64,
    /// True memory cost. Shared pages are divided among the processes mapping them,
    /// so this stays correct when summed across an app.
    pub mem_pss: u64,
    /// Resident memory. Summing this across processes double-counts every shared
    /// page and is kept only for comparison.
    pub mem_rss: u64,
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    /// Share of one GPU's engine time, 0.0–1.0.
    pub gpu_busy: f64,
    pub gpu_mem: u64,
    /// TCP throughput in bytes per second. Always an underestimate: UDP, and
    /// therefore most modern browser traffic, carries no per-socket counter.
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
    /// Bytes in this total that came from RSS because no PSS reading was
    /// available. Kept as a quantity rather than a flag: with hundreds of
    /// processes on a machine, *some* process always misses the sampling window,
    /// and a boolean would mark every total approximate forever — which trains
    /// the reader to ignore the mark entirely.
    pub mem_unmeasured: u64,
}

impl Usage {
    pub fn of(p: &ProcSample) -> Self {
        // PSS is sampled less often than CPU because it is far more expensive. When
        // it is missing, RSS stands in and the total is flagged as estimated.
        let (pss, unmeasured) = match p.mem {
            Some(m) => (m.pss_bytes, 0),
            None => (p.rss_bytes, p.rss_bytes),
        };
        Usage {
            cpu_cores: p.cpu_cores.unwrap_or(0.0),
            mem_pss: pss,
            mem_rss: p.rss_bytes,
            disk_read_bps: p.disk_read_bps.unwrap_or(0.0),
            disk_write_bps: p.disk_write_bps.unwrap_or(0.0),
            gpu_busy: p.gpu_busy.unwrap_or(0.0),
            gpu_mem: p.gpu_mem_bytes,
            net_rx_bps: p.net_rx_bps.unwrap_or(0.0),
            net_tx_bps: p.net_tx_bps.unwrap_or(0.0),
            mem_unmeasured: unmeasured,
        }
    }

    /// Whether enough of this total is substituted for the figure to deserve a
    /// qualifier. Under a twentieth, the error is smaller than the rounding.
    pub fn is_approximate(&self) -> bool {
        self.mem_unmeasured * 20 > self.mem_pss
    }

    /// Share of the whole machine, the unit this tool reports by default.
    pub fn cpu_percent_of_machine(&self) -> f64 {
        self.cpu_cores / conf::cpu_count() as f64 * 100.0
    }

    /// Share of a single core — the `top` convention, where 100% means one core and
    /// values above it are normal.
    pub fn cpu_percent_of_core(&self) -> f64 {
        self.cpu_cores * 100.0
    }

    /// Total network throughput, both directions.
    pub fn net_bps(&self) -> f64 {
        self.net_rx_bps + self.net_tx_bps
    }

    /// How much of the reported memory is double-counted by tools that sum RSS.
    pub fn rss_overcount(&self) -> u64 {
        self.mem_rss.saturating_sub(self.mem_pss)
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Self) {
        self.cpu_cores += o.cpu_cores;
        self.mem_pss += o.mem_pss;
        self.mem_rss += o.mem_rss;
        self.disk_read_bps += o.disk_read_bps;
        self.disk_write_bps += o.disk_write_bps;
        self.gpu_busy += o.gpu_busy;
        self.gpu_mem += o.gpu_mem;
        self.net_rx_bps += o.net_rx_bps;
        self.net_tx_bps += o.net_tx_bps;
        self.mem_unmeasured += o.mem_unmeasured;
    }
}

impl std::iter::Sum for Usage {
    fn sum<I: Iterator<Item = Usage>>(iter: I) -> Usage {
        iter.fold(Usage::default(), |mut acc, u| {
            acc += u;
            acc
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_add_up_and_carry_the_estimate_flag() {
        let mut a = Usage { cpu_cores: 1.0, mem_pss: 100, mem_rss: 300, ..Default::default() };
        let b = Usage {
            cpu_cores: 0.5,
            mem_pss: 50,
            mem_rss: 200,
            mem_unmeasured: 50,
            ..Default::default()
        };
        a += b;
        assert_eq!(a.cpu_cores, 1.5);
        assert_eq!(a.mem_pss, 150);
        assert_eq!(a.rss_overcount(), 350);
        // A third of this total is substituted, so it is worth qualifying.
        assert!(a.is_approximate());
    }

    #[test]
    fn a_trivial_substitution_does_not_flag_the_whole_total() {
        // The common case: a few tiny processes missed the sampling window.
        let u = Usage { mem_pss: 1_000_000, mem_unmeasured: 2_000, ..Default::default() };
        assert!(!u.is_approximate());
    }
}
