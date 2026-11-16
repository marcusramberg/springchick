//! The notification server apps send to: `org.freedesktop.Notifications` on
//! the session bus. libnotify always uses it, and GLib's `GNotification`
//! falls back to it outside a sandbox.
//!
//! Notifications stay until the user opens or clears them (or the app closes
//! them), like on iOS, so expiry timeouts are ignored.

use std::ffi::CString;
use std::os::fd::RawFd;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbus::arg::{prop_cast, PropMap};
use dbus::blocking::stdintf::org_freedesktop_dbus::RequestNameReply;
use dbus::blocking::SyncConnection;
use dbus::channel::{BusType, Channel, MatchingReceiver, Sender};
use dbus::message::MatchRule;
use dbus::Message;

use crate::center::{Notification, DEFAULT_ACTION};

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// Why a notification closed, as the specification numbers them.
const CLOSED_BY_USER: u32 = 2;
const CLOSED_BY_CALL: u32 = 3;
const INTROSPECTION: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.freedesktop.Notifications">
    <method name="GetCapabilities"><arg type="as" direction="out"/></method>
    <method name="Notify">
      <arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="s" direction="in"/>
      <arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="as" direction="in"/>
      <arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="out"/>
    </method>
    <method name="CloseNotification"><arg type="u" direction="in"/></method>
    <method name="GetServerInformation">
      <arg type="s" direction="out"/><arg type="s" direction="out"/>
      <arg type="s" direction="out"/><arg type="s" direction="out"/>
    </method>
    <signal name="NotificationClosed"><arg type="u"/><arg type="u"/></signal>
    <signal name="ActionInvoked"><arg type="u"/><arg type="s"/></signal>
  </interface>
</node>"#;

pub enum Event {
    Notify(Notification),
    /// The app took one back.
    Close(u32),
}

/// The server, and what tells apps what became of their notifications.
pub struct Handle {
    bus: SyncConnection,
    fd: RawFd,
}

impl Handle {
    /// Readable when apps have sent something: `process` then.
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    pub fn process(&self) {
        while let Ok(true) = self.bus.process(Duration::ZERO) {}
    }

    /// The user opened one, or pressed its `action` button: that action, if
    /// it has it, then it's done.
    pub fn opened(&self, n: &Notification, action: Option<&str>) {
        let action = action.unwrap_or(DEFAULT_ACTION);
        if n.actions.iter().any(|(key, _)| key == action) {
            signal(&self.bus, "ActionInvoked", |m| m.append2(n.id, action));
        }
        self.cleared(n.id);
    }

    pub fn cleared(&self, id: u32) {
        signal(&self.bus, "NotificationClosed", |m| {
            m.append2(id, CLOSED_BY_USER)
        });
    }
}

fn signal(bus: &SyncConnection, name: &str, args: impl FnOnce(Message) -> Message) {
    match Message::new_signal(PATH, NAME, name) {
        Ok(m) => {
            let _ = bus.send(args(m));
            bus.channel().flush();
        }
        Err(e) => eprintln!("sc-bar: {name}: {e}"),
    }
}

/// Other people's text, cut to what a card could ever show.
fn capped(s: &str, chars: usize) -> String {
    s.chars().take(chars).collect()
}

/// Reads a `Notify` call; `id` is the one it gets.
fn notification(msg: &Message, id: u32) -> Option<Notification> {
    let mut args = msg.iter_init();
    let app_name: String = args.read().ok()?;
    let _replaces: u32 = args.read().ok()?;
    let icon: String = args.read().ok()?;
    let summary: String = args.read().ok()?;
    let body: String = args.read().ok()?;
    let actions: Vec<String> = args.read().ok()?;
    let hints: PropMap = args.read().ok()?;
    let app = prop_cast::<String>(&hints, "desktop-entry")
        .map(|d| d.trim_end_matches(".desktop").to_string())
        .filter(|d| is_app_id(d));
    Some(Notification {
        id,
        app,
        app_name: capped(&app_name, 64),
        icon: if icon.len() > 4096 {
            String::new()
        } else {
            icon
        },
        summary: capped(&summary, 256),
        // The markup the specification allows is not drawn.
        body: capped(&strip_markup(&capped(&body, 4096)), 1024),
        // `main` wraps it: the font is there.
        lines: Vec::new(),
        actions: actions
            .as_chunks::<2>()
            .0
            .iter()
            .take(16)
            .map(|[key, label]| (capped(key, 64), capped(label, 32)))
            .collect(),
        transient: prop_cast::<bool>(&hints, "transient")
            .copied()
            .unwrap_or(false),
        received: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    })
}

