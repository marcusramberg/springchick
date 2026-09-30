//! Per-app resource tiers: focused app vs. the ones behind it. Each app has
//! its own systemd scope ([`crate::launcher`]), so tiering is a property change
//! on that cgroup and never moves processes.
//!
//! The unit comes from the client's pid, not our scope name: flatpak moves the
//! app into its own scope and ours is collected empty.

use sc_config::Resources;
use std::process::{Child, Command};
use std::sync::OnceLock;
use tracing::{debug, info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Foreground,
    Background,
}

/// Only scopes are tiered. The shell's helpers are services (dms, wvkbd,
/// portals); some map toplevels, and they must never be throttled.
pub fn unit_from_cgroup(body: &str) -> Option<AppCgroup> {
    // Take the v2 line; hybrid systems also list v1 lines.
    let path = body.lines().find_map(|l| l.strip_prefix("0::"))?;
    let leaf = path.rsplit('/').next()?;
    leaf.ends_with(".scope").then(|| AppCgroup {
        unit: leaf.to_string(),
        path: path.to_string(),
    })
}

/// The path says whether a controller reached it ([`cpuset_available`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppCgroup {
    pub unit: String,
    pub path: String,
}

pub fn unit_of_pid(pid: i32) -> Option<AppCgroup> {
    unit_from_cgroup(&std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?)
}

/// CPUs at the lowest `cpu_capacity`; `None` when all are equal. Derived: the
/// FP5 is 4x382 + 3x889 + 1x1024, so a hardcoded mask won't port.
pub fn little_cluster(caps: &[(u32, u32)]) -> Option<Vec<u32>> {
    let min = caps.iter().map(|(_, c)| *c).filter(|c| *c > 0).min()?;
    let max = caps.iter().map(|(_, c)| *c).max()?;
    if min >= max {
        return None;
    }
    let mut cpus: Vec<u32> = caps
        .iter()
        .filter(|(_, c)| *c == min)
        .map(|(cpu, _)| *cpu)
        .collect();
    cpus.sort_unstable();
    Some(cpus)
}

pub fn cpu_ranges(cpus: &[u32]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < cpus.len() {
        let start = cpus[i];
        let mut end = start;
        while i + 1 < cpus.len() && cpus[i + 1] == end + 1 {
            i += 1;
            end = cpus[i];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}-{end}"));
        }
        i += 1;
    }
    out
}

/// `"auto"` derives the little cluster, `"off"` disables, else a CPU list.
pub fn resolve_allowed_cpus(cfg: &str) -> Option<String> {
    match cfg {
        "off" => None,
        "auto" => {
            let caps = crate::uclamp::read_capacities();
            let cpus = little_cluster(&caps);
            if cpus.is_none() {
                debug!(
                    cpus = caps.len(),
                    "bg_allowed_cpus auto: no capacity asymmetry, not pinning"
                );
            }
            cpus.map(|c| cpu_ranges(&c))
        }
        explicit => Some(explicit.to_string()),
    }
}

/// `user@.service` delegates only `pids memory cpu` without the drop-in in
/// `nix/module.nix`, and then every `AllowedCPUs=` is refused.
fn cpuset_available(path: &str) -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let controllers =
            std::fs::read_to_string(format!("/sys/fs/cgroup{path}/cgroup.controllers"))
                .unwrap_or_default();
        let ok = controllers.split_whitespace().any(|c| c == "cpuset");
        if !ok {
            info!(
                %controllers,
                "cpuset not delegated to the user manager; not pinning background apps"
            );
        }
        ok
    })
}

/// `CPUQuota=` rejects `infinity`; empty means no limit.
fn clear_infinity(quota: &str) -> &str {
    if quota.eq_ignore_ascii_case("infinity") {
        ""
    } else {
        quota
    }
}

/// Every property is set in both tiers: promotion must clear the demotion's
/// values.
fn args(unit: &str, tier: Tier, res: &Resources) -> Vec<String> {
    let (weight, high, quota) = match tier {
        Tier::Foreground => (
            res.fg_cpu_weight,
            "infinity".to_string(),
            "infinity".to_string(),
        ),
        Tier::Background => (
            res.bg_cpu_weight,
            res.bg_memory_high.clone(),
            res.bg_cpu_quota.clone(),
        ),
    };
    vec![
        "--user".into(),
        "--quiet".into(),
        // A tier describes this session's focus; don't outlive it.
        "--runtime".into(),
        "set-property".into(),
        unit.into(),
        format!("CPUWeight={weight}"),
        format!("MemoryHigh={high}"),
        // CPUWeight is proportional and does nothing for an app spinning alone on an
        // idle phone; the quota is what saves power. It must be empty, not
        // `infinity`: set-property is all-or-nothing and would drop the weight too.
        format!("CPUQuota={}", clear_infinity(&quota)),
    ]
}

