use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, WrapErr, eyre};
use x11_clipboard::Clipboard;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, KeyButMask};
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;

use crate::linux_input::{KEY_PRESS, KEY_RELEASE, XK_CONTROL_L, XK_SHIFT_L, connect_x11};
use crate::linux_session::LinuxSession;

const XK_V: u32 = 0x76;
const HELPER_TIMEOUT: Duration = Duration::from_secs(3);
const PASTE_SETTLE: Duration = Duration::from_millis(100);
/// Process names of terminals, which paste with Ctrl+Shift+V.
const TERMINALS: &[&str] = &[
    "alacritty",
    "foot",
    "ghostty",
    "kgx",
    "kitty",
    "konsole",
    "ptyxis",
    "terminal",
    "tilix",
    "urxvt",
    "wezterm",
    "xterm",
];

pub struct LinuxPaster {
    backend: Backend,
    stop: Arc<AtomicBool>,
    paste_with_shift: bool,
}

enum Backend {
    X11(Box<X11Paster>),
    /// A kernel uinput keyboard, which every compositor reads, including
    /// GNOME and KDE without the virtual-keyboard protocol.
    Uinput(VirtualKeyboard),
    /// `wtype`'s virtual keyboard carries its own modifiers, so keys the
    /// user still holds never turn its Ctrl+V into another chord.
    Wtype,
}

/// How the focused application takes a transcript.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Target {
    /// Emacs binds C-v to scrolling, so its server inserts the text.
    Emacs,
    Keys {
        shift: bool,
    },
}

impl Target {
    fn of(application: Option<&str>, paste_with_shift: bool) -> Self {
        let application = application.unwrap_or_default();
        if application.contains("emacs") {
            return Self::Emacs;
        }
        let terminal = TERMINALS.iter().any(|name| application.contains(name));
        Self::Keys {
            shift: paste_with_shift || terminal,
        }
    }
}

impl LinuxPaster {
    pub fn new(stop: Arc<AtomicBool>, paste_with_shift: bool) -> Result<Self> {
        let backend = match LinuxSession::detect() {
            LinuxSession::X11 => Backend::X11(Box::new(X11Paster::new()?)),
            LinuxSession::Wayland => match VirtualKeyboard::new() {
                Ok(keyboard) => Backend::Uinput(keyboard),
                Err(error) => {
                    tracing::info!(error = %format!("{error:#}"), "pasting with wtype");
                    Backend::Wtype
                }
            },
        };
        Ok(Self {
            backend,
            stop,
            paste_with_shift,
        })
    }

