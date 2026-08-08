//! Cross-platform capability traits implemented across Linux, Windows, and macOS backends
//! (`HotkeySource` for push-to-talk / cancel events, `Typer` for keystroke injection).

use anyhow::Result;

use crate::HotkeyEvent;

/// Source of push-to-talk / cancel events.
pub trait HotkeySource: Send {
    /// Block until the next hotkey press/release event occurs.
    fn next_event(&mut self) -> Result<HotkeyEvent>;
    /// Discard any queued events that arrived while the listener was idle.
    fn drain_pending(&mut self);
}

/// Injects keystrokes into the focused window. Typing remains fail-closed
/// at the call site: only run when `type_output = true` in config.
pub trait Typer: Send {
    /// Emit keystrokes corresponding to `text` into the currently focused window.
    fn type_text(&mut self, text: &str) -> Result<()>;
}
