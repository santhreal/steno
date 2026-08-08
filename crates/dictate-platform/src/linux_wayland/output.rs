//! Wayland result delivery: stdout (default) or synthetic keystrokes via
//! `wtype` (ydotool optional fallback). Mirrors the X11 `xdotool` Emitter:
//! fail-closed typing, sanitize control characters, never panic on I/O.

use anyhow::{Context, Result, bail, ensure};

use crate::traits::OutputMode;
use crate::traits::Typer;
use dictate_core::InjectTyper;
use std::process::Command;

/// Progressive emitter for Wayland sessions (same surface as X11 `Emitter`).
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

    /// Finish the stream: trailing newline on stdout, nothing for typing.
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

impl Typer for Emitter {
    fn type_text(&mut self, text: &str) -> Result<()> {
        ensure!(
            self.mode == OutputMode::Type,
            "Emitter is in Stdout mode; typing is refused (fail-closed). Construct Emitter::new(OutputMode::Type) to enable keystrokes.",
        );
        type_text(text)
    }
}

impl InjectTyper for Emitter {
    fn type_text(&mut self, text: &str) -> Result<()> {
        <Self as Typer>::type_text(self, text)
    }
}

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
    type_with_wtype(&text).or_else(|wtype_err| {
        if is_missing_binary(&wtype_err) {
            match type_with_ydotool(&text) {
                Ok(()) => Ok(()),
                Err(ydotool_err) if is_missing_binary(&ydotool_err) => {
                    bail!("{}", missing_wayland_typer_hint());
                }
                Err(ydotool_err) => Err(ydotool_err).context(format!(
                    "wtype unavailable ({wtype_err}); ydotool also failed"
                )),
            }
        } else {
            Err(wtype_err)
        }
    })
}

fn is_missing_binary(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            || e.to_string().contains("needs wtype")
            || e.to_string().contains("needs ydotool")
    })
}

/// Install / session hint used when neither Wayland typer is present.
pub fn missing_wayland_typer_hint() -> &'static str {
    "--type on Wayland needs wtype (preferred) or ydotool. Install with: \
     sudo apt install wtype   # or: sudo apt install ydotool (requires ydotoold). \
     Check WAYLAND_DISPLAY is set and a window is focused. \
     If you are on X11/XWayland instead, ensure DISPLAY is set so xdotool is used."
}

fn type_with_wtype(text: &str) -> Result<()> {
    // -d 5: match xdotool --delay 5. `--` keeps transcript text out of option parsing.
    let out = match Command::new("wtype")
        .args(["-d", "5", "--", text])
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("--type needs wtype (Wayland). Install with: sudo apt install wtype");
        }
        Err(e) => return Err(e).context("failed to run wtype"),
    };
    ensure!(
        out.status.success(),
        "wtype failed ({status}): {stderr} — check WAYLAND_DISPLAY, that the compositor supports \
         virtual-keyboard, and that a window is focused",
        status = out.status,
        stderr = String::from_utf8_lossy(&out.stderr).trim(),
    );
    Ok(())
}

fn type_with_ydotool(text: &str) -> Result<()> {
    let out = match Command::new("ydotool")
        .args(["type", "--key-delay", "5", "--", text])
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("--type needs ydotool (Wayland fallback). Install with: sudo apt install ydotool");
        }
        Err(e) => return Err(e).context("failed to run ydotool"),
    };
    ensure!(
        out.status.success(),
        "ydotool failed ({status}): {stderr} — is ydotoold running? \
         (systemctl --user status ydotoold) Check WAYLAND_DISPLAY and focus.",
        status = out.status,
        stderr = String::from_utf8_lossy(&out.stderr).trim(),
    );
    Ok(())
}

/// `wtype` / `ydotool type` can turn control characters into real keys.
/// Only `\n` is kept (voice "new line"); everything else is stripped.
fn sanitize_for_typing(text: &str) -> String {
    // Keep intentional '\n' (voice "new line"). Strip Cc controls AND Unicode
    // Zl/Zp line/paragraph separators (U+2028/U+2029) — Rust's is_control()
    // only covers Cc, so those would otherwise inject breaks via xdotool/wtype.
    let clean: String = text
        .chars()
        .filter(|&c| c == '\n' || (!c.is_control() && !is_unicode_line_break(c)))
        .collect();
    if clean.len() != text.len() {
        log::warn!("stripped control / line-break characters from the transcript before typing");
    }
    clean
}

/// U+2028 LINE SEPARATOR / U+2029 PARAGRAPH SEPARATOR (Zl / Zp).
fn is_unicode_line_break(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}')
}

#[cfg(test)]
mod tests {
    //! WHY: missing-binary errors must name the package to install; sanitize
    //! must never panic; stdout mode must refuse typing (fail-closed).
    use super::*;

    #[test]
    fn tab_escape_and_cr_are_stripped() {
        assert_eq!(sanitize_for_typing("hello\tworld"), "helloworld");
        assert_eq!(sanitize_for_typing("run\u{1b}away"), "runaway");
        assert_eq!(sanitize_for_typing("a\rb"), "ab");
    }

    #[test]
    fn unicode_line_and_paragraph_separators_are_stripped() {
        assert_eq!(sanitize_for_typing("a\u{2028}b\u{2029}c"), "abc");
        assert_eq!(
            sanitize_for_typing("keep\nme\u{2028}not"),
            "keep\nmenot"
        );
    }

    #[test]
    fn newline_is_kept_by_design() {
        assert_eq!(
            sanitize_for_typing("line one\nline two"),
            "line one\nline two"
        );
    }

    #[test]
    fn missing_binary_hint_names_wtype_and_ydotool() {
        let hint = missing_wayland_typer_hint();
        assert!(hint.contains("wtype"), "{hint}");
        assert!(hint.contains("ydotool"), "{hint}");
        assert!(hint.contains("apt install"), "{hint}");
    }

    #[test]
    fn typer_refuses_stdout_mode_without_panic() {
        let mut e = Emitter::new(OutputMode::Stdout);
        let err = Typer::type_text(&mut e, "hi").expect_err("stdout must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("Stdout mode") || msg.contains("fail-closed"),
            "{msg}"
        );
    }

    #[test]
    fn stdout_emit_of_plain_text_succeeds() {
        let mut e = Emitter::new(OutputMode::Stdout);
        e.push("hello").unwrap();
        e.finish().unwrap();
    }

    #[test]
    fn missing_wtype_error_text_is_actionable() {
        let err =
            anyhow::anyhow!("--type needs wtype (Wayland). Install with: sudo apt install wtype");
        assert!(is_missing_binary(&err));
        let hint = missing_wayland_typer_hint();
        assert!(hint.contains("sudo apt install wtype"), "{hint}");
    }
}
