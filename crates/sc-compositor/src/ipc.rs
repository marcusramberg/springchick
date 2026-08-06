//! `springchick ipc` — a thin client that talks to a running compositor over
//! its control socket, reusing the [`crate::debug_input`] line protocol.
//!
//! The compositor always listens on [`socket_path`]; `springchick ipc <verb>
//! [args...]` connects there, sends one line, prints the reply, and exits
//! non-zero if the reply is an error. Today the verbs are the debug-input
//! gestures (`tap`, `swipe`, `key`, `settle`, …); future control verbs
//! (`reload`, `state`, …) slot in the same way.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

/// Resolve the control-socket path. `SPRINGCHICK_IPC_SOCK` wins, then the legacy
/// `SPRINGCHICK_DEBUG_SOCK` (kept so existing test harnesses keep working), else
/// `$XDG_RUNTIME_DIR/springchick-ipc.sock` (falling back to `/tmp`).
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("SPRINGCHICK_IPC_SOCK") {
        return p.into();
    }
    if let Ok(p) = std::env::var("SPRINGCHICK_DEBUG_SOCK") {
        return p.into();
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(dir).join("springchick-ipc.sock")
}

/// Run the `ipc` subcommand: connect, send `args` joined as one line, print the
/// reply. Exit 0 on an `ok` reply, 1 on `err` or any I/O failure, 2 on misuse.
pub fn run_client(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: springchick ipc <command> [args...]");
        eprintln!("  e.g. springchick ipc tap 640 400");
        eprintln!("       springchick ipc swipe 640 788 1080 788 500");
        return ExitCode::from(2);
    }

    let path = socket_path();
    let stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("springchick ipc: cannot connect to {}: {e}", path.display());
            eprintln!("(is the compositor running?)");
            return ExitCode::from(1);
        }
    };

    let line = args.join(" ");
    let mut writer = &stream;
    if let Err(e) = writeln!(writer, "{line}") {
        eprintln!("springchick ipc: write failed: {e}");
        return ExitCode::from(1);
    }

    // One line of reply: `ok` / `ok <data>` / `err <msg>`.
    let mut reply = String::new();
    if let Err(e) = BufReader::new(&stream).read_line(&mut reply) {
        eprintln!("springchick ipc: read failed: {e}");
        return ExitCode::from(1);
    }
    let reply = reply.trim();
    if !reply.is_empty() {
        println!("{reply}");
    }
    if reply.starts_with("ok") {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
