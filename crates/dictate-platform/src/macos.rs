//! macOS OS backends: CGEvent typing, CGEventTap Caps Lock hotkey, NullOverlay.
//!
//! ## Overlay (v1)
//! There is **no NSPanel overlay yet**. [`create`] always returns
//! [`NullOverlay`]. Status UI on macOS is intentionally headless until a
//! real AppKit panel lands; hotkey + typing work without it.
//!
//! ## Permissions
//! Global taps and synthetic keystrokes require Accessibility trust. Failures
//! tell you to grant access to the terminal (or the `dictate` binary) under
//! System Settings → Privacy & Security → Accessibility, then restart.
//!
//! ## Hotkey
//! Hold Caps Lock to record (KeyDown/KeyUp via `CGEventTap` at the HID point).
//! Caps Lock events are swallowed so the Lock toggle does not latch while we
//! run. Any other non-modifier key while held cancels. Typing remains
//! fail-closed at the call site (`type_output` arming lives in core/session).

use anyhow::{Result, bail, ensure};
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop};
use core_foundation::string::CFString;
use core_graphics::event::{
    CallbackResult, CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventType, EventField, KeyCode,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use dictate_core::config::UiConfig;
use dictate_core::overlay::{NullOverlay, OverlayBackend, Stage};
use dictate_core::InjectTyper;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::traits::{HotkeySource, Typer};

/// Corrective Accessibility hint shared by hotkey + typer failure paths.
const ACCESSIBILITY_HINT: &str = "Grant Accessibility to this terminal (or the dictate binary) in \
     System Settings → Privacy & Security → Accessibility, then restart dictate";

/// Auto-repeat / chatter grace after Caps Lock press before cancel-any-key.
const CANCEL_GRACE: Duration = Duration::from_millis(150);

/// Small gap between unicode key events so focused apps do not drop glyphs.
const TYPE_GAP: Duration = Duration::from_millis(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Press,
    Release,
    Cancel,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Stdout,
    Type,
}

/// Caps Lock hold via `CGEventTap` (KeyDown = press, KeyUp = release).
pub struct Hotkey {
    rx: Receiver<HotkeyEvent>,
    stop: Arc<AtomicBool>,
    /// Worker runloop pointer (`CFRunLoopRef` as usize) for cross-thread stop.
    runloop: Arc<Mutex<Option<usize>>>,
    worker: Option<JoinHandle<()>>,
    /// Hold state for [`HotkeySource::next_event`].
    source_held: bool,
}

impl Hotkey {
    /// Install a HID event tap for Caps Lock hold-to-talk.
    ///
    /// Requires Accessibility. Caps Lock is dropped at the tap so the system
    /// Lock flag does not toggle for the lifetime of this grab.
    pub fn grab_caps_lock() -> Result<Self> {
        ensure_accessibility(/* prompt */ true)?;

        let (tx, rx) = mpsc::channel::<HotkeyEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let runloop = Arc::new(Mutex::new(None));
        let stop_w = Arc::clone(&stop);
        let runloop_w = Arc::clone(&runloop);

        let worker = thread::Builder::new()
            .name("dictate-macos-hotkey".into())
            .spawn(move || hotkey_tap_thread(tx, stop_w, runloop_w))
            .map_err(|e| anyhow::anyhow!("failed to spawn macOS hotkey thread: {e}"))?;

        // Give the worker a moment to fail fast if the tap cannot install.
        thread::sleep(Duration::from_millis(50));
        if worker.is_finished() {
            // Channel closed without events ⇒ tap setup failed.
            bail!(
                "macOS CGEventTap for Caps Lock failed to install. {ACCESSIBILITY_HINT}"
            );
        }

        Ok(Self {
            rx,
            stop,
            runloop,
            worker: Some(worker),
            source_held: false,
        })
    }

    pub fn drain_pending(&mut self) {
        while self.rx.try_recv().is_ok() {}
    }

    pub fn next_event(&mut self, held: &mut bool) -> Result<HotkeyEvent> {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(250)) {
                Ok(ev) => {
                    match ev {
                        HotkeyEvent::Press => *held = true,
                        HotkeyEvent::Release | HotkeyEvent::Cancel | HotkeyEvent::Shutdown => {
                            *held = false;
                        }
                    }
                    return Ok(ev);
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.stop.load(Ordering::SeqCst) {
                        return Ok(HotkeyEvent::Shutdown);
                    }
                    if self.worker.as_ref().is_some_and(|w| w.is_finished()) {
                        return Ok(HotkeyEvent::Shutdown);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(HotkeyEvent::Shutdown),
            }
        }
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(guard) = self.runloop.lock() {
            if let Some(ptr) = *guard {
                // SAFETY: pointer was stored from CFRunLoop::get_current on the
                // worker; CFRunLoopStop is documented as safe from any thread.
                let rl = unsafe {
                    CFRunLoop::wrap_under_get_rule(ptr as core_foundation::runloop::CFRunLoopRef)
                };
                rl.stop();
            }
        }
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl HotkeySource for Hotkey {
    fn next_event(&mut self) -> Result<HotkeyEvent> {
        let mut held = self.source_held;
        let ev = Hotkey::next_event(self, &mut held)?;
        self.source_held = held;
        Ok(ev)
    }

    fn drain_pending(&mut self) {
        Hotkey::drain_pending(self);
    }
}

fn hotkey_tap_thread(
    tx: Sender<HotkeyEvent>,
    stop: Arc<AtomicBool>,
    runloop_slot: Arc<Mutex<Option<usize>>>,
) {
    let held = Arc::new(AtomicBool::new(false));
    let press_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let tx_cb = tx.clone();
    let held_cb = Arc::clone(&held);
    let press_cb = Arc::clone(&press_at);

    let tap = match CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::TapDisabledByTimeout,
            CGEventType::TapDisabledByUserInput,
        ],
        move |_proxy, etype, event| {
            if matches!(
                etype,
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
            ) {
                // Tap reference lives outside the callback; the runloop loop
                // re-enables via the outer `tap.enable()` after timeouts when
                // possible. Pass through.
                return CallbackResult::Keep;
            }

            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let autorepeat =
                event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0;

            if keycode == KeyCode::CAPS_LOCK {
                match etype {
                    CGEventType::KeyDown if !autorepeat => {
                        if !held_cb.swap(true, Ordering::SeqCst) {
                            *press_cb.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(Instant::now());
                            let _ = tx_cb.send(HotkeyEvent::Press);
                        }
                        return CallbackResult::Drop;
                    }
                    CGEventType::KeyUp => {
                        if held_cb.swap(false, Ordering::SeqCst) {
                            *press_cb.lock().unwrap_or_else(|e| e.into_inner()) = None;
                            let _ = tx_cb.send(HotkeyEvent::Release);
                        }
                        return CallbackResult::Drop;
                    }
                    CGEventType::FlagsChanged => {
                        // Swallow flag churn from Caps Lock so Lock does not
                        // latch even if the system synthesizes FlagsChanged.
                        return CallbackResult::Drop;
                    }
                    _ => return CallbackResult::Drop,
                }
            }

            if matches!(etype, CGEventType::KeyDown)
                && !autorepeat
                && held_cb.load(Ordering::SeqCst)
                && !is_modifier_keycode(keycode)
            {
                let ok_to_cancel = press_cb
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .map(|t| t.elapsed() >= CANCEL_GRACE)
                    .unwrap_or(false);
                if ok_to_cancel {
                    held_cb.store(false, Ordering::SeqCst);
                    *press_cb.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    let _ = tx_cb.send(HotkeyEvent::Cancel);
                }
            }

            CallbackResult::Keep
        },
    ) {
        Ok(t) => t,
        Err(()) => {
            // Probe sender closed → grab_caps_lock sees finished worker.
            return;
        }
    };

    let rl = CFRunLoop::get_current();
    if let Ok(mut slot) = runloop_slot.lock() {
        *slot = Some(rl.as_CFTypeRef() as usize);
    }

    let Ok(loop_source) = tap.mach_port().create_runloop_source(0) else {
        return;
    };
    rl.add_source(&loop_source, unsafe { kCFRunLoopCommonModes });
    tap.enable();

    while !stop.load(Ordering::SeqCst) {
        let _ = CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            Duration::from_millis(250),
            false,
        );
        // Re-enable in case the OS disabled the tap after a timeout.
        tap.enable();
    }

    let _ = tx.send(HotkeyEvent::Shutdown);
}

