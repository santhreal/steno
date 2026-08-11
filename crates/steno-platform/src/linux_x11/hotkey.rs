//! Global Caps Lock grab via X11 (`XGrabKey` on the root window).
//!
//! Hold = record, release = stop. While recording, ANY other key cancels
//! the utterance (listened passively via XInput2 raw key events: the
//! key still reaches the focused app, nothing is swallowed). Modifier
//! keys never cancel.
//!
//! Caps Lock is swallowed via a SYNC passive grab: when the grabbed key
//! fires, the X server freezes the keyboard and queues the event without
//! processing the XKB Lock action. We receive the event, call
//! `XAllowEvents(AsyncKeyboard)` to discard it and unfreeze, so the Lock
//! modifier never latches and caps state never toggles. No keymap
//! modification is needed — when the daemon dies (even SIGKILL), the X
//! server automatically releases the passive grab and Caps Lock works
//! normally again instantly.
//!
//! Failures are loud: if another client already owns the grab (e.g. a
//! GNOME custom shortcut on the same key), we say so instead of
//! silently never firing.

use anyhow::{Context, Result, anyhow, bail};
use crate::traits::HotkeyEvent;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, XIEventMask};
use x11rb::protocol::xproto::{
    Allow, ConnectionExt as _, GrabMode, Keycode, Keysym, ModMask, Window,
};
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb_protocol::xauth;

/// X11 `CurrentTime` (0) — used with `allow_events` to release queued events.
const CURRENT_TIME: u32 = 0;
const NO_SYMBOL: Keysym = 0;
/// Typical PC keyboard Caps Lock keycode (evdev / xfree86).
const FALLBACK_CAPS_KEYCODE: Keycode = 66;
/// How long after Press a second key counts as a cancel vs. pre-held.
const CANCEL_GRACE: Duration = Duration::from_millis(120);
/// X11 keysym for Caps Lock.
const XK_CAPS_LOCK: Keysym = 0xffe5;


/// Global X11 Caps Lock push-to-talk hotkey grabber.
pub struct Hotkey {
    conn: RustConnection,
    root: Window,
    /// Caps Lock keycode -- the push-to-talk trigger.
    trigger: Keycode,
    /// Modifier keycodes (Shift/Ctrl/Alt/Super/Lock/Mod*): never cancel.
    modifiers: Vec<Keycode>,
    /// Device id of the "Virtual core XTEST keyboard" slave: its fake key
    /// events (xdotool — including the daemon's OWN typing) must never
    /// cancel an utterance.
    xtest_device: Option<u16>,
    /// When the current hold started (auto-repeat grace for cancels).
    press_at: Option<Instant>,
    /// Keys whose XI2 presses arrived during CANCEL_GRACE (already held
    /// before Caps Lock). Their auto-repeats must not Cancel after grace.
    suppress_cancel: HashSet<Keycode>,
    /// Peeked events held back from auto-repeat coalescing.
    pending: VecDeque<Event>,
    /// Modifier masks we grabbed (plain + Caps/NumLock variants).
    masks: Vec<ModMask>,
    /// Hold state for [`crate::HotkeySource::next_event`]. The daemon still
    /// passes its own `held` into the inherent methods.
    source_held: bool,
}
/// Restores Caps Lock keysyms if a prior daemon left the keycode mapped
/// to NoSymbol (SIGKILL / failed grab before `Hotkey` was constructed).
///
/// Safe to call when the daemon is not running. No-ops when Caps Lock is
/// already mapped. Returns whether a remap was applied.
pub fn restore_caps_lock_mapping() -> Result<bool> {
    let (conn, _screen_num) = connect_x11_for_restore()?;
    let Some((trigger, keysyms, per_slot)) = resolve_caps_trigger(&conn)? else {
        return Ok(false);
    };
    if !is_all_nosymbol(&keysyms) {
        return Ok(false);
    }
    let restore = recover_orig_keysyms(keysyms);
    let payload = pad_keysyms(&restore, per_slot);
    conn.change_keyboard_mapping(1, trigger, payload.len() as u8, &payload)?
        .check()
        .context("cannot restore Caps Lock keysyms")?;
    conn.flush()?;
    Ok(true)
}

