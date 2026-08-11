//! Global Caps Lock grab via X11 (`XGrabKey` on the root window).
//!
//! Hold = record, release = stop. While recording, ANY other key cancels
//! the utterance (listened passively via XInput2 raw key events: the
//! key still reaches the focused app, nothing is swallowed). Modifier
//! keys never cancel.
//!
//! Caps Lock is fully swallowed while the daemon runs: the keycode is
//! remapped to NoSymbol for the daemon's lifetime (restored on exit),
//! so the Lock modifier can never latch and caps state never toggles:
//! a passive grab alone would NOT stop XKB from locking caps on press.
//!
//! Failures are loud: if another client already owns the grab (e.g. a
//! GNOME custom shortcut on the same key), we say so instead of
//! silently never firing.
//!
//! **SIGKILL / stuck Caps Lock.** `kill -9` never runs `Drop`, so the
//! keycode can stay mapped to NoSymbol and Caps Lock appears dead even
//! with the daemon gone. Recovery:
//! 1. `restore_caps_lock_mapping()` (also called from `steno stop`)
//! 2. Next `grab_caps_lock()` resolves the keycode via keysym, then a
//!    persisted keycode cache, then PC-keyboard fallback 66: looking
//!    up Caps_Lock by keysym alone fails once the mapping is empty.
//!
//! Manual fix: `xmodmap -e 'keycode 66 = Caps_Lock'`

use anyhow::{Context, Result, anyhow, bail};
use crate::traits::HotkeyEvent;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, XIEventMask};
use x11rb::protocol::xproto::{
    ConnectionExt as _, GrabMode, Keycode, Keysym, ModMask, Window,
};
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb_protocol::xauth;

/// X11 keysym for Caps Lock (`XK_Caps_Lock`).
const XK_CAPS_LOCK: Keysym = 0xffe5;
/// `NoSymbol` -- remapping the Caps Lock keycode to this disables the
/// caps toggle entirely while keeping the raw key events.
const NO_SYMBOL: Keysym = 0;
/// Typical PC keyboard Caps Lock keycode (evdev / xfree86).
const FALLBACK_CAPS_KEYCODE: Keycode = 66;


