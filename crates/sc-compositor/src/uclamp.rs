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
pub(crate) const RELEASE_AFTER: Duration = Duration::from_millis(400);

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

/// Steps from the placement floor to the top of the scale.
const LEVELS: u32 = 4;
/// Frames judged together. Not a burst: a session can draw without pause.
const WINDOW: u32 = 60;
/// Missed frames in a window that raise the floor a level. Every phone
/// drops the odd frame at any clock.
const MISSES_TO_RAISE: u32 = WINDOW / 5;
const MAX_CALM_WINDOWS: u32 = 256;

/// `auto` only. The placement floor moves the thread off the little cluster.
/// Where frames still miss it climbs, and is tried one level lower after
/// windows where nine frames in ten had half the budget to spare, doubling
/// the wait each time that fails.
// ponytail: a frame that is slow for another reason (GPU-bound, a stalled
// client) climbs to 1024, cluster clocks with it, for nothing; split CPU from
// GPU time if that shows.
#[derive(Debug)]
struct Adaptive {
    base: u32,
    level: u32,
    frames: u32,
    misses: u32,
    roomy: u32,
    calm_windows: u32,
    calm_needed: u32,
    /// The last change was a step down that no window has confirmed yet.
    probing: bool,
}

impl Adaptive {
    fn new(base: u32) -> Self {
        Self {
            base,
            level: 0,
            frames: 0,
            misses: 0,
            roomy: 0,
            calm_windows: 0,
            calm_needed: 1,
            probing: false,
        }
    }

    fn floor(&self) -> u32 {
        self.base + (1024u32.saturating_sub(self.base)) * self.level / LEVELS
    }

    /// True when the floor moved.
    fn observe(&mut self, dt: Duration, budget: Duration) -> bool {
        self.frames += 1;
        self.roomy += u32::from(dt * 2 < budget);
        // A frame this close to the budget loses its vblank to the flip's own
        // latency.
        self.misses += u32::from(dt * 20 > budget * 17);
        let was = self.level;
        if self.misses >= MISSES_TO_RAISE && self.level < LEVELS {
            self.level += 1;
            self.calm_windows = 0;
            if std::mem::take(&mut self.probing) {
                self.calm_needed = (self.calm_needed * 2).min(MAX_CALM_WINDOWS);
            }
        } else if self.frames >= WINDOW {
            let calm = self.roomy * 10 >= WINDOW * 9 && self.misses * 20 <= WINDOW;
            self.probing = false;
            self.calm_windows = if calm { self.calm_windows + 1 } else { 0 };
            if self.calm_windows >= self.calm_needed && self.level > 0 {
                self.level -= 1;
                self.calm_windows = 0;
                self.probing = true;
            }
        } else {
            return false;
        }
        (self.frames, self.misses, self.roomy) = (0, 0, 0);
        self.level != was
    }
}

pub struct Uclamp {
    /// `None` disables the mechanism.
    floor: Option<u32>,
    adaptive: Option<Adaptive>,
    /// The `util_min` in force.
    applied: Option<u32>,
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
            adaptive: floor.filter(|_| cfg == UclampMin::Auto).map(Adaptive::new),
            applied: None,
            last_active: None,
            broken: false,
        }
    }

    /// A frame's render time against the refresh interval.
    pub fn observe(&mut self, dt: Duration, budget: Duration) {
        let Some(a) = &mut self.adaptive else { return };
        if a.observe(dt, budget) {
            debug!(util_min = a.floor(), "uclamp auto: floor moved");
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
        let want = want.then(|| self.adaptive.as_ref().map_or(floor, Adaptive::floor));
        if want == self.applied {
            return;
        }
        let value = want.unwrap_or(0);
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
        assert!(u.applied.is_none());
    }

    #[test]
    fn fixed_is_clamped_to_the_scale() {
        assert_eq!(Uclamp::new(UclampMin::Fixed(5000)).floor, Some(1024));
    }

    #[test]
    fn holds_the_floor_briefly_after_drawing_stops() {
        let mut u = Uclamp::new(UclampMin::Fixed(450));
        let t0 = Instant::now();
        u.update(true, t0);
        if u.broken {
            return; // kernel without uclamp support (nix sandbox)
        }
        assert_eq!(u.applied, Some(450), "floor applies while drawing");
        u.update(false, t0 + Duration::from_millis(100));
        assert!(u.applied.is_some(), "still held inside the release delay");
        u.update(false, t0 + RELEASE_AFTER + Duration::from_millis(1));
        assert!(u.applied.is_none(), "released once the delay elapses");
    }

    const BUDGET: Duration = Duration::from_micros(8_333);

    fn feed(a: &mut Adaptive, frames: u32, dt_us: u64) {
        for _ in 0..frames {
            a.observe(Duration::from_micros(dt_us), BUDGET);
        }
    }

    #[test]
    fn fixed_floor_never_adapts() {
        assert!(Uclamp::new(UclampMin::Fixed(450)).adaptive.is_none());
    }

    #[test]
    fn frames_inside_the_budget_leave_the_placement_floor() {
        let mut a = Adaptive::new(316);
        feed(&mut a, 600, 7_000);
        assert_eq!(a.floor(), 316);
    }

    #[test]
    fn missed_frames_climb_to_the_top_of_the_scale() {
        let mut a = Adaptive::new(316);
        feed(&mut a, MISSES_TO_RAISE - 1, 15_000);
        assert_eq!(a.level, 0);
        assert!(a.observe(Duration::from_millis(15), BUDGET));
        assert_eq!(a.floor(), 493);
        feed(&mut a, 100, 15_000);
        assert_eq!(a.floor(), 1024);
    }

    #[test]
    fn frames_just_inside_the_budget_still_count_as_misses() {
        let mut a = Adaptive::new(316);
        feed(&mut a, WINDOW, 7_800);
        assert!(a.level >= 1);
    }

    #[test]
    fn a_tight_fit_holds_the_raised_floor() {
        let mut a = Adaptive::new(316);
        feed(&mut a, MISSES_TO_RAISE, 15_000);
        feed(&mut a, 600, 7_000);
        assert_eq!(a.level, 1);
    }

    #[test]
    fn a_failed_step_down_is_retried_less_often() {
        let mut a = Adaptive::new(316);
        feed(&mut a, MISSES_TO_RAISE, 15_000);
        feed(&mut a, WINDOW, 3_000);
        assert_eq!(a.level, 0, "half the budget to spare: try lower");
        feed(&mut a, MISSES_TO_RAISE, 15_000);
        assert_eq!(a.level, 1);
        feed(&mut a, WINDOW, 3_000);
        assert_eq!(a.level, 1, "one calm window is no longer enough");
        feed(&mut a, WINDOW, 3_000);
        assert_eq!(a.level, 0);
    }

    /// FP5 at full clock: 1.4% of frames over, the rest far inside.
    #[test]
    fn the_odd_dropped_frame_neither_raises_nor_blocks_a_step_down() {
        let budget = Duration::from_micros(11_111);
        let mut a = Adaptive::new(429);
        feed(&mut a, MISSES_TO_RAISE, 15_000);
        assert_eq!(a.level, 1);
        for i in 0..WINDOW {
            let us = if i == 7 { 12_000 } else { 1_000 };
            a.observe(Duration::from_micros(us), budget);
        }
        assert_eq!(a.level, 0);
        for i in 0..700 {
            let us = if i % 70 == 0 { 12_000 } else { 9_000 };
            a.observe(Duration::from_micros(us), budget);
        }
        assert_eq!(a.level, 0);
    }
}
