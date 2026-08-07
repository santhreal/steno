//! OS backends for light-dictate: hotkey, overlay, typing.
//!
//! Re-exports overlay trait types from `dictate-core` so embedders can
//! depend on `dictate-platform` alone for UI wiring.

pub mod null;
pub mod traits;

#[cfg(target_os = "linux")]
pub mod linux_x11;

pub use dictate_core::overlay::{NullOverlay, OverlayBackend, Stage};
pub use null::{NullHotkey, NullTyper};
pub use traits::{HotkeySource, Typer};

#[cfg(target_os = "linux")]
pub use linux_x11::{Emitter, Hotkey, HotkeyEvent, OutputMode, Overlay, create};

#[cfg(not(target_os = "linux"))]
mod unsupported {
    use anyhow::{Result, bail};
    use dictate_core::config::UiConfig;
    use dictate_core::overlay::{NullOverlay, OverlayBackend};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum HotkeyEvent {
        Press,
        Release,
        Cancel,
        Shutdown,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum OutputMode {
        Stdout,
        Type,
    }

    pub struct Hotkey;
    impl Hotkey {
        pub fn grab_caps_lock() -> Result<Self> {
            bail!("hotkey grab is only implemented on Linux X11")
        }
    }

    pub struct Emitter {
        _mode: OutputMode,
    }
    impl Emitter {
        pub fn new(mode: OutputMode) -> Self {
            Self { _mode: mode }
        }
        pub fn push(&mut self, _chunk: &str) -> Result<()> {
            bail!("typed output is only implemented on Linux X11")
        }
        pub fn started(&self) -> bool {
            false
        }
        pub fn finish(&mut self) -> Result<()> {
            Ok(())
        }
    }

    pub fn create(_cfg: &UiConfig) -> Box<dyn OverlayBackend> {
        Box::new(NullOverlay)
    }
}

#[cfg(not(target_os = "linux"))]
pub use unsupported::{Emitter, Hotkey, HotkeyEvent, OutputMode, create};
