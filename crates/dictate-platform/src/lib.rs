//! OS backends for light-dictate: hotkey, overlay, typing.
//!
//! Re-exports overlay trait types from `dictate-core` so embedders can
//! depend on `dictate-platform` alone for UI wiring.

pub mod null;
pub mod traits;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub mod linux_wayland;
#[cfg(target_os = "linux")]
pub mod linux_x11;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "macos")]
pub mod macos;

pub use dictate_core::overlay::{NullOverlay, OverlayBackend, Stage};
pub use null::{NullHotkey, NullTyper};
pub use traits::{HotkeySource, Typer};

#[cfg(target_os = "linux")]
pub use linux::{Emitter, Hotkey, HotkeyEvent, OutputMode, Overlay, create};
#[cfg(target_os = "linux")]
pub use linux_x11::restore_caps_lock_mapping;
#[cfg(target_os = "windows")]
pub use windows::{Emitter, Hotkey, HotkeyEvent, OutputMode, Overlay, create};
#[cfg(target_os = "macos")]
pub use macos::{Emitter, Hotkey, HotkeyEvent, OutputMode, Overlay, create};
