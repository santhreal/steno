//! Headless stand-ins for tests and servers.

use anyhow::Result;

use crate::traits::{HotkeySource, Typer};
use crate::HotkeyEvent;

pub use dictate_core::overlay::NullOverlay;

/// Never emits hotkey events (polls as Cancel-free idle).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullHotkey;

impl HotkeySource for NullHotkey {
    fn next_event(&mut self) -> Result<HotkeyEvent> {
        // Block briefly so a tight poll loop does not spin the CPU.
        std::thread::sleep(std::time::Duration::from_millis(50));
        Ok(HotkeyEvent::Shutdown)
    }

    fn drain_pending(&mut self) {}
}

/// Never types. Prefer this whenever typing must stay disarmed.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullTyper;

impl Typer for NullTyper {
    fn type_text(&mut self, _text: &str) -> Result<()> {
        Ok(())
    }
}
