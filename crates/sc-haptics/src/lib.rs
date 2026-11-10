//! Haptics through feedbackd, which honours the user's feedback profile
//! (silent/quiet/full).

#![forbid(unsafe_code)]

use dbus::blocking::Connection;
use dbus::channel::Sender;
use dbus::Message;

pub struct Haptics {
    conn: Connection,
    app_id: &'static str,
}

impl Haptics {
    /// `None` without a session bus; the error says why.
    pub fn new(app_id: &'static str) -> Result<Haptics, dbus::Error> {
        Connection::new_session().map(|conn| Haptics { conn, app_id })
    }

    /// A short `button-pressed` tap.
    pub fn tap(&self) {
        self.trigger("button-pressed");
    }

    /// Any feedbackd event name (`button-pressed`, `window-close`,
    /// `message-new-instant`, …). Fire and forget: a blocking call would stall
    /// the caller on the bus.
    pub fn trigger(&self, event: &str) {
        let Ok(msg) = Message::new_method_call(
            "org.sigxcpu.Feedback",
            "/org/sigxcpu/Feedback",
            "org.sigxcpu.Feedback",
            "TriggerFeedback",
        ) else {
            return;
        };
        let hints: dbus::arg::PropMap = Default::default();
        let mut msg = msg.append3(self.app_id, event, hints).append1(-1i32);
        msg.set_no_reply(true);
        let _ = self.conn.send(msg);
        self.conn.channel().flush();
    }
}
