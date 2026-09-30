//! inotify watch over the XDG `applications/` dirs. The thread only sets a
//! flag; the rescan runs on the compositor thread at the next event-loop wake.

use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags};
use rustix::fs::inotify;
use tracing::{debug, warn};

/// Coalesces a package-manager transaction into one rescan.
const DEBOUNCE: Duration = Duration::from_millis(750);

pub type Dirty = Arc<AtomicBool>;

pub fn spawn() -> Dirty {
    let dirty: Dirty = Arc::new(AtomicBool::new(false));
    // NONBLOCK so the queue can be drained; the thread parks in `poll`.
    let fd = match inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK) {
        Ok(fd) => fd,
        Err(e) => {
            warn!(%e, "inotify init failed; catalog will only update on ipc reload");
            return dirty;
        }
    };

    let flags = inotify::WatchFlags::CREATE
        | inotify::WatchFlags::DELETE
        | inotify::WatchFlags::MOVED_TO
        | inotify::WatchFlags::MOVED_FROM
        | inotify::WatchFlags::CLOSE_WRITE
        | inotify::WatchFlags::ONLYDIR;
    let mut watched = 0;
    for dir in sc_catalog::xdg_data_dirs() {
        let dir = dir.join("applications");
        match inotify::add_watch(&fd, &dir, flags) {
            Ok(_) => watched += 1,
            Err(e) => debug!(path = %dir.display(), %e, "not watching"),
        }
    }
    debug!(watched, "catalog watch");

    let flag = dirty.clone();
    std::thread::Builder::new()
        .name("catalog-watch".into())
        .spawn(move || {
            let mut buf = [MaybeUninit::<u8>::uninit(); 4096];
            loop {
                let mut pfd = [PollFd::new(&fd, PollFlags::IN)];
                match rustix::event::poll(&mut pfd, None) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => {
                        warn!(%e, "catalog watch stopped");
                        return;
                    }
                }
                drain(&fd, &mut buf);
                std::thread::sleep(DEBOUNCE);
                drain(&fd, &mut buf);
                flag.store(true, Ordering::Relaxed);
            }
        })
        .ok();

    dirty
}

fn drain(fd: &impl std::os::fd::AsFd, buf: &mut [MaybeUninit<u8>]) {
    let mut reader = inotify::Reader::new(fd, buf);
    while reader.next().is_ok() {}
}

pub fn take(dirty: &Dirty) -> bool {
    dirty.swap(false, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_a_new_desktop_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("applications")).unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        std::env::set_var("XDG_DATA_DIRS", " ");

        let dirty = spawn();
        assert!(!take(&dirty));

        std::fs::write(tmp.path().join("applications/x.desktop"), "[Desktop Entry]").unwrap();
        std::thread::sleep(DEBOUNCE * 3);

        assert!(take(&dirty));
        assert!(!take(&dirty), "flag is edge-triggered");
    }
}
