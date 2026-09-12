//! Per-app resource tiers: what the focused app gets, and what the apps behind
//! it are squeezed to.
//!
//! Apps are launched into their own transient systemd scope (see
//! [`crate::launcher`]), so each one already owns a cgroup holding it and
//! everything it forked. Tiering is therefore only a property change on that
//! cgroup — no process is ever moved between tiers, which is what makes it safe
//! to do on a focus change: a forking app cannot leave half its children in the
//! wrong tier.
//!
//! The unit is resolved from the client's pid rather than from the scope name we
//! chose at launch, because an app may end up somewhere else entirely: a flatpak
//! `Exec` hands off to `flatpak-session-helper`, which registers its *own* scope
//! and moves the app there, leaving ours empty and collected.

use sc_config::Resources;
use std::process::{Child, Command};
use std::sync::OnceLock;
use tracing::{debug, info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Foreground,
    Background,
}

/// The leaf unit of a `/proc/<pid>/cgroup` body, if the process is in a scope.
///
/// Only scopes are tiered. Every app — ours (`springchick-*.scope`) and
/// anything that re-registered itself (`app-flatpak-*.scope`) — is in one, while
/// the shell's own long-running helpers are services: `dms.service`,
/// `wvkbd.service`, `xdg-desktop-portal-*.service`. Some of those do map
/// toplevels (a portal file chooser), and throttling the shell's own panel or
/// keyboard because a dialog lost focus is exactly the wrong outcome.
pub fn unit_from_cgroup(body: &str) -> Option<AppCgroup> {
    // The cgroup-v2 line is `0::/path`; v1 lines (`1:name=systemd:/path`) may
    // also be present on a hybrid system and are not what we want.
    let path = body.lines().find_map(|l| l.strip_prefix("0::"))?;
    let leaf = path.rsplit('/').next()?;
    leaf.ends_with(".scope").then(|| AppCgroup {
        unit: leaf.to_string(),
        path: path.to_string(),
    })
}

/// A tierable app: the scope unit to address it by, and the cgroup path it sits
/// at (which is what says whether a controller reached it — see
/// [`cpuset_available`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppCgroup {
    pub unit: String,
    pub path: String,
}

/// The scope `pid` runs in, or `None` when it is not in one (or is gone).
pub fn unit_of_pid(pid: i32) -> Option<AppCgroup> {
    unit_from_cgroup(&std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?)
}

/// The CPUs making up the efficiency cluster: those at the lowest
/// `cpu_capacity`. `None` when every CPU is the same size, where pinning would
/// only take cores away for nothing.
///
/// Derived rather than configured because the layout differs per phone — the
/// FP5 is not the 4+4 the name suggests but 4x382 + 3x889 + 1x1024, so any
/// hardcoded mask would be wrong on the next device.
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

/// Format a sorted CPU list the way `AllowedCPUs=` wants it: `0-3`, `0-1,4`.
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

/// Resolve `[resources].bg_allowed_cpus` against this machine.
///
/// `"auto"` derives the efficiency cluster, `"off"` disables pinning, anything
/// else is passed through as an explicit CPU list.
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

/// Whether the `cpuset` controller actually reached `path`.
///
/// systemd ships `Delegate=pids memory cpu` on `user@.service`, so on a system
/// without the drop-in `nix/module.nix` adds, `cpuset` never gets enabled down
/// the user tree and every `AllowedCPUs=` would be refused. Checked once: a
/// controller does not come and go mid-session.
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

/// `""` for "no limit", which is the only spelling `CPUQuota=` accepts.
fn clear_infinity(quota: &str) -> &str {
    if quota.eq_ignore_ascii_case("infinity") {
        ""
    } else {
        quota
    }
}