/// It ends up in a line of springchick's IPC, which takes any verb.
fn is_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Drops `<b>`-style tags and decodes the entities that come with them.
fn strip_markup(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut tag = false;
    for c in body.chars() {
        match c {
            '<' => tag = true,
            '>' if tag => tag = false,
            _ if !tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Takes the notification server's name, if it is free; `send` gets what
/// apps send, from inside `Handle::process`.
pub fn serve(send: impl Fn(Event) + Send + Sync + 'static) -> Result<Handle, String> {
    let mut channel = Channel::get_private(BusType::Session).map_err(|e| e.to_string())?;
    channel.set_watch_enabled(true);
    let fd = channel.watch().fd;
    let bus = SyncConnection::from(channel);
    // Never from under another daemon, and never queued behind one.
    match bus.request_name(NAME, false, false, true) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        Ok(_) => return Err("another notification daemon is running".into()),
        Err(e) => return Err(e.to_string()),
    }
    let mut next_id = 0u32;
    bus.start_receive(
        MatchRule::new_method_call(),
        Box::new(move |msg, bus| {
            let member = msg.member().map(|m| m.to_string()).unwrap_or_default();
            let interface = msg.interface().map(|i| i.to_string()).unwrap_or_default();
            let reply = match (interface.as_str(), member.as_str()) {
                (NAME, "GetCapabilities") => msg.method_return().append1(vec![
                    "body",
                    "body-markup",
                    "actions",
                    "persistence",
                ]),
                (NAME, "GetServerInformation") => msg
                    .method_return()
                    .append3("springchick", "springchick", env!("CARGO_PKG_VERSION"))
                    .append1("1.2"),
                (NAME, "Notify") => {
                    let replaces: u32 = msg.get2::<String, u32>().1.unwrap_or(0);
                    // Only one handed out since the bar started: an older
                    // one would meet the counter again.
                    let id = if replaces != 0 && replaces <= next_id {
                        replaces
                    } else {
                        next_id = next_id.wrapping_add(1).max(1);
                        next_id
                    };
                    match notification(&msg, id) {
                        Some(n) => {
                            send(Event::Notify(n));
                            msg.method_return().append1(id)
                        }
                        None => error(&msg, "org.freedesktop.DBus.Error.InvalidArgs"),
                    }
                }
                (NAME, "CloseNotification") => {
                    if let Some(id) = msg.get1::<u32>() {
                        send(Event::Close(id));
                        signal(bus, "NotificationClosed", |m| m.append2(id, CLOSED_BY_CALL));
                    }
                    msg.method_return()
                }
                ("org.freedesktop.DBus.Introspectable", "Introspect") => {
                    msg.method_return().append1(INTROSPECTION)
                }
                _ => error(&msg, "org.freedesktop.DBus.Error.UnknownMethod"),
            };
            let _ = bus.send(reply);
            true
        }),
    );
    Ok(Handle { bus, fd })
}

fn error(msg: &Message, name: &str) -> Message {
    msg.error(&name.into(), &CString::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_dropped_and_entities_decoded() {
        assert_eq!(
            strip_markup("<b>Ada</b>: 1 &lt; 2 &amp;&amp; <i>so</i> on"),
            "Ada: 1 < 2 && so on"
        );
    }

    #[test]
    fn an_app_id_cannot_carry_a_second_ipc_line() {
        assert!(is_app_id("org.gnome.Console"));
        assert!(is_app_id("foot-server_2"));
        for bad in ["", "x\nquit", "foot new", "a/b"] {
            assert!(!is_app_id(bad), "{bad:?}");
        }
    }
}
