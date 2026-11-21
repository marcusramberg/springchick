//! `springchick complete [WORDS...]`: candidates for the word after WORDS, one
//! `word<TAB>description` per line. The shell completions call this.

const VERBS: &[(&str, &str)] = &[
    ("tap", "X Y: a real contact"),
    ("swipe", "X1 Y1 X2 Y2 [MS]: shell gesture"),
    ("tswipe", "X1 Y1 X2 Y2 [MS]: finger drag"),
    ("down", "X Y: finger down"),
    ("move", "X Y: move the finger"),
    ("up", "Lift the finger"),
    ("key", "NAME [MS]: press a keysym"),
    ("keydown", "NAME: hold a keysym"),
    ("keyup", "NAME: release a keysym"),
    ("settle", "[MS]: wait for animations"),
    ("launch", "APP_ID [new]"),
    ("drag", "APP_ID [X Y]: arrange-drag an icon"),
    ("action", "NAME: run a built-in action"),
    ("reload", "Re-read config.toml, rescan apps"),
    ("haptic", "[EVENT]: feedbackd event"),
    ("layers", "Dump layer surfaces and popups"),
    ("busy", "What keeps the frame loop running"),
    ("home", "Dump home grid and catalog"),
    ("shortcuts", "[APP_ID]: control sheet apps"),
    ("pill", "Blink the home pill"),
    ("layout", "[N]: list or lock xkb layout"),
    ("rotation", "[enable|disable]: whole-UI rotation"),
    ("orientation", "NAME: fake the accelerometer"),
    ("record", "start PATH [SCALE] | stop"),
    ("quit", "End the session"),
];

const ACTIONS: &[&str] = &[
    "close-app",
    "home",
    "toggle-display",
    "volume-up",
    "volume-down",
    "volume-mute",
    "toggle-fullscreen",
    "search",
    "switcher-next",
    "switcher-prev",
    "screenshot",
];

const ORIENTATIONS: &[&str] = &["normal", "bottom-up", "left-up", "right-up", "undefined"];

pub fn run(words: &[String]) {
    for (word, desc) in candidates(words) {
        if desc.is_empty() {
            println!("{word}");
        } else {
            println!("{word}\t{desc}");
        }
    }
}

type Candidates = Vec<(String, String)>;

fn described(list: &[(&str, &str)]) -> Candidates {
    list.iter()
        .map(|(w, d)| (w.to_string(), d.to_string()))
        .collect()
}

fn plain(list: &[&str]) -> Candidates {
    list.iter()
        .map(|w| (w.to_string(), String::new()))
        .collect()
}

fn candidates(words: &[String]) -> Candidates {
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] => described(&[
            ("ipc", "Drive a running compositor"),
            ("--version", "Print version"),
        ]),
        ["ipc"] => described(VERBS),
        // The same scan the compositor's catalog comes from.
        ["ipc", "launch" | "drag" | "shortcuts"] => sc_catalog::scan_apps()
            .into_iter()
            .map(|a| (a.id, a.name))
            .collect(),
        ["ipc", "launch", _] => described(&[("new", "Open another window")]),
        ["ipc", "action"] => plain(ACTIONS),
        ["ipc", "layout"] => layouts(),
        ["ipc", "rotation"] => plain(&["enable", "disable"]),
        ["ipc", "orientation"] => plain(ORIENTATIONS),
        ["ipc", "record"] => plain(&["start", "stop"]),
        ["ipc", "haptic"] => plain(&[
            "button-pressed",
            "window-close",
            "message-new-instant",
            "bell-terminal",
        ]),
        // Any xkb keysym name works; these are the ones the phone has.
        ["ipc", "key" | "keydown" | "keyup"] => plain(&[
            "XF86PowerOff",
            "XF86AudioRaiseVolume",
            "XF86AudioLowerVolume",
            "Super_L",
            "Escape",
            "Return",
            "Tab",
        ]),
        _ => Vec::new(),
    }
}

/// Asks the running compositor; empty when there is none.
fn layouts() -> Candidates {
    // `ACTIVE NAME|NAME|…`
    let Some(reply) = crate::ipc::query("layout") else {
        return Vec::new();
    };
    let names = reply.split_once(' ').map_or("", |(_, names)| names);
    names
        .split('|')
        .enumerate()
        .map(|(i, name)| (i.to_string(), name.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_input::parse_line;

    fn words(line: &str) -> Vec<String> {
        candidates(
            &line
                .split_whitespace()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
        .into_iter()
        .map(|(w, _)| w)
        .collect()
    }

    #[test]
    fn positions() {
        assert_eq!(words(""), ["ipc", "--version"]);
        assert!(words("ipc").contains(&"launch".to_string()));
        assert_eq!(words("ipc launch some.App"), ["new"]);
        assert_eq!(words("ipc rotation"), ["enable", "disable"]);
        assert!(words("ipc tap").is_empty());
        assert!(words("nonsense").is_empty());
    }

    #[test]
    fn candidates_parse() {
        for (verb, _) in VERBS {
            let unknown =
                parse_line(verb, 100.0, 100.0).is_err_and(|e| e.starts_with("parse: unknown verb"));
            assert!(!unknown, "{verb}");
        }
        for verb in ["action", "rotation", "orientation"] {
            for arg in words(&format!("ipc {verb}")) {
                let line = format!("{verb} {arg}");
                assert!(parse_line(&line, 100.0, 100.0).is_ok(), "{line}");
            }
        }
    }
}