    /// The focused application's selected text: the primary selection,
    /// which X11 and Wayland compositors update as the user selects.
    pub fn selected_text(&mut self) -> Option<String> {
        let bytes = match &mut self.backend {
            Backend::X11(paster) => {
                let atoms = &paster.clipboard.getter.atoms;
                paster
                    .clipboard
                    .load(
                        atoms.primary,
                        atoms.utf8_string,
                        atoms.property,
                        Duration::from_millis(500),
                    )
                    .ok()?
            }
            Backend::Uinput(_) | Backend::Wtype => {
                let output = Command::new("wl-paste")
                    .args(["--primary", "--no-newline", "--type", "text/plain"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .ok()?;
                output.status.success().then_some(output.stdout)?
            }
        };
        let text = String::from_utf8(bytes).ok()?;
        (!text.trim().is_empty()).then_some(text)
    }

    /// Puts the transcript on the clipboard, where it stays, and inserts it
    /// into the focused application.
    pub fn paste(&mut self, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Err(eyre!("refusing to paste an empty transcript"));
        }
        let application = crate::linux_context::frontmost_application();
        let target = Target::of(application.as_deref(), self.paste_with_shift);
        self.copy(text)?;
        match target {
            Target::Emacs => insert_into_emacs(text, &self.stop),
            Target::Keys { shift } => {
                self.send_paste(shift)?;
                // Give the target time to request the selection before the next queued
                // transcript replaces it. This is a bounded settling policy, not an ACK.
                thread::sleep(PASTE_SETTLE);
                Ok(())
            }
        }
    }

    fn copy(&mut self, text: &str) -> Result<()> {
        match &mut self.backend {
            Backend::X11(paster) => paster.copy(text),
            Backend::Uinput(_) | Backend::Wtype => {
                let mut copy = Command::new("wl-copy");
                copy.args(["--type", "text/plain;charset=utf-8"]);
                // wl-copy's parent exits only after the selection is installed.
                // Never read a pipe inherited by its clipboard-serving daemon.
                run_helper(copy, text.as_bytes(), &self.stop, HELPER_TIMEOUT)
                    .wrap_err("could not own the Wayland clipboard; install wl-clipboard and check compositor support")
            }
        }
    }

    /// Sends Ctrl+V, or Ctrl+Shift+V, to the focused application.
    fn send_paste(&mut self, shift: bool) -> Result<()> {
        match &mut self.backend {
            Backend::X11(paster) => paster.send_paste(&self.stop, shift),
            Backend::Uinput(keyboard) => {
                // The compositor merges this keyboard's keys with the physical
                // ones, so a shortcut modifier still held would join Ctrl+V.
                wait_for_modifiers(&self.stop, || Ok(crate::linux_evdev::modifiers_held()))?;
                keyboard.send_paste(shift)
            }
            Backend::Wtype => {
                let mut keys = Command::new("wtype");
                keys.args(paste_keys(shift));
                run_helper(keys, &[], &self.stop, HELPER_TIMEOUT)
                    .wrap_err("could not send the Wayland paste shortcut; give Hex write access to /dev/uinput, or install wtype and use a compositor supporting virtual keyboards")
            }
        }
    }
}

/// Inserts the transcript at point in Emacs's selected window through the
/// Emacs server, which `(server-start)` runs. No keys are involved, so held
/// shortcut modifiers cannot change it. The transcript stays off helper
/// argv: it travels in an owner-only file that is removed afterwards.
fn insert_into_emacs(text: &str, stop: &AtomicBool) -> Result<()> {
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = directory.join(format!("hex-emacs-{}.txt", std::process::id()));
    let result = (|| {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?
            .write_all(text.as_bytes())?;
        // Rust's string escapes for a path match Emacs Lisp's.
        let read = format!(
            "(with-temp-buffer (let ((coding-system-for-read 'utf-8)) (insert-file-contents {path:?})) (buffer-string))"
        );
        let mut emacsclient = Command::new("emacsclient");
        emacsclient.args([
            "--eval",
            &format!("(with-selected-window (selected-window) (insert {read}))"),
        ]);
        run_helper(emacsclient, &[], stop, HELPER_TIMEOUT)
    })();
    let _ = std::fs::remove_file(&path);
    result.wrap_err("Emacs did not insert the transcript, which is on the clipboard; start its server with (server-start)")
}

// From <linux/uinput.h> and <linux/input-event-codes.h>.
const UI_SET_EVBIT: libc::Ioctl = 0x4004_5564;
const UI_SET_KEYBIT: libc::Ioctl = 0x4004_5565;
const UI_DEV_SETUP: libc::Ioctl = 0x405c_5503;
const UI_DEV_CREATE: libc::Ioctl = 0x5501;
const BUS_VIRTUAL: u16 = 0x06;
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_REPORT: u16 = 0;
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTSHIFT: u16 = 42;
// lessismore: the QWERTY V position. Revisit when a layout moves V.
const KEY_V: u16 = 47;

/// A uinput keyboard that lives as long as the paster: compositors attach a
/// new device asynchronously, so one created per paste could lose its keys.
/// Closing the device removes it.
struct VirtualKeyboard {
    device: File,
}

impl VirtualKeyboard {
    fn new() -> Result<Self> {
        let device = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .wrap_err("could not open /dev/uinput")?;
        let fd = device.as_raw_fd();
        let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
        setup.id.bustype = BUS_VIRTUAL;
        for (byte, name) in setup.name.iter_mut().zip(b"Hex paste keyboard") {
            *byte = *name as libc::c_char;
        }
        // Compositors accept only devices that udev classifies as keyboards,
        // which takes the whole basic key range.
        let created = unsafe {
            libc::ioctl(fd, UI_SET_EVBIT, libc::c_int::from(EV_KEY)) >= 0
                && (1..248).all(|key: libc::c_int| libc::ioctl(fd, UI_SET_KEYBIT, key) >= 0)
                && libc::ioctl(fd, UI_DEV_SETUP, &setup) >= 0
                && libc::ioctl(fd, UI_DEV_CREATE) >= 0
        };
        if !created {
            return Err(std::io::Error::last_os_error())
                .wrap_err("could not create the uinput paste keyboard");
        }
        thread::sleep(Duration::from_millis(200));
        Ok(Self { device })
    }

