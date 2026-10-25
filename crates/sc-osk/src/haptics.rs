//! Key-down haptics through feedbackd, which also honours the user's
//! feedback profile (silent/quiet/full).

use dbus::blocking::Connection;
use dbus::channel::Sender;
use dbus::Message;

pub struct Haptics {
    conn: Connection,
}

impl Haptics {
    pub fn new() -> Option<Haptics> {
        Connection::new_session()
            .map_err(|e| eprintln!("sc-osk: no session bus ({e}); no haptics"))
            .ok()
            .map(|conn| Haptics { conn })
    }

    /// Fire and forget: a blocking call would stall the key on the bus.
    pub fn press(&self) {
        let Ok(msg) = Message::new_method_call(
            "org.sigxcpu.Feedback",
            "/org/sigxcpu/Feedback",
            "org.sigxcpu.Feedback",
            "TriggerFeedback",
        ) else {
            return;
        };
        let hints: dbus::arg::PropMap = Default::default();
        let mut msg = msg
            .append3("sc-osk", "button-pressed", hints)
            .append1(-1i32);
        msg.set_no_reply(true);
        let _ = self.conn.send(msg);
        self.conn.channel().flush();
    }
}
