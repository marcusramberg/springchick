//! Attributing a mapped toplevel to the launch that produced it.
//!
//! The shell's notion of "which app is this window" cannot come from the
//! client-reported xdg `app_id`: a `Terminal=true` entry runs through
//! `foot -e …` and reports `foot`, and a PWA runner reports its own id rather
//! than the per-PWA `.desktop` stem. Both cases end up mis-tagged, which is
//! what makes tap-to-raise pick the wrong window (or spawn a duplicate).
//!
//! So identity comes from the launch instead. Two independent signals, both
//! established here: the xdg-activation token we minted before spawning (the
//! standards-blessed route, honoured by GTK/Qt/wlroots clients), and the client
//! process's ancestry, which catches everything that drops the token —
//! terminals, shell wrappers, and anything exec'd behind `gio launch`.
//!
//! The pid walk is the fiddly half, so it is factored into pure functions and
//! unit-tested without touching `/proc`.

/// How many parent links to follow from the client pid before giving up. Deep
/// enough for `foot -e sh -c 'exec app'` (three links) with room to spare, short
/// enough that a client unrelated to any launch can't accidentally reach one via
/// a long ancestor chain up to the session leader.
pub const MAX_DEPTH: usize = 6;

/// Extract `PPid:` from the body of `/proc/<pid>/status`.
pub fn parse_ppid(status: &str) -> Option<i32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|v| v.trim().parse().ok())
}

/// The parent of `pid` according to `/proc`. `None` when the process is gone
/// (already reaped) or `/proc` is unreadable.
pub fn parent_of(pid: i32) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_ppid(&status)
}

/// The chain `[pid, parent, grandparent, …]`, at most `MAX_DEPTH` links long.
///
/// Stops at pid 1 (and at 0, which is what `PPid` reports for a reparented
/// orphan's ancestor) so init never appears — every launch shares it, so
/// reaching it would let any client match any launch.
pub fn ancestry_with<F>(pid: i32, max_depth: usize, mut parent_of: F) -> Vec<i32>
where
    F: FnMut(i32) -> Option<i32>,
{
    let mut chain = vec![pid];
    let mut cur = pid;
    for _ in 0..max_depth {
        match parent_of(cur) {
            Some(p) if p > 1 => {
                chain.push(p);
                cur = p;
            }
            _ => break,
        }
    }
    chain
}

/// [`ancestry_with`] against the real `/proc`.
pub fn ancestry(pid: i32) -> Vec<i32> {
    ancestry_with(pid, MAX_DEPTH, parent_of)
}

/// Index into `launch_pids` of the launch this client belongs to, or `None`.
///
/// `chain` is ordered child→ancestor, so the search walks it outward: the
/// *nearest* ancestor that is a launch wins. That matters when one launched app
/// spawns another (a terminal launching an editor) — the window belongs to the
/// innermost launch that claims it.
pub fn match_ancestry(launch_pids: &[i32], chain: &[i32]) -> Option<usize> {
    chain
        .iter()
        .find_map(|pid| launch_pids.iter().position(|lp| lp == pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ppid_from_status() {
        let status = "Name:\tfoot\nUmask:\t0022\nState:\tS (sleeping)\nTgid:\t42\nPid:\t42\nPPid:\t17\nTracerPid:\t0\n";
        assert_eq!(parse_ppid(status), Some(17));
    }

    #[test]
    fn missing_ppid_is_none() {
        assert_eq!(parse_ppid("Name:\tfoot\nPid:\t42\n"), None);
        assert_eq!(parse_ppid(""), None);
    }

    /// A fake process tree: `(child, parent)` links.
    fn tree(links: &[(i32, i32)]) -> impl Fn(i32) -> Option<i32> + '_ {
        move |pid| links.iter().find(|(c, _)| *c == pid).map(|(_, p)| *p)
    }

    #[test]
    fn walks_up_to_max_depth() {
        let links = [(10, 9), (9, 8), (8, 7), (7, 6), (6, 5), (5, 4), (4, 3)];
        let chain = ancestry_with(10, 3, tree(&links));
        assert_eq!(chain, vec![10, 9, 8, 7]);
    }

    #[test]
    fn stops_at_init() {
        let links = [(10, 9), (9, 1)];
        assert_eq!(ancestry_with(10, MAX_DEPTH, tree(&links)), vec![10, 9]);
    }

    #[test]
    fn stops_when_process_is_gone() {
        let links = [(10, 9)];
        assert_eq!(ancestry_with(10, MAX_DEPTH, tree(&links)), vec![10, 9]);
    }

    #[test]
    fn direct_child_matches() {
        assert_eq!(match_ancestry(&[100, 200], &[200]), Some(1));
    }

    /// The app exec'd *by* a launched wrapper (`gio launch`, a login shell)
    /// resolves through the parent links rather than as a direct child.
    #[test]
    fn grandchild_matches_through_ancestry() {
        assert_eq!(match_ancestry(&[100], &[400, 300, 100]), Some(0));
    }

    #[test]
    fn nearest_launch_ancestor_wins() {
        // 300 (a terminal we launched) itself launched 400's parent; the window
        // belongs to the inner launch, not the outer one.
        assert_eq!(match_ancestry(&[100, 300], &[400, 300, 100]), Some(1));
    }

    #[test]
    fn unrelated_client_matches_nothing() {
        assert_eq!(match_ancestry(&[100, 200], &[400, 300]), None);
    }

    #[test]
    fn no_launches_matches_nothing() {
        assert_eq!(match_ancestry(&[], &[400, 300]), None);
    }
}
