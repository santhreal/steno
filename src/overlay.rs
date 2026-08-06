//! Status overlay: a small borderless window at the bottom center of the
//! screen showing the current stage (recording / transcribing / done /
//! error). Pure display: override-redirect, takes no input, never focuses.
//!
//! Cosmetic and fail-open: no DISPLAY, no X connection, or any X error
//! simply disables the overlay — dictation itself is unaffected.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Show the bottom-center status overlay (X11 only).
    pub overlay: bool,
    /// How long the "done"/"error" stage stays visible before exit.
    pub done_flash_ms: u64,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            overlay: true,
            done_flash_ms: 600,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Recording,
    Transcribing,
    Done,
    Error,
}

fn label(stage: Stage) -> &'static str {
    match stage {
        Stage::Recording => "dictate - recording...",
        Stage::Transcribing => "dictate - transcribing...",
        Stage::Done => "dictate - done",
        Stage::Error => "dictate - error",
    }
}

pub struct Overlay {
    tx: Option<Sender<Stage>>,
}

impl Overlay {
    /// Start the overlay thread, or a no-op handle when disabled/unavailable.
    pub fn start(cfg: &UiConfig) -> Self {
        if !cfg.overlay || std::env::var_os("DISPLAY").is_none() {
            return Self { tx: None };
        }
        let (tx, rx) = channel::<Stage>();
        match thread::Builder::new()
            .name("dictate-overlay".into())
            .spawn(move || run(rx))
        {
            Ok(_) => Self { tx: Some(tx) },
            Err(e) => {
                log::debug!("overlay disabled: cannot spawn thread: {e}");
                Self { tx: None }
            }
        }
    }

    pub fn set(&self, stage: Stage) {
        if let Some(tx) = &self.tx {
            // A dead overlay thread must never block dictation.
            let _ = tx.send(stage);
        }
    }

    /// True when the overlay window actually exists.
    pub fn active(&self) -> bool {
        self.tx.is_some()
    }
}

// Dropping the Overlay closes the channel; the thread notices and
// destroys the window before exiting.

fn run(rx: Receiver<Stage>) {
    if let Err(e) = run_inner(&rx) {
        log::debug!("overlay disabled: {e}");
    }
}

fn run_inner(rx: &Receiver<Stage>) -> anyhow::Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;

    let (conn, screen_num) = x11rb::rust_connection::RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let (sw, sh) = (
        i32::from(screen.width_in_pixels),
        i32::from(screen.height_in_pixels),
    );
    const W: i32 = 320;
    const H: i32 = 34;
    const MARGIN: i32 = 80;
    let x = ((sw - W) / 2) as i16;
    let y = (sh - H - MARGIN) as i16;

    let win = conn.generate_id()?;
    conn.create_window(
        0, // depth 0 = copy from parent
        win,
        screen.root,
        x,
        y,
        W as u16,
        H as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new()
            .background_pixel(0x20_20_20)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE),
    )?;
    conn.map_window(win)?;
    conn.configure_window(
        win,
        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
    )?;

    // Core X font: no fontconfig/freetype dependency. Try a few common ones.
    let font = conn.generate_id()?;
    let mut opened = false;
    for name in [&b"9x15bold"[..], b"9x15", b"fixed"] {
        if conn.open_font(font, name).is_ok() {
            opened = true;
            break;
        }
    }
    if !opened {
        conn.destroy_window(win)?;
        anyhow::bail!("no core X font available (9x15bold/9x15/fixed)");
    }

    let gc = conn.generate_id()?;
    conn.create_gc(
        gc,
        win,
        &CreateGCAux::new()
            .foreground(0xe8_e8_e8)
            .background(0x20_20_20)
            .font(font),
    )?;

    let mut text = String::from("dictate");
    let redraw = |text: &str| -> anyhow::Result<()> {
        x11rb::protocol::xproto::clear_area(&conn, false, win, 0, 0, W as u16, H as u16)?;
        // ~9 px per char for 9x15 fonts; center approximately.
        let tw = text.len() as i16 * 9;
        let tx = ((W as i16 - tw) / 2).max(4);
        x11rb::protocol::xproto::image_text8(&conn, win, gc, tx, 22, text.as_bytes())?;
        conn.flush()?;
        Ok(())
    };
    redraw(&text)?;

    loop {
        // Drain stage updates (also detects main-thread exit).
        loop {
            match rx.try_recv() {
                Ok(stage) => {
                    text = label(stage).to_string();
                    redraw(&text)?;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    conn.destroy_window(win)?;
                    conn.flush()?;
                    return Ok(());
                }
            }
        }
        // Redraw on expose; otherwise nap briefly.
        match conn.poll_for_event() {
            Ok(Some(x11rb::protocol::Event::Expose(_))) => redraw(&text)?,
            Ok(_) => thread::sleep(Duration::from_millis(30)),
            Err(e) => return Err(e.into()),
        }
    }
}
