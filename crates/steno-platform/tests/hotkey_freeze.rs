//! The Caps Lock grab must never blackhole the keyboard.
//!
//! WHY: the hotkey uses a passive grab with `GrabMode::SYNC`, so every
//! Caps Lock press freezes the WHOLE keyboard inside the X server until
//! someone calls `XAllowEvents`. When the daemon serviced that grab from
//! its own main loop, a Caps Lock press that arrived while the daemon was
//! transcribing, refining, or typing left the freeze in place until the
//! daemon came back — and forever if it never did. Every key on the
//! machine died. This suite pins the invariant that the unfreeze happens
//! independently of the application: a grab is held while the "app" does
//! nothing at all, and ordinary keys must still be delivered.
//!
//! What it does NOT catch: a keyboard frozen by some other X client, and
//! the pure-Wayland evdev backend (no X freeze semantics there).
//!
//! Skipped (not failed) when Xvfb is unavailable. Never runs against a
//! live session: it always creates its own throwaway X server.

#![cfg(target_os = "linux")]

use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const CAPS_KEYCODE: u8 = 66;
/// Keycode of `a` on a stock evdev/xfree86 layout.
const KEY_A: u8 = 38;
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;

/// `DISPLAY` and the grab are process-global: two of these tests running
/// concurrently would grab each other's server.
fn display_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

struct Xvfb {
    child: Child,
    display: String,
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a throwaway X server on a display number nothing else owns.
fn start_xvfb() -> Option<Xvfb> {
    for n in 90..110 {
        let display = format!(":{n}");
        if RustConnection::connect(Some(&display)).is_ok() {
            continue; // already taken
        }
        let child = Command::new("Xvfb")
            .args([&display, "-screen", "0", "800x600x24"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let server = Xvfb { child, display };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if RustConnection::connect(Some(&server.display)).is_ok() {
                return Some(server);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        return None;
    }
    None
}

/// A focused window on its own connection that records core key presses.
fn focused_listener(display: &str) -> (RustConnection, u32) {
    let (conn, screen_num) = RustConnection::connect(Some(display)).expect("listener connect");
    let screen = &conn.setup().roots[screen_num];
    let win = conn.generate_id().expect("window id");
    conn.create_window(
        screen.root_depth,
        win,
        screen.root,
        0,
        0,
        100,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE),
    )
    .expect("create window");
    conn.map_window(win).expect("map window");
    conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
        .expect("focus");
    conn.sync().expect("sync");
    (conn, win)
}

fn fake_key(conn: &RustConnection, code: u8, press: bool) {
    let ty = if press { KEY_PRESS } else { KEY_RELEASE };
    conn.xtest_fake_input(ty, code, 0, x11rb::NONE, 0, 0, 0)
        .expect("xtest fake key");
    conn.flush().expect("flush");
}

/// Wait for a core KeyPress of `code` on the listener connection.
fn saw_key(conn: &RustConnection, code: u8, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match conn.poll_for_event() {
            Ok(Some(Event::KeyPress(e))) if e.detail == code => return true,
            Ok(Some(_)) => continue,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return false,
        }
    }
    false
}

#[test]
fn ordinary_keys_survive_a_caps_press_while_the_app_never_polls() {
    let _guard = display_lock();
    let Some(server) = start_xvfb() else {
        eprintln!("skipping: Xvfb unavailable");
        return;
    };
    unsafe { std::env::set_var("DISPLAY", &server.display) };

    let (listener, _win) = focused_listener(&server.display);
    let (injector, _) = RustConnection::connect(Some(&server.display)).expect("injector connect");

    // Baseline: without any grab the listener sees a normal key. Without
    // this the test could pass vacuously on a display that delivers
    // nothing at all.
    fake_key(&injector, KEY_A, true);
    fake_key(&injector, KEY_A, false);
    assert!(
        saw_key(&listener, KEY_A, Duration::from_secs(2)),
        "baseline: focused window never received a key press — harness is broken"
    );

    let _hotkey = steno_platform::create_hotkey().expect("grab caps lock");

    // The application does NOTHING from here on: no next_event, no
    // drain_pending. This is the daemon mid-transcription.
    fake_key(&injector, CAPS_KEYCODE, true);
    std::thread::sleep(Duration::from_millis(300));
    fake_key(&injector, CAPS_KEYCODE, false);
    std::thread::sleep(Duration::from_millis(100));

    fake_key(&injector, KEY_A, true);
    fake_key(&injector, KEY_A, false);
    assert!(
        saw_key(&listener, KEY_A, Duration::from_secs(3)),
        "keyboard is frozen: a Caps Lock press while the app was busy blackholed every key"
    );
}

#[test]
fn keyboard_is_usable_again_after_the_hotkey_is_dropped() {
    let _guard = display_lock();
    let Some(server) = start_xvfb() else {
        eprintln!("skipping: Xvfb unavailable");
        return;
    };
    unsafe { std::env::set_var("DISPLAY", &server.display) };

    let (listener, _win) = focused_listener(&server.display);
    let (injector, _) = RustConnection::connect(Some(&server.display)).expect("injector connect");

    {
        let _hotkey = steno_platform::create_hotkey().expect("grab caps lock");
        // Leave the grab activated and unserviced, then tear down: Drop
        // must unfreeze rather than hand back a dead keyboard.
        fake_key(&injector, CAPS_KEYCODE, true);
        std::thread::sleep(Duration::from_millis(100));
        fake_key(&injector, CAPS_KEYCODE, false);
    }

    while listener.poll_for_event().ok().flatten().is_some() {}
    fake_key(&injector, KEY_A, true);
    fake_key(&injector, KEY_A, false);
    assert!(
        saw_key(&listener, KEY_A, Duration::from_secs(3)),
        "keyboard still frozen after the hotkey was dropped"
    );
}
