//! Exercise the real hotkey grab for off-host testing.
//! `cargo run --release --example hotkey_demo [seconds]`
//! Prints every hotkey event (Press/Release/Cancel) so injected keystrokes
//! on a throwaway X server can verify grab + cancel-any-key semantics.

use std::time::Duration;

#[path = "../src/hotkey.rs"]
mod hotkey;

use hotkey::{Hotkey, HotkeyEvent};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let debug = std::env::var_os("HK_DEBUG").is_some();
    let mut hk = match Hotkey::grab_ctrl_space() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("grab failed: {e:#}");
            std::process::exit(1);
        }
    };
    if debug {
        println!("space keycode = {}", hk.space_keycode());
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut held = false;
    while std::time::Instant::now() < deadline {
        match hk.next_event_debug(&mut held, debug, &std::sync::atomic::AtomicBool::new(false)) {
            Ok(HotkeyEvent::Press) => println!("EVENT press"),
            Ok(HotkeyEvent::Release) => println!("EVENT release"),
            Ok(HotkeyEvent::Cancel) => println!("EVENT cancel"),
            Ok(HotkeyEvent::Shutdown) => break,
            Err(e) => {
                eprintln!("event error: {e:#}");
                std::process::exit(2);
            }
        }
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }
    println!("DONE");
}
