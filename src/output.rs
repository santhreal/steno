//! Result delivery: stdout (default, composable) or synthetic keystrokes
//! into the focused X11 window via xdotool. Typing is fail-closed: it
//! only runs when the user armed `type_output = true` in their config
//! (enforced in main.rs before recording even starts), and control
//! characters other than '\n' are stripped so a transcript can never
//! smuggle Tab/Escape/CR keystrokes into the target.

use anyhow::{Context, Result, bail, ensure};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Stdout,
    Type,
}

pub fn emit(text: &str, mode: OutputMode) -> Result<()> {
    if text.is_empty() {
        log::debug!("empty transcript, nothing to emit");
        return Ok(());
    }
    match mode {
        OutputMode::Stdout => {
            println!("{text}");
            Ok(())
        }
        OutputMode::Type => type_text(text),
    }
}

fn type_text(text: &str) -> Result<()> {
    let text = sanitize_for_typing(text);
    if text.is_empty() {
        log::warn!("transcript contained only untypeable control characters; nothing typed");
        return Ok(());
    }
    // --delay 5: fast enough for dictation, slow enough that no app drops keys.
    // argv, not a shell: the transcript is never reparsed.
    let out = match Command::new("xdotool")
        .args(["type", "--clearmodifiers", "--delay", "5", "--", &text])
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("--type needs xdotool (X11 only). Install with: sudo apt install xdotool");
        }
        Err(e) => return Err(e).context("failed to run xdotool"),
    };
    ensure!(
        out.status.success(),
        "xdotool failed ({status}): {stderr} — check that you are in an X11 session (echo $DISPLAY) and that a window is focused",
        status = out.status,
        stderr = String::from_utf8_lossy(&out.stderr).trim(),
    );
    Ok(())
}

/// `xdotool type` sends control characters as real keystrokes: Tab moves
/// focus to another widget, Escape cancels dialogs, CR can submit forms.
/// Only '\n' is sent (the "new line" voice command is intentional);
/// every other control character is stripped before typing.
fn sanitize_for_typing(text: &str) -> String {
    let clean: String = text
        .chars()
        .filter(|&c| c == '\n' || !c.is_control())
        .collect();
    if clean.len() != text.len() {
        log::warn!("stripped control characters from the transcript before typing");
    }
    clean
}

#[cfg(test)]
mod tests {
    //! Regression tests for keystroke sanitization. WHY: a transcript
    //! containing Tab would switch focus mid-typing (sending the rest of
    //! the dictation into another widget), and Escape could dismiss the
    //! very dialog being dictated into.
    use super::*;

    #[test]
    fn tab_escape_and_cr_are_stripped() {
        assert_eq!(sanitize_for_typing("hello\tworld"), "helloworld");
        assert_eq!(sanitize_for_typing("run\u{1b}away"), "runaway");
        assert_eq!(sanitize_for_typing("a\rb"), "ab");
        assert_eq!(sanitize_for_typing("nul\u{0}byte"), "nulbyte");
        assert_eq!(sanitize_for_typing("del\u{7f}char"), "delchar");
    }

    #[test]
    fn newline_is_kept_by_design() {
        assert_eq!(
            sanitize_for_typing("line one\nline two"),
            "line one\nline two"
        );
    }

    #[test]
    fn printable_and_unicode_text_passes_through() {
        let s = "Hello, Welt! 100% — naïve café 日本語";
        assert_eq!(sanitize_for_typing(s), s);
    }

    #[test]
    fn stdout_emit_of_plain_text_succeeds() {
        emit("hello", OutputMode::Stdout).unwrap();
        emit("", OutputMode::Stdout).unwrap();
    }
}
