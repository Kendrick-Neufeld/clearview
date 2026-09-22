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
    /// Set when any contributing process had no PSS reading and its RSS was
    /// substituted, so the interface can mark the total as approximate rather than
    /// quietly presenting a guess as a measurement.
    pub mem_estimated: bool,
}

impl Usage {
    pub fn of(p: &ProcSample) -> Self {
        // PSS is sampled less often than CPU because it is far more expensive. When
        // it is missing, RSS stands in and the total is flagged as estimated.
        let (pss, estimated) = match p.mem {
            Some(m) => (m.pss_bytes, false),
            None => (p.rss_bytes, true),
        };
        Usage {
            cpu_cores: p.cpu_cores.unwrap_or(0.0),
            mem_pss: pss,
            mem_rss: p.rss_bytes,
            disk_read_bps: p.disk_read_bps.unwrap_or(0.0),
            disk_write_bps: p.disk_write_bps.unwrap_or(0.0),
            mem_estimated: estimated,
        }
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
        self.mem_estimated |= o.mem_estimated;
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
            mem_estimated: true,
            ..Default::default()
        };
        a += b;
        assert_eq!(a.cpu_cores, 1.5);
        assert_eq!(a.mem_pss, 150);
        // One approximate member makes the whole total approximate.
        assert!(a.mem_estimated);
        assert_eq!(a.rss_overcount(), 350);
    }
}