/// Global X11 Caps Lock push-to-talk hotkey grabber.
pub struct Hotkey {
    conn: RustConnection,
    root: Window,
    /// Caps Lock keycode -- the push-to-talk trigger.
    trigger: Keycode,
    /// Keysyms the trigger keycode had before we remapped it to
    /// NoSymbol; restored on Drop. Synthesized as plain Caps_Lock when
    /// a previous crashed daemon left it unmapped.
    orig_keysyms: Vec<Keysym>,
    /// Slot width from GetKeyboardMapping — Drop must pad to this count.
    keysyms_per_keycode: usize,
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

/// Window after the activating Caps Lock press during which non-trigger
/// XI2 key events are recorded as pre-held (auto-repeat), not Cancel.
/// After grace, those keycodes stay suppressed; a *different* key cancels.
const CANCEL_GRACE: Duration = Duration::from_millis(150);

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
            let d = d.strip_prefix(':').unwrap_or(&d);
            d.split('.').next().map(|s| s.to_string())
        });

    let mut candidates = Vec::new();
    if let Some(n) = &display_num {
        candidates.push(n.clone());
    }
    // Also scan /tmp/.X11-unix/ for filesystem sockets (may exist even
    // when the abstract socket is the one that works).
    if let Ok(entries) = std::fs::read_dir("/tmp/.X11-unix") {
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
    let screen = 0;

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
    /// Grab Caps Lock system-wide on the default display, and remap the
    /// keycode to NoSymbol so the caps toggle is dead while we run.
    pub fn grab_caps_lock() -> Result<Self> {
        let (conn, screen_num) = connect_x11()
            .context("cannot connect to X11: is DISPLAY set?")?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let (trigger, mapped, per_slot) = resolve_caps_trigger(&conn)?.ok_or_else(|| {
            anyhow!(
                "keyboard has no Caps Lock keycode: cannot bind it. \
                 If Caps Lock was blackholed by a killed daemon, run: \
                 xmodmap -e 'keycode 66 = Caps_Lock'"
            )
        })?;

        // Swallow caps: remap the keycode to NoSymbol for our lifetime.
        // A passive grab alone does NOT stop XKB from latching Lock on
        // press; with NoSymbol the key gets no action and the toggle can
        // never fire. Key events still flow, so our grab still works.
        // SIGKILL skips Drop, leaving NoSymbol — recover so Drop / stop
        // can hand Caps_Lock back.
        let orig_keysyms = recover_orig_keysyms(mapped);
        remember_caps_keycode(trigger);
        let dead = nosymbol_mapping(per_slot);
        conn.change_keyboard_mapping(1, trigger, dead.len() as u8, &dead)?
            .check()
            .context("cannot remap Caps Lock to NoSymbol")?;

        // Own the connection in a guard so ANY failure after remap restores
        // Caps Lock (Hotkey does not exist yet, so Hotkey::Drop cannot run).
        let mut guard = CapsRemapGuard {
            conn: Some(conn),
            root,
            trigger,
            orig_keysyms,
            keysyms_per_keycode: per_slot,
            masks: Vec::new(),
            armed: true,
        };

        // NumLock (Mod2) and friends are sticky; grab every combo so the
        // hotkey still fires when they are on.
        let mask_list = [
            ModMask::from(0u16),
            ModMask::LOCK,
            ModMask::M2,
            ModMask::LOCK | ModMask::M2,
        ];

        for mask in mask_list {
            let cookie = guard.conn.as_ref().unwrap()
                .grab_key(
                    false, // owner_events: we alone get the events
                    root,
                    mask,
                    trigger,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .with_context(|| format!("XGrabKey(CapsLock, mask={mask:?}) request failed"))?;
            if let Err(e) = cookie.check() {
                bail!(
                    "XGrabKey(CapsLock) failed ({e}). Another client may already own that \
                     shortcut: remove any GNOME/KDE binding on Caps Lock and retry."
                );
            }
            guard.masks.push(mask);
        }
        guard.conn.as_ref().unwrap().flush()?;

        // Passive cancel listener: raw key presses for every key, still
        // delivered to the focused app (nothing is grabbed or swallowed).
        let version = xinput::xi_query_version(guard.conn.as_ref().unwrap(), 2, 0)?
            .reply()
            .context("XIQueryVersion failed: XInput2 is required for cancel-any-key")?;
        if version.major_version < 2 {
            bail!(
                "X server has XInput {}.{}, need 2.0+ for cancel-any-key: upgrade the X server",
                version.major_version,
                version.minor_version
            );
        }
        xinput::xi_select_events(
            guard.conn.as_ref().unwrap(),
            root,
            &[xinput::EventMask {
                deviceid: xinput::Device::ALL_MASTER.into(),
                mask: vec![XIEventMask::RAW_KEY_PRESS],
            }],
        )?
        .check()
        .context("XISelectEvents(RawKeyPress) failed")?;
        let modifier_reply = guard.conn.as_ref().unwrap()
            .get_modifier_mapping()?
            .reply()
            .context("GetModifierMapping failed")?;
        let modifiers = modifier_reply.keycodes;
        // Find the XTEST slave so its synthetic keys can't cancel.
        let xtest_device = xinput::xi_query_device(guard.conn.as_ref().unwrap(), xinput::Device::ALL)
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
        guard.conn.as_ref().unwrap().flush()?;

        let (conn, trigger, orig_keysyms, keysyms_per_keycode, masks) = guard.into_parts();
        Ok(Self {
            conn,
            root,
            trigger,
            orig_keysyms,
            keysyms_per_keycode,
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

    /// Spawn a background thread that restores Caps Lock when `shutdown`
    /// is set — a safety net for SIGTERM arriving while the main thread
    /// is blocked in a long sherpa transcription.
    ///
    /// The signal handler sets `shutdown`; this thread detects it within
    /// 200 ms and restores the keysyms via its own X11 connection. Without
    /// it, `steno stop` escalates to SIGKILL after 10 s, `Drop` never runs,
    /// and Caps Lock stays mapped to NoSymbol (blackholed).
    ///
    /// Fire-and-forget: in the normal exit path `Drop` restores first
    /// (redundant, harmless — same keysyms written twice). On SIGKILL the
    /// thread is killed too, but by then it has long finished; the final
    /// fallback is `repair_caps_lock_if_needed()` in `steno stop`/`start`.
    pub fn spawn_shutdown_watchdog(
        &self,
        shutdown: &'static AtomicBool,
    ) -> thread::JoinHandle<()> {
        let trigger = self.trigger;
        let orig_keysyms = self.orig_keysyms.clone();
        let keysyms_per_keycode = self.keysyms_per_keycode;

        thread::Builder::new()
            .name("steno-caps-watchdog".into())
            .spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(200));
                }
                // Restore via our own connection — the main thread may
                // be stuck in sherpa and unable to flush its connection.
                if let Ok((conn, _)) = connect_x11_for_restore() {
                    let restore = caps_lock_restore_keysyms(&orig_keysyms);
                    let payload = pad_keysyms(restore, keysyms_per_keycode);
                    let _ = conn.change_keyboard_mapping(
                        1,
                        trigger,
                        payload.len() as u8,
                        &payload,
                    );
                    let _ = conn.flush();
                }
            })
            .expect("cannot spawn caps watchdog thread")
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
                        continue; // auto-repeat
                    }
                    *held = true;
                    self.press_at = Some(Instant::now());
                    self.suppress_cancel.clear();
                    return Ok(HotkeyEvent::Press);
                }
                Some(Event::KeyRelease(ev)) => {
                    if !*held || ev.detail != self.trigger {
                        continue;
                    }
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
        for mask in &self.masks {
            let _ = self.conn.ungrab_key(self.trigger, self.root, *mask);
        }
        // Hand Caps Lock back: restore the keycode's original keysyms so
        // the caps toggle works again once the daemon exits.
        // SIGKILL never runs Drop — see recover_orig_keysyms / PLATFORM_TRAITS.
        let restore = caps_lock_restore_keysyms(&self.orig_keysyms);
        let payload = pad_keysyms(restore, self.keysyms_per_keycode);
        let _ = self.conn.change_keyboard_mapping(
            1,
            self.trigger,
            payload.len() as u8,
            &payload,
        );
        let _ = self.conn.flush();
    }
}

