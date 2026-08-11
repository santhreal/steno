//! Linux platform facade: prefer X11 when `DISPLAY` works; Wayland MVP
//! (`wtype` typing) when only `WAYLAND_DISPLAY` is set.
//!
//! Public surface matches the historical `linux_x11` re-exports so
//! `steno` / embedders keep importing `Hotkey`, `Emitter`, `OutputMode`,
//! `Overlay`, `create`, and `HotkeyEvent` from `steno_platform`.

pub mod selection;

use anyhow::{Result, bail};
use steno_core::config::UiConfig;
use steno_core::overlay::{NullOverlay, OverlayBackend};
use steno_core::InjectTyper;

use crate::linux_wayland;
use crate::linux_x11;
use crate::traits::{HotkeySource, Typer};
use selection::{
    HotkeyBackend, OverlayBackendChoice, TypingBackend, hotkey_backend, overlay_backend,
    pure_wayland_hotkey_error, pure_wayland_overlay_warn, typing_backend,
};

pub use linux_x11::Overlay;
pub use crate::traits::{HotkeyEvent, OutputMode};

/// Caps Lock hotkey. Uses X11 grab when `DISPLAY` is set (including
/// XWayland); evdev direct input on pure Wayland (with `wayland` feature).
#[allow(clippy::large_enum_variant)]
pub enum Hotkey {
    X11(linux_x11::Hotkey),
    #[cfg(feature = "wayland")]
    Evdev(Box<linux_wayland::hotkey::EvdevHotkey>),
}

impl Hotkey {
    /// Grab Caps Lock system-wide. On pure Wayland without the `wayland`
    /// feature, returns an error with corrective actions.
    pub fn grab_caps_lock() -> Result<Self> {
        match hotkey_backend() {
            HotkeyBackend::X11 => Ok(Self::X11(linux_x11::Hotkey::grab_caps_lock()?)),
            #[cfg(feature = "wayland")]
            HotkeyBackend::Evdev => {
                linux_wayland::hotkey::EvdevHotkey::grab_caps_lock()
                    .map(|h| Self::Evdev(Box::new(h)))
            }
            HotkeyBackend::Unavailable => bail!("{}", pure_wayland_hotkey_error()),
        }
    }

    /// Restore Caps Lock if a prior daemon left it mapped to NoSymbol.
    /// No-op on evdev (no X11 mapping to repair; turns off LED).
    pub fn restore_caps_lock_mapping() -> Result<bool> {
        match hotkey_backend() {
            HotkeyBackend::X11 => linux_x11::hotkey::restore_caps_lock_mapping(),
            #[cfg(feature = "wayland")]
            HotkeyBackend::Evdev => linux_wayland::hotkey::EvdevHotkey::restore_caps_lock_mapping(),
            HotkeyBackend::Unavailable => Ok(false),
        }
    }

    pub fn drain_pending(&mut self) {
        match self {
            Self::X11(h) => h.drain_pending(),
            #[cfg(feature = "wayland")]
            Self::Evdev(h) => h.drain_pending(),
        }
    }

    pub fn next_event(&mut self, held: &mut bool) -> Result<HotkeyEvent> {
        match self {
            Self::X11(h) => h.next_event(held),
            #[cfg(feature = "wayland")]
            Self::Evdev(h) => h.next_event(held),
        }
    }

    pub fn next_event_debug(
        &mut self,
        held: &mut bool,
        debug: bool,
        shutdown: &std::sync::atomic::AtomicBool,
    ) -> Result<HotkeyEvent> {
        match self {
            Self::X11(h) => h.next_event_debug(held, debug, shutdown),
            #[cfg(feature = "wayland")]
            Self::Evdev(h) => h.next_event_debug(held, debug, shutdown),
        }
    }

    /// X11 keycode for the trigger key. Returns 0 on evdev (no X11 keycode).
    pub fn trigger_keycode(&self) -> x11rb::protocol::xproto::Keycode {
        match self {
            Self::X11(h) => h.trigger_keycode(),
            #[cfg(feature = "wayland")]
            Self::Evdev(_) => 0,
        }
    }

}

impl HotkeySource for Hotkey {
    fn next_event(&mut self) -> Result<HotkeyEvent> {
        match self {
            Self::X11(h) => HotkeySource::next_event(h),
            #[cfg(feature = "wayland")]
            Self::Evdev(h) => HotkeySource::next_event(h.as_mut()),
        }
    }

