//! cpufreq `scaling_min_freq` floor on the non-little clusters, driven by the
//! adaptive `uclamp_min` level. uclamp alone loses the clock each time the
//! render thread sleeps on the GPU, and can't reach the kernel workers.

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::uclamp::read_capacities;

const POLICY_DIR: &str = "/sys/devices/system/cpu/cpufreq";

fn floor_khz(min: u64, max: u64, level: u32, levels: u32) -> u64 {
    min + max.saturating_sub(min) * u64::from(level) / u64::from(levels.max(1))
}

fn read_khz(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

struct Policy {
    min_freq: PathBuf,
    min: u64,
    max: u64,
}

pub struct CpuBoost {
    policies: Vec<Policy>,
    level: u32,
}

impl CpuBoost {
    /// `enabled` is false unless the uclamp floor adapts.
    pub fn new(enabled: bool) -> Self {
        let mut policies = Vec::new();
        let caps = read_capacities();
        let little = caps.iter().map(|(_, c)| *c).min().unwrap_or(0);
        let dirs = std::fs::read_dir(POLICY_DIR)
            .into_iter()
            .flatten()
            .flatten();
        for dir in dirs.map(|e| e.path()).filter(|_| enabled) {
            let cpu = std::fs::read_to_string(dir.join("related_cpus"))
                .ok()
                .and_then(|s| s.split_whitespace().next()?.parse::<u32>().ok());
            let cap = caps.iter().find(|(c, _)| Some(*c) == cpu).map(|(_, v)| *v);
            if cap.is_none_or(|c| c <= little) {
                continue;
            }
            let (Some(min), Some(max)) = (
                read_khz(&dir.join("cpuinfo_min_freq")),
                read_khz(&dir.join("cpuinfo_max_freq")),
            ) else {
                continue;
            };
            let min_freq = dir.join("scaling_min_freq");
            // Also clears a floor left by a killed predecessor.
            // ponytail: resets a minimum the admin set, and a crash mid-frame
            // leaves ours until the next start. Remember theirs if anyone does.
            if let Err(e) = std::fs::write(&min_freq, min.to_string()) {
                warn!(%e, path = %min_freq.display(), "cpu boost: scaling_min_freq not writable; disabled");
                policies.clear();
                break;
            }
            policies.push(Policy { min_freq, min, max });
        }
        if policies.is_empty() {
            debug!("cpu clock floor disabled");
        } else {
            info!(
                clusters = policies.len(),
                "cpu clock floor follows the uclamp level"
            );
        }
        Self { policies, level: 0 }
    }

    /// `level` of `levels` steps from each cluster's lowest clock to its top.
    pub fn set(&mut self, level: u32, levels: u32) {
        if level == self.level {
            return;
        }
        self.level = level;
        for p in &self.policies {
            let khz = floor_khz(p.min, p.max, level, levels);
            if let Err(e) = std::fs::write(&p.min_freq, khz.to_string()) {
                warn!(%e, "cpu boost: writing scaling_min_freq failed");
            }
        }
    }
}

impl Drop for CpuBoost {
    fn drop(&mut self) {
        self.set(0, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_spans_lowest_to_top_clock() {
        // Pixel 9 Pro mid cluster.
        assert_eq!(floor_khz(357_000, 2_600_000, 0, 4), 357_000);
        assert_eq!(floor_khz(357_000, 2_600_000, 2, 4), 1_478_500);
        assert_eq!(floor_khz(357_000, 2_600_000, 4, 4), 2_600_000);
    }
}