fn connect_x11() -> Result<(RustConnection, usize)> {
    // Try DISPLAY first via x11rb's native connect (filesystem socket + TCP).
    if let Ok(res) = RustConnection::connect(None) {
        return Ok(res);
    }

    // x11rb only tries the filesystem socket /tmp/.X11-unix/Xn and TCP
    // localhost:6000+n. GDM/GNOME XWayland often listens ONLY on the
    // abstract socket (\0/tmp/.X11-unix/Xn) with no filesystem socket
    // file. Scan for display numbers and try abstract sockets.
    let display_num = std::env::var("DISPLAY")
        .ok()
        .and_then(|d| {
            // DISPLAY format: [protocol/][host]:display[.screen]
            // Abstract sockets are local-only; skip remote hosts.
            // Strip everything up to and including the last ':'.
            let after = d.rsplit_once(':')?.1;
            after.split('.').next().map(|s| s.to_string())
        });

    let mut candidates = Vec::new();
    if let Some(n) = &display_num {
        candidates.push(n.clone());
    }
    // Also scan for filesystem sockets. X11 sockets live in /tmp/.X11-unix/
    // on most systems, but some configurations use $XDG_RUNTIME_DIR/X11-unix/.
    let socket_dirs: Vec<String> = [
        Some("/tmp/.X11-unix".to_string()),
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|d| std::path::PathBuf::from(d).join("X11-unix").to_string_lossy().into_owned()),
    ]
    .into_iter()
    .flatten()
    .collect();
    for dir in &socket_dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                if let Some(num) = s.strip_prefix('X') {
                    if !candidates.contains(&num.to_string()) {
                        candidates.push(num.to_string());
                    }
                }
            }
        }
    }
    for n in ["0", "1", "2"] {
        if !candidates.iter().any(|c| c == n) {
            candidates.push(n.to_string());
        }
    }

    for num in &candidates {
        if let Ok(res) = connect_abstract(num) {
            return Ok(res);
        }
        // Also try x11rb's native connect for this display number.
        if let Ok(res) = RustConnection::connect(Some(&format!(":{num}"))) {
            return Ok(res);
        }
    }

    RustConnection::connect(None).context("cannot connect to X11: is DISPLAY set?")
}

/// Connect to an X11 display via the abstract Unix socket
/// (\0/tmp/.X11-unix/Xn). This is the only socket type GDM/GNOME
/// XWayland often provides — no filesystem socket file exists.
fn connect_abstract(display_num: &str) -> Result<(RustConnection, usize)> {
    use std::os::unix::io::FromRawFd;
    use std::os::unix::net::UnixStream;

    let path = format!("\0/tmp/.X11-unix/X{display_num}");
    let display: u16 = display_num.parse().unwrap_or(0);
    // Parse the screen number from DISPLAY (e.g. ":1.0" → screen 0).
    // Default to 0 when absent — most single-monitor and XWayland setups
    // only expose screen 0.
    let screen = std::env::var("DISPLAY")
        .ok()
        .and_then(|d| {
            d.rsplit_once(':')
                .and_then(|(_, after)| after.split('.').nth(1))
                .and_then(|s| s.parse::<usize>().ok())
        })
        .unwrap_or(0);

    // Create a Unix socket and connect to the abstract namespace address.
    let fd = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot create Unix socket for abstract X11 connection");
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as u16;
        let bytes = path.as_bytes();
        // sun_path is 108 bytes; abstract socket starts with \0.
        if bytes.len() >= addr.sun_path.len() {
            libc::close(fd);
            bail!("abstract socket path too long: {path}");
        }
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            addr.sun_path.as_mut_ptr() as *mut u8,
            bytes.len(),
        );
        let addr_len = (std::mem::size_of::<u16>() + bytes.len()) as libc::socklen_t;
        if libc::connect(fd, &addr as *const _ as *const libc::sockaddr, addr_len) < 0 {
            let err = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(err).context(format!(
                "cannot connect to abstract X11 socket {path}"
            ));
        }
        fd
    };

    // Wrap the raw fd in a UnixStream, then into x11rb's DefaultStream.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let (stream, (family, address)) = DefaultStream::from_unix_stream(stream)
        .context("cannot wrap abstract X11 socket as DefaultStream")?;

    // Get auth info from XAUTHORITY (or ~/.Xauthority).
    let (auth_name, auth_data) = xauth::get_auth(family, &address, display)
        .unwrap_or(None)
        .unwrap_or_else(|| (Vec::new(), Vec::new()));

    let conn = RustConnection::connect_to_stream_with_auth_info(
        stream, screen, auth_name, auth_data,
    )
    .context("cannot complete X11 handshake on abstract socket")?;

    Ok((conn, screen))
}

