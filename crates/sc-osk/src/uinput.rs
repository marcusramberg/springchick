//! A keyboard through `/dev/uinput`, so every client (terminals, XWayland, the
//! VT) sees real key events. Unsafe, like main's SIGRTMIN handler. Closing the
//! fd removes the device.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;

const UI_SET_EVBIT: libc::Ioctl = 0x4004_5564;
const UI_SET_KEYBIT: libc::Ioctl = 0x4004_5565;
const UI_DEV_SETUP: libc::Ioctl = 0x405c_5503;
const UI_DEV_CREATE: libc::Ioctl = 0x5501;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const BUS_VIRTUAL: u16 = 0x06;
/// Every key a keyboard grid can name; beyond are buttons and media keys.
const LAST_KEY: u16 = 248;

#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

pub struct Uinput(File);

fn check(r: libc::c_int) -> io::Result<()> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Uinput {
    pub fn new(name: &str) -> io::Result<Self> {
        // Blocking: a short write would drop a keystroke, and one event
        // never fills the kernel's buffer.
        let file = OpenOptions::new().write(true).open("/dev/uinput")?;
        let fd = file.as_raw_fd();
        let mut setup = UinputSetup {
            bustype: BUS_VIRTUAL,
            vendor: 0x5343,
            product: 0x0001,
            version: 1,
            name: [0; 80],
            ff_effects_max: 0,
        };
        let n = name.len().min(79);
        setup.name[..n].copy_from_slice(&name.as_bytes()[..n]);
        // SAFETY: `fd` is open for the whole block; the int-argument ioctls
        // take values, and UI_DEV_SETUP only reads the `repr(C)` struct.
        unsafe {
            check(libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_ulong))?;
            for key in 1..=LAST_KEY {
                check(libc::ioctl(fd, UI_SET_KEYBIT, key as libc::c_ulong))?;
            }
            check(libc::ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup))?;
            check(libc::ioctl(fd, UI_DEV_CREATE))?;
        }
        Ok(Uinput(file))
    }

    pub fn key(&mut self, code: u16, down: bool) -> io::Result<()> {
        let mut buf = Vec::with_capacity(2 * 24);
        event(&mut buf, EV_KEY, code, down as i32);
        event(&mut buf, EV_SYN, 0, 0);
        self.0.write_all(&buf)
    }
}

/// `struct input_event`; a zero time is stamped by the kernel.
fn event(buf: &mut Vec<u8>, kind: u16, code: u16, value: i32) {
    buf.resize(buf.len() + std::mem::size_of::<libc::timeval>(), 0);
    buf.extend_from_slice(&kind.to_ne_bytes());
    buf.extend_from_slice(&code.to_ne_bytes());
    buf.extend_from_slice(&value.to_ne_bytes());
}
