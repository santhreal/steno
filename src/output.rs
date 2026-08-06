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

/// Progressive emitter for streaming decode: receives FINAL (post-pipeline)
/// text chunks as whisper finalizes segments, joins them with correct
/// spacing, and writes each one out immediately — stdout flushes per chunk,
/// typing happens per chunk. The clipboard is never involved, so it can
/// never be clobbered.
pub struct Emitter {
    mode: OutputMode,
    /// Last character actually written, for join decisions.
    last: Option<char>,
}

impl Emitter {
    pub fn new(mode: OutputMode) -> Self {
        Self { mode, last: None }
    }

    /// Emit one processed chunk. Empty chunks are skipped.
    ///
    /// This runs inside whisper-rs's FFI callback, where a panic would
    /// cross an `extern "C"` boundary and ABORT the process — every error
    /// path here MUST return `Err`, never panic (no print!()/unwrap()).
    pub fn push(&mut self, chunk: &str) -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        let piece = join(self.last, chunk);
        match self.mode {
            OutputMode::Stdout => {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                out.write_all(piece.as_bytes())
                    .and_then(|()| out.flush())
                    .context("failed to write transcript to stdout")?;
                self.last = piece.chars().last();
            }
            OutputMode::Type => {
                // Track the last character ACTUALLY typed (sanitized), so
                // a trailing stripped control char can't skew the next join.
                let typed = sanitize_for_typing(&piece);
                type_text(&typed)?;
                self.last = typed.chars().last();
            }
        }
        Ok(())
    }

    /// True once at least one chunk has been written.
    pub fn started(&self) -> bool {
        self.last.is_some()
    }

    /// Finish the stream: trailing newline on stdout, nothing to do for typing.
    pub fn finish(&mut self) -> Result<()> {
        if self.mode == OutputMode::Stdout && self.last.is_some() {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            out.write_all(b"\n")
                .and_then(|()| out.flush())
                .context("failed to write transcript to stdout")?;
        }
        Ok(())
    }
}

/// Join a chunk onto the stream: insert one space when the previous chunk
/// ended on a word/punctuation and the next begins on a word character.
/// Chunks arrive pre-formatted, so no other spacing fixups happen here.
fn join(last: Option<char>, chunk: &str) -> String {
    let first = chunk.chars().next().expect("chunk is non-empty");
    let space = match last {
        None => false,
        Some(l) => {
            first.is_alphanumeric()
                && (l.is_alphanumeric()
                    || matches!(l, '.' | '!' | '?' | ',' | ';' | ':' | '%' | ')' | '"'))
        }
    };
    let mut piece = String::with_capacity(chunk.len() + 1);
    if space {
        piece.push(' ');
    }
    piece.push_str(chunk);
    piece
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
    fn join_inserts_space_between_words() {
        assert_eq!(join(None, "Hello"), "Hello");
        assert_eq!(join(Some('d'), "world"), " world");
        assert_eq!(join(Some('.'), "Next"), " Next");
        assert_eq!(join(Some(','), "main"), " main");
        assert_eq!(join(Some('"'), "quoted"), " quoted");
    }

    #[test]
    fn join_adds_no_space_before_punctuation_or_newlines() {
        assert_eq!(join(Some('d'), ","), ",");
        assert_eq!(join(Some('d'), "."), ".");
        assert_eq!(join(Some('d'), "\nnext"), "\nnext");
        assert_eq!(join(Some('\n'), "Next"), "Next");
    }

    #[test]
    fn stdout_emit_of_plain_text_succeeds() {
        let mut e = Emitter::new(OutputMode::Stdout);
        e.push("hello").unwrap();
        e.finish().unwrap();
        Emitter::new(OutputMode::Stdout).finish().unwrap();
    }
}
