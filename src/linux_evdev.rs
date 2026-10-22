//! Keyboards read straight from /dev/input. On Wayland desktops whose
//! shortcuts portal cannot serve Hex, such as GNOME, they carry Hex's
//! shortcuts, and paste reads them to wait for held modifiers. Reading needs
//! the `input` group. It sees every key and suppresses none, so a shortcut
//! also reaches the focused application.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// From <linux/input.h> and <linux/input-event-codes.h>. The bitmaps cover
// every key up to `KEY_MAX`.
const EVIOCGKEY: libc::Ioctl = 0x8060_4518;
const EVIOCGBIT_KEY: libc::Ioctl = 0x8060_4521;
const BITMAP_BYTES: usize = 96;
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_DROPPED: u16 = 3;
pub(crate) const KEY_ESC: u16 = 1;
const KEY_A: u16 = 30;
/// Left and right Ctrl, Shift, Alt, and Super.
const MODIFIER_KEYS: [u16; 8] = [29, 97, 42, 54, 56, 100, 125, 126];
/// Keyboards plugged in later join at the next scan.
const RESCAN_INTERVAL: Duration = Duration::from_secs(1);

/// The modifiers a shortcut names, or that keyboards hold.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Modifiers {
    pub(crate) control: bool,
    pub(crate) alt: bool,
    pub(crate) shift: bool,
    pub(crate) super_key: bool,
}

impl Modifiers {
    /// `held` is indexed like `MODIFIER_KEYS`.
    fn of(held: [bool; 8]) -> Self {
        Self {
            control: held[0] || held[1],
            shift: held[2] || held[3],
            alt: held[4] || held[5],
            super_key: held[6] || held[7],
        }
    }
}

/// A key press or release, with the modifiers every keyboard held at that
/// moment. Auto-repeats are dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KeyEvent {
    pub(crate) code: u16,
    pub(crate) pressed: bool,
    pub(crate) modifiers: Modifiers,
}

struct Keyboard {
    path: PathBuf,
    file: File,
    /// Which `MODIFIER_KEYS` are down, from the events read so far.
    held: [bool; 8],
}

impl Keyboard {
    /// Reads the modifiers down now. The kernel then drops the key events
    /// queued on this file, so it runs only before reading or after a drop.
    fn sync(&mut self) {
        if let Some(keys) = bitmap(&self.file, EVIOCGKEY) {
            for (held, key) in self.held.iter_mut().zip(MODIFIER_KEYS) {
                *held = is_set(&keys, key);
            }
        }
    }
}

pub(crate) struct Keyboards {
    devices: Vec<Keyboard>,
    scanned: Instant,
}

impl Keyboards {
    /// Every keyboard Hex can read; none without access to /dev/input.
    pub(crate) fn open() -> Self {
        let mut keyboards = Self {
            devices: Vec::new(),
            scanned: Instant::now(),
        };
        keyboards.scan();
        keyboards
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    fn scan(&mut self) {
        self.scanned = Instant::now();
        let Ok(entries) = std::fs::read_dir("/dev/input") else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_name().as_bytes().starts_with(b"event")
                || self.devices.iter().any(|keyboard| keyboard.path == path)
            {
                continue;
            }
            let Ok(file) = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
            else {
                continue;
            };
            if bitmap(&file, EVIOCGBIT_KEY).is_some_and(|keys| is_set(&keys, KEY_A)) {
                let mut keyboard = Keyboard {
                    path,
                    file,
                    held: [false; 8],
                };
                keyboard.sync();
                self.devices.push(keyboard);
            }
        }
    }

