//! Global Ctrl+Space grab via X11 (`XGrabKey` on the root window).
//!
//! Hold = record, release = stop. Failures are loud: if another client
//! already owns the grab (e.g. a GNOME custom shortcut on the same combo),
//! we say so instead of silently never firing.

use anyhow::{Context, Result, anyhow, bail};
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ConnectionExt as _, GrabMode, Keycode, Keysym, ModMask, Window,
};
use x11rb::rust_connection::RustConnection;

/// X11 keysym for Space (`XK_space`).
const XK_SPACE: Keysym = 0x0020;
/// `XK_Control_L` / `XK_Control_R`.
const XK_CONTROL_L: Keysym = 0xffe3;
const XK_CONTROL_R: Keysym = 0xffe4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Press,
    Release,
}

pub struct Hotkey {
    conn: RustConnection,
    root: Window,
    space: Keycode,
    /// Modifier masks we grabbed (plain Ctrl + Caps/NumLock variants).
    masks: Vec<ModMask>,
}

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

        Ok(Self {
            conn,
            root,
            space,
            masks: masks.to_vec(),
        })
    }

    /// Block until the next Press or Release of the grabbed combo.
    /// Auto-repeats while held are collapsed into a single Press.
    pub fn next_event(&mut self, held: &mut bool) -> Result<HotkeyEvent> {
        loop {
            self.conn
                .flush()
                .context("X11 flush failed while waiting for Ctrl+Space")?;
            match self.conn.poll_for_event().context("X11 poll failed")? {
                None => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Some(Event::KeyPress(ev)) => {
                    if ev.detail != self.space {
                        continue;
                    }
                    if u16::from(ev.state) & u16::from(ModMask::CONTROL) == 0 {
                        continue;
                    }
                    if *held {
                        continue; // auto-repeat
                    }
                    *held = true;
                    return Ok(HotkeyEvent::Press);
                }
                Some(Event::KeyRelease(ev)) => {
                    if !*held {
                        continue;
                    }
                    let space_up = ev.detail == self.space;
                    let ctrl_up = is_control_keycode(&self.conn, ev.detail).unwrap_or(false);
                    if !space_up && !ctrl_up {
                        continue;
                    }
                    *held = false;
                    return Ok(HotkeyEvent::Release);
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

fn is_control_keycode(conn: &RustConnection, code: Keycode) -> Result<bool> {
    let reply = conn.get_keyboard_mapping(code, 1)?.reply()?;
    Ok(reply
        .keysyms
        .iter()
        .any(|&ks| ks == XK_CONTROL_L || ks == XK_CONTROL_R))
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
