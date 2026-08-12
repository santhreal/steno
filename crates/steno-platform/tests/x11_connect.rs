//! The X11 connection helper must reach a display that has no socket file.
//!
//! WHY: GDM/GNOME XWayland commonly binds ONLY the abstract Unix socket
//! (`\0/tmp/.X11-unix/Xn`) and creates no file in `/tmp/.X11-unix`, which
//! is empty on such a session. `RustConnection::connect(None)` tries the
//! socket file and TCP, so it fails there. The hotkey carried a private
//! fallback and worked; the overlay used the plain connect and silently
//! disabled itself, which is what "the UI is gone" looks like. Both now
//! share `linux_x11::conn::connect_x11`, and this pins that it handles a
//! display the plain connect cannot.
//!
//! Skipped (not failed) when Xvfb is unavailable. Always uses its own
//! throwaway X server, never the live session.

#![cfg(target_os = "linux")]

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};
use x11rb::rust_connection::RustConnection;

struct Xvfb(Child);

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_xvfb() -> Option<(Xvfb, u16)> {
    static NEXT: AtomicU16 = AtomicU16::new(160);
    for _ in 0..20 {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        if n >= 190 {
            return None;
        }
        let display = format!(":{n}");
        if RustConnection::connect(Some(&display)).is_ok() {
            continue;
        }
        let child = Command::new("Xvfb")
            .args([&display, "-screen", "0", "320x240x24"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let server = Xvfb(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if RustConnection::connect(Some(&display)).is_ok() {
                return Some((server, n));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    None
}

#[test]
fn connects_to_a_display_with_no_socket_file() {
    let Some((_server, n)) = start_xvfb() else {
        eprintln!("skipping: Xvfb unavailable");
        return;
    };
    let display = format!(":{n}");
    let socket = format!("/tmp/.X11-unix/X{n}");

    // Our own server's socket file, standing in for an XWayland session
    // that never created one. The abstract socket stays bound.
    if std::fs::remove_file(&socket).is_err() {
        eprintln!("skipping: no socket file to remove at {socket}");
        return;
    }

    unsafe { std::env::set_var("DISPLAY", &display) };
    assert!(
        RustConnection::connect(None).is_err(),
        "harness: the plain connect still worked, so this proves nothing"
    );
    assert!(
        steno_platform::linux_x11::conn::connect_x11().is_ok(),
        "connect_x11 could not reach a display that has only an abstract socket"
    );
}