/// The `systemctl set-property` arguments putting `unit` in `tier`.
///
/// Both properties are set in both tiers: promoting an app has to *clear* the
/// ceiling the demotion put on it, and a property left unmentioned keeps its
/// old value.
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
        // Not persisted: a tier is a fact about this session's focus, and a
        // drop-in surviving a reboot would outlive the app it describes.
        "--runtime".into(),
        "set-property".into(),
        unit.into(),
        format!("CPUWeight={weight}"),
        format!("MemoryHigh={high}"),
        // The cap that actually saves power: CPUWeight is proportional, so it
        // does nothing for a background app spinning alone on an idle phone.
        //
        // `CPUQuota=` is the only one of the three that rejects `infinity`
        // ("Failed to parse CPUQuota= value") — an empty value is how it is
        // cleared. Since `set-property` applies all-or-nothing, getting this
        // wrong drops the CPUWeight in the same call.
        format!("CPUQuota={}", clear_infinity(&quota)),
    ]
}

/// Move `app` into `tier`, returning the `systemctl` children for the caller to
/// reap.
///
/// Deliberately not waited on: each call is a D-Bus round-trip to the user
/// manager and this runs on the render thread. Nothing depends on the result —
/// if it fails (the app exited, taking its scope with it) the tier simply does
/// not apply, and `systemctl` says so in the journal.
///
/// The CPU pinning is a *second* call rather than two more properties on the
/// first. `set-property` is all-or-nothing, so one refused `AllowedCPUs=` — a
/// kernel without cpuset, a user manager without the delegation — would
/// otherwise silently drop the weight and quota that do work.
pub fn apply(app: &AppCgroup, tier: Tier, res: &Resources, cpus: Option<&str>) -> Vec<Child> {
    let unit = app.unit.as_str();
    debug!(unit, ?tier, "applying resource tier");
    let mut children = Vec::new();
    children.extend(spawn(args(unit, tier, res), unit));

    // Pinning is background-only, and clearing it on promotion has to happen
    // whether or not this session is pinning at all: the app may have been
    // demoted before a reload turned `bg_allowed_cpus` off.
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

    /// A flatpak app is in the scope flatpak made for it, not the one we did.
    #[test]
    fn reads_a_scope_we_did_not_create() {
        let body = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-org.gnome.Fractal.Devel-3308067827.scope\n";
        assert_eq!(
            unit_from_cgroup(body).map(|a| a.unit).as_deref(),
            Some("app-flatpak-org.gnome.Fractal.Devel-3308067827.scope")
        );
    }

    /// The shell's own helpers are services and must never be tiered, even when
    /// they map a toplevel (a portal dialog).
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
            fg_cpu_weight: 200,
            bg_cpu_weight: 20,
            bg_memory_high: "512M".into(),
            bg_cpu_quota: "30%".into(),
            bg_allowed_cpus: "auto".into(),
        };
        let fg = args("a.scope", Tier::Foreground, &res);
        assert!(fg.contains(&"CPUWeight=200".to_string()));
        assert!(fg.contains(&"MemoryHigh=infinity".to_string()));
        // Empty, not "infinity": CPUQuota= refuses that word, and set-property
        // applies all-or-nothing, so the CPUWeight above would go with it.
        assert!(fg.contains(&"CPUQuota=".to_string()));
        let bg = args("a.scope", Tier::Background, &res);
        assert!(bg.contains(&"CPUWeight=20".to_string()));
        assert!(bg.contains(&"MemoryHigh=512M".to_string()));
        assert!(bg.contains(&"CPUQuota=30%".to_string()));
        // Nothing is persisted across reboots.
        assert!(bg.contains(&"--runtime".to_string()));
    }

    /// The FP5's real topology: three capacity tiers, not the 4+4 the label
    /// suggests. Only the 382s are the efficiency cluster.
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

    /// A symmetric machine (a dev box, the VM) has nothing to pin to.
    #[test]
    fn no_little_cluster_without_asymmetry() {
        assert_eq!(little_cluster(&[(0, 1024), (1, 1024)]), None);
        assert_eq!(little_cluster(&[]), None);
    }

    /// The efficiency cores need not be numbered first or contiguously.
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

    /// `bg_cpu_quota = "infinity"` is how the config says "don't cap", and has
    /// to reach systemd as the empty value it actually accepts.
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
