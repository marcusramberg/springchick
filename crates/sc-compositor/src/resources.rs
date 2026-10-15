//! Per-app resource tiers: focused app vs. the ones behind it. Each app has
//! its own systemd scope ([`crate::launcher`]), so tiering is a property change
//! on that cgroup and never moves processes.
//!
//! The unit comes from the client's pid, not our scope name: flatpak moves the
//! app into its own scope and ours is collected empty.

use sc_config::{AppPolicy, Resources};
use std::collections::{HashMap, HashSet};
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
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

/// Category default, overridden per app in `[resources.apps]`. Media and
/// terminals keep running, browsers keep their downloads, chat wakes up for
/// notifications, the rest freezes. An app with no catalog entry can't be
/// classified and is only throttled.
pub fn policy_for(categories: Option<&[String]>, res: &Resources, app_id: &str) -> AppPolicy {
    if let Some(p) = res.apps.get(app_id) {
        return *p;
    }
    let Some(cats) = categories else {
        return AppPolicy::Throttle;
    };
    let has = |names: &[&str]| cats.iter().any(|c| names.contains(&c.as_str()));
    if has(&["InstantMessaging", "Chat", "Email"]) {
        AppPolicy::Messenger
    } else if has(&[
        "AudioVideo",
        "Audio",
        "Video",
        "Player",
        "Music",
        "TerminalEmulator",
        "WebBrowser",
    ]) {
        AppPolicy::Throttle
    } else {
        AppPolicy::Freeze
    }
}

fn cgroup_file(path: &str, name: &str) -> String {
    format!("/sys/fs/cgroup{path}/{name}")
}

/// Delegated scopes are owned by the user, so this is a plain write.
fn set_frozen(path: &str, frozen: bool) {
    if let Err(e) = std::fs::write(
        cgroup_file(path, "cgroup.freeze"),
        if frozen { "1" } else { "0" },
    ) {
        warn!(%e, path, frozen, "cgroup.freeze write failed");
    }
}

fn cgroup_pids(path: &str) -> HashSet<i32> {
    std::fs::read_to_string(cgroup_file(path, "cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Pids with a running PipeWire stream (audio out, mic, camera). The pid is
/// the client object's kernel credential, not the stream's self-reported one.
pub fn active_stream_pids(dump: &[u8]) -> HashSet<i32> {
    let Ok(serde_json::Value::Array(objs)) = serde_json::from_slice(dump) else {
        return HashSet::new();
    };
    let client_pid: HashMap<i64, i32> = objs
        .iter()
        .filter(|o| o["type"] == "PipeWire:Interface:Client")
        .filter_map(|o| {
            let pid = o["info"]["props"]["pipewire.sec.pid"].as_i64()?;
            Some((o["id"].as_i64()?, pid as i32))
        })
        .collect();
    objs.iter()
        .filter_map(|o| {
            let info = &o["info"];
            let props = &info["props"];
            if !props["media.class"].as_str()?.starts_with("Stream/")
                || info["state"].as_str()? != "running"
            {
                return None;
            }
            client_pid.get(&props["client.id"].as_i64()?).copied()
        })
        .collect()
}

fn spawn_probe() -> Receiver<HashSet<i32>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let pids = match Command::new("pw-dump").output() {
            Ok(out) => active_stream_pids(&out.stdout),
            Err(e) => {
                debug!(%e, "pw-dump unavailable; freezing without an audio guard");
                HashSet::new()
            }
        };
        let _ = tx.send(pids);
    });
    rx
}

struct BgApp {
    path: String,
    policy: AppPolicy,
    /// Earliest freeze; pushed back while the app streams audio.
    due: Instant,
    frozen: bool,
    wake_at: Option<Instant>,
}

/// Background apps by scope unit. Driven once a second from the backend loop,
/// since page-flips stop on a static screen.
#[derive(Default)]
pub struct Freezer {
    bg: HashMap<String, BgApp>,
    probe: Option<Receiver<HashSet<i32>>>,
    next_tick: Option<Instant>,
}

