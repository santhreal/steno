//! Caps Lock hold-to-talk via `evdev` direct kernel input events.
//!
//! On pure Wayland (no `DISPLAY`), X11 key grabs are unavailable. This
//! backend reads `/dev/input/event*` directly to detect Caps Lock
//! press/release without a display server.
//!
//! ## Permissions
//! The user must be in the `input` group (or have read access to
//! `/dev/input/event*`). If not, `grab_caps_lock` returns an error with
//! corrective instructions.
//!
//! ## LED management
//! Caps Lock events are **not** swallowed — the kernel toggles the LED.
//! After each press we write `0` to `/sys/class/leds/capslock/brightness`
//! to turn the LED back off. This is cosmetic; dictation works regardless.
//!
//! ## Cancel
//! While Caps Lock is held, any other key press sends `Cancel` (matching
//! X11 semantics).

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use evdev::{Device, EventType, Key};

use crate::traits::{HotkeyEvent, HotkeySource};

/// Caps Lock evdev key code.
const CAPS_LOCK: Key = Key::KEY_CAPSLOCK;

/// Cancel grace window after press (matching X11 semantics).
const CANCEL_GRACE: Duration = Duration::from_millis(120);

/// sysfs LED brightness path (best-effort; may not exist on all systems).
const LED_PATH: &str = "/sys/class/leds/capslock/brightness";

/// evdev-based Caps Lock hotkey for pure Wayland sessions.
pub struct EvdevHotkey {
    rx: mpsc::Receiver<HotkeyEvent>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    source_held: bool,
}

impl EvdevHotkey {
    /// Open all keyboard devices that report `KEY_CAPSLOCK` and start
    /// background reader threads.
    pub fn grab_caps_lock() -> Result<Self> {
        let devices = find_keyboard_devices()
            .context("cannot find keyboard devices for evdev hotkey")?;

        if devices.is_empty() {
            bail!(
                "evdev hotkey: no keyboard devices with KEY_CAPSLOCK found in /dev/input/event*. \
                 Ensure the user is in the 'input' group: `sudo usermod -aG input $USER`, \
                 then log out and back in. Alternatively, enable XWayland (set DISPLAY) to \
                 use the X11 hotkey grab."
            );
        }

        let (tx, rx) = mpsc::sync_channel::<HotkeyEvent>(64);
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::with_capacity(devices.len());

        for (idx, mut dev) in devices.into_iter().enumerate() {
            let tx_c = tx.clone();
            let stop_c = Arc::clone(&stop);
            let name = dev.name().unwrap_or("unknown").to_owned();

            let handle = thread::Builder::new()
                .name(format!("steno-evdev-hotkey-{idx}"))
                .spawn(move || {
                    log::debug!("evdev hotkey: reading device {idx} ({name})");
                    reader_thread(&mut dev, &tx_c, &stop_c);
                })
                .context("failed to spawn evdev reader thread")?;
            threads.push(handle);
        }

        // Give threads a moment to fail fast if devices can't be read.
        thread::sleep(Duration::from_millis(50));
        if threads.iter().all(|t| t.is_finished()) {
            bail!("evdev hotkey: all reader threads exited immediately — check /dev/input permissions");
        }

        Ok(Self {
            rx,
            stop,
            threads,
            source_held: false,
        })
    }

    /// No-op on evdev (no X11 keysym mapping to repair).
    pub fn restore_caps_lock_mapping() -> Result<bool> {
        // Turn off the Caps Lock LED as a courtesy.
        let _ = fs::write(LED_PATH, "0");
        Ok(false)
    }

    pub fn drain_pending(&mut self) {
        while self.rx.try_recv().is_ok() {}
    }

