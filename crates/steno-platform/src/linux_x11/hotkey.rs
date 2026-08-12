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
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, XIEventMask};
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Allow, ConnectionExt as _, GrabMode, Keycode, Keysym, ModMask, Window,
};
use super::conn::connect_x11;
use x11rb::rust_connection::RustConnection;

/// X11 `CurrentTime` (0) — used with `allow_events` to release queued events.
const CURRENT_TIME: u32 = 0;
const NO_SYMBOL: Keysym = 0;
/// Typical PC keyboard Caps Lock keycode (evdev / xfree86).
const FALLBACK_CAPS_KEYCODE: Keycode = 66;
/// How long after Press a second key counts as a cancel vs. pre-held.
const CANCEL_GRACE: Duration = Duration::from_millis(120);
/// Idle sleep of the grab-servicing thread. This bounds how long the X
/// server keeps the keyboard frozen after a SYNC grab fires.
const POLL_IDLE: Duration = Duration::from_millis(5);
/// Bound on how long `drain_pending` waits for the worker to acknowledge.
const DRAIN_ACK_TIMEOUT: Duration = Duration::from_millis(250);
/// X11 keysym for Caps Lock.
const XK_CAPS_LOCK: Keysym = 0xffe5;


/// Global X11 Caps Lock push-to-talk hotkey grabber.
///
/// The connection owning the SYNC grab is serviced by a dedicated thread
/// that does nothing but unfreeze the keyboard and classify events.
///
/// WHY: with `GrabMode::SYNC` every Caps Lock press freezes the WHOLE
/// keyboard until someone calls `XAllowEvents`. Servicing the grab from
/// the daemon's main loop meant any long or wedged work (transcription,
/// LLM refine, typing) held that freeze: a Caps Lock press during
/// transcription blackholed the entire keyboard until the daemon came
/// back, forever if it never did. The worker runs no application work,
/// so the freeze window is bounded by [`POLL_IDLE`], and if the worker
/// dies the connection closes and the X server releases the freeze.
pub struct Hotkey {
    /// Caps Lock keycode -- the push-to-talk trigger.
    trigger: Keycode,
    /// Classified events from the worker.
    rx: Receiver<HotkeyEvent>,
    /// Tells the worker to ungrab and exit.
    stop: Arc<AtomicBool>,
    /// Set to request a queue flush; the worker clears it when done.
    drain: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    /// Hold state for [`crate::HotkeySource::next_event`].
    source_held: bool,
}

/// State owned exclusively by the grab-servicing thread. Nothing else may
/// touch the connection: that is what keeps the freeze window bounded.
struct Servicer {
    conn: RustConnection,
    root: Window,
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
    held: bool,
    debug: bool,
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

        // XKB is required to undo the Lock latch the grabbed press causes.
        // Without it the daemon would type in capitals, so fail loudly
        // rather than dictate uppercase for the rest of the session.
        conn.xkb_use_extension(1, 0)
            .context("XkbUseExtension request failed")?
            .reply()
            .context("X server has no XKB extension: Caps Lock would latch on every hold")?;

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

