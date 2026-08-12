use std::path::{Path, PathBuf};

/// XDG base data directories, highest precedence first: `$XDG_DATA_HOME`
/// (default `~/.local/share`), then each `$XDG_DATA_DIRS` entry (default
/// `/usr/local/share:/usr/share`) left-to-right. Callers append `applications`
/// (desktop files) or `icons` (themes) to each.
pub fn xdg_data_dirs() -> Vec<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());

    let mut dirs: Vec<PathBuf> = data_home.into_iter().collect();
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AppEntry {
    pub id: String, // file stem, e.g. "org.gnome.Maps"
    pub name: String,
    pub exec: String, // raw Exec line (field codes like %U left for the launcher to strip)
    pub icon: String, // icon name or absolute path
    /// `Terminal=true`: the app is a CLI program and must be run inside a
    /// terminal emulator, not spawned bare.
    pub terminal: bool,
    /// `Path=`: working directory to run the program in.
    pub path: Option<PathBuf>,
    /// `DBusActivatable=true`: may be launched over D-Bus, and is allowed to
    /// omit Exec entirely.
    pub dbus_activatable: bool,
    /// Where this entry was read from; `%k` expands to it.
    pub desktop_file: PathBuf,
}

/// Scan `.desktop` files from every XDG data dir's `applications/`, highest
/// precedence first (the first entry seen for a given id wins). Skips
/// NoDisplay/Hidden/non-Application entries via [`parse_desktop`]. Shared by the
/// compositor and the search app so both see the same catalog.
pub fn scan_apps() -> Vec<AppEntry> {
    let env = DesktopEnv::from_env();
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in xdg_data_dirs() {
        let Ok(read) = std::fs::read_dir(dir.join("applications")) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "desktop") {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if let Some(app) = parse_desktop_in(&path, &contents, &env) {
                        if seen.insert(app.id.clone()) {
                            entries.push(app);
                        }
                    }
                }
            }
        }
    }
    entries
}

/// Split an Exec line into argv, honouring Desktop Entry quoting and dropping
/// field codes (`%U %f %F %u %i %c %k` …). Shared by the compositor's launcher
/// and the search app.
///
/// Splitting on whitespace alone is not enough: the spec reserves space, tab,
/// newline and ``"'\><~|&;$*?#()` `` and requires an argument containing any of
/// them to be double-quoted. Any URL with a query string therefore arrives
/// quoted, and a naive split hands `Command::new` a program name with the quote
/// characters still attached.
///
/// Inside quotes, `\` escapes `"`, `` ` ``, `$` and itself, and a `%` is
/// literal - so percent-encoded URLs survive intact.
///
/// Single quotes are not part of the spec, which defines double quotes only,
/// but enough real-world .desktop files use them that we accept both. They
/// follow shell convention: everything up to the closing quote is literal, with
/// no escape processing.
pub fn parse_exec(exec: &str) -> Vec<String> {
    parse_exec_with(exec, &ExecContext::default())
}

/// Values the expandable field codes resolve to. `%i`, `%c` and `%k` carry
/// real content rather than being dropped like the file/URL codes.
#[derive(Debug, Default)]
pub struct ExecContext<'a> {
    pub icon: &'a str,
    pub name: &'a str,
    pub desktop_file: &'a str,
}

/// [`parse_exec`], expanding `%i` (`--icon <Icon>`), `%c` (the name) and `%k`
/// (the desktop file path).
pub fn parse_exec_with(exec: &str, ctx: &ExecContext) -> Vec<String> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut has_current = false;
    let mut chars = exec.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            c if c.is_whitespace() => {
                if has_current {
                    argv.push(std::mem::take(&mut current));
                    has_current = false;
                }
            }
            quote @ ('"' | '\'') => {
                // A quoted argument exists even if it is empty.
                has_current = true;
                while let Some(c) = chars.next() {
                    if c == quote {
                        break;
                    }
                    // Single quotes are literal, shell-style; only double
                    // quotes process escapes.
                    if c == '\\' && quote == '"' {
                        match chars.peek() {
                            // Only these four are escapable; anything else
                            // keeps the backslash, as a path like C:\x would.
                            Some('"' | '`' | '$' | '\\') => {
                                current.push(chars.next().unwrap_or_default())
                            }
                            _ => current.push('\\'),
                        }
                    } else {
                        current.push(c);
                    }
                }
            }
            '%' => match chars.next() {
                // %% is an escaped literal percent sign.
                Some('%') => {
                    current.push('%');
                    has_current = true;
                }
                // %i expands to two arguments, and to nothing when there is no
                // icon to name.
                Some('i') => {
                    if has_current {
                        argv.push(std::mem::take(&mut current));
                        has_current = false;
                    }
                    if !ctx.icon.is_empty() {
                        argv.push("--icon".to_string());
                        argv.push(ctx.icon.to_string());
                    }
                }
                // %c and %k are single arguments that may contain spaces, so
                // they are pushed whole rather than appended to `current`.
                Some(code @ ('c' | 'k')) => {
                    let value = if code == 'c' {
                        ctx.name
                    } else {
                        ctx.desktop_file
                    };
                    if has_current {
                        argv.push(std::mem::take(&mut current));
                        has_current = false;
                    }
                    if !value.is_empty() {
                        argv.push(value.to_string());
                    }
                }
                // The file/URL codes expand to nothing: we launch with no
                // document argument.
                Some(_) => {}
                None => {}
            },
            c => {
                current.push(c);
                has_current = true;
            }
        }
    }

    if has_current {
        argv.push(current);
    }
    argv
}

