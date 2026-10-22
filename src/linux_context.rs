//! The frontmost application on Linux, for dictation modes and paste. X11
//! reports the active window's process. On GNOME Wayland the FocusBridge
//! Shell extension reports it; other Wayland compositors don't expose the
//! focused window, so modes fall back to the global settings there.

use gtk::gio;
use gtk::glib::VariantTy;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

use crate::context::ContextSnapshot;

const FOCUS_BRIDGE: &str = "com.freestyle.FocusBridge";
const FOCUS_BRIDGE_TIMEOUT_MS: i32 = 250;

pub fn capture() -> ContextSnapshot {
    ContextSnapshot {
        application: frontmost_application(),
        ..ContextSnapshot::default()
    }
}

/// The focused window's process name, such as "firefox" or "wezterm-gui".
pub fn frontmost_application() -> Option<String> {
    let pid = if crate::linux_session::LinuxSession::detect().is_wayland() {
        focus_bridge_pid()?
    } else {
        x11_active_pid()?
    };
    process_name(pid)
}

fn x11_active_pid() -> Option<u32> {
    let (connection, screen) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    let active = intern(&connection, b"_NET_ACTIVE_WINDOW")?;
    let pid = intern(&connection, b"_NET_WM_PID")?;
    let window = property(&connection, root, active, AtomEnum::WINDOW)?;
    if window == 0 {
        return None;
    }
    property(&connection, window, pid, AtomEnum::CARDINAL)
}

/// The focused window's process, from the FocusBridge GNOME Shell extension
/// (freestyle's `integrations/gnome-focus-bridge`). GNOME hides the focused
/// window from other clients, so only an extension can report it. `None`
/// when the extension is missing, as on every other Wayland compositor.
fn focus_bridge_pid() -> Option<u32> {
    let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
    let reply = connection
        .call_sync(
            Some(FOCUS_BRIDGE),
            "/com/freestyle/FocusBridge",
            FOCUS_BRIDGE,
            "GetActiveWindow",
            None,
            Some(VariantTy::new("(s)").ok()?),
            gio::DBusCallFlags::NONE,
            FOCUS_BRIDGE_TIMEOUT_MS,
            gio::Cancellable::NONE,
        )
        .ok()?;
    let (payload,) = reply.get::<(String,)>()?;
    let window: serde_json::Value = serde_json::from_str(&payload).ok()?;
    window.get("pid")?.as_u64()?.try_into().ok()
}

fn intern(connection: &impl Connection, name: &[u8]) -> Option<u32> {
    Some(connection.intern_atom(true, name).ok()?.reply().ok()?.atom)
}

fn property(
    connection: &impl Connection,
    window: u32,
    property: u32,
    kind: AtomEnum,
) -> Option<u32> {
    connection
        .get_property(false, window, property, kind, 0, 1)
        .ok()?
        .reply()
        .ok()?
        .value32()?
        .next()
}

/// The process's executable name, lowercased, as the application catalog
/// names applications ("code", "firefox").
fn process_name(pid: u32) -> Option<String> {
    let executable = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    executable
        .as_deref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .or_else(|| std::fs::read_to_string(format!("/proc/{pid}/comm")).ok())
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_current_process_has_a_name() {
        assert!(super::process_name(std::process::id()).is_some());
    }
}
