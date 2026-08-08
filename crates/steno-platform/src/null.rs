//! Headless stand-ins for tests and servers.

use anyhow::Result;

use crate::traits::{HotkeySource, Typer};
use steno_core::InjectTyper;
use crate::HotkeyEvent;

pub use steno_core::overlay::NullOverlay;

/// Never emits hotkey events (polls as Cancel-free idle).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullHotkey;

impl NullHotkey {
    pub fn new() -> Self {
        Self
    }
}

impl HotkeySource for NullHotkey {
    fn next_event(&mut self) -> Result<HotkeyEvent> {
        // Block briefly so a tight poll loop does not spin the CPU.
        std::thread::sleep(std::time::Duration::from_millis(50));
        Ok(HotkeyEvent::Shutdown)
    }

    fn drain_pending(&mut self) {}
}

/// Never types. Prefer this whenever typing must stay disarmed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullTyper;

impl NullTyper {
    pub fn new() -> Self {
        Self
    }
}

impl Typer for NullTyper {
    fn type_text(&mut self, _text: &str) -> Result<()> {
        Ok(())
    }
}

impl InjectTyper for NullTyper {
    fn type_text(&mut self, text: &str) -> Result<()> {
        <Self as Typer>::type_text(self, text)
    }
}

#[cfg(test)]
mod tests {
    //! WHY: Headless/fallback platform implementations (`NullHotkey`, `NullTyper`) must operate
    //! without side effects or panics when running in headless environments or tests.
    use super::*;

    #[test]
    fn test_null_hotkey_new() {
        let mut hk = NullHotkey::new();
        let ev = hk.next_event().unwrap();
        assert_eq!(ev, HotkeyEvent::Shutdown);
    }

    #[test]
    fn test_null_typer_new() {
        let mut typer = NullTyper::new();
        assert!(crate::traits::Typer::type_text(&mut typer, "test").is_ok());
        assert!(InjectTyper::type_text(&mut typer, "test").is_ok());
    }

    /// WHY: Verify `NullHotkey`, `NullTyper`, and `NullOverlay` derive `PartialEq, Eq` and `Default`
    /// for equivalence checks in test environments.
    #[test]
    fn test_null_types_partial_eq() {
        assert_eq!(NullHotkey::new(), NullHotkey);
        assert_eq!(NullTyper::new(), NullTyper);
        assert_eq!(NullOverlay::new(), NullOverlay);
    }

    #[test]
    fn test_null_overlay_new() {
        // WHY: NullOverlay::new must return a valid NullOverlay instance that implements
        // OverlayBackend with active() == false and no-op set/flash calls.
        use steno_core::overlay::{OverlayBackend, Stage};
        let overlay = NullOverlay::new();
        assert!(!overlay.active(), "NullOverlay::new active() must return false");
        overlay.set(Stage::Recording);
        overlay.set(Stage::Transcribing);
        overlay.set(Stage::Done);
        overlay.flash(100);
    }
}