fn connect_x11_for_restore() -> Result<(RustConnection, usize)> {
    connect_x11()
}

impl Hotkey {
    /// Grab Caps Lock system-wide on the default display. Uses a SYNC
    /// passive grab so the XKB Lock action never fires while we own the
    /// grab. No keymap modification — Caps Lock works normally the
    /// instant the daemon exits (even SIGKILL).
    pub fn grab_caps_lock() -> Result<Self> {
        let (conn, screen_num) = connect_x11()
            .context("cannot connect to X11: is DISPLAY set?")?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let (trigger, _mapped, _per_slot) = resolve_caps_trigger(&conn)?.ok_or_else(|| {
            anyhow!(
                "keyboard has no Caps Lock keycode: cannot bind it. \
                 If Caps Lock was blackholed by a killed daemon, run: \
                 xmodmap -e 'keycode 66 = Caps_Lock'"
            )
        })?;

        // No keymap remap: a SYNC passive grab freezes the keyboard when
        // Caps Lock fires, preventing the XKB Lock action from latching.
        // We call XAllowEvents(AsyncKeyboard) to discard the event and
        // unfreeze. When the daemon dies (even SIGKILL), the X server
        // releases the grab automatically — Caps Lock works again
        // instantly with no recovery needed.
        remember_caps_keycode(trigger);

        // NumLock (Mod2) and friends are sticky; grab every combo so the
        // hotkey still fires when they are on.
        let mask_list = [
            ModMask::from(0u16),
            ModMask::LOCK,
            ModMask::M2,
            ModMask::LOCK | ModMask::M2,
        ];

        let mut masks = Vec::new();
        for mask in mask_list {
            let cookie = conn
                .grab_key(
                    false, // owner_events: we alone get the events
                    root,
                    mask,
                    trigger,
                    GrabMode::ASYNC, // pointer: not affected
                    GrabMode::SYNC,  // keyboard: freeze to prevent XKB Lock latch
                )
                .with_context(|| format!("XGrabKey(CapsLock, mask={mask:?}) request failed"))?;
            if let Err(e) = cookie.check() {
                // Release any grabs we already established before bailing.
                for m in &masks {
                    let _ = conn.ungrab_key(trigger, root, *m);
                }
                bail!(
                    "XGrabKey(CapsLock) failed ({e}). Another client may already own that \
                     shortcut: remove any GNOME, KDE, sxhkd, or custom window manager binding \
                     on Caps Lock and retry."
                );
            }
            masks.push(mask);
        }
        conn.flush()?;

        // Passive cancel listener: raw key presses for every key, still
        // delivered to the focused app (nothing is grabbed or swallowed).
        let version = xinput::xi_query_version(&conn, 2, 0)?
            .reply()
            .context("XIQueryVersion failed: XInput2 is required for cancel-any-key")?;
        if version.major_version < 2 {
            for m in &masks {
                let _ = conn.ungrab_key(trigger, root, *m);
            }
            bail!(
                "X server has XInput {}.{}, need 2.0+ for cancel-any-key: upgrade the X server",
                version.major_version,
                version.minor_version
            );
        }
        xinput::xi_select_events(
            &conn,
            root,
            &[xinput::EventMask {
                deviceid: xinput::Device::ALL_MASTER.into(),
                mask: vec![XIEventMask::RAW_KEY_PRESS],
            }],
        )?
        .check()
        .context("XISelectEvents(RawKeyPress) failed")?;
        let modifier_reply = conn
            .get_modifier_mapping()?
            .reply()
            .context("GetModifierMapping failed")?;
        let modifiers = modifier_reply.keycodes;
        // Find the XTEST slave so its synthetic keys can't cancel.
        let xtest_device = xinput::xi_query_device(&conn, xinput::Device::ALL)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| {
                r.infos.iter().find_map(|info| {
                    let name = String::from_utf8_lossy(&info.name);
                    if info.type_ == xinput::DeviceType::SLAVE_KEYBOARD && name.contains("XTEST") {
                        Some(info.deviceid)
                    } else {
                        None
                    }
                })
            });
        conn.flush()?;

