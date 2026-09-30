//! Which launch a mapped toplevel belongs to. The client `app_id` is useless
//! for this (`Terminal=true` apps report `foot`, PWAs report the runner), so
//! we match the xdg-activation token, then the client's process ancestry.

/// Covers `foot -e sh -c 'exec app'` (three links); short enough that an
/// unrelated client can't reach a launch through the session leader.
pub const MAX_DEPTH: usize = 6;

pub fn parse_ppid(status: &str) -> Option<i32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|v| v.trim().parse().ok())
}

pub fn parent_of(pid: i32) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_ppid(&status)
}

/// `[pid, parent, …]`, at most `max_depth` long. Stops before pid 1 (and 0):
/// every launch shares init, so reaching it would match anything.
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

pub fn ancestry(pid: i32) -> Vec<i32> {
    ancestry_with(pid, MAX_DEPTH, parent_of)
}

/// Index into `launch_pids` of the nearest ancestor launch, so a terminal
/// launching an editor doesn't claim the editor's window.
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

    #[test]
    fn grandchild_matches_through_ancestry() {
        assert_eq!(match_ancestry(&[100], &[400, 300, 100]), Some(0));
    }

    #[test]
    fn nearest_launch_ancestor_wins() {
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