    fn drain_pending(&mut self) {
        match self {
            Self::X11(h) => HotkeySource::drain_pending(h),
            #[cfg(feature = "wayland")]
            Self::Evdev(h) => HotkeySource::drain_pending(h.as_mut()),
        }
    }
}

/// Progressive emitter: X11 `xdotool` or Wayland `wtype` by session.
pub enum Emitter {
    X11(linux_x11::Emitter),
    Wayland(linux_wayland::Emitter),
}

impl Emitter {
    pub fn new(mode: OutputMode) -> Self {
        match typing_backend() {
            TypingBackend::Xdotool => Self::X11(linux_x11::Emitter::new(mode)),
            TypingBackend::Wtype => Self::Wayland(linux_wayland::Emitter::new(mode)),
        }
    }

    pub fn push(&mut self, chunk: &str) -> Result<()> {
        match self {
            Self::X11(e) => e.push(chunk),
            Self::Wayland(e) => e.push(chunk),
        }
    }

    pub fn started(&self) -> bool {
        match self {
            Self::X11(e) => e.started(),
            Self::Wayland(e) => e.started(),
        }
    }

    pub fn finish(&mut self) -> Result<()> {
        match self {
            Self::X11(e) => e.finish(),
            Self::Wayland(e) => e.finish(),
        }
    }
}

impl Typer for Emitter {
    fn type_text(&mut self, text: &str) -> Result<()> {
        match self {
            Self::X11(e) => Typer::type_text(e, text),
            Self::Wayland(e) => Typer::type_text(e, text),
        }
    }
}

impl InjectTyper for Emitter {
    fn type_text(&mut self, text: &str) -> Result<()> {
        <Self as Typer>::type_text(self, text)
    }
}

/// Build an overlay from [`UiConfig`].
///
/// Pure Wayland without `DISPLAY` returns [`NullOverlay`] and logs a
/// corrective warning (layer-shell chip is a follow-up). Hybrid / X11
/// sessions keep the existing pill via `linux_x11::create`.
pub fn create(cfg: &UiConfig) -> Box<dyn OverlayBackend> {
    match overlay_backend() {
        OverlayBackendChoice::X11 => linux_x11::create(cfg),
        OverlayBackendChoice::Wayland => {
            #[cfg(feature = "wayland")]
            {
                if cfg.overlay {
                    match crate::linux_wayland::overlay::WaylandOverlay::new(cfg) {
                        Ok(o) => return Box::new(o),
                        Err(e) => log::warn!("Wayland overlay init failed: {e:#}; falling back to null"),
                    }
                }
            }
            Box::new(NullOverlay)
        }
        OverlayBackendChoice::NullWarn => {
            if cfg.overlay {
                match cfg.theme.as_str() {
                    "null" | "none" | "off" => {}
                    _ => log::warn!("{}", pure_wayland_overlay_warn()),
                }
            }
            Box::new(NullOverlay)
        }
    }
}

#[cfg(test)]
mod tests {
    //! WHY: facade must not panic constructing stdout emitters; pure-Wayland
    //! hotkey error must stay actionable even through the wrapper.
    use super::*;
    use crate::linux::selection::{
        hotkey_backend_from, pure_wayland_hotkey_error, typing_backend_from,
    };

    #[test]
    fn stdout_emitter_constructs_for_either_backend_path() {
        // Construction itself must not panic regardless of ambient env;
        // push/finish on stdout never spawns typers.
        let mut e = Emitter::new(OutputMode::Stdout);
        e.push("ok").unwrap();
        e.finish().unwrap();
        assert!(e.started());
    }

    #[test]
    fn selection_wiring_matches_policy() {
        assert_eq!(
            typing_backend_from(None, Some("wayland-1")),
            TypingBackend::Wtype
        );
        assert_eq!(
            hotkey_backend_from(None, Some("wayland-1")),
            {
                #[cfg(feature = "wayland")]
                { HotkeyBackend::Evdev }
                #[cfg(not(feature = "wayland"))]
                { HotkeyBackend::Unavailable }
            }
        );
        let msg = pure_wayland_hotkey_error();
        assert!(msg.contains("Corrective actions"), "{msg}");
    }
}
