//! Result delivery: stdout (default, composable) or synthetic keystrokes
//! into the focused X11 window via xdotool.

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
    if !Command::new("xdotool")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        bail!("--type needs xdotool (X11 only). Install with: sudo apt install xdotool");
    }
    // --delay 5: fast enough for dictation, slow enough that no app drops keys.
    // argv, not a shell: the transcript is never reparsed.
    let status = Command::new("xdotool")
        .args(["type", "--clearmodifiers", "--delay", "5", "--", text])
        .status()
        .context("failed to spawn xdotool")?;
    ensure!(status.success(), "xdotool exited with {status}");
    Ok(())
}