fn is_modifier_keycode(keycode: u16) -> bool {
    matches!(
        keycode,
        KeyCode::CAPS_LOCK
            | KeyCode::SHIFT
            | KeyCode::RIGHT_SHIFT
            | KeyCode::CONTROL
            | KeyCode::RIGHT_CONTROL
            | KeyCode::OPTION
            | KeyCode::RIGHT_OPTION
            | KeyCode::COMMAND
            | KeyCode::RIGHT_COMMAND
            | KeyCode::FUNCTION
    )
}

/// Progressive emitter: stdout chunks or CGEvent unicode keystrokes.
///
/// Typing uses `CGEventKeyboardSetUnicodeString` (via [`CGEvent::set_string`])
/// — **no clipboard**. Control characters other than `'\n'` are stripped.
pub struct Emitter {
    mode: OutputMode,
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
                    .map_err(|e| anyhow::anyhow!("failed to write transcript to stdout: {e}"))?;
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

    pub fn started(&self) -> bool {
        self.last.is_some()
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.mode == OutputMode::Stdout && self.last.is_some() {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            out.write_all(b"\n")
                .and_then(|()| out.flush())
                .map_err(|e| anyhow::anyhow!("failed to write transcript to stdout: {e}"))?;
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

fn type_text(text: &str) -> Result<()> {
    let text = sanitize_for_typing(text);
    if text.is_empty() {
        log::warn!("transcript contained only untypeable control characters; nothing typed");
        return Ok(());
    }
    ensure_accessibility(/* prompt */ false)?;

    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).map_err(|()| {
        anyhow::anyhow!(
            "CGEventSourceCreate failed while typing. {ACCESSIBILITY_HINT}"
        )
    })?;

    for ch in text.chars() {
        if ch == '\n' {
            post_keycode(source.clone(), KeyCode::RETURN)?;
        } else {
            post_unicode(source.clone(), ch)?;
        }
        thread::sleep(TYPE_GAP);
    }
    Ok(())
}

fn post_unicode(source: CGEventSource, ch: char) -> Result<()> {
    let mut buf = [0u16; 2];
    let encoded = ch.encode_utf16(&mut buf);
    let down = CGEvent::new_keyboard_event(source.clone(), 0, true).map_err(|()| {
        anyhow::anyhow!("CGEventCreateKeyboardEvent(down) failed. {ACCESSIBILITY_HINT}")
    })?;
    down.set_string_from_utf16_unchecked(encoded);
    down.post(CGEventTapLocation::HID);

    let up = CGEvent::new_keyboard_event(source, 0, false).map_err(|()| {
        anyhow::anyhow!("CGEventCreateKeyboardEvent(up) failed. {ACCESSIBILITY_HINT}")
    })?;
    up.post(CGEventTapLocation::HID);
    Ok(())
}

fn post_keycode(source: CGEventSource, keycode: u16) -> Result<()> {
    let down = CGEvent::new_keyboard_event(source.clone(), keycode, true).map_err(|()| {
        anyhow::anyhow!("CGEventCreateKeyboardEvent(down) failed. {ACCESSIBILITY_HINT}")
    })?;
    down.post(CGEventTapLocation::HID);
    let up = CGEvent::new_keyboard_event(source, keycode, false).map_err(|()| {
        anyhow::anyhow!("CGEventCreateKeyboardEvent(up) failed. {ACCESSIBILITY_HINT}")
    })?;
    up.post(CGEventTapLocation::HID);
    Ok(())
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

fn ensure_accessibility(prompt: bool) -> Result<()> {
    let trusted = if prompt {
        accessibility_trusted_with_prompt()
    } else {
        unsafe { AXIsProcessTrusted() }
    };
    if trusted {
        return Ok(());
    }
    bail!(
        "macOS Accessibility permission missing. {ACCESSIBILITY_HINT}"
    );
}

fn accessibility_trusted_with_prompt() -> bool {
    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let value = CFBoolean::true_value();
    let dict = CFDictionary::from_CFType_pairs(&[(key, value)]);
    unsafe { AXIsProcessTrustedWithOptions(dict.as_concrete_TypeRef()) }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(
        options: core_foundation::dictionary::CFDictionaryRef,
    ) -> bool;
}

/// Status overlay stub. Prefer [`create`] — v1 ships [`NullOverlay`] only
/// (no NSPanel yet). Methods are no-ops so accidental construction is safe.
pub struct Overlay;

impl Overlay {
    pub fn start(_cfg: &UiConfig) -> Self {
        Self
    }

    pub fn set(&self, _stage: Stage) {}

    pub fn active(&self) -> bool {
        false
    }

    pub fn flash(&self, _ms: u64) {}
}

impl OverlayBackend for Overlay {
    fn set(&self, stage: Stage) {
        Overlay::set(self, stage);
    }

    fn flash(&self, ms: u64) {
        Overlay::flash(self, ms);
    }

    fn active(&self) -> bool {
        Overlay::active(self)
    }
}

/// Always returns [`NullOverlay`].
///
/// **macOS v1 has no visual overlay** (NSPanel deferred). Headless embeds and
/// daemon status still work; use Linux X11 for the pill UI.
pub fn create(_cfg: &UiConfig) -> Box<dyn OverlayBackend> {
    Box::new(NullOverlay)
}
