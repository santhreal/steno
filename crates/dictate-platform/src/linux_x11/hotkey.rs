//! Global Caps Lock grab via X11 (`XGrabKey` on the root window).
//!
//! Hold = record, release = stop. While recording, ANY other key cancels
//! the utterance (listened passively via XInput2 raw key events — the
//! key still reaches the focused app, nothing is swallowed). Modifier
//! keys never cancel.
//!
//! Caps Lock is fully swallowed while the daemon runs: the keycode is
//! remapped to NoSymbol for the daemon's lifetime (restored on exit),
//! so the Lock modifier can never latch and caps state never toggles —
//! a passive grab alone would NOT stop XKB from locking caps on press.
//!
//! Failures are loud: if another client already owns the grab (e.g. a
//! GNOME custom shortcut on the same key), we say so instead of
//! silently never firing.

use anyhow::{Context, Result, anyhow, bail};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, XIEventMask};
use x11rb::protocol::xproto::{
    ConnectionExt as _, GrabMode, Keycode, Keysym, ModMask, Window,
};
use x11rb::rust_connection::RustConnection;

/// X11 keysym for Caps Lock (`XK_Caps_Lock`).
const XK_CAPS_LOCK: Keysym = 0xffe5;
/// `NoSymbol` — remapping the Caps Lock keycode to this disables the
/// caps toggle entirely while keeping the raw key events.
const NO_SYMBOL: Keysym = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Press,
    Release,
    /// A non-modifier key other than the hotkey was pressed while
    /// recording: the user wants the utterance dropped.
    Cancel,
    /// SIGTERM arrived: exit the event loop so Drop impls run.
    Shutdown,
}

pub struct Hotkey {
    conn: RustConnection,
    root: Window,
    /// Caps Lock keycode — the push-to-talk trigger.
    trigger: Keycode,
    /// Keysyms the trigger keycode had before we remapped it to
    /// NoSymbol; restored on Drop. Synthesized as plain Caps_Lock when
    /// a previous crashed daemon left it unmapped.
    orig_keysyms: Vec<Keysym>,
    /// Modifier keycodes (Shift/Ctrl/Alt/Super/Lock/Mod*): never cancel.
    modifiers: Vec<Keycode>,
    /// Device id of the "Virtual core XTEST keyboard" slave: its fake key
    /// events (xdotool — including the daemon's OWN typing) must never
    /// cancel an utterance.
    xtest_device: Option<u16>,
    /// When the current hold started (auto-repeat grace for cancels).
    press_at: Option<Instant>,
    /// One peeked event held back from auto-repeat coalescing.
    pending: Option<Event>,
    /// Modifier masks we grabbed (plain + Caps/NumLock variants).
    masks: Vec<ModMask>,
    /// Hold state for [`crate::HotkeySource::next_event`]. The daemon still
    /// passes its own `held` into the inherent methods.
    source_held: bool,
}

/// A cancel keypress within this window after the activating press is
/// almost certainly auto-repeat of a key the user was already holding
/// (XI2 raw events fire for repeats), not a deliberate cancel.
const CANCEL_GRACE: Duration = Duration::from_millis(150);