        let servicer = Servicer {
            conn,
            root,
            trigger,
            modifiers,
            xtest_device,
            press_at: None,
            suppress_cancel: HashSet::new(),
            pending: VecDeque::new(),
            masks,
            held: false,
            debug: std::env::var_os("HK_DEBUG").is_some(),
        };

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let drain = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            let drain = Arc::clone(&drain);
            std::thread::Builder::new()
                .name("steno-hotkey".into())
                .spawn(move || servicer.run(&tx, &stop, &drain))
                .context("cannot spawn hotkey servicing thread")?
        };

        Ok(Self {
            trigger,
            rx,
            stop,
            drain,
            worker: Some(worker),
            source_held: false,
        })
    }

    /// Discard any queued events. Called after the daemon finishes typing
    /// so late raw events from its own xdotool keystrokes cannot leak
    /// into the next utterance (belt-and-suspenders over the XTEST
    /// device filter).
    #[allow(dead_code)] // used by the daemon binary, not the example harness
    pub fn drain_pending(&mut self) {
        // Ask the worker to flush the X queue, then drop anything it had
        // already classified. Bounded: a wedged worker must not stall the
        // daemon, and stale events are harmless compared to a hang.
        self.drain.store(true, Ordering::Release);
        let deadline = Instant::now() + DRAIN_ACK_TIMEOUT;
        while self.drain.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(POLL_IDLE);
        }
        while self.rx.try_recv().is_ok() {}
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

    /// `next_event` with a shutdown flag the daemon's SIGTERM handler
    /// sets. Reads classified events from the servicing thread; it never
    /// touches the X connection, so nothing the caller does — however
    /// slow or wedged — can hold the SYNC keyboard freeze.
    pub fn next_event_debug(
        &mut self,
        held: &mut bool,
        _debug: bool,
        shutdown: &AtomicBool,
    ) -> Result<HotkeyEvent> {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return Ok(HotkeyEvent::Shutdown);
            }
            match self.rx.recv_timeout(POLL_IDLE) {
                Ok(ev) => {
                    match ev {
                        HotkeyEvent::Press => *held = true,
                        HotkeyEvent::Release | HotkeyEvent::Cancel => *held = false,
                        HotkeyEvent::Shutdown => {}
                    }
                    return Ok(ev);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    bail!(
                        "hotkey servicing thread exited: the X11 connection owning the \
                         Caps Lock grab was lost. Caps Lock is released; restart the daemon."
                    )
                }
            }
        }
    }
}

impl Servicer {
    /// Unfreeze the keyboard. Called for EVERY key event pulled off the
    /// wire, before any classification: the SYNC grab freezes the whole
    /// keyboard until this runs, so it must never sit behind a decision
    /// that could be slow or wrong.
    fn unfreeze(&self) {
        let _ = self.conn.allow_events(Allow::ASYNC_KEYBOARD, CURRENT_TIME);
        let _ = self.conn.flush();
    }

    /// Clear the Lock modifier.
    ///
    /// WHY: `AllowEvents(AsyncKeyboard)` resumes normal processing of the
    /// frozen Caps Lock press, and normal processing runs the XKB Lock
    /// action. The freeze alone does not swallow the latch, so without
    /// this every following keystroke — the user's and the daemon's own
    /// `xdotool` output — arrives capitalised. Unlike a keymap edit this
    /// leaves nothing to repair if the process dies.
    fn clear_lock(&self) {
        let _ = xkb::latch_lock_state(
            &self.conn,
            xkb::ID::USE_CORE_KBD.into(),
            ModMask::LOCK,      // affect_mod_locks: only the Lock modifier
            ModMask::from(0u16), // mod_locks: clear it
            false,
            0.into(),
            ModMask::from(0u16),
            false,
            0,
        );
        let _ = self.conn.flush();
    }

    /// Unfreeze after a key event, and undo the Lock latch when that
    /// event was our trigger.
    fn release(&self, ev: &Event) {
        let detail = match ev {
            Event::KeyPress(e) => Some(e.detail),
            Event::KeyRelease(e) => Some(e.detail),
            _ => return,
        };
        self.unfreeze();
        if detail == Some(self.trigger) {
            self.clear_lock();
        }
    }

    /// True once the cancel grace window after the activating press has
    /// elapsed (see CANCEL_GRACE).
    fn past_grace(&self) -> bool {
        self.press_at.is_none_or(|t| t.elapsed() >= CANCEL_GRACE)
    }

