//! Exercise the real hotkey grab for off-host testing.
//! `cargo run --release --example hotkey_demo [seconds]`
//! Prints every hotkey event (Press/Release/Cancel) so injected keystrokes
//! on a throwaway X server can verify grab + cancel-any-key semantics.

use std::time::Duration;

use steno_platform::{Hotkey, HotkeyEvent};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `hotkey_demo inject <keycode> <down|up|tap> [hold_ms]` — raw XTEST
    // keycode injection. xdotool remaps a spare keycode for named keys,
    // which bypasses both our grab and the NoSymbol swallow; raw keycodes
    // behave like real hardware.
    if args.first().map(String::as_str) == Some("inject") {
        inject(&args);
        return;
    }
    // `hotkey_demo inject-seq "66:down 800 53:tap 400 66:up ..."` — one
    // persistent connection, space-separated `keycode:mode` steps, each
    // followed by a pause in ms. Multiple short-lived injector clients
    // confuse Xvfb's XTEST device when a grab is active.
    if args.first().map(String::as_str) == Some("inject-seq") {
        inject_seq(args.get(1).map(String::as_str).unwrap_or(""));
        return;
    }
    let secs: u64 = args
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let debug = std::env::var_os("HK_DEBUG").is_some();
    let mut hk = match Hotkey::grab_caps_lock() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("grab failed: {e:#}");
            std::process::exit(1);
        }
    };
    if debug {
        println!("trigger keycode = {}", hk.trigger_keycode());
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

fn inject(args: &[String]) {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xtest::ConnectionExt as _;
    use x11rb::protocol::xproto::{KEY_PRESS_EVENT, KEY_RELEASE_EVENT};
    let keycode: u8 = args.get(1).and_then(|s| s.parse().ok()).expect("inject <keycode>");
    let mode = args.get(2).map(String::as_str).unwrap_or("tap");
    let hold_ms: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let (conn, _) = x11rb::rust_connection::RustConnection::connect(None).expect("connect");
    let fake = |typ: u8| {
        conn.xtest_fake_input(typ, keycode, 0, x11rb::NONE, 0, 0, 0)
            .expect("fake_input");
        conn.flush().expect("flush");
    };
    match mode {
        "down" => fake(KEY_PRESS_EVENT),
        "up" => fake(KEY_RELEASE_EVENT),
        "tap" => {
            fake(KEY_PRESS_EVENT);
            if hold_ms > 0 {
                std::thread::sleep(Duration::from_millis(hold_ms));
            }
            fake(KEY_RELEASE_EVENT);
        }
        other => panic!("unknown inject mode {other}"),
    }
    println!("injected {mode} keycode={keycode}");
}

fn inject_seq(script: &str) {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{KEY_PRESS_EVENT, KEY_RELEASE_EVENT};
    use x11rb::protocol::xtest::ConnectionExt as _;
    let (conn, _) = x11rb::rust_connection::RustConnection::connect(None).expect("connect");
    let fake = |typ: u8, keycode: u8| {
        conn.xtest_fake_input(typ, keycode, 0, x11rb::NONE, 0, 0, 0)
            .expect("fake_input");
        conn.flush().expect("flush");
    };
    for tok in script.split_whitespace() {
        if let Ok(ms) = tok.parse::<u64>() {
            std::thread::sleep(Duration::from_millis(ms));
            continue;
        }
        let (kc, mode) = tok.split_once(':').expect("step must be keycode:mode");
        let keycode: u8 = kc.parse().expect("keycode must be a number");
        match mode {
            "down" => fake(KEY_PRESS_EVENT, keycode),
            "up" => fake(KEY_RELEASE_EVENT, keycode),
            "tap" => {
                fake(KEY_PRESS_EVENT, keycode);
                std::thread::sleep(Duration::from_millis(30));
                fake(KEY_RELEASE_EVENT, keycode);
            }
            other => panic!("unknown step mode {other}"),
        }
    }
    println!("sequence done");
}