/// Restores Caps Lock if `grab_caps_lock` fails after remapping to NoSymbol
/// but before `Hotkey` exists (so `Hotkey::Drop` cannot run).
struct CapsRemapGuard {
    conn: Option<RustConnection>,
    root: Window,
    trigger: Keycode,
    orig_keysyms: Vec<Keysym>,
    keysyms_per_keycode: usize,
    masks: Vec<ModMask>,
    armed: bool,
}

impl CapsRemapGuard {
    fn into_parts(mut self) -> (RustConnection, Keycode, Vec<Keysym>, usize, Vec<ModMask>) {
        self.armed = false;
        (
            self.conn.take().expect("CapsRemapGuard conn"),
            self.trigger,
            std::mem::take(&mut self.orig_keysyms),
            self.keysyms_per_keycode,
            std::mem::take(&mut self.masks),
        )
    }
}

impl Drop for CapsRemapGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(conn) = self.conn.as_ref() else {
            return;
        };
        for mask in &self.masks {
            let _ = conn.ungrab_key(self.trigger, self.root, *mask);
        }
        let restore = caps_lock_restore_keysyms(&self.orig_keysyms);
        let payload = pad_keysyms(restore, self.keysyms_per_keycode);
        let _ = conn.change_keyboard_mapping(
            1,
            self.trigger,
            payload.len() as u8,
            &payload,
        );
        let _ = conn.flush();
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

/// Keysyms Drop / grab-failure cleanup write back for the Caps Lock keycode.
///
/// Always the captured (or SIGKILL-recovered) mapping — never NoSymbol.
fn caps_lock_restore_keysyms(orig_keysyms: &[Keysym]) -> &[Keysym] {
    orig_keysyms
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

/// Remap payload that disables the caps toggle while leaving raw key events.
fn nosymbol_mapping(keysyms_per_keycode: usize) -> Vec<Keysym> {
    vec![NO_SYMBOL; keysyms_per_keycode.max(1)]
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
    fn recover_preserves_real_caps_lock_mapping() {
        let orig = vec![XK_CAPS_LOCK, NO_SYMBOL, NO_SYMBOL, NO_SYMBOL];
        assert_eq!(recover_orig_keysyms(orig.clone()), orig);
    }

    #[test]
    fn recover_synthesizes_caps_lock_when_all_nosymbol() {
        // Prior daemon SIGKILL'd before Drop — keycode stuck on NoSymbol.
        assert_eq!(
            recover_orig_keysyms(vec![NO_SYMBOL, NO_SYMBOL, NO_SYMBOL, NO_SYMBOL]),
            vec![XK_CAPS_LOCK]
        );
    }

    #[test]
    fn recover_synthesizes_caps_lock_for_empty_mapping() {
        assert_eq!(recover_orig_keysyms(vec![]), vec![XK_CAPS_LOCK]);
    }

    #[test]
    fn nosymbol_mapping_matches_slot_count() {
        assert_eq!(nosymbol_mapping(4), vec![NO_SYMBOL; 4]);
        assert_eq!(nosymbol_mapping(1), vec![NO_SYMBOL]);
        // X11 should never report 0, but keep Drop/grab safe.
        assert_eq!(nosymbol_mapping(0), vec![NO_SYMBOL]);
    }

    #[test]
    fn drop_restore_payload_is_exactly_orig_keysyms() {
        let recovered = recover_orig_keysyms(vec![NO_SYMBOL; 4]);
        assert_eq!(caps_lock_restore_keysyms(&recovered), &[XK_CAPS_LOCK]);

        let live = vec![XK_CAPS_LOCK, 0xffe5, NO_SYMBOL, NO_SYMBOL];
        assert_eq!(caps_lock_restore_keysyms(&live), live.as_slice());
    }

    #[test]
    fn swallow_then_recover_round_trip_yields_caps_lock() {
        let dead = nosymbol_mapping(4);
        assert!(dead.iter().all(|&k| k == NO_SYMBOL));
        assert_eq!(recover_orig_keysyms(dead), vec![XK_CAPS_LOCK]);
    }
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