/// Parse a single .desktop file. Returns None if it should not be shown
/// (NoDisplay/Hidden, not an Application, TryExec missing, or excluded by
/// OnlyShowIn/NotShowIn).
pub fn parse_desktop(path: &Path, contents: &str) -> Option<AppEntry> {
    parse_desktop_in(path, contents, &DesktopEnv::from_env())
}

/// The parts of the environment that affect parsing: which localized names we
/// accept, and which desktop we claim to be. Taken as a value so callers read
/// the environment once per scan, and so tests are not at the mercy of the
/// process environment.
#[derive(Clone, Debug, Default)]
pub struct DesktopEnv {
    /// Locale suffixes to accept, best match first.
    pub locales: Vec<String>,
    /// `$XDG_CURRENT_DESKTOP`, split on `:`.
    pub desktops: Vec<String>,
}

impl DesktopEnv {
    pub fn from_env() -> Self {
        let raw_locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|var| std::env::var(var).ok())
            .filter(|v| !v.is_empty() && v != "C" && v != "POSIX")
            .unwrap_or_default();
        let desktops = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        Self {
            locales: locale_candidates(&raw_locale),
            desktops: desktops
                .split(':')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        }
    }
}

/// [`parse_desktop`] against an explicit environment.
pub fn parse_desktop_in(path: &Path, contents: &str, env: &DesktopEnv) -> Option<AppEntry> {
    let id = path.file_stem()?.to_string_lossy().to_string();
    let locales = &env.locales;

    let mut name: Option<String> = None;
    // Rank of the locale that supplied `name`; lower is a better match, and
    // usize::MAX marks the unlocalized fallback.
    let mut name_rank = usize::MAX;
    let mut exec = None;
    let mut icon = String::new();
    let mut in_entry = false;
    let mut typ = String::new();
    let (mut no_display, mut hidden) = (false, false);
    let (mut terminal, mut dbus_activatable) = (false, false);
    let mut work_path = None;
    let mut try_exec = None;
    let (mut only_show_in, mut not_show_in) = (None, None);

    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (key, locale) = split_locale(k.trim());
        let v = unescape_value(v.trim());

        match key {
            "Name" => {
                // Prefer the best locale match, and let any localized value
                // beat the unlocalized one.
                let rank = match locale {
                    Some(l) => match locales.iter().position(|c| c == l) {
                        Some(rank) => rank,
                        None => continue, // a locale we do not speak
                    },
                    None => usize::MAX,
                };
                if name.is_none() || rank < name_rank {
                    name = Some(v);
                    name_rank = rank;
                }
            }
            "Exec" => exec = Some(v),
            "Icon" => icon = v,
            "Type" => typ = v,
            "Path" => work_path = Some(PathBuf::from(v)),
            "TryExec" => try_exec = Some(v),
            "NoDisplay" => no_display = v == "true",
            "Hidden" => hidden = v == "true",
            "Terminal" => terminal = v == "true",
            "DBusActivatable" => dbus_activatable = v == "true",
            "OnlyShowIn" => only_show_in = Some(v),
            "NotShowIn" => not_show_in = Some(v),
            _ => {}
        }
    }

    if no_display || hidden || typ != "Application" {
        return None;
    }
    if !show_in_this_desktop(
        only_show_in.as_deref(),
        not_show_in.as_deref(),
        &env.desktops,
    ) {
        return None;
    }
    // TryExec names the binary to test for: if it is not installed, the entry
    // is not supposed to be shown at all.
    if let Some(try_exec) = try_exec {
        resolve_program(&try_exec)?;
    }

    // Exec is required, except for entries that are launched over D-Bus.
    let exec = match exec {
        Some(exec) => exec,
        None if dbus_activatable => String::new(),
        None => return None,
    };

    Some(AppEntry {
        id,
        name: name?,
        exec,
        icon,
        terminal,
        path: work_path,
        dbus_activatable,
        desktop_file: path.to_path_buf(),
    })
}

