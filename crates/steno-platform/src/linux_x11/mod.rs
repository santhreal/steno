//! Linux X11 backends: Caps Lock hotkey, status pill overlay, xdotool typing.

pub mod conn;
pub mod hotkey;
pub mod output;
pub mod overlay;

pub use hotkey::{Hotkey, restore_caps_lock_mapping};
pub use output::Emitter;
pub use overlay::{Overlay, create};
pub use crate::traits::{HotkeyEvent, OutputMode};