        Ok(Self {
            conn,
            root,
            trigger,
            modifiers,
            xtest_device,
            press_at: None,
            suppress_cancel: HashSet::new(),
            pending: VecDeque::new(),
            masks,
            source_held: false,
        })
    }

    /// Discard any queued events. Called after the daemon finishes typing
    /// so late raw events from its own xdotool keystrokes cannot leak
    /// into the next utterance (belt-and-suspenders over the XTEST
    /// device filter).
    #[allow(dead_code)] // used by the daemon binary, not the example harness
    pub fn drain_pending(&mut self) {
        self.pending.clear();
        let _ = self.conn.flush();
        while let Ok(Some(_)) = self.conn.poll_for_event() {}
    }


    /// The resolved trigger keycode (used by the off-host test harness).
    #[allow(dead_code)]
    pub fn trigger_keycode(&self) -> Keycode {
        self.trigger
    }

    /// Block until the next Press or Release of the grabbed combo.
    /// Auto-repeats while held are collapsed into a single Press.
    /// Convenience wrapper with shutdown polling disabled (used by the
    /// off-host test harness).
    #[allow(dead_code)]
    pub fn next_event(&mut self, held: &mut bool) -> Result<HotkeyEvent> {
        self.next_event_debug(held, false, &AtomicBool::new(false))
    }

    /// True once the cancel grace window after the activating press has
    /// elapsed (see CANCEL_GRACE).
    fn past_grace(&self) -> bool {
        self.press_at.is_none_or(|t| t.elapsed() >= CANCEL_GRACE)
    }

    /// `next_event` with raw event logging and a shutdown flag the
    /// daemon's SIGTERM handler sets.
    pub fn next_event_debug(
        &mut self,
        held: &mut bool,
        debug: bool,
        shutdown: &AtomicBool,
    ) -> Result<HotkeyEvent> {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return Ok(HotkeyEvent::Shutdown);
            }
            self.conn
                .flush()
                .context("X11 flush failed while waiting for Caps Lock")?;
            let ev = match self.pending.pop_front() {
                Some(e) => Some(e),
                None => self.conn.poll_for_event().context("X11 poll failed")?,
            };
            if debug {
                match &ev {
                    Some(Event::KeyPress(e)) => {
                        println!("RAW press detail={} state={:?}", e.detail, e.state)
                    }
                    Some(Event::KeyRelease(e)) => {
                        println!("RAW release detail={} state={:?}", e.detail, e.state)
                    }
                    Some(Event::XinputRawKeyPress(e)) => {
                        println!("RAW xinput detail={}", e.detail)
                    }
                    Some(other) => println!("RAW {other:?}"),
                    None => {}
                }
            }
            match ev {
                None => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Some(Event::KeyPress(ev)) => {
                    if ev.detail != self.trigger {
                        // While the grab is active (key held), other keys
                        // are delivered to us: that is the user cancelling.
                        // Same rules as XI2 RawKeyPress (grace + pre-held).
                        if *held
                            && should_cancel_key(
                                ev.detail,
                                self.trigger,
                                &self.modifiers,
                                !self.past_grace(),
                                &mut self.suppress_cancel,
                            )
                        {
                            *held = false;
                            self.suppress_cancel.clear();
                            return Ok(HotkeyEvent::Cancel);
                        }
                        continue;
                    }
                    if *held {
                        // Auto-repeat: unfreeze and discard, then continue.
                        let _ = self.conn.allow_events(Allow::ASYNC_KEYBOARD, CURRENT_TIME);
                        let _ = self.conn.flush();
                        continue;
                    }
                    // Unfreeze the keyboard and discard the queued event
                    // so the XKB Lock action never fires.
                    let _ = self.conn.allow_events(Allow::ASYNC_KEYBOARD, CURRENT_TIME);
                    let _ = self.conn.flush();
                    *held = true;
                    self.press_at = Some(Instant::now());
                    self.suppress_cancel.clear();
                    return Ok(HotkeyEvent::Press);
                }
                Some(Event::KeyRelease(ev)) => {
                    if !*held || ev.detail != self.trigger {
                        continue;
                    }
                    // Unfreeze the keyboard so the next event (auto-repeat
                    // press or a different key) can arrive for peek-ahead.
                    let _ = self.conn.allow_events(Allow::ASYNC_KEYBOARD, CURRENT_TIME);
                    let _ = self.conn.flush();
                    // X auto-repeat emits a release+press pair with the
                    // SAME timestamp for a held key. Peek past unrelated
                    // events (esp. XI2 RawKeyPress) for a matching press —
                    // otherwise an interleaved XI2 event looks like a real
                    // release and cuts the utterance at the repeat delay.
                    let mut deferred: Vec<Event> = Vec::new();
                    let mut is_repeat = false;
                    while let Ok(Some(peeked)) = self.conn.poll_for_event() {
                        match &peeked {
                            Event::KeyPress(p)
                                if p.detail == ev.detail && p.time == ev.time =>
                            {
                                is_repeat = true;
                                break;
                            }
                            Event::KeyPress(_) | Event::KeyRelease(_) => {
                                deferred.push(peeked);
                                break;
                            }
                            _ => deferred.push(peeked),
                        }
                    }
                    if is_repeat {
                        // Drop deferred XI2 noise from this coalesce window.
                        continue;
                    }
                    self.pending.extend(deferred);
                    *held = false;
                    self.suppress_cancel.clear();
                    return Ok(HotkeyEvent::Release);
                }
                Some(Event::XinputRawKeyPress(ev)) => {
                    if !*held {
                        continue; // normal typing while idle is not a cancel
                    }
                    // Synthetic XTEST keys (xdotool — including the daemon's
                    // own typing) never cancel.
                    if self.xtest_device == Some(ev.sourceid) {
                        continue;
                    }
                    let key = ev.detail as u8;
                    if u32::from(key) != ev.detail {
                        continue; // out-of-range keycode: not a cancel candidate
                    }
                    if !should_cancel_key(
                        key,
                        self.trigger,
                        &self.modifiers,
                        !self.past_grace(),
                        &mut self.suppress_cancel,
                    ) {
                        continue;
                    }
                    *held = false;
                    self.suppress_cancel.clear();
                    return Ok(HotkeyEvent::Cancel);
                }
                Some(_) => continue,
            }
        }
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        // Release the passive grabs. No keymap restore needed — we never
        // modified the keymap. The X server releases grabs automatically
        // on connection close (even SIGKILL), but explicit ungrab is
        // cleaner for normal shutdown.
        for mask in &self.masks {
            let _ = self.conn.ungrab_key(self.trigger, self.root, *mask);
        }
        let _ = self.conn.flush();
    }
}

