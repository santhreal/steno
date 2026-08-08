//! Linux Wayland MVP: `wtype` typing (ydotool fallback). Hotkey/overlay
//! stay on the X11 path when `DISPLAY` is available; pure Wayland uses
//! this module for typing and fails/warns loudly for hotkey/overlay.

pub mod output;

pub use output::Emitter;