    /// The key presses and releases since the last call, and whether a
    /// keyboard went away or dropped events, so held keys are unknown.
    pub(crate) fn read(&mut self) -> (Vec<KeyEvent>, bool) {
        if self.scanned.elapsed() >= RESCAN_INTERVAL {
            self.scan();
        }
        let mut events = Vec::new();
        let mut lost = false;
        let size = size_of::<libc::input_event>();
        let mut buffer = vec![0u8; size * 64];
        let mut index = 0;
        while index < self.devices.len() {
            let mut others = [false; 8];
            for (other, keyboard) in self.devices.iter().enumerate() {
                if other != index {
                    for (held, down) in others.iter_mut().zip(keyboard.held) {
                        *held |= down;
                    }
                }
            }
            let keyboard = &mut self.devices[index];
            let read = match keyboard.file.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    index += 1;
                    continue;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    lost = true;
                    self.devices.remove(index);
                    continue;
                }
            };
            for chunk in buffer[..read].chunks_exact(size) {
                let event =
                    unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast::<libc::input_event>()) };
                match (event.type_, event.value) {
                    (EV_KEY, 0 | 1) => {
                        let pressed = event.value == 1;
                        if let Some(slot) = MODIFIER_KEYS.iter().position(|&key| key == event.code)
                        {
                            keyboard.held[slot] = pressed;
                        }
                        let mut held = keyboard.held;
                        for (held, other) in held.iter_mut().zip(others) {
                            *held |= other;
                        }
                        events.push(KeyEvent {
                            code: event.code,
                            pressed,
                            modifiers: Modifiers::of(held),
                        });
                    }
                    (EV_SYN, _) if event.code == SYN_DROPPED => {
                        lost = true;
                        keyboard.sync();
                        break;
                    }
                    _ => {}
                }
            }
        }
        (events, lost)
    }

    /// The modifiers any keyboard holds.
    pub(crate) fn modifiers(&self) -> Modifiers {
        let mut held = [false; 8];
        for keyboard in &self.devices {
            for (held, down) in held.iter_mut().zip(keyboard.held) {
                *held |= down;
            }
        }
        Modifiers::of(held)
    }
}

/// Whether a keyboard holds Ctrl, Shift, Alt, or Super now. It opens the
/// keyboards afresh, so it never drops a shortcut listener's queued keys.
/// Without access to /dev/input nothing counts as held.
pub(crate) fn modifiers_held() -> bool {
    Keyboards::open().modifiers() != Modifiers::default()
}

fn bitmap(device: &File, request: libc::Ioctl) -> Option<[u8; BITMAP_BYTES]> {
    let mut bits = [0u8; BITMAP_BYTES];
    let result = unsafe { libc::ioctl(device.as_raw_fd(), request, bits.as_mut_ptr()) };
    (result >= 0).then_some(bits)
}

fn is_set(bits: &[u8; BITMAP_BYTES], key: u16) -> bool {
    let key = usize::from(key);
    bits[key / 8] & (1 << (key % 8)) != 0
}

/// The evdev code of a key by the xkb keysym name the portal uses, on a US
/// layout: the physical key, whatever the active layout prints on it.
pub(crate) fn key_code(name: &str) -> Option<u16> {
    let rows = [
        ("1234567890", 2),
        ("qwertyuiop", 16),
        ("asdfghjkl", 30),
        ("zxcvbnm", 44),
    ];
    if let [character] = name.as_bytes() {
        return rows.iter().find_map(|(row, first)| {
            row.bytes()
                .position(|key| key == *character)
                .map(|index| first + index as u16)
        });
    }
    if let Some(number) = name.strip_prefix('F').and_then(|n| n.parse::<u16>().ok()) {
        return match number {
            1..=10 => Some(58 + number),
            11 | 12 => Some(76 + number),
            13..=24 => Some(170 + number),
            _ => None,
        };
    }
    Some(match name {
        "space" => 57,
        "Return" => 28,
        "Tab" => 15,
        "BackSpace" => 14,
        "minus" => 12,
        "equal" => 13,
        "bracketleft" => 26,
        "bracketright" => 27,
        "semicolon" => 39,
        "apostrophe" => 40,
        "grave" => 41,
        "backslash" => 43,
        "comma" => 51,
        "period" => 52,
        "slash" => 53,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::key_code;

    #[test]
    fn keysym_names_map_to_us_layout_evdev_codes() {
        for (name, code) in [
            ("m", 50),
            ("q", 16),
            ("a", 30),
            ("1", 2),
            ("0", 11),
            ("space", 57),
            ("F1", 59),
            ("F10", 68),
            ("F12", 88),
            ("F13", 183),
            ("F24", 194),
            ("slash", 53),
        ] {
            assert_eq!(key_code(name), Some(code), "{name}");
        }
        assert_eq!(key_code("F25"), None);
        assert_eq!(key_code("Menu"), None);
    }
}