impl crate::HotkeySource for Hotkey {
    fn next_event(&mut self) -> Result<HotkeyEvent> {
        let mut held = self.source_held;
        let ev = Hotkey::next_event(self, &mut held)?;
        self.source_held = held;
        Ok(ev)
    }

    fn drain_pending(&mut self) {
        Hotkey::drain_pending(self);
    }
}


/// If a prior daemon was SIGKILL'd, Drop never ran and the Caps Lock keycode
/// may still be all-NoSymbol. Synthesize the conventional Caps_Lock mapping
/// so the next grab (and its Drop) can hand the key back.
///
/// Manual recovery on a typical PC keyboard (keycode 66):
/// `xmodmap -e 'keycode 66 = Caps_Lock'`
fn recover_orig_keysyms(keysyms: Vec<Keysym>) -> Vec<Keysym> {
    if is_all_nosymbol(&keysyms) {
        vec![XK_CAPS_LOCK]
    } else {
        keysyms
    }
}


/// Whether an XI2/raw key should Cancel the current hold.
///
/// `in_grace`: still inside CANCEL_GRACE after Caps Lock press.
/// `suppress`: keycodes seen during grace (pre-held). Mutated when
/// `in_grace` is true — the key is recorded and never cancels yet.
fn should_cancel_key(
    key: Keycode,
    trigger: Keycode,
    modifiers: &[Keycode],
    in_grace: bool,
    suppress: &mut HashSet<Keycode>,
) -> bool {
    if key == trigger || modifiers.contains(&key) {
        return false;
    }
    if in_grace {
        suppress.insert(key);
        return false;
    }
    if suppress.contains(&key) {
        return false;
    }
    true
}

