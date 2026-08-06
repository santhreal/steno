//! Global Ctrl+Space grab via X11 (`XGrabKey` on the root window).
//!
//! Hold = record, release = stop. While recording, ANY other key cancels
//! the utterance (listened passively via XInput2 raw key events — the
//! key still reaches the focused app, nothing is swallowed). Modifier
//! keys and Space itself never cancel: pressing Ctrl+Space must not
//! instantly cancel the recording it starts.
//!
//! Failures are loud: if another client already owns the grab (e.g. a
//! GNOME custom shortcut on the same combo), we say so instead of
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

/// X11 keysym for Space (`XK_space`).
const XK_SPACE: Keysym = 0x0020;

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
    space: Keycode,
    /// Modifier keycodes (Shift/Ctrl/Alt/Super/Lock/Mod*): never cancel.
    modifiers: Vec<Keycode>,
    /// Control keycodes specifically (releasing one ends the hold).
    control: Vec<Keycode>,
    /// Device id of the "Virtual core XTEST keyboard" slave: its fake key
    /// events (xdotool — including the daemon's OWN typing) must never
    /// cancel an utterance.
    xtest_device: Option<u16>,
    /// When the current hold started (auto-repeat grace for cancels).
    press_at: Option<Instant>,
    /// Modifier masks we grabbed (plain Ctrl + Caps/NumLock variants).
    masks: Vec<ModMask>,
}

/// A cancel keypress within this window after the activating press is
/// almost certainly auto-repeat of a key the user was already holding
/// (XI2 raw events fire for repeats), not a deliberate cancel.
const CANCEL_GRACE: Duration = Duration::from_millis(150);

impl Hotkey {
    /// Grab Ctrl+Space system-wide on the default display.
    pub fn grab_ctrl_space() -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None)
            .context("cannot connect to X11 — is DISPLAY set?")?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let space = keycode_for_keysym(&conn, XK_SPACE)?
            .ok_or_else(|| anyhow!("keyboard has no Space keycode — cannot bind Ctrl+Space"))?;

        // CapsLock (Lock) and NumLock (Mod2) are sticky; grab every combo
        // so the hotkey still fires when those are on.
        let masks = [
            ModMask::CONTROL,
            ModMask::CONTROL | ModMask::LOCK,
            ModMask::CONTROL | ModMask::M2,
            ModMask::CONTROL | ModMask::LOCK | ModMask::M2,
        ];

        for mask in masks {
            let cookie = conn
                .grab_key(
                    false, // owner_events: we alone get the events
                    root,
                    mask,
                    space,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .with_context(|| format!("XGrabKey(Ctrl+Space, mask={mask:?}) request failed"))?;
            if let Err(e) = cookie.check() {
                bail!(
                    "XGrabKey(Ctrl+Space) failed ({e}). Another client may already own that \
                     shortcut — remove any GNOME/KDE custom shortcut on Ctrl+Space and retry."
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
        // Slot order: Shift, Lock, Control, Mod1..Mod5.
        let per_slot = (modifiers.len() / 8).max(1);
        let control: Vec<Keycode> = modifiers
            .get(2 * per_slot..3 * per_slot)
            .unwrap_or(&[])
            .to_vec();
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
            space,
            modifiers,
            control,
            xtest_device,
            press_at: None,
            masks: masks.to_vec(),
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

    /// The resolved Space keycode (used by the off-host test harness).
    #[allow(dead_code)]
    pub fn space_keycode(&self) -> Keycode {
        self.space
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
                .context("X11 flush failed while waiting for Ctrl+Space")?;
            let ev = self.conn.poll_for_event().context("X11 poll failed")?;
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
                    if ev.detail != self.space {
                        // While the grab is active (combo held), other keys
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
                    if u16::from(ev.state) & u16::from(ModMask::CONTROL) == 0 {
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
                    if !*held {
                        continue;
                    }
                    let space_up = ev.detail == self.space;
                    let ctrl_up = self.control.contains(&ev.detail);
                    if !space_up && !ctrl_up {
                        continue;
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
                    if key == self.space || self.modifiers.contains(&key) || !self.past_grace() {
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
            let _ = self.conn.ungrab_key(self.space, self.root, *mask);
        }
        let _ = self.conn.flush();
    }
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