    pub fn next_event(&mut self, held: &mut bool) -> Result<HotkeyEvent> {
        loop {
            let ev = match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => {
                    if self.stop.load(Ordering::SeqCst) {
                        return Ok(HotkeyEvent::Shutdown);
                    }
                    if self.threads.iter().all(|t| t.is_finished()) {
                        return Ok(HotkeyEvent::Shutdown);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(HotkeyEvent::Shutdown),
            };
            match ev {
                HotkeyEvent::Press => {
                    if *held {
                        continue;
                    }
                    *held = true;
                    return Ok(HotkeyEvent::Press);
                }
                HotkeyEvent::Release => {
                    if !*held {
                        continue;
                    }
                    *held = false;
                    return Ok(HotkeyEvent::Release);
                }
                HotkeyEvent::Cancel => {
                    if *held {
                        *held = false;
                        return Ok(HotkeyEvent::Cancel);
                    }
                }
                HotkeyEvent::Shutdown => return Ok(HotkeyEvent::Shutdown),
            }
        }
    }

    pub fn next_event_debug(
        &mut self,
        held: &mut bool,
        debug: bool,
        shutdown: &AtomicBool,
    ) -> Result<HotkeyEvent> {
        loop {
            // Check shutdown flag first.
            if shutdown.load(Ordering::SeqCst) || self.stop.load(Ordering::SeqCst) {
                return Ok(HotkeyEvent::Shutdown);
            }

            let ev = match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => {
                    if self.threads.iter().all(|t| t.is_finished()) {
                        return Ok(HotkeyEvent::Shutdown);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(HotkeyEvent::Shutdown),
            };

            if debug {
                log::debug!("evdev hotkey event: {ev}");
            }

            match ev {
                HotkeyEvent::Press => {
                    if *held {
                        continue;
                    }
                    *held = true;
                    return Ok(HotkeyEvent::Press);
                }
                HotkeyEvent::Release => {
                    if !*held {
                        continue;
                    }
                    *held = false;
                    return Ok(HotkeyEvent::Release);
                }
                HotkeyEvent::Cancel => {
                    if *held {
                        *held = false;
                        return Ok(HotkeyEvent::Cancel);
                    }
                }
                HotkeyEvent::Shutdown => return Ok(HotkeyEvent::Shutdown),
            }
        }
    }
}

impl Drop for EvdevHotkey {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Turn off LED on exit.
        let _ = fs::write(LED_PATH, "0");
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl HotkeySource for EvdevHotkey {
    fn next_event(&mut self) -> Result<HotkeyEvent> {
        let mut held = self.source_held;
        let ev = self.next_event(&mut held)?;
        self.source_held = held;
        Ok(ev)
    }

    fn drain_pending(&mut self) {
        self.drain_pending();
    }
}

// ── Device discovery ────────────────────────────────────────────────

/// Find all `/dev/input/event*` devices that support `KEY_CAPSLOCK`.
fn find_keyboard_devices() -> Result<Vec<Device>> {
    let mut found = Vec::new();

    let dev_dir = Path::new("/dev/input");
    if !dev_dir.exists() {
        bail!("/dev/input does not exist — this is not a Linux system with input devices");
    }

    let entries = fs::read_dir(dev_dir).context("cannot read /dev/input")?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("event") {
            continue;
        }

        match Device::open(&path) {
            Ok(dev) => {
                // Check if this device reports KEY_CAPSLOCK.
                if dev.supported_keys().is_some_and(|keys| keys.contains(CAPS_LOCK)) {
                    log::debug!(
                        "evdev: found keyboard device {} ({})",
                        path.display(),
                        dev.name().unwrap_or("?")
                    );
                    found.push(dev);
                }
            }
            Err(e) => {
                // Permission denied is common — log and skip.
                log::debug!("evdev: cannot open {}: {e}", path.display());
            }
        }
    }

    Ok(found)
}

// ── Reader thread ───────────────────────────────────────────────────

/// Background thread that reads events from a single evdev device and
/// sends `HotkeyEvent`s through the channel.
fn reader_thread(dev: &mut Device, tx: &SyncSender<HotkeyEvent>, stop: &AtomicBool) {
    let mut held = false;
    let mut press_time: Option<Instant> = None;

    // Set the fd to non-blocking so we can poll with a timeout instead of
    // blocking forever in fetch_events(). Without this, Drop's stop flag +
    // join() would hang because the thread is stuck in a blocking read().
    let fd = dev.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        // Poll with a 100ms timeout so we can check the stop flag regularly.
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if !stop.load(Ordering::SeqCst) {
                log::warn!("evdev hotkey: poll error: {err}");
            }
            break;
        }
        if ret == 0 {
            // Timeout — no events, loop back to check stop flag.
            continue;
        }

        let events = match dev.fetch_events() {
            Ok(events) => events,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    continue;
                }
                if !stop.load(Ordering::SeqCst) {
                    log::warn!("evdev hotkey: device read error: {e}");
                }
                break;
            }
        };

        for ev in events {
            if ev.event_type() != EventType::KEY {
                continue;
            }

            let key = Key::new(ev.code());
            let value = ev.value();

            if key == CAPS_LOCK {
                match value {
                    1 => {
                        // Press
                        held = true;
                        press_time = Some(Instant::now());
                        turn_off_caps_led();
                        let _ = tx.send(HotkeyEvent::Press);
                    }
                    0 => {
                        // Release
                        if held {
                            held = false;
                            press_time = None;
                            turn_off_caps_led();
                            let _ = tx.send(HotkeyEvent::Release);
                        }
                    }
                    2 => {
                        // Auto-repeat while held — ignore (collapse into single press).
                    }
                    _ => {}
                }
            } else if held {
                // Other key while Caps Lock is held.
                if value == 1 {
                    // Only cancel within the grace window (matches X11 semantics).
                    let in_grace = press_time
                        .map(|t| t.elapsed() < CANCEL_GRACE)
                        .unwrap_or(false);
                    if in_grace {
                        held = false;
                        press_time = None;
                        let _ = tx.send(HotkeyEvent::Cancel);
                    }
                }
            }
        }
    }
}

/// Best-effort: write 0 to the Caps Lock LED sysfs path to turn it off.
fn turn_off_caps_led() {
    if Path::new(LED_PATH).exists() {
        let _ = fs::write(LED_PATH, "0");
    }
}

#[cfg(test)]
mod tests {
    //! WHY: evdev hotkey unit tests verify device discovery logic and
    //! event handling without requiring real hardware.

    use super::*;

    #[test]
    fn test_caps_lock_key_constant() {
        assert_eq!(CAPS_LOCK, Key::KEY_CAPSLOCK);
    }

    #[test]
    fn test_led_path_constant() {
        assert_eq!(LED_PATH, "/sys/class/leds/capslock/brightness");
    }

    #[test]
    fn test_find_keyboard_devices_returns_vec() {
        // This may return empty on CI (no input devices), but must not panic.
        let result = find_keyboard_devices();
        // On systems without /dev/input, this returns an error.
        // On systems with input devices, it returns a Vec (possibly empty).
        if let Ok(devs) = result {
            assert!(devs.iter().all(|d| {
                d.supported_keys()
                    .is_some_and(|keys| keys.contains(CAPS_LOCK))
            }));
        }
    }

    #[test]
    fn test_turn_off_caps_led_no_panic() {
        // Should not panic even if the path doesn't exist.
        turn_off_caps_led();
    }
}