    fn send_paste(&mut self, shift: bool) -> Result<()> {
        let keys: &[u16] = if shift {
            &[KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_V]
        } else {
            &[KEY_LEFTCTRL, KEY_V]
        };
        let presses = keys.iter().map(|&key| (key, 1));
        let releases = keys.iter().rev().map(|&key| (key, 0));
        for (key, value) in presses.chain(releases) {
            for (type_, code, value) in [(EV_KEY, key, value), (EV_SYN, SYN_REPORT, 0)] {
                let event = libc::input_event {
                    time: libc::timeval {
                        tv_sec: 0,
                        tv_usec: 0,
                    },
                    type_,
                    code,
                    value,
                };
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        (&raw const event).cast::<u8>(),
                        size_of::<libc::input_event>(),
                    )
                };
                self.device
                    .write_all(bytes)
                    .wrap_err("could not send the paste shortcut through uinput")?;
            }
            thread::sleep(Duration::from_millis(8));
        }
        Ok(())
    }
}

struct X11Paster {
    clipboard: Clipboard,
    connection: RustConnection,
    root: u32,
    control: u8,
    shift: u8,
    v: u8,
}

impl X11Paster {
    pub fn new() -> Result<Self> {
        let clipboard = Clipboard::new().wrap_err("could not open the X11 clipboard")?;
        let (connection, root, keymap) = connect_x11()?;
        Ok(Self {
            clipboard,
            connection,
            root,
            control: keymap.keycode(XK_CONTROL_L)?,
            shift: keymap.keycode(XK_SHIFT_L)?,
            v: keymap.keycode(XK_V)?,
        })
    }

    fn copy(&mut self, text: &str) -> Result<()> {
        let atoms = &self.clipboard.getter.atoms;
        self.clipboard
            .store(atoms.clipboard, atoms.utf8_string, text.as_bytes())
            .wrap_err("could not own the X11 clipboard")?;
        thread::sleep(Duration::from_millis(60));
        Ok(())
    }

    fn send_paste(&mut self, stop: &AtomicBool, shift: bool) -> Result<()> {
        self.wait_for_modifier_release(stop)?;
        for (type_, key) in [
            (KEY_PRESS, self.control),
            (KEY_PRESS, self.shift),
            (KEY_PRESS, self.v),
            (KEY_RELEASE, self.v),
            (KEY_RELEASE, self.shift),
            (KEY_RELEASE, self.control),
        ] {
            if key == self.shift && !shift {
                continue;
            }
            xtest::fake_input(&self.connection, type_, key, 0, self.root, 0, 0, 0)?.check()?;
        }
        self.connection.flush()?;
        Ok(())
    }

    fn wait_for_modifier_release(&self, stop: &AtomicBool) -> Result<()> {
        wait_for_modifiers(stop, || {
            let mask = self.connection.query_pointer(self.root)?.reply()?.mask;
            let held = mask
                & (KeyButMask::SHIFT | KeyButMask::CONTROL | KeyButMask::MOD1 | KeyButMask::MOD4);
            Ok(held != KeyButMask::default())
        })
    }
}

fn paste_keys(with_shift: bool) -> &'static [&'static str] {
    // GTK 3's reverse keymap lookup skips the final keycode. Keep V before a
    // neutral VoidSymbol release so its shortcut remains discoverable (#28).
    if with_shift {
        &[
            "-M",
            "ctrl",
            "-M",
            "shift",
            "-k",
            "v",
            "-m",
            "shift",
            "-m",
            "ctrl",
            "-p",
            "VoidSymbol",
        ]
    } else {
        &["-M", "ctrl", "-k", "v", "-m", "ctrl", "-p", "VoidSymbol"]
    }
}

fn wait_for_modifiers(stop: &AtomicBool, mut held: impl FnMut() -> Result<bool>) -> Result<()> {
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(eyre!("paste cancelled while stopping the listener"));
        }
        if !held()? {
            return Ok(());
        }
        // Another capture may own these modifiers. Keep its predecessor queued
        // instead of throwing away accepted output after an arbitrary timeout.
        thread::sleep(Duration::from_millis(20));
    }
}

