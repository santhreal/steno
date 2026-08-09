//! Linux Wayland platform: `wtype` typing + native layer-shell overlay.
//!
//! Hotkey stays on the X11 path when `DISPLAY` is available; pure Wayland
//! uses this module for typing and the layer-shell status pill.

pub mod output;
#[cfg(feature = "wayland")]
pub mod overlay;

pub use output::Emitter;
