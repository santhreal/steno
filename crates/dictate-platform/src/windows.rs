//! Windows OS backends (compile stubs).
//!
//! Real backends will use RegisterHotKey / SendInput / layered windows.
//! Until then every capability method fails closed with a corrective hint;
//! [`create`] returns [`NullOverlay`] so headless embeds still work.

use anyhow::{Result, bail};
use dictate_core::config::UiConfig;
use dictate_core::overlay::{NullOverlay, OverlayBackend, Stage};

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

/// Caps Lock grab — not implemented on Windows yet.
pub struct Hotkey;

impl Hotkey {
    pub fn grab_caps_lock() -> Result<Self> {
        bail!(
            "Windows backend not implemented yet — use Linux X11 or NullHotkey \
             (RegisterHotKey path is Phase 4)"
        )
    }

    pub fn drain_pending(&mut self) {}

    pub fn next_event(&mut self, _held: &mut bool) -> Result<HotkeyEvent> {
        bail!(
            "Windows backend not implemented yet — use Linux X11 or NullHotkey \
             (RegisterHotKey path is Phase 4)"
        )
    }
}

/// Progressive emitter — typing not implemented on Windows yet.
pub struct Emitter {
    _mode: OutputMode,
}

impl Emitter {
    pub fn new(mode: OutputMode) -> Self {
        Self { _mode: mode }
    }

    pub fn push(&mut self, _chunk: &str) -> Result<()> {
        bail!(
            "Windows backend not implemented yet — use Linux X11 or NullTyper \
             (SendInput path is Phase 4)"
        )
    }

    pub fn started(&self) -> bool {
        false
    }

    pub fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Status overlay stub. Prefer [`create`] + [`NullOverlay`] for embeds.
pub struct Overlay;

impl Overlay {
    pub fn start(_cfg: &UiConfig) -> Self {
        Self
    }

    pub fn set(&self, _stage: Stage) {}

    pub fn active(&self) -> bool {
        false
    }

    pub fn flash(&self, _ms: u64) {}
}

impl OverlayBackend for Overlay {
    fn set(&self, stage: Stage) {
        Overlay::set(self, stage);
    }

    fn flash(&self, ms: u64) {
        Overlay::flash(self, ms);
    }

    fn active(&self) -> bool {
        Overlay::active(self)
    }
}

/// Always returns [`NullOverlay`] so headless / embed builds work on Windows.
pub fn create(_cfg: &UiConfig) -> Box<dyn OverlayBackend> {
    Box::new(NullOverlay)
}

