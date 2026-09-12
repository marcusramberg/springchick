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
use tracing::{debug, warn};

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
pub fn unit_from_cgroup(body: &str) -> Option<String> {
    // The cgroup-v2 line is `0::/path`; v1 lines (`1:name=systemd:/path`) may
    // also be present on a hybrid system and are not what we want.
    let path = body.lines().find_map(|l| l.strip_prefix("0::"))?;
    let leaf = path.rsplit('/').next()?;
    leaf.ends_with(".scope").then(|| leaf.to_string())
}

/// The scope unit `pid` runs in, or `None` when it is not in one (or is gone).
pub fn unit_of_pid(pid: i32) -> Option<String> {
    unit_from_cgroup(&std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?)
}

/// The `systemctl set-property` arguments putting `unit` in `tier`.
///
/// Both properties are set in both tiers: promoting an app has to *clear* the
/// ceiling the demotion put on it, and a property left unmentioned keeps its
/// old value.
fn args(unit: &str, tier: Tier, res: &Resources) -> Vec<String> {
    let (weight, high) = match tier {
        Tier::Foreground => (res.fg_cpu_weight, "infinity".to_string()),
        Tier::Background => (res.bg_cpu_weight, res.bg_memory_high.clone()),
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
    ]
}

/// Move `unit` into `tier`, returning the `systemctl` child for the caller to
/// reap.
///
/// Deliberately not waited on: the call is a D-Bus round-trip to the user
/// manager and this runs on the render thread. Nothing depends on the result —
/// if it fails (the app exited, taking its scope with it) the tier simply does
/// not apply, and `systemctl` says so in the journal.
pub fn apply(unit: &str, tier: Tier, res: &Resources) -> Option<Child> {
    debug!(unit, ?tier, "applying resource tier");
    match Command::new("systemctl")
        .args(args(unit, tier, res))
        .spawn()
    {
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
        assert_eq!(
            unit_from_cgroup(V2).as_deref(),
            Some("springchick-42-0-foot.scope")
        );
    }

    /// A flatpak app is in the scope flatpak made for it, not the one we did.
    #[test]
    fn reads_a_scope_we_did_not_create() {
        let body = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-org.gnome.Fractal.Devel-3308067827.scope\n";
        assert_eq!(
            unit_from_cgroup(body).as_deref(),
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
        };
        let fg = args("a.scope", Tier::Foreground, &res);
        assert!(fg.contains(&"CPUWeight=200".to_string()));
        assert!(fg.contains(&"MemoryHigh=infinity".to_string()));
        let bg = args("a.scope", Tier::Background, &res);
        assert!(bg.contains(&"CPUWeight=20".to_string()));
        assert!(bg.contains(&"MemoryHigh=512M".to_string()));
        // Nothing is persisted across reboots.
        assert!(bg.contains(&"--runtime".to_string()));
    }
}