/// A resolved command line for an entry: what to run, and where.
#[derive(Clone, Debug, PartialEq)]
pub struct LaunchCommand {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
}

/// Resolve an entry to the command that launches it.
///
/// Handles the three things a bare `Exec` split does not: `Terminal=true` apps
/// are wrapped in a terminal emulator (otherwise a CLI program is spawned with
/// no tty and dies immediately), `Path=` becomes the working directory, and a
/// D-Bus-activated entry with no `Exec` is launched through `gio launch`.
///
/// Returns None if there is nothing runnable, including when a terminal app
/// cannot be run because no terminal emulator is installed.
pub fn launch_command(entry: &AppEntry) -> Option<LaunchCommand> {
    let mut argv = if entry.exec.is_empty() {
        if !entry.dbus_activatable {
            return None;
        }
        // We do not speak D-Bus activation ourselves; gio does, and ships with
        // glib on any system running GTK apps.
        vec![
            "gio".to_string(),
            "launch".to_string(),
            entry.desktop_file.to_string_lossy().to_string(),
        ]
    } else {
        parse_exec_with(
            &entry.exec,
            &ExecContext {
                icon: &entry.icon,
                name: &entry.name,
                desktop_file: &entry.desktop_file.to_string_lossy(),
            },
        )
    };

    if argv.is_empty() {
        return None;
    }

    if entry.terminal {
        let mut wrapped = terminal_prefix()?;
        wrapped.append(&mut argv);
        argv = wrapped;
    }

    Some(LaunchCommand {
        argv,
        cwd: entry.path.clone(),
    })
}

/// Terminal emulator argv prefix that runs the rest of the line as a command.
/// `$TERMINAL` wins; otherwise the first known emulator on PATH, since the flag
/// that means "run this command" differs between them.
fn terminal_prefix() -> Option<Vec<String>> {
    const TERMINALS: &[(&str, &[&str])] = &[
        ("foot", &["-e"]),
        ("alacritty", &["-e"]),
        ("kitty", &[]),
        ("wezterm", &["start", "--"]),
        ("gnome-terminal", &["--"]),
        ("xterm", &["-e"]),
    ];

    if let Ok(terminal) = std::env::var("TERMINAL") {
        if !terminal.is_empty() && resolve_program(&terminal).is_some() {
            let args = TERMINALS
                .iter()
                .find(|(name, _)| terminal.ends_with(name))
                .map(|(_, args)| *args)
                .unwrap_or(&["-e"]);
            let mut prefix = vec![terminal];
            prefix.extend(args.iter().map(|a| a.to_string()));
            return Some(prefix);
        }
    }

    TERMINALS.iter().find_map(|(name, args)| {
        resolve_program(name)?;
        let mut prefix = vec![name.to_string()];
        prefix.extend(args.iter().map(|a| a.to_string()));
        Some(prefix)
    })
}

/// Split `Name[nb_NO]` into `("Name", Some("nb_NO"))`.
fn split_locale(key: &str) -> (&str, Option<&str>) {
    match key.split_once('[') {
        Some((key, rest)) => (key, rest.strip_suffix(']')),
        None => (key, None),
    }
}

