//! Cross-platform capability traits. Linux X11 implements these today;
//! Windows/macOS backends will follow the same surface.

use anyhow::Result;

use crate::HotkeyEvent;

/// Source of push-to-talk / cancel events.
pub trait HotkeySource: Send {
    fn next_event(&mut self) -> Result<HotkeyEvent>;
    fn drain_pending(&mut self);
}

/// Injects keystrokes into the focused window. Typing remains fail-closed
/// at the call site: only run when `type_output = true` in config.
pub trait Typer: Send {
    fn type_text(&mut self, text: &str) -> Result<()>;
}