impl Hotkey {
    /// Grab Caps Lock system-wide on the default display, and remap the
    /// keycode to NoSymbol so the caps toggle is dead while we run.
    pub fn grab_caps_lock() -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None)
            .context("cannot connect to X11 — is DISPLAY set?")?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let trigger = keycode_for_keysym(&conn, XK_CAPS_LOCK)?
            .ok_or_else(|| anyhow!("keyboard has no Caps Lock keycode — cannot bind it"))?;

        // Swallow caps: remap the keycode to NoSymbol for our lifetime.
        // A passive grab alone does NOT stop XKB from latching Lock on
        // press; with NoSymbol the key gets no action and the toggle can
        // never fire. Key events still flow, so our grab still works.
        let mapping = conn
            .get_keyboard_mapping(trigger, 1)?
            .reply()
            .context("GetKeyboardMapping(Caps Lock) failed")?;
        let per_slot = mapping.keysyms_per_keycode as usize;
        // SIGKILL skips Drop, leaving NoSymbol. Recover the conventional
        // Caps_Lock mapping so a later Drop (or fresh grab) can hand it back.
        let orig_keysyms = recover_orig_keysyms(mapping.keysyms);
        let dead = nosymbol_mapping(per_slot);
        conn.change_keyboard_mapping(1, trigger, dead.len() as u8, &dead)?
            .check()
            .context("cannot remap Caps Lock to NoSymbol")?;

        // NumLock (Mod2) and friends are sticky; grab every combo so the
        // hotkey still fires when they are on.
        let masks = [
            ModMask::from(0u16),
            ModMask::LOCK,
            ModMask::M2,
            ModMask::LOCK | ModMask::M2,
        ];

        for mask in masks {
            let cookie = conn
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
                // Restore the mapping before reporting failure.
                let restore = caps_lock_restore_keysyms(&orig_keysyms);
                let _ = conn.change_keyboard_mapping(
                    1,
                    trigger,
                    restore.len() as u8,
                    restore,
                );
                let _ = conn.flush();
                bail!(
                    "XGrabKey(CapsLock) failed ({e}). Another client may already own that \
                     shortcut — remove any GNOME/KDE binding on Caps Lock and retry."
                );
            }
        }
        conn.flush()?;

        // Passive cancel listener: raw key presses for every key, still
        // delivered to the focused app (nothing is grabbed or swallowed).
        let version = xinput::xi_query_version(&conn, 2, 0)?
            .reply()
            .context("XIQueryVersion failed — XInput2 is required for cancel-any-key")?;
        if version.major_version < 2 {
            bail!(
                "X server has XInput {}.{}, need 2.0+ for cancel-any-key — upgrade the X server",
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
            orig_keysyms,
            modifiers,
            xtest_device,
            press_at: None,
            pending: None,
            masks: masks.to_vec(),
            source_held: false,
        })
    }

    /// Discard any queued events. Called after the daemon finishes typing
    /// so late raw events from its own xdotool keystrokes cannot leak
    /// into the next utterance (belt-and-suspenders over the XTEST
    /// device filter).
    #[allow(dead_code)] // used by the daemon binary, not the example harness
    pub fn drain_pending(&mut self) {
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
            let ev = match self.pending.take() {
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
                        if *held
                            && !self.modifiers.contains(&ev.detail)
                            && self.past_grace()
                        {
                            *held = false;
                            return Ok(HotkeyEvent::Cancel);
                        }
                        continue;
                    }
                    if *held {
                        continue; // auto-repeat
                    }
                    *held = true;
                    self.press_at = Some(Instant::now());
                    return Ok(HotkeyEvent::Press);
                }
                Some(Event::KeyRelease(ev)) => {
                    if !*held || ev.detail != self.trigger {
                        continue;
                    }
                    // X auto-repeat emits a release+press pair with the
                    // SAME timestamp for a held key. Peek: a matching press
                    // means the key never went up — swallow both and stay
                    // held. Without this every hold longer than the repeat
                    // delay (~600ms) looked like a release and cut the
                    // utterance.
                    if let Ok(Some(peeked)) = self.conn.poll_for_event() {
                        let is_repeat = matches!(
                            &peeked,
                            Event::KeyPress(p) if p.detail == ev.detail && p.time == ev.time
                        );
                        if is_repeat {
                            continue;
                        }
                        self.pending = Some(peeked);
                    }
                    *held = false;
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
                    if key == self.trigger || self.modifiers.contains(&key) || !self.past_grace() {
                        continue;
                    }
                    *held = false;
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
        let _ = self.conn.change_keyboard_mapping(
            1,
            self.trigger,
            restore.len() as u8,
            restore,
        );
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
    if keysyms.is_empty() || keysyms.iter().all(|&k| k == NO_SYMBOL) {
        vec![XK_CAPS_LOCK]
    } else {
        keysyms
    }
}

/// Remap payload that disables the caps toggle while leaving raw key events.
fn nosymbol_mapping(keysyms_per_keycode: usize) -> Vec<Keysym> {
    vec![NO_SYMBOL; keysyms_per_keycode.max(1)]
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

#[cfg(test)]
mod tests {
    //! Restore helpers must not need a live X display. WHY: SIGKILL leaves
    //! Caps Lock as NoSymbol; Drop must restore captured keysyms on clean
    //! exit; recovery must synthesize Caps_Lock when the mapping is empty.

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
}