impl Freezer {
    pub fn due(&self, now: Instant) -> bool {
        self.next_tick.is_none_or(|t| now >= t)
    }

    /// The next [`Self::tick`] runs at once: the foreground changed.
    pub fn force(&mut self) {
        self.next_tick = None;
    }

    pub fn is_frozen(&self, unit: &str) -> bool {
        self.bg.get(unit).is_some_and(|e| e.frozen)
    }

    /// `bg` is every mapped background app with a freezable policy; anything
    /// else (foreground, closed, policy changed) is thawed and forgotten.
    /// `ready` is false while the app still owes a resize: frozen with a stale
    /// buffer, its switcher card would stay the wrong size.
    pub fn tick(
        &mut self,
        now: Instant,
        res: &Resources,
        bg: impl IntoIterator<Item = (AppCgroup, AppPolicy, bool)>,
    ) {
        if !self.due(now) {
            return;
        }
        self.next_tick = Some(now + Duration::from_secs(1));
        let delay = Duration::from_secs(res.freeze_delay_secs);

        let mut keep = HashSet::new();
        let mut ready_units = HashSet::new();
        for (cg, policy, ready) in bg {
            keep.insert(cg.unit.clone());
            if ready {
                ready_units.insert(cg.unit.clone());
            }
            let e = self.bg.entry(cg.unit).or_insert(BgApp {
                path: cg.path,
                policy,
                due: now + delay,
                frozen: false,
                wake_at: None,
            });
            if e.policy != policy {
                e.policy = policy;
                e.wake_at = e
                    .frozen
                    .then(|| now + Duration::from_secs(res.freeze_wake_secs));
            }
        }
        self.bg.retain(|unit, e| {
            let stay = keep.contains(unit);
            if !stay && e.frozen {
                info!(unit, "thaw");
                set_frozen(&e.path, false);
            }
            stay
        });

        for (unit, e) in &mut self.bg {
            if e.frozen && e.wake_at.is_some_and(|t| now >= t) {
                info!(unit, "wake");
                set_frozen(&e.path, false);
                e.frozen = false;
                e.wake_at = None;
                e.due = now + Duration::from_secs(res.freeze_wake_for_secs);
            }
        }

        if let Some(rx) = &self.probe {
            match rx.try_recv() {
                Ok(active) => {
                    self.probe = None;
                    for (unit, e) in &mut self.bg {
                        if e.frozen || now < e.due || !ready_units.contains(unit) {
                            continue;
                        }
                        if cgroup_pids(&e.path).is_disjoint(&active) {
                            info!(unit, "freeze");
                            set_frozen(&e.path, true);
                            e.frozen = true;
                            e.wake_at = (e.policy == AppPolicy::Messenger)
                                .then(|| now + Duration::from_secs(res.freeze_wake_secs));
                        } else {
                            info!(unit, "streaming, not freezing");
                            e.due = now + delay;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.probe = None,
            }
        }
        if self.probe.is_none()
            && self
                .bg
                .iter()
                .any(|(u, e)| !e.frozen && now >= e.due && ready_units.contains(u))
        {
            self.probe = Some(spawn_probe());
        }
    }

    /// Before the app has to act: a close request, or an activation of a
    /// single-instance app from a fresh launcher process.
    pub fn thaw(&mut self, unit: &str, res: &Resources) {
        if let Some(e) = self.bg.get_mut(unit) {
            if e.frozen {
                info!(unit, "thaw");
                set_frozen(&e.path, false);
                e.frozen = false;
                e.wake_at = None;
            }
            // Give it the full delay again.
            e.due = Instant::now() + Duration::from_secs(res.freeze_delay_secs);
        }
    }

    pub fn thaw_all(&mut self) {
        for (unit, e) in &mut self.bg {
            if e.frozen {
                info!(unit, "thaw");
                set_frozen(&e.path, false);
                e.frozen = false;
            }
        }
    }
}

/// `user@N.service/app.slice`, where [`crate::launcher`] puts every scope.
fn app_slice_from_cgroup(body: &str) -> Option<String> {
    let path = body.lines().find_map(|l| l.strip_prefix("0::"))?;
    let n = path
        .split('/')
        .position(|seg| seg.starts_with("user@") && seg.ends_with(".service"))?;
    let root: Vec<&str> = path.split('/').take(n + 1).collect();
    Some(format!("{}/{}", root.join("/"), crate::launcher::APP_SLICE))
}

/// A crashed predecessor leaves its apps frozen with a dead socket, and they
/// can't notice until thawed.
pub fn thaw_stale_scopes() {
    let body = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let Some(slice) = app_slice_from_cgroup(&body) else {
        return;
    };
    let Ok(dir) = std::fs::read_dir(format!("/sys/fs/cgroup{slice}")) else {
        return;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("springchick-") || !name.ends_with(".scope") {
            continue;
        }
        let path = format!("{slice}/{name}");
        if std::fs::read_to_string(cgroup_file(&path, "cgroup.freeze"))
            .is_ok_and(|v| v.trim() == "1")
        {
            info!(unit = %name, "thawing scope left frozen by a previous instance");
            set_frozen(&path, false);
        }
    }
}

/// A compositor exit must not leave apps frozen with a dead socket.
impl Drop for Freezer {
    fn drop(&mut self) {
        self.thaw_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cats(c: &[&str]) -> Vec<String> {
        c.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn app_slice_from_the_user_manager_path() {
        assert_eq!(
            app_slice_from_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/springchick.service\n"
            )
            .as_deref(),
            Some("/user.slice/user-1000.slice/user@1000.service/app.slice")
        );
        assert_eq!(
            app_slice_from_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/tmux-spawn-1.scope\n"
            )
            .as_deref(),
            Some("/user.slice/user-1000.slice/user@1000.service/app.slice")
        );
        assert_eq!(app_slice_from_cgroup("0::/init.scope\n"), None);
    }

    #[test]
    fn category_defaults() {
        let res = Resources::default();
        let p = |c: &[&str]| policy_for(Some(&cats(c)), &res, "x");
        assert_eq!(
            p(&["Network", "InstantMessaging", "Chat"]),
            AppPolicy::Messenger
        );
        assert_eq!(p(&["Office", "Email"]), AppPolicy::Messenger);
        assert_eq!(p(&["AudioVideo", "Player"]), AppPolicy::Throttle);
        assert_eq!(p(&["System", "TerminalEmulator"]), AppPolicy::Throttle);
        assert_eq!(p(&["Network", "WebBrowser"]), AppPolicy::Throttle);
        assert_eq!(p(&["Office", "Viewer"]), AppPolicy::Freeze);
        assert_eq!(p(&[]), AppPolicy::Freeze);
        assert_eq!(policy_for(None, &res, "unknown_3"), AppPolicy::Throttle);
    }

    #[test]
    fn app_override_beats_categories() {
        let mut res = Resources::default();
        res.apps.insert("mpv".into(), AppPolicy::Ignore);
        assert_eq!(
            policy_for(Some(&cats(&["AudioVideo"])), &res, "mpv"),
            AppPolicy::Ignore
        );
    }

    #[test]
    fn running_streams_resolve_to_client_pids() {
        let dump = br#"[
          {"id": 88, "type": "PipeWire:Interface:Client", "info": {"props": {"pipewire.sec.pid": 4210}}},
          {"id": 95, "type": "PipeWire:Interface:Client", "info": {"props": {"pipewire.sec.pid": 2860}}},
          {"id": 89, "type": "PipeWire:Interface:Node", "info": {"state": "running", "props": {"media.class": "Stream/Output/Audio", "client.id": 88}}},
          {"id": 97, "type": "PipeWire:Interface:Node", "info": {"state": "idle", "props": {"media.class": "Stream/Output/Audio", "client.id": 95}}},
          {"id": 40, "type": "PipeWire:Interface:Node", "info": {"state": "running", "props": {"media.class": "Audio/Sink", "client.id": 95}}}
        ]"#;
        assert_eq!(active_stream_pids(dump), HashSet::from([4210]));
        assert!(active_stream_pids(b"garbage").is_empty());
    }

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
            ..Resources::default()
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
