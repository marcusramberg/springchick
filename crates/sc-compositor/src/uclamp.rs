//! Scheduler utilization floor (`util_min`) for the render thread.
//!
//! On a big.LITTLE phone the render thread's tracked utilization decays while
//! the screen is idle, so schedutil parks it on the little cluster at a low OPP.
//! The next touch then renders its first frames there, and short interactions —
//! a tap, a flick — finish before the governor has ramped, so *every* frame in
//! them is slow, not just the first.
//!
//! Measured on the FP5 (little `cpu_capacity` 382, budget 11.11ms at 90Hz),
//! first frame after 12s idle over 6 trials each:
//!
//! | | no floor | `util_min` 450 |
//! |---|---|---|
//! | first frame | 11.78ms (6/6 over budget) | 8.88ms (0/6 over) |
//! | follow-up frames | 10.21ms | 4.01ms |
//!
//! The floor is applied only while the compositor is actually drawing and
//! dropped again shortly after it settles, so an idle phone is not holding a
//! big core awake. Raising `util_min` on a task you own needs no privilege.

use std::time::{Duration, Instant};

use sc_config::UclampMin;
use tracing::{debug, info, warn};

/// How long the floor stays applied after the last frame. Short interactions
/// arrive in bursts, and dropping the clamp between them would pay the ramp-up
/// cost again on the very next touch, which is the thing this exists to avoid.
const RELEASE_AFTER: Duration = Duration::from_millis(400);

/// Where the kernel exposes per-CPU capacity, used to find the migration knee.
const CPU_DIR: &str = "/sys/devices/system/cpu";

/// Pick a floor from the per-CPU `cpu_capacity` values.
///
/// The useful floor sits just above the little cluster's capacity: that is the
/// point where the load balancer stops considering a little core big enough and
/// migrates the task. Below it nothing changes; far above it only burns extra
/// frequency on the big core. Returns `None` when every CPU has the same
/// capacity, since there is no larger core to be moved to.
pub fn derive_floor(capacities: &[u32]) -> Option<u32> {
    let min = *capacities.iter().filter(|c| **c > 0).min()?;
    let max = *capacities.iter().max()?;
    if min >= max {
        return None;
    }
    // ~12% over the little cluster clears the knee without reaching for the top
    // of the big cluster's range. On the FP5 (382) this gives 429; measured, 400
    // was already enough to move the thread and 800 bought nothing further.
    let floor = min.saturating_add((min / 8).max(1));
    Some(floor.min(1024))
}

/// Read `cpu_capacity` for every CPU the kernel exposes.
fn read_capacities() -> Vec<u32> {
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
        if let Ok(s) = std::fs::read_to_string(e.path().join("cpu_capacity")) {
            if let Ok(v) = s.trim().parse::<u32>() {
                out.push(v);
            }
        }
    }
    out
}

/// Applies and releases the floor as the compositor starts and stops drawing.
pub struct Uclamp {
    /// The floor to apply while drawing. `None` disables the whole mechanism.
    floor: Option<u32>,
    /// Whether the floor is currently applied, so transitions only syscall once.
    applied: bool,
    /// When the compositor was last drawing, for the release delay.
    last_active: Option<Instant>,
    /// Set once the kernel refuses a request, so a kernel without
    /// `CONFIG_UCLAMP_TASK` produces one warning rather than one per frame.
    broken: bool,
}

impl Uclamp {
    /// Resolve the configured policy against this machine's topology.
    pub fn new(cfg: UclampMin) -> Self {
        let floor = match cfg {
            UclampMin::Off => None,
            UclampMin::Fixed(v) => Some(v.min(1024)),
            UclampMin::Auto => {
                let caps = read_capacities();
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

    /// Call once per loop iteration, before rendering, with whether the
    /// compositor is about to draw. Applying before the render is what lets the
    /// *first* frame of a touch benefit; applying afterwards would always be a
    /// frame late.
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

/// `sched_attr` as the kernel expects it. Not in libc, so it is spelled out
/// here; `size` is validated by the kernel against what it knows.
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

// Keep the existing policy, priority and nice; change only the utilization
// floor. Without KEEP_POLICY/KEEP_PARAMS the zeroed fields above would be
// applied as a real (SCHED_OTHER, nice 0) request.
const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;

/// Set `util_min` on the calling thread.
fn set_util_min(value: u32) -> std::io::Result<()> {
    let mut attr = SchedAttr {
        size: std::mem::size_of::<SchedAttr>() as u32,
        sched_flags: SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS | SCHED_FLAG_UTIL_CLAMP_MIN,
        sched_util_min: value,
        ..Default::default()
    };
    // SAFETY: `attr` is a live, correctly sized `sched_attr` owned by this
    // frame, and pid 0 addresses the calling thread. The kernel only reads
    // `size` bytes from the pointer and writes nothing back.
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
        // FP5: 4x382, 3x889, 1x1024. Measured knee is at 382.
        let fp5 = [382, 382, 382, 382, 889, 889, 889, 1024];
        let floor = derive_floor(&fp5).unwrap();
        assert!(floor > 382, "must clear the little cluster, got {floor}");
        assert!(floor < 889, "must not reach the mid cluster, got {floor}");
    }

    #[test]
    fn two_cluster_split_also_clears_the_knee() {
        // sdm845-style 4+4 split.
        let caps = [400, 400, 400, 400, 1024, 1024, 1024, 1024];
        let floor = derive_floor(&caps).unwrap();
        assert!(floor > 400 && floor < 1024, "got {floor}");
    }

    #[test]
    fn homogeneous_cpus_get_no_floor() {
        // Nothing bigger to migrate to, so clamping only raises frequency.
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
        // Must not syscall or change state.
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
            return; // kernel without uclamp support; nothing to assert
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