/// Unescape a desktop-entry string value: `\s` is a space, plus `\n`, `\t`,
/// `\r` and `\\`. Applied before Exec quoting, per the spec.
fn unescape_value(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            // Not a defined escape: keep both characters, so an Exec value
            // like "C:\x" survives to the quoting stage intact.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The spec's fallback order: lang_COUNTRY@MODIFIER, lang_COUNTRY, lang@MODIFIER, lang.
fn locale_candidates(raw: &str) -> Vec<String> {
    // Encoding is not used for matching.
    let raw = raw.split('.').next().unwrap_or(raw);
    let (base, modifier) = match raw.split_once('@') {
        Some((base, modifier)) => (base, Some(modifier)),
        None => (raw, None),
    };
    if base.is_empty() {
        return Vec::new();
    }
    let lang = base.split('_').next().unwrap_or(base);
    let has_country = base != lang;

    let mut candidates = Vec::new();
    if let Some(modifier) = modifier {
        if has_country {
            candidates.push(format!("{base}@{modifier}"));
        }
        candidates.push(base.to_string());
        candidates.push(format!("{lang}@{modifier}"));
    } else if has_country {
        candidates.push(base.to_string());
    }
    candidates.push(lang.to_string());
    candidates.dedup();
    candidates
}

/// Apply OnlyShowIn/NotShowIn against the current desktop names.
fn show_in_this_desktop(
    only_show_in: Option<&str>,
    not_show_in: Option<&str>,
    current: &[String],
) -> bool {
    let listed = |list: &str| {
        list.split(';')
            .filter(|s| !s.is_empty())
            .any(|env| current.iter().any(|c| c == env))
    };

    if let Some(only) = only_show_in {
        return listed(only);
    }
    if let Some(not) = not_show_in {
        return !listed(not);
    }
    true
}

/// Find an executable: a path with a `/` is checked directly, a bare name is
/// looked up in `$PATH`.
fn resolve_program(program: &str) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable(&path).then_some(path);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| is_executable(candidate))
    })
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SAMPLE: &str =
        "[Desktop Entry]\nType=Application\nName=Maps\nExec=gnome-maps %U\nIcon=org.gnome.Maps\n";

    #[test]
    fn parses_basic_entry() {
        let e = parse_desktop(Path::new("/x/org.gnome.Maps.desktop"), SAMPLE).unwrap();
        assert_eq!(e.id, "org.gnome.Maps");
        assert_eq!(e.name, "Maps");
        assert_eq!(e.exec, "gnome-maps %U");
        assert_eq!(e.icon, "org.gnome.Maps");
    }

    #[test]
    fn skips_nodisplay() {
        let hidden = format!("{SAMPLE}NoDisplay=true\n");
        assert!(parse_desktop(Path::new("/x/a.desktop"), &hidden).is_none());
    }

    #[test]
    fn skips_non_application() {
        let link = "[Desktop Entry]\nType=Link\nName=X\nURL=http://x\n";
        assert!(parse_desktop(Path::new("/x/a.desktop"), link).is_none());
    }

    #[test]
    fn parse_exec_strips_field_codes() {
        assert_eq!(parse_exec("gnome-maps %U"), ["gnome-maps"]);
        assert_eq!(parse_exec("firefox %u %F"), ["firefox"]);
        assert_eq!(
            parse_exec("env VAR=1 app %f --flag"),
            ["env", "VAR=1", "app", "--flag"]
        );
        assert_eq!(parse_exec("%i%c%k app"), ["app"]);
        assert_eq!(parse_exec("foot"), ["foot"]);
        assert_eq!(parse_exec("app --arg val"), ["app", "--arg", "val"]);
    }

    #[test]
    fn parse_exec_unquotes_arguments() {
        // The program itself may be quoted - previously this reached
        // Command::new with the quote characters still attached and no such
        // path existed, so the app silently failed to start.
        assert_eq!(
            parse_exec(r#""/nix/store/x y/bin/app" --run "https://h/?a=1&b=2""#),
            ["/nix/store/x y/bin/app", "--run", "https://h/?a=1&b=2"]
        );
        // A quoted argument may contain whitespace
        assert_eq!(
            parse_exec(r#"app "two words" tail"#),
            ["app", "two words", "tail"]
        );
        assert_eq!(parse_exec(r#"app """#), ["app", ""]);
    }

    #[test]
    fn parse_exec_accepts_single_quotes() {
        // Not in the spec, but common in the wild
        assert_eq!(
            parse_exec("'/nix/store/x y/bin/app' --run 'https://h/?a=1&b=2'"),
            ["/nix/store/x y/bin/app", "--run", "https://h/?a=1&b=2"]
        );
        assert_eq!(parse_exec("app 'two words'"), ["app", "two words"]);
        // Literal, shell-style: no escape processing inside single quotes
        assert_eq!(parse_exec(r"app 'a\b'"), ["app", r"a\b"]);
        // The other quote character is just a character inside them
        assert_eq!(parse_exec(r#"app 'say "hi"'"#), ["app", r#"say "hi""#]);
        assert_eq!(parse_exec(r#"app "it's""#), ["app", "it's"]);
    }

    #[test]
    fn parse_exec_handles_escapes() {
        assert_eq!(parse_exec(r#"app "a\"b""#), [r#"app"#, r#"a"b"#]);
        assert_eq!(parse_exec(r#"app "a\\b""#), [r"app", r"a\b"]);
        assert_eq!(parse_exec(r#"app "p\$v""#), ["app", "p$v"]);
        // Not an escapable character: the backslash is kept
        assert_eq!(parse_exec(r#"app "C:\x""#), [r"app", r"C:\x"]);
    }

    #[test]
    fn parse_exec_keeps_percent_encoding_in_quoted_urls() {
        // Field codes are not recognised inside quotes, so %20 survives rather
        // than being eaten as a field code
        assert_eq!(
            parse_exec(r#"app "https://h/a%20b?x=1""#),
            ["app", "https://h/a%20b?x=1"]
        );
        // %% is an escaped literal percent
        assert_eq!(parse_exec("app 50%%"), ["app", "50%"]);
    }

    #[test]
    fn expands_icon_name_and_desktop_file_codes() {
        let ctx = ExecContext {
            icon: "org.gnome.Maps",
            name: "Maps of the World",
            desktop_file: "/x/org.gnome.Maps.desktop",
        };
        assert_eq!(
            parse_exec_with("app %i", &ctx),
            ["app", "--icon", "org.gnome.Maps"]
        );
        // %c is one argument even though the name contains spaces
        assert_eq!(
            parse_exec_with("app %c", &ctx),
            ["app", "Maps of the World"]
        );
        assert_eq!(
            parse_exec_with("app %k", &ctx),
            ["app", "/x/org.gnome.Maps.desktop"]
        );
        // Nothing to expand to: the code disappears rather than leaving an
        // empty argument behind
        assert_eq!(
            parse_exec_with("app %i %c", &ExecContext::default()),
            ["app"]
        );
    }

    #[test]
    fn unescapes_string_values() {
        let entry = parse_desktop(
            Path::new("/x/a.desktop"),
            "[Desktop Entry]\nType=Application\nName=Foo\\sBar\nExec=app\\sname\nIcon=i\n",
        )
        .unwrap();
        assert_eq!(entry.name, "Foo Bar");
        assert_eq!(entry.exec, "app name");
        assert_eq!(unescape_value(r"a\\b"), r"a\b");
        assert_eq!(unescape_value(r"line\nbreak"), "line\nbreak");
        // Undefined escape: both characters survive
        assert_eq!(unescape_value(r"C:\x"), r"C:\x");
    }

    #[test]
    fn locale_fallback_order_follows_spec() {
        assert_eq!(
            locale_candidates("sr_RS@latin.UTF-8"),
            ["sr_RS@latin", "sr_RS", "sr@latin", "sr"]
        );
        assert_eq!(locale_candidates("nb_NO.UTF-8"), ["nb_NO", "nb"]);
        assert_eq!(locale_candidates("de"), ["de"]);
        assert!(locale_candidates("").is_empty());
    }

    fn env_for(locale: &str, desktops: &[&str]) -> DesktopEnv {
        DesktopEnv {
            locales: locale_candidates(locale),
            desktops: desktops.iter().map(|d| d.to_string()).collect(),
        }
    }

    #[test]
    fn picks_best_localized_name() {
        let contents = "[Desktop Entry]\nType=Application\nExec=x\n\
                        Name=Files\nName[nb]=Filer\nName[nb_NO]=Filer NO\nName[de]=Dateien\n";
        let parse = |locale: &str| {
            parse_desktop_in(
                Path::new("/x/a.desktop"),
                contents,
                &env_for(locale, &["springchick"]),
            )
            .unwrap()
            .name
        };

        // Most specific match wins, then the language, then unlocalized
        assert_eq!(parse("nb_NO.UTF-8"), "Filer NO");
        assert_eq!(parse("nb"), "Filer");
        assert_eq!(parse("de_DE.UTF-8"), "Dateien");
        assert_eq!(parse(""), "Files");
        // A locale we do not speak must not win over the unlocalized name
        assert_eq!(parse("fr_FR"), "Files");
    }

    #[test]
    fn localized_name_wins_regardless_of_line_order() {
        // The localized line comes first here; the unlocalized one must not
        // overwrite it
        let contents = "[Desktop Entry]\nType=Application\nExec=x\nName[nb]=Filer\nName=Files\n";
        let entry =
            parse_desktop_in(Path::new("/x/a.desktop"), contents, &env_for("nb", &[])).unwrap();
        assert_eq!(entry.name, "Filer");
    }

    #[test]
    fn only_show_in_filters_by_current_desktop() {
        let contents = "[Desktop Entry]\nType=Application\nName=A\nExec=x\nOnlyShowIn=GNOME;KDE;\n";
        let shown = |desktops: &[&str]| {
            parse_desktop_in(Path::new("/x/a.desktop"), contents, &env_for("", desktops)).is_some()
        };
        assert!(!shown(&["springchick"]));
        assert!(shown(&["GNOME"]));
        // XDG_CURRENT_DESKTOP may list several
        assert!(shown(&["springchick", "KDE"]));
    }

    #[test]
    fn not_show_in_excludes_listed_desktops() {
        let contents = "[Desktop Entry]\nType=Application\nName=A\nExec=x\nNotShowIn=GNOME;\n";
        let shown = |desktops: &[&str]| {
            parse_desktop_in(Path::new("/x/a.desktop"), contents, &env_for("", desktops)).is_some()
        };
        assert!(shown(&["springchick"]));
        assert!(!shown(&["GNOME"]));
    }

    #[test]
    fn parses_terminal_path_and_dbus_keys() {
        let contents = "[Desktop Entry]\nType=Application\nName=Top\nExec=htop\n\
                        Terminal=true\nPath=/var/log\nDBusActivatable=true\n";
        let entry = parse_desktop(Path::new("/x/top.desktop"), contents).unwrap();
        assert!(entry.terminal);
        assert_eq!(entry.path, Some(PathBuf::from("/var/log")));
        assert!(entry.dbus_activatable);
        assert_eq!(entry.desktop_file, PathBuf::from("/x/top.desktop"));
    }

    #[test]
    fn dbus_activatable_entry_may_omit_exec() {
        let with_exec = "[Desktop Entry]\nType=Application\nName=A\nDBusActivatable=true\n";
        let entry = parse_desktop(Path::new("/x/a.desktop"), with_exec).unwrap();
        assert!(entry.exec.is_empty());
        // gio speaks D-Bus activation for us
        let command = launch_command(&entry).unwrap();
        assert_eq!(command.argv, ["gio", "launch", "/x/a.desktop"]);

        // Without DBusActivatable, a missing Exec still drops the entry
        let no_exec = "[Desktop Entry]\nType=Application\nName=A\n";
        assert!(parse_desktop(Path::new("/x/a.desktop"), no_exec).is_none());
    }

    #[test]
    fn hides_entries_whose_tryexec_is_missing() {
        let missing = "[Desktop Entry]\nType=Application\nName=A\nExec=x\n\
                       TryExec=/nonexistent/springchick-test-binary\n";
        assert!(parse_desktop(Path::new("/x/a.desktop"), missing).is_none());

        // An installed binary keeps the entry
        let present = "[Desktop Entry]\nType=Application\nName=A\nExec=x\nTryExec=/bin/sh\n";
        assert!(parse_desktop(Path::new("/x/a.desktop"), present).is_some());
    }

    #[test]
    fn show_in_defaults_to_visible() {
        assert!(show_in_this_desktop(None, None, &[]));
    }

    #[test]
    fn launch_command_carries_working_directory() {
        let entry = AppEntry {
            exec: "app --flag".into(),
            path: Some(PathBuf::from("/var/log")),
            ..Default::default()
        };
        let command = launch_command(&entry).unwrap();
        assert_eq!(command.argv, ["app", "--flag"]);
        assert_eq!(command.cwd, Some(PathBuf::from("/var/log")));
    }

    #[test]
    fn terminal_apps_are_wrapped_in_a_terminal() {
        let entry = AppEntry {
            exec: "htop".into(),
            terminal: true,
            ..Default::default()
        };
        // When a terminal emulator is installed, htop must not be argv[0] but
        // must still be the command being run. When none is installed,
        // launch_command returns None rather than spawning a tty-less CLI app.
        if let Some(command) = launch_command(&entry) {
            assert_ne!(command.argv.first().map(String::as_str), Some("htop"));
            assert_eq!(command.argv.last().map(String::as_str), Some("htop"));
        }
    }

    #[test]
    fn parse_exec_handles_empty_input() {
        assert!(parse_exec("").is_empty());
        assert!(parse_exec("   ").is_empty());
        assert!(parse_exec("%U").is_empty());
    }
}
