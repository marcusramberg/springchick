//! Scheduler `util_min` floor for the render thread, held only while drawing.
//!
//! Idle decays the thread onto the little cluster at a low OPP, and short
//! interactions finish before schedutil ramps. On the FP5 at 90Hz, first frame
//! after idle: 11.78ms (6/6 over budget) without, 8.88ms (0/6) with 450.
//! No privilege needed to raise `util_min` on our own task.

use std::time::{Duration, Instant};

use sc_config::UclampMin;
use tracing::{debug, info, warn};

/// Interactions come in bursts; dropping between them pays the ramp again.
const RELEASE_AFTER: Duration = Duration::from_millis(400);

const CPU_DIR: &str = "/sys/devices/system/cpu";

/// Just above the little cluster's capacity, where the balancer migrates the
/// task. `None` when all CPUs are equal.
pub fn derive_floor(capacities: &[u32]) -> Option<u32> {
    let min = *capacities.iter().filter(|c| **c > 0).min()?;
    let max = *capacities.iter().max()?;
    if min >= max {
        return None;
    }
    // ~12% over the little cluster. FP5 (382) → 429; 400 already moved the
    // thread, 800 bought nothing.
    let floor = min.saturating_add((min / 8).max(1));
    Some(floor.min(1024))
}

/// `(cpu index, cpu_capacity)`; also used by [`crate::resources`].
pub fn read_capacities() -> Vec<(u32, u32)> {
    let Ok(entries) = std::fs::read_dir(CPU_DIR) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(cpu) = name[3..].parse::<u32>() else {
            continue;
        };
        if let Ok(s) = std::fs::read_to_string(e.path().join("cpu_capacity")) {
            if let Ok(v) = s.trim().parse::<u32>() {
                out.push((cpu, v));
            }
        }
    }
    out
}

pub struct Uclamp {
    /// `None` disables the mechanism.
    floor: Option<u32>,
    applied: bool,
    last_active: Option<Instant>,
    /// Set once the kernel refuses (no `CONFIG_UCLAMP_TASK`), to warn only once.
    broken: bool,
}

impl Uclamp {
    pub fn new(cfg: UclampMin) -> Self {
        let floor = match cfg {
            UclampMin::Off => None,
            UclampMin::Fixed(v) => Some(v.min(1024)),
            UclampMin::Auto => {
                let caps: Vec<u32> = read_capacities().into_iter().map(|(_, c)| c).collect();
                let derived = derive_floor(&caps);
                if derived.is_none() {
                    debug!(
                        cpus = caps.len(),
                        "uclamp auto: no capacity asymmetry, leaving the scheduler alone"
                    );
                }
                derived
            }
        };
        match floor {
            Some(v) => info!(util_min = v, "uclamp floor active while rendering"),
            None => debug!("uclamp floor disabled"),
        }
        Self {
            floor,
            applied: false,
            last_active: None,
            broken: false,
        }
    }

    /// Call before rendering, so the first frame of a touch benefits.
    pub fn update(&mut self, drawing: bool, now: Instant) {
        let Some(floor) = self.floor else { return };
        if self.broken {
            return;
        }
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
        let value = if want { floor } else { 0 };
        match set_util_min(value) {
            Ok(()) => self.applied = want,
            Err(e) => {
                warn!(%e, util_min = value, "uclamp: sched_setattr failed; disabling");
                self.broken = true;
            }
        }
    }
}

/// Not in libc.
#[repr(C)]
#[derive(Default)]
struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
    sched_util_min: u32,
    sched_util_max: u32,
}

// Without KEEP_POLICY/KEEP_PARAMS the zeroed fields would apply as
// SCHED_OTHER, nice 0.
const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;

fn set_util_min(value: u32) -> std::io::Result<()> {
    let mut attr = SchedAttr {
        size: std::mem::size_of::<SchedAttr>() as u32,
        sched_flags: SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS | SCHED_FLAG_UTIL_CLAMP_MIN,
        sched_util_min: value,
        ..Default::default()
    };
    // SAFETY: `attr` is a live, correctly sized `sched_attr`; pid 0 is the
    // calling thread; the kernel only reads it.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_sched_setattr,
            0,
            &mut attr as *mut SchedAttr,
            0u32,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_just_above_the_little_cluster() {
        let fp5 = [382, 382, 382, 382, 889, 889, 889, 1024];
        let floor = derive_floor(&fp5).unwrap();
        assert!(floor > 382, "must clear the little cluster, got {floor}");
        assert!(floor < 889, "must not reach the mid cluster, got {floor}");
    }

    #[test]
    fn two_cluster_split_also_clears_the_knee() {
        let caps = [400, 400, 400, 400, 1024, 1024, 1024, 1024];
        let floor = derive_floor(&caps).unwrap();
        assert!(floor > 400 && floor < 1024, "got {floor}");
    }

    #[test]
    fn homogeneous_cpus_get_no_floor() {
        assert_eq!(derive_floor(&[1024, 1024, 1024, 1024]), None);
    }

    #[test]
    fn no_capacity_information_yields_no_floor() {
        assert_eq!(derive_floor(&[]), None);
    }

    #[test]
    fn floor_never_exceeds_the_scale() {
        assert!(derive_floor(&[1000, 1024]).unwrap() <= 1024);
    }

    #[test]
    fn off_disables_entirely() {
        let mut u = Uclamp::new(UclampMin::Off);
        assert!(u.floor.is_none());
        u.update(true, Instant::now());
        assert!(!u.applied);
    }

    #[test]
    fn fixed_is_clamped_to_the_scale() {
        assert_eq!(Uclamp::new(UclampMin::Fixed(5000)).floor, Some(1024));
    }

    #[test]
    fn holds_the_floor_briefly_after_drawing_stops() {
        let mut u = Uclamp::new(UclampMin::Fixed(450));
        if u.broken {
            return; // kernel without uclamp support
        }
        let t0 = Instant::now();
        u.update(true, t0);
        assert!(u.applied, "floor applies while drawing");
        u.update(false, t0 + Duration::from_millis(100));
        assert!(u.applied, "still held inside the release delay");
        u.update(false, t0 + RELEASE_AFTER + Duration::from_millis(1));
        assert!(!u.applied, "released once the delay elapses");
    }
}