    /// Service the grab until asked to stop or the connection dies.
    /// Runs no application work of any kind.
    fn run(mut self, tx: &Sender<HotkeyEvent>, stop: &AtomicBool, drain: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            if drain.load(Ordering::Acquire) {
                self.pending.clear();
                let _ = self.conn.flush();
                while let Ok(Some(ev)) = self.conn.poll_for_event() {
                    self.release(&ev);
                }
                drain.store(false, Ordering::Release);
            }
            let _ = self.conn.flush();
            let ev = match self.pending.pop_front() {
                Some(e) => Some(e),
                None => match self.conn.poll_for_event() {
                    Ok(e) => e,
                    // Connection lost: dropping it releases the grab and
                    // any freeze. Report by closing the channel.
                    Err(_) => return,
                },
            };
            let Some(ev) = ev else {
                std::thread::sleep(POLL_IDLE);
                continue;
            };
            self.release(&ev);
            if self.debug {
                match &ev {
                    Event::KeyPress(e) => {
                        println!("RAW press detail={} state={:?}", e.detail, e.state)
                    }
                    Event::KeyRelease(e) => {
                        println!("RAW release detail={} state={:?}", e.detail, e.state)
                    }
                    Event::XinputRawKeyPress(e) => println!("RAW xinput detail={}", e.detail),
                    other => println!("RAW {other:?}"),
                }
            }
            if let Some(out) = self.classify(ev) {
                if tx.send(out).is_err() {
                    return; // receiver gone: shut down
                }
            }
        }
    }

    /// Turn one X event into a hotkey event, or `None` to keep waiting.
    /// The keyboard is already unfrozen by the time this runs.
    fn classify(&mut self, ev: Event) -> Option<HotkeyEvent> {
        match ev {
            Event::KeyPress(ev) => {
                if ev.detail != self.trigger {
                    // While the grab is active (key held), other keys are
                    // delivered to us: that is the user cancelling. Same
                    // rules as XI2 RawKeyPress (grace + pre-held).
                    if self.held
                        && should_cancel_key(
                            ev.detail,
                            self.trigger,
                            &self.modifiers,
                            !self.past_grace(),
                            &mut self.suppress_cancel,
                        )
                    {
                        self.held = false;
                        self.suppress_cancel.clear();
                        return Some(HotkeyEvent::Cancel);
                    }
                    return None;
                }
                if self.held {
                    return None; // auto-repeat: already unfrozen and discarded
                }
                self.held = true;
                self.press_at = Some(Instant::now());
                self.suppress_cancel.clear();
                Some(HotkeyEvent::Press)
            }
            Event::KeyRelease(ev) => {
                if !self.held || ev.detail != self.trigger {
                    return None;
                }
                // X auto-repeat emits a release+press pair with the SAME
                // timestamp for a held key. Peek past unrelated events
                // (esp. XI2 RawKeyPress) for a matching press — otherwise
                // an interleaved XI2 event looks like a real release and
                // cuts the utterance at the repeat delay.
                let mut deferred: Vec<Event> = Vec::new();
                let mut is_repeat = false;
                while let Ok(Some(peeked)) = self.conn.poll_for_event() {
                    self.release(&peeked);
                    match &peeked {
                        Event::KeyPress(p) if p.detail == ev.detail && p.time == ev.time => {
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
                    return None;
                }
                self.pending.extend(deferred);
                self.held = false;
                self.suppress_cancel.clear();
                Some(HotkeyEvent::Release)
            }
            Event::XinputRawKeyPress(ev) => {
                if !self.held {
                    return None; // normal typing while idle is not a cancel
                }
                // Synthetic XTEST keys (xdotool — including the daemon's
                // own typing) never cancel.
                if self.xtest_device == Some(ev.sourceid) {
                    return None;
                }
                let key = ev.detail as u8;
                if u32::from(key) != ev.detail {
                    return None; // out-of-range keycode: not a cancel candidate
                }
                if !should_cancel_key(
                    key,
                    self.trigger,
                    &self.modifiers,
                    !self.past_grace(),
                    &mut self.suppress_cancel,
                ) {
                    return None;
                }
                self.held = false;
                self.suppress_cancel.clear();
                Some(HotkeyEvent::Cancel)
            }
            _ => None,
        }
    }
}

impl Drop for Servicer {
    fn drop(&mut self) {
        // Unfreeze first: if we are torn down while a SYNC grab is frozen,
        // leaving it frozen would blackhole the keyboard for as long as the
        // connection lives. Then release the passive grabs. No keymap
        // restore needed — we never modified the keymap.
        self.unfreeze();
        for mask in &self.masks {
            let _ = self.conn.ungrab_key(self.trigger, self.root, *mask);
        }
        let _ = self.conn.flush();
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
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
