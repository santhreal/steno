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

/// Caps Lock hotkey. Pure Wayland without `DISPLAY` fails loudly; hybrid
/// XWayland sessions reuse the X11 grab.
pub struct Hotkey {
    inner: linux_x11::Hotkey,
}

impl Hotkey {
    /// Grab Caps Lock system-wide. On pure Wayland (no `DISPLAY`), returns
    /// an error with corrective actions instead of a silent no-op.
    pub fn grab_caps_lock() -> Result<Self> {
        match hotkey_backend() {
            HotkeyBackend::X11 => Ok(Self {
                inner: linux_x11::Hotkey::grab_caps_lock()?,
            }),
            HotkeyBackend::Unavailable => bail!("{}", pure_wayland_hotkey_error()),
        }
    }

    /// Restore Caps Lock if a prior daemon left it mapped to NoSymbol.
    /// No-op on pure Wayland (no X11 mapping to repair).
    pub fn restore_caps_lock_mapping() -> Result<bool> {
        match hotkey_backend() {
            HotkeyBackend::X11 => linux_x11::hotkey::restore_caps_lock_mapping(),
            HotkeyBackend::Unavailable => Ok(false),
        }
    }

    pub fn drain_pending(&mut self) {
        self.inner.drain_pending();
    }

    pub fn next_event(&mut self, held: &mut bool) -> Result<HotkeyEvent> {
        self.inner.next_event(held)
    }

    pub fn next_event_debug(
        &mut self,
        held: &mut bool,
        debug: bool,
        shutdown: &std::sync::atomic::AtomicBool,
    ) -> Result<HotkeyEvent> {
        self.inner.next_event_debug(held, debug, shutdown)
    }

    pub fn trigger_keycode(&self) -> x11rb::protocol::xproto::Keycode {
        self.inner.trigger_keycode()
    }
}

impl HotkeySource for Hotkey {
    fn next_event(&mut self) -> Result<HotkeyEvent> {
        HotkeySource::next_event(&mut self.inner)
    }

    fn drain_pending(&mut self) {
        HotkeySource::drain_pending(&mut self.inner)
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
            HotkeyBackend::Unavailable
        );
        let msg = pure_wayland_hotkey_error();
        assert!(msg.contains("Corrective actions"), "{msg}");
    }
}
