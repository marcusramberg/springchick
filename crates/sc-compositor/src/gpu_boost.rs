//! GPU devfreq `min_freq` floor, held only while drawing. simple_ondemand
//! starts a gesture at the lowest OPP (FP5: 315 of 812 MHz) and ramps after
//! the first frames have missed. Thermal limits are a PM QoS maximum, which
//! devfreq clamps this minimum to, so the floor can't override throttling.

use std::path::PathBuf;
use std::time::Instant;

use sc_config::GpuMinMhz;
use tracing::{debug, info, warn};

use crate::uclamp::RELEASE_AFTER;

/// Lowest OPP at or above half the top one. FP5 → 450 MHz.
pub fn derive_floor(available_hz: &[u64]) -> Option<u64> {
    let max = *available_hz.iter().max()?;
    available_hz.iter().copied().filter(|f| *f >= max / 2).min()
}

fn find_gpu() -> Option<PathBuf> {
    std::fs::read_dir("/sys/class/devfreq")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().contains("gpu"))
        })
}

pub struct GpuBoost {
    /// `None` disables the mechanism.
    min_freq: Option<(PathBuf, u64)>,
    applied: bool,
    last_active: Option<Instant>,
}

impl GpuBoost {
    pub fn new(cfg: GpuMinMhz) -> Self {
        let min_freq = (|| {
            let dir = find_gpu()?;
            let hz = match cfg {
                GpuMinMhz::Off => return None,
                GpuMinMhz::Fixed(mhz) => mhz as u64 * 1_000_000,
                GpuMinMhz::Auto => {
                    let avail = std::fs::read_to_string(dir.join("available_frequencies")).ok()?;
                    let avail: Vec<u64> = avail
                        .split_whitespace()
                        .filter_map(|f| f.parse().ok())
                        .collect();
                    derive_floor(&avail)?
                }
            };
            let path = dir.join("min_freq");
            // Also clears a floor left by a killed predecessor.
            if let Err(e) = std::fs::write(&path, "0") {
                warn!(%e, path = %path.display(), "gpu boost: min_freq not writable; disabled");
                return None;
            }
            Some((path, hz))
        })();
        match &min_freq {
            Some((p, hz)) => {
                info!(mhz = hz / 1_000_000, path = %p.display(), "gpu floor active while rendering")
            }
            None => debug!("gpu floor disabled"),
        }
        Self {
            min_freq,
            applied: false,
            last_active: None,
        }
    }

    /// Same hysteresis as [`crate::uclamp::Uclamp::update`].
    pub fn update(&mut self, drawing: bool, now: Instant) {
        let Some((path, hz)) = &self.min_freq else {
            return;
        };
        if drawing {
            self.last_active = Some(now);
        }
        let want = drawing
            || self
                .last_active
                .is_some_and(|t| now.duration_since(t) < RELEASE_AFTER);
        if want == self.applied {
            return;
        }
        // 0 is the device's lowest, whatever its OPP table says.
        let value = if want { *hz } else { 0 };
        match std::fs::write(path, value.to_string()) {
            Ok(()) => self.applied = want,
            Err(e) => {
                warn!(%e, "gpu boost: writing min_freq failed; disabling");
                self.min_freq = None;
            }
        }
    }
}

impl Drop for GpuBoost {
    fn drop(&mut self) {
        if let (Some((path, _)), true) = (&self.min_freq, self.applied) {
            let _ = std::fs::write(path, "0");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fp5_floor_is_450() {
        let fp5 = [315, 450, 550, 608, 700, 812].map(|m| m * 1_000_000u64);
        assert_eq!(derive_floor(&fp5), Some(450_000_000));
    }

    #[test]
    fn no_table_no_floor() {
        assert_eq!(derive_floor(&[]), None);
    }
}