fn is_all_nosymbol(keysyms: &[Keysym]) -> bool {
    keysyms.is_empty() || keysyms.iter().all(|&k| k == NO_SYMBOL)
}

fn pad_keysyms(keysyms: &[Keysym], per_slot: usize) -> Vec<Keysym> {
    let n = per_slot.max(1).max(keysyms.len());
    let mut out = vec![NO_SYMBOL; n];
    for (i, &k) in keysyms.iter().enumerate() {
        out[i] = k;
    }
    out
}


/// Resolve the Caps Lock keycode even when a dead daemon left it NoSymbol.
///
/// Order: live Caps_Lock keysym → cached keycode from last grab → PC
/// fallback 66 when that slot is empty/NoSymbol.
fn resolve_caps_trigger(
    conn: &RustConnection,
) -> Result<Option<(Keycode, Vec<Keysym>, usize)>> {
    if let Some(kc) = keycode_for_keysym(conn, XK_CAPS_LOCK)? {
        let (keysyms, per) = read_keycode_mapping(conn, kc)?;
        return Ok(Some((kc, keysyms, per)));
    }

    for candidate in cached_caps_keycode()
        .into_iter()
        .chain(std::iter::once(FALLBACK_CAPS_KEYCODE))
    {
        if !keycode_in_range(conn, candidate) {
            continue;
        }
        let (keysyms, per) = read_keycode_mapping(conn, candidate)?;
        if is_all_nosymbol(&keysyms) {
            return Ok(Some((candidate, keysyms, per)));
        }
    }
    Ok(None)
}

fn read_keycode_mapping(conn: &RustConnection, keycode: Keycode) -> Result<(Vec<Keysym>, usize)> {
    let mapping = conn
        .get_keyboard_mapping(keycode, 1)?
        .reply()
        .context("GetKeyboardMapping(Caps Lock) failed")?;
    Ok((
        mapping.keysyms,
        mapping.keysyms_per_keycode as usize,
    ))
}

fn keycode_in_range(conn: &RustConnection, keycode: Keycode) -> bool {
    let setup = conn.setup();
    keycode >= setup.min_keycode && keycode <= setup.max_keycode
}

fn keycode_for_keysym(conn: &RustConnection, want: Keysym) -> Result<Option<Keycode>> {
    let setup = conn.setup();
    let count = setup
        .max_keycode
        .saturating_sub(setup.min_keycode)
        .saturating_add(1);
    let reply = conn
        .get_keyboard_mapping(setup.min_keycode, count)?
        .reply()
        .context("GetKeyboardMapping failed")?;
    let per = reply.keysyms_per_keycode as usize;
    if per == 0 {
        return Ok(None);
    }
    for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
        if chunk.contains(&want) {
            return Ok(Some(setup.min_keycode + i as u8));
        }
    }
    Ok(None)
}

fn caps_keycode_cache_path() -> Option<PathBuf> {
    let dir = match std::env::var_os("XDG_CACHE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("steno"),
        _ => {
            let home = std::env::var_os("HOME")?;
            if home.is_empty() {
                return None;
            }
            PathBuf::from(home).join(".cache/steno")
        }
    };
    Some(dir.join("caps_keycode"))
}

