//! `springchick ipc <verb> [args...]`: sends one line over the control socket
//! (the [`crate::debug_input`] protocol) and prints the one-line reply.
//! Multi-record replies (`layers`) are joined with ` | `.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

/// `SPRINGCHICK_IPC_SOCK`, then legacy `SPRINGCHICK_DEBUG_SOCK`, else
/// `$XDG_RUNTIME_DIR/springchick-ipc.sock` (or `/tmp`).
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

/// Exit 0 on `ok`, 1 on `err` or I/O failure, 2 on misuse.
pub fn run_client(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: springchick ipc <command> [args...]");
        eprintln!("  e.g. springchick ipc tap 640 400");
        eprintln!("       springchick ipc swipe 640 788 1080 788 500");
        eprintln!("       springchick ipc launch org.gnome.Maps [new]");
        eprintln!("       springchick ipc reload   # re-read config.toml + rescan apps");
        eprintln!("       springchick ipc layers   # dump layer surfaces + their popups");
        eprintln!("       springchick ipc home     # dump home grid + catalog resolution");
        eprintln!("       springchick ipc quit     # end the session");
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

    let mut reply = String::new();
    if let Err(e) = BufReader::new(&stream).read_line(&mut reply) {
        eprintln!("springchick ipc: read failed: {e}");
        return ExitCode::from(1);
    }
    let reply = reply.trim();
    if !reply.is_empty() {
        println!("{}", reply.replace(" | ", "\n"));
    }
    if reply.starts_with("ok") {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