/// Not waited on: it's a D-Bus round-trip and this is the render thread.
/// Pinning is a separate call because set-property is all-or-nothing and a
/// refused `AllowedCPUs=` would drop the weight and quota with it.
pub fn apply(app: &AppCgroup, tier: Tier, res: &Resources, cpus: Option<&str>) -> Vec<Child> {
    let unit = app.unit.as_str();
    debug!(unit, ?tier, "applying resource tier");
    let mut children = Vec::new();
    children.extend(spawn(args(unit, tier, res), unit));

    // Clear pinning on promotion even when not pinning now: a reload may have
    // turned `bg_allowed_cpus` off after the demotion.
    let pin = match tier {
        Tier::Foreground => Some(String::new()),
        Tier::Background => cpus.map(|c| c.to_string()),
    };
    if let Some(pin) = pin {
        if cpuset_available(&app.path) {
            children.extend(spawn(
                vec![
                    "--user".into(),
                    "--quiet".into(),
                    "--runtime".into(),
                    "set-property".into(),
                    unit.into(),
                    format!("AllowedCPUs={pin}"),
                ],
                unit,
            ));
        }
    }
    children
}

fn spawn(args: Vec<String>, unit: &str) -> Option<Child> {
    match Command::new("systemctl").args(args).spawn() {
        Ok(child) => Some(child),
        Err(e) => {
            warn!(%e, unit, "failed to run systemctl set-property");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V2: &str =
        "0::/user.slice/user-1000.slice/user@1000.service/app.slice/springchick-42-0-foot.scope\n";

    #[test]
    fn reads_scope_from_cgroup_v2_line() {
        let app = unit_from_cgroup(V2).expect("a scope");
        assert_eq!(app.unit, "springchick-42-0-foot.scope");
        assert!(app.path.starts_with("/user.slice/"));
    }

    #[test]
    fn reads_a_scope_we_did_not_create() {
        let body = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-org.gnome.Fractal.Devel-3308067827.scope\n";
        assert_eq!(
            unit_from_cgroup(body).map(|a| a.unit).as_deref(),
            Some("app-flatpak-org.gnome.Fractal.Devel-3308067827.scope")
        );
    }

    #[test]
    fn services_are_not_tiered() {
        let body = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/wvkbd.service\n";
        assert_eq!(unit_from_cgroup(body), None);
    }

    #[test]
    fn ignores_cgroup_v1_lines() {
        let body = "1:name=systemd:/user.slice/whatever.scope\n";
        assert_eq!(unit_from_cgroup(body), None);
    }

    #[test]
    fn empty_cgroup_body_is_none() {
        assert_eq!(unit_from_cgroup(""), None);
    }

    #[test]
    fn foreground_clears_the_background_ceiling() {
        let res = Resources {
            enable: true,
            fg_cpu_weight: 100,
            bg_cpu_weight: 20,
            bg_memory_high: "512M".into(),
            bg_cpu_quota: "30%".into(),
            bg_allowed_cpus: "auto".into(),
        };
        let fg = args("a.scope", Tier::Foreground, &res);
        assert!(fg.contains(&"CPUWeight=100".to_string()));
        assert!(fg.contains(&"MemoryHigh=infinity".to_string()));
        assert!(fg.contains(&"CPUQuota=".to_string()));
        let bg = args("a.scope", Tier::Background, &res);
        assert!(bg.contains(&"CPUWeight=20".to_string()));
        assert!(bg.contains(&"MemoryHigh=512M".to_string()));
        assert!(bg.contains(&"CPUQuota=30%".to_string()));
        assert!(bg.contains(&"--runtime".to_string()));
    }

    #[test]
    fn little_cluster_on_a_three_tier_phone() {
        let caps = [
            (0, 382),
            (1, 382),
            (2, 382),
            (3, 382),
            (4, 889),
            (5, 889),
            (6, 889),
            (7, 1024),
        ];
        assert_eq!(little_cluster(&caps), Some(vec![0, 1, 2, 3]));
        assert_eq!(cpu_ranges(&little_cluster(&caps).unwrap()), "0-3");
    }

    #[test]
    fn no_little_cluster_without_asymmetry() {
        assert_eq!(little_cluster(&[(0, 1024), (1, 1024)]), None);
        assert_eq!(little_cluster(&[]), None);
    }

    #[test]
    fn little_cluster_handles_scattered_numbering() {
        let caps = [(0, 1024), (1, 400), (2, 1024), (3, 400), (4, 400)];
        assert_eq!(little_cluster(&caps), Some(vec![1, 3, 4]));
        assert_eq!(cpu_ranges(&[1, 3, 4]), "1,3-4");
    }

    #[test]
    fn cpu_ranges_formats_singles_and_runs() {
        assert_eq!(cpu_ranges(&[0]), "0");
        assert_eq!(cpu_ranges(&[0, 1, 2, 3]), "0-3");
        assert_eq!(cpu_ranges(&[0, 2, 4]), "0,2,4");
        assert_eq!(cpu_ranges(&[0, 1, 4, 5, 7]), "0-1,4-5,7");
        assert_eq!(cpu_ranges(&[]), "");
    }

    #[test]
    fn allowed_cpus_off_disables_pinning() {
        assert_eq!(resolve_allowed_cpus("off"), None);
        assert_eq!(resolve_allowed_cpus("0-3"), Some("0-3".to_string()));
    }

    #[test]
    fn background_quota_of_infinity_is_sent_empty() {
        let res = Resources {
            bg_cpu_quota: "infinity".into(),
            ..Resources::default()
        };
        let bg = args("a.scope", Tier::Background, &res);
        assert!(bg.contains(&"CPUQuota=".to_string()));
    }
}