fn run_helper(
    mut command: Command,
    input: &[u8],
    stop: &AtomicBool,
    timeout: Duration,
) -> Result<()> {
    if stop.load(Ordering::Acquire) {
        return Err(eyre!("paste cancelled before starting a helper"));
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .wrap_err("could not start the paste helper")?;
    let process_group = child.id();
    let result = (|| {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| eyre!("missing helper stdin"))?;
        let fd = stdin.as_raw_fd();
        // Nonblocking writes keep a stalled helper from bypassing cancellation
        // or the deadline when a long transcript fills its stdin pipe.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let deadline = Instant::now() + timeout;
        let mut remaining = input;
        while !remaining.is_empty() {
            if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(eyre!("paste helper cancelled or timed out"));
            }
            match stdin.write(remaining) {
                Ok(0) => return Err(eyre!("paste helper stopped reading its input")),
                Ok(written) => remaining = &remaining[written..],
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        drop(stdin);
        loop {
            if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(eyre!("paste helper cancelled or timed out"));
            }
            if let Some(status) = child.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(eyre!("paste helper failed with {status}"))
                };
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        // Terminate descendants too: a failed wl-copy must not claim the
        // clipboard later, after this request has been reported as cancelled.
        unsafe { libc::kill(-(process_group as i32), libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn helper_failure_is_not_reported_as_a_successful_paste() {
        assert!(
            run_helper(
                shell("exit 7"),
                &[],
                &AtomicBool::new(false),
                HELPER_TIMEOUT
            )
            .is_err()
        );
    }

    #[test]
    fn emacs_inserts_through_its_server_and_terminals_paste_with_shift() {
        assert_eq!(Target::of(Some(".emacs-30.2-wrapped"), true), Target::Emacs);
        assert_eq!(
            Target::of(Some("wezterm-gui"), false),
            Target::Keys { shift: true }
        );
        assert_eq!(
            Target::of(Some("firefox"), false),
            Target::Keys { shift: false }
        );
        assert_eq!(Target::of(None, true), Target::Keys { shift: true });
    }

    #[test]
    fn standard_and_terminal_paste_balance_their_modifiers() {
        assert_eq!(
            paste_keys(false),
            ["-M", "ctrl", "-k", "v", "-m", "ctrl", "-p", "VoidSymbol"]
        );
        assert_eq!(
            paste_keys(true),
            [
                "-M",
                "ctrl",
                "-M",
                "shift",
                "-k",
                "v",
                "-m",
                "shift",
                "-m",
                "ctrl",
                "-p",
                "VoidSymbol"
            ]
        );
    }

    #[test]
    fn helper_can_read_a_large_transcript_without_argv_or_files() {
        let text = b"--help\nplain text\n".repeat(32_768);
        let mut command = shell("test \"$(wc -c)\" -eq \"$1\"");
        command.args(["hex-paste-test", &text.len().to_string()]);
        assert!(run_helper(command, &text, &AtomicBool::new(false), HELPER_TIMEOUT).is_ok());
    }

    #[test]
    fn helper_timeout_includes_a_full_stdin_pipe() {
        let start = Instant::now();
        assert!(
            run_helper(
                shell("sleep 10"),
                &vec![b'x'; 1_048_576],
                &AtomicBool::new(false),
                Duration::from_millis(100),
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn stop_cancels_a_helper_that_never_finishes() {
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = stop.clone();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            cancel.store(true, Ordering::Release);
        });
        let start = Instant::now();
        let result = run_helper(shell("sleep 10"), &[], &stop, HELPER_TIMEOUT);
        worker.join().unwrap();
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_clipboard_daemon_inheriting_stderr_does_not_block_parent_completion() {
        let path = std::env::temp_dir().join(format!("hex-paste-daemon-{}", std::process::id()));
        let mut command = shell("cat >/dev/null; sleep 10 >&2 & echo $! >\"$1\"");
        command.arg("hex-paste-test").arg(&path);
        let result = run_helper(
            command,
            b"example",
            &AtomicBool::new(false),
            Duration::from_secs(2),
        );
        if let Ok(pid) = std::fs::read_to_string(&path) {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            let _ = std::fs::remove_file(path);
        }
        assert!(result.is_ok());
    }

    #[test]
    fn accepted_output_waits_through_another_capture() {
        let start = Instant::now();
        let mut checks = 0;
        wait_for_modifiers(&AtomicBool::new(false), || {
            checks += 1;
            Ok(start.elapsed() < Duration::from_millis(550))
        })
        .unwrap();
        assert!(checks > 1);
    }

    #[test]
    fn stopping_breaks_the_modifier_wait_without_checking_the_keyboard() {
        assert!(
            wait_for_modifiers(&AtomicBool::new(true), || panic!("should not read devices"))
                .is_err()
        );
        assert!(run_helper(shell("exit 0"), &[], &AtomicBool::new(true), HELPER_TIMEOUT).is_err());
    }

    #[test]
    #[ignore = "run scripts/test-wayland-paste.sh with the compiled test executable"]
    fn native_wayland_clipboard_shortcut() {
        assert!(LinuxSession::detect().is_wayland());
        let path = std::env::var_os("HEX_WAYLAND_PASTE_OUTPUT")
            .expect("the isolated Wayland test runner must supply its output file");
        let expected = "Example dictation: --flags, punctuation, \u{6f22}\u{5b57}.\nSecond line.";
        LinuxPaster::new(Arc::new(AtomicBool::new(false)), false)
            .unwrap()
            .paste(expected)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let actual = std::fs::read_to_string(&path).unwrap_or_default();
            if actual == expected {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "GTK did not receive the transcript: {actual:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}