fn remember_caps_keycode(keycode: Keycode) {
    let Some(path) = caps_keycode_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{keycode}\n"));
}

fn cached_caps_keycode() -> Option<Keycode> {
    let path = caps_keycode_cache_path()?;
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    //! Restore helpers must not need a live X display. WHY: SIGKILL leaves
    //! Caps Lock as NoSymbol; Drop must restore captured keysyms on clean
    //! exit; recovery must synthesize Caps_Lock when the mapping is empty.
    //! Looking up Caps_Lock by keysym alone cannot recover a stuck mapping.

    use super::*;

    #[test]
    fn preheld_key_does_not_cancel_after_grace() {
        let mut suppress = HashSet::new();
        let mods = vec![];
        // During grace: record, do not cancel.
        assert!(!should_cancel_key(38, 66, &mods, true, &mut suppress));
        assert!(suppress.contains(&38));
        // After grace: same key still suppressed.
        assert!(!should_cancel_key(38, 66, &mods, false, &mut suppress));
        // A different key cancels.
        assert!(should_cancel_key(39, 66, &mods, false, &mut suppress));
    }

    #[test]
    fn trigger_and_modifiers_never_cancel() {
        let mut suppress = HashSet::new();
        let mods = vec![50u8]; // Shift_L typical
        assert!(!should_cancel_key(66, 66, &mods, false, &mut suppress));
        assert!(!should_cancel_key(50, 66, &mods, false, &mut suppress));
    }


    #[test]
    fn is_all_nosymbol_detects_stuck_mapping() {
        assert!(is_all_nosymbol(&[]));
        assert!(is_all_nosymbol(&[NO_SYMBOL, NO_SYMBOL]));
        assert!(!is_all_nosymbol(&[XK_CAPS_LOCK, NO_SYMBOL]));
    }

    #[test]
    fn pad_keysyms_fills_slots() {
        assert_eq!(pad_keysyms(&[XK_CAPS_LOCK], 4), vec![XK_CAPS_LOCK, 0, 0, 0]);
    }

    #[test]
    fn pending_queue_preserves_multiple_deferred_events_in_fifo_order() {
        let mut queue: VecDeque<Event> = VecDeque::new();
        let deferred = vec![
            Event::KeyPress(x11rb::protocol::xproto::KeyPressEvent {
                detail: 1,
                time: 100,
                response_type: 0,
                sequence: 0,
                root: 0,
                event: 0,
                child: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0u16.into(),
                same_screen: false,
            }),
            Event::KeyPress(x11rb::protocol::xproto::KeyPressEvent {
                detail: 2,
                time: 200,
                response_type: 0,
                sequence: 0,
                root: 0,
                event: 0,
                child: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0u16.into(),
                same_screen: false,
            }),
            Event::KeyPress(x11rb::protocol::xproto::KeyPressEvent {
                detail: 3,
                time: 300,
                response_type: 0,
                sequence: 0,
                root: 0,
                event: 0,
                child: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0u16.into(),
                same_screen: false,
            }),
        ];
        queue.extend(deferred);
        assert_eq!(queue.len(), 3);
        assert!(matches!(queue.pop_front(), Some(Event::KeyPress(e)) if e.detail == 1));
        assert!(matches!(queue.pop_front(), Some(Event::KeyPress(e)) if e.detail == 2));
        assert!(matches!(queue.pop_front(), Some(Event::KeyPress(e)) if e.detail == 3));
        assert!(queue.pop_front().is_none());
    }
    #[test]
    fn connect_x11_for_restore_handles_missing_display_without_panic() {
        // WHY: connect_x11_for_restore must attempt display probing and return Err or Ok, never panic.
        let prev = std::env::var_os("DISPLAY");
        unsafe { std::env::remove_var("DISPLAY") };
        let res = connect_x11_for_restore();
        if let Some(val) = prev {
            unsafe { std::env::set_var("DISPLAY", val) };
        }
        // Result is either Ok(conn) if probing found a display, or Err(e).
        let _ = res;
    }
}
