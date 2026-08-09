//! Linux Wayland platform: `wtype` typing, native layer-shell overlay,
//! and evdev Caps Lock hotkey for pure Wayland sessions.
//!
//! When `DISPLAY` is available the X11 hotkey/overlay path is preferred;
//! pure Wayland uses evdev for the hotkey and layer-shell for the overlay.

pub mod output;
#[cfg(feature = "wayland")]
pub mod overlay;
#[cfg(feature = "wayland")]
pub mod hotkey;

pub use output::Emitter;
