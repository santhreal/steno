//! Cross-platform capability traits implemented across Linux, Windows, and macOS backends
//! (`HotkeySource` for push-to-talk / cancel events, `Typer` for keystroke injection).

use anyhow::Result;

/// Push-to-talk hotkey event variants emitted by platform listeners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// Hotkey pressed: start recording.
    Press,
    /// Hotkey released: stop recording and begin transcription.
    Release,
    /// Hotkey cancelled (e.g. Escape pressed or modifier interrupted).
    Cancel,
    /// Platform backend requested shutdown.
    Shutdown,
}

impl std::fmt::Display for HotkeyEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Press => write!(f, "Press"),
            Self::Release => write!(f, "Release"),
            Self::Cancel => write!(f, "Cancel"),
            Self::Shutdown => write!(f, "Shutdown"),
        }
    }
}

/// Target output destination for transcriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// Print transcripts to standard output.
    Stdout,
    /// Inject synthetic keystrokes into the active focused window.
    #[default]
    Type,
}

/// Source of push-to-talk / cancel events.
pub trait HotkeySource: Send {
    /// Block until the next hotkey press/release event occurs.
    fn next_event(&mut self) -> Result<HotkeyEvent>;
    /// Discard any queued events that arrived while the listener was idle.
    fn drain_pending(&mut self) {}
}

/// Injects keystrokes into the focused window. Typing remains fail-closed
/// at the call site: only run when `type_output = true` in config.
pub trait Typer: Send {
    /// Emit keystrokes corresponding to `text` into the currently focused window.
    fn type_text(&mut self, text: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WHY: Ensure `HotkeyEvent` `Display` formatting matches exact variant string representations
    /// so logging and error reporting are consistent across platform implementations.
    #[test]
    fn test_hotkey_event_display() {
        assert_eq!(HotkeyEvent::Press.to_string(), "Press");
        assert_eq!(HotkeyEvent::Release.to_string(), "Release");
        assert_eq!(HotkeyEvent::Cancel.to_string(), "Cancel");
        assert_eq!(HotkeyEvent::Shutdown.to_string(), "Shutdown");
    }

    /// WHY: Verify `OutputMode` `Default` defaults to `OutputMode::Type` as required by platform specifications.
    #[test]
    fn test_output_mode_default() {
        assert_eq!(OutputMode::default(), OutputMode::Type);
    }
}
