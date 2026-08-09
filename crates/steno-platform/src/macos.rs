//! macOS OS backends: CGEvent typing, CGEventTap Caps Lock hotkey, AppKit overlay.
//!
//! ## Overlay
//! AppKit [`NSPanel`] status chip rendered with tiny-skia into an `NSImageView`
//! (`objc2-app-kit`) implementing [`OverlayBackend`]. [`create`] returns
//! [`NullOverlay`] when `ui.overlay = false` or `theme` is `null`/`none`/`off`;
//! otherwise the chip.
//!
//! **Visual delta vs Linux X11 pill:** same soft `box_blur_alpha` shadow, icon
//! disc + waveform/spinner/check/x, and recording timer (`show_timer`) as the
//! Windows chip, not a pixel-perfect Linux port (AppKit Retina backing scale
//! via `backingScaleFactor`; AppKit panel chrome instead of an X
//! override-redirect window; coarser motion).
//! Colors/labels come from [`steno_core::resolve_ui`]. Bottom-center
//! placement; fail-open like Linux.
//!
//! ## Permissions
//! Global taps and synthetic keystrokes require Accessibility trust. Failures
//! tell you to grant access to the terminal (or the `steno` binary) under
//! System Settings → Privacy & Security → Accessibility, then restart.
//!
//! ## Hotkey
//! Hold Caps Lock to record (KeyDown/KeyUp via `CGEventTap` at the HID point).
//! Caps Lock events are swallowed so the Lock toggle does not latch while we
//! run. Any other non-modifier key while held cancels. Typing remains
//! fail-closed at the call site (`type_output` arming lives in core/session).
use anyhow::{Context, Result, bail, ensure};
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
use steno_core::config::UiConfig;
use steno_core::overlay::{NullOverlay, OverlayBackend, Stage};
use steno_core::{InjectTyper, ResolvedUi, resolve_ui};
use fontdue::{Font, FontSettings};
use std::f32::consts::PI;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tiny_skia::{
    Color, Paint, Path as SkPath, PathBuilder, Pixmap as SkPixmap, PixmapPaint,
    PremultipliedColorU8, Transform,
};

use crate::traits::{HotkeyEvent, HotkeySource, OutputMode, Typer};

/// Corrective Accessibility hint shared by hotkey + typer failure paths.
const ACCESSIBILITY_HINT: &str = "Grant Accessibility to this terminal (or the steno binary) in \
     System Settings → Privacy & Security → Accessibility, then restart steno";

/// Auto-repeat / chatter grace after Caps Lock press before cancel-any-key.
const CANCEL_GRACE: Duration = Duration::from_millis(150);

/// Small gap between unicode key events so focused apps do not drop glyphs.
const TYPE_GAP: Duration = Duration::from_millis(2);


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
            .name("steno-macos-hotkey".into())
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

    /// Same as `next_event` but also checks `shutdown` between poll timeouts.
    pub fn next_event_debug(
        &mut self,
        held: &mut bool,
        _debug: bool,
        shutdown: &std::sync::atomic::AtomicBool,
    ) -> Result<HotkeyEvent> {
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
                    if shutdown.load(Ordering::Relaxed) {
                        return Ok(HotkeyEvent::Shutdown);
                    }
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

    /// No-op watchdog on macOS: Caps Lock is handled by CGEventTap, not X11
    /// keymap manipulation, so there is nothing to restore on kill.
    pub fn spawn_shutdown_watchdog(
        &self,
        shutdown: &'static std::sync::atomic::AtomicBool,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("steno-mac-watchdog".into())
            .spawn(move || {
                while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                }
                // Nothing to restore on macOS — the CGEventTap's Drop
                // disables it. This thread exists only for API parity.
            })
            .expect("cannot spawn macOS watchdog thread")
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
/// with **no clipboard**. Control characters other than `'\n'` are stripped.
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
    // Keep intentional '\n' (voice "new line"). Strip Cc controls AND Unicode
    // Zl/Zp line/paragraph separators (U+2028/U+2029) — Rust's is_control()
    // only covers Cc, so those would otherwise inject breaks via CGEvent.
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

/// Stage copy from resolved UI (owned strings live on [`ResolvedUi`]).
fn stage_text(ui: &ResolvedUi, stage: Stage) -> &str {
    match stage {
        Stage::Hidden => "",
        Stage::Recording => ui.stages.recording.as_str(),
        Stage::Transcribing => ui.stages.transcribing.as_str(),
        Stage::Done => ui.stages.done.as_str(),
        Stage::Error => ui.stages.error.as_str(),
    }
}


/// AppKit status chip (`NSPanel` + tiny-skia `NSImageView`).
///
/// Pure display: nonactivating floating panel, ignores mouse, takes no focus.
/// Cosmetic and fail-open: AppKit/font/init failures disable the overlay
/// without affecting dictation.
pub struct Overlay {
    tx: Option<Sender<Stage>>,
    /// Set when the overlay thread failed to start or aborted.
    failed: Arc<AtomicBool>,
}

impl Overlay {
    /// Start the AppKit overlay thread, or a no-op handle when disabled.
    pub fn start(cfg: &UiConfig) -> Self {
        let failed = Arc::new(AtomicBool::new(false));
        if !cfg.overlay {
            return Self { tx: None, failed };
        }
        let ui = resolve_ui(cfg);
        let (tx, rx) = mpsc::channel::<Stage>();
        let failed2 = failed.clone();
        match thread::Builder::new()
            .name("steno-overlay".into())
            .spawn(move || run_overlay(rx, failed2, ui))
        {
            Ok(_) => Self {
                tx: Some(tx),
                failed,
            },
            Err(e) => {
                log::debug!("overlay disabled: cannot spawn thread: {e}");
                Self { tx: None, failed }
            }
        }
    }

    pub fn set(&self, stage: Stage) {
        if let Some(tx) = &self.tx {
            // A dead overlay thread must never block dictation.
            let _ = tx.send(stage);
        }
    }

    /// True unless the overlay is disabled or already known-dead.
    pub fn active(&self) -> bool {
        self.tx.is_some() && !self.failed.load(Ordering::Relaxed)
    }

    /// Keep the final stage visible briefly before the caller hides it.
    pub fn flash(&self, ms: u64) {
        if self.active() {
            thread::sleep(Duration::from_millis(ms));
        }
    }
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

/// Build an overlay from [`UiConfig`].
///
/// - `overlay = false` → [`NullOverlay`]
/// - `theme` of `"null"` / `"none"` / `"off"` → [`NullOverlay`]
/// - otherwise → AppKit [`Overlay`] (palette via [`resolve_ui`])
pub fn create(cfg: &UiConfig) -> Box<dyn OverlayBackend> {
    if !cfg.overlay {
        return Box::new(NullOverlay);
    }
    match cfg.theme.as_str() {
        "null" | "none" | "off" => Box::new(NullOverlay),
        _ => Box::new(Overlay::start(cfg)),
    }
}

/// Logical design metrics (Linux-adjacent soft-shadow chip, not pixel-matched).
mod chip {
    pub const WIN_W: u32 = 268;
    pub const WIN_H: u32 = 120;
    pub const PILL_H: f32 = 46.0;
    pub const ICON: f32 = 26.0;
    pub const PAD_X: f32 = 16.0;
    pub const GAP: f32 = 12.0;
    pub const LABEL_PX: f32 = 13.0;
    pub const META_PX: f32 = 11.0;
    pub const BOTTOM_MARGIN: f64 = 48.0;
    pub const TOP_PAD: f32 = 24.0;
    pub const SHADOW_DY: f32 = 12.0;
    pub const SHADOW_BLUR: f32 = 12.0;
}

fn rgba(c: [u8; 4]) -> Color {
    Color::from_rgba8(c[0], c[1], c[2], c[3])
}

fn run_overlay(rx: Receiver<Stage>, failed: Arc<AtomicBool>, ui: ResolvedUi) {
    if let Err(e) = run_overlay_inner(rx, &ui) {
        log::debug!("overlay disabled: {e:#}");
        failed.store(true, Ordering::Relaxed);
    }
}

fn run_overlay_inner(rx: Receiver<Stage>, ui: &ResolvedUi) -> Result<()> {
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor,
        NSEventMask, NSImageView, NSPanel, NSStatusWindowLevel,
        NSWindowCollectionBehavior, NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize};

    let font = load_font().context("overlay font")?;

    // AppKit is main-thread-affine. This dedicated overlay thread becomes the
    // AppKit "main" for the accessory NSApp we create here (daemon has no UI
    // run loop of its own). Fail-open on any subsequent AppKit error.
    // SAFETY: we never hand this marker to another thread; all AppKit calls
    // below stay on this worker for its lifetime.
    let mtm = unsafe { MainThreadMarker::new_unchecked() };

    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(chip::WIN_W as f64, chip::WIN_H as f64),
        ),
        style,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: panel is retained by this thread until we order it out and drop.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    // Soft shadow is painted by tiny-skia; disable the system window shadow.
    panel.setHasShadow(false);
    panel.setLevel(NSStatusWindowLevel);
    panel.setIgnoresMouseEvents(true);
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );

    let image_view = NSImageView::initWithFrame(
        NSImageView::alloc(mtm),
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(chip::WIN_W as f64, chip::WIN_H as f64),
        ),
    );
    image_view.setEditable(false);
    image_view.setAnimates(false);

    let content = panel
        .contentView()
        .ok_or_else(|| anyhow::anyhow!("NSPanel missing contentView"))?;
    content.setWantsLayer(true);
    content.addSubview(&image_view);

    // Retina backing scale: render at physical-pixel resolution, display at
    // logical points. AppKit handles the logical↔physical mapping.
    let scale = panel.backingScaleFactor() as f32;
    let pw = ((chip::WIN_W as f32 * scale).round() as u32).max(1);
    let ph = ((chip::WIN_H as f32 * scale).round() as u32).max(1);
    let mut pixmap = SkPixmap::new(pw, ph)
        .ok_or_else(|| anyhow::anyhow!("tiny-skia pixmap alloc failed"))?;
    let mut shadow_mask = SkPixmap::new(pw, ph)
        .ok_or_else(|| anyhow::anyhow!("tiny-skia shadow mask alloc failed"))?;

    let mut current = Stage::Hidden;
    let mut stage_changed_at = Instant::now();
    let mut recording_started = Instant::now();
    let anim_start = Instant::now();
    let mut visible = false;

    loop {
        let mut got = false;
        match rx.recv_timeout(Duration::from_millis(16)) {
            Ok(stage) => {
                if stage == Stage::Recording && current != Stage::Recording {
                    recording_started = Instant::now();
                }
                current = stage;
                stage_changed_at = Instant::now();
                got = true;
                while let Ok(more) = rx.try_recv() {
                    if more == Stage::Recording && current != Stage::Recording {
                        recording_started = Instant::now();
                    }
                    current = more;
                    stage_changed_at = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if current == Stage::Hidden {
            if visible {
                panel.orderOut(None);
                visible = false;
            }
        } else {
            let anim_t = anim_start.elapsed().as_secs_f32();
            let stage_age = stage_changed_at.elapsed().as_secs_f32();
            let rec_secs = recording_started.elapsed().as_secs();
            draw_chip(
                &mut pixmap,
                &mut shadow_mask,
                &font,
                current,
                anim_t,
                stage_age,
                rec_secs,
                ui,
                scale,
            );
            let image = pixmap_to_nsimage(&pixmap, chip::WIN_W as f64, chip::WIN_H as f64)?;
            image_view.setImage(Some(&image));
            place_panel(&panel, mtm)?;
            if !visible {
                panel.orderFrontRegardless();
                visible = true;
            }
        }

        // Non-blocking AppKit pump so orderFront/orderOut take effect.
        loop {
            // SAFETY: NSDefaultRunLoopMode is a process-wide immutable CFString.
            let event = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&NSDate::distantPast()),
                unsafe { NSDefaultRunLoopMode },
                true,
            );
            match event {
                Some(ev) => app.sendEvent(&ev),
                None => break,
            }
        }

        // ~30 fps while visible; skip sleep if we just got a stage change.
        if current != Stage::Hidden && !got {
            thread::sleep(Duration::from_millis(33));
        }
    }

    panel.orderOut(None);
    drop(panel);
    Ok(())
}

fn place_panel(panel: &objc2_app_kit::NSPanel, mtm: objc2::MainThreadMarker) -> Result<()> {
    use objc2_app_kit::NSScreen;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let screen = NSScreen::mainScreen(mtm)
        .ok_or_else(|| anyhow::anyhow!("no main NSScreen for overlay"))?;
    let vis = screen.visibleFrame();
    let chip_w = chip::WIN_W as f64;
    let chip_h = chip::WIN_H as f64;
    let x = vis.origin.x + (vis.size.width - chip_w) * 0.5;
    let y = vis.origin.y + chip::BOTTOM_MARGIN;
    panel.setFrame_display(
        NSRect::new(NSPoint::new(x, y), NSSize::new(chip_w, chip_h)),
        true,
    );
    Ok(())
}

fn pixmap_to_nsimage(
    pixmap: &SkPixmap,
    logical_w: f64,
    logical_h: f64,
) -> Result<objc2::rc::Retained<objc2_app_kit::NSImage>> {
    use objc2::AnyThread;
    use objc2_app_kit::{NSBitmapImageRep, NSDeviceRGBColorSpace, NSImage};
    use objc2_foundation::NSSize;

    // Physical pixel dimensions for the bitmap representation (Retina-aware).
    let pw = pixmap.width() as isize;
    let ph = pixmap.height() as isize;
    // Null planes → AppKit allocates; we copy premultiplied RGBA into it.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            pw,
            ph,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            pw * 4,
            32,
        )
    }
    .ok_or_else(|| anyhow::anyhow!("NSBitmapImageRep init failed"))?;

    let src = pixmap.data();
    let dst_ptr = rep.bitmapData();
    if dst_ptr.is_null() {
        bail!("NSBitmapImageRep bitmapData is null; check AppKit pixel format support");
    }
    // SAFETY: AppKit-owned buffer sized bytesPerRow * height; we requested pw*4.
    let dst = unsafe { std::slice::from_raw_parts_mut(dst_ptr, src.len()) };
    dst.copy_from_slice(src);

    // NSImage size = logical points so AppKit maps the physical-pixel rep
    // to the correct logical dimensions on the NSImageView.
    let image = NSImage::initWithSize(
        NSImage::alloc(),
        NSSize::new(logical_w, logical_h),
    );
    image.addRepresentation(&rep);
    Ok(image)
}

fn load_font() -> Result<Font> {
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/Library/Fonts/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Helvetica.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        // Cross-check / CI hosts:
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
                log::debug!("overlay font: {}", Path::new(path).display());
                return Ok(font);
            }
        }
    }
    bail!("no usable overlay font (tried Arial / Helvetica / DejaVu)")
}

fn pill_width(stage: Stage) -> f32 {
    match stage {
        Stage::Recording => 188.0,
        Stage::Transcribing => 164.0,
        Stage::Done => 118.0,
        Stage::Error => 128.0,
        Stage::Hidden => 118.0,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_chip(
    pixmap: &mut SkPixmap,
    shadow_mask: &mut SkPixmap,
    font: &Font,
    stage: Stage,
    anim_t: f32,
    stage_age: f32,
    rec_secs: u64,
    ui: &ResolvedUi,
    scale: f32,
) {
    pixmap.fill(Color::from_rgba8(0, 0, 0, 0));
    if stage == Stage::Hidden {
        return;
    }

    let colors = &ui.colors;
    let pill_w = pill_width(stage) * scale;
    let pill_h = chip::PILL_H * scale;
    let x = (chip::WIN_W as f32 * scale - pill_w) * 0.5;
    let y = chip::TOP_PAD * scale;

    // Optional stage-change scale pulse (pulse_ms==0 disables).
    let pulse_secs = ui.stages.pulse_ms as f32 / 1000.0;
    let pulse = if ui.stages.pulse_ms == 0 || pulse_secs <= 0.0 {
        1.0
    } else if stage_age < pulse_secs {
        let t = stage_age / pulse_secs;
        let e = 1.0 - (1.0 - t).powi(3);
        0.97 + 0.03 * e
    } else {
        1.0
    };
    let cx = x + pill_w * 0.5;
    let cy = y + pill_h * 0.5;
    let pw = pill_w * pulse;
    let ph = pill_h * pulse;
    let px = cx - pw * 0.5;
    let py = cy - ph * 0.5;

    // Soft drop shadow (Linux-style separable box blur on alpha mask).
    draw_shadow(pixmap, shadow_mask, px, py, pw, ph, colors.shadow, scale);

    draw_round_rect(pixmap, px, py, pw, ph, ph * 0.5, rgba(colors.bg));
    stroke_round_rect(
        pixmap,
        px + 0.5,
        py + 0.5,
        pw - 1.0,
        ph - 1.0,
        ph * 0.5,
        rgba(colors.border),
        1.0,
    );

    let icon = chip::ICON * scale * pulse;
    let pad_x = chip::PAD_X * scale * pulse;
    let gap = chip::GAP * scale * pulse;
    let ix = px + pad_x;
    let iy = py + (ph - icon) * 0.5;
    let icon_disc = if stage == Stage::Error {
        rgba(colors.error)
    } else {
        rgba(colors.icon_bg)
    };
    fill_circle(
        pixmap,
        ix + icon * 0.5,
        iy + icon * 0.5,
        icon * 0.5,
        icon_disc,
    );

    let glyph = rgba(colors.icon_fg);
    match stage {
        Stage::Recording => draw_wave(pixmap, ix, iy, icon, anim_t, glyph),
        Stage::Transcribing => draw_spinner(pixmap, ix, iy, icon, anim_t, glyph),
        Stage::Done => draw_check(pixmap, ix, iy, icon, stage_age, glyph),
        Stage::Error => draw_x(pixmap, ix, iy, icon, glyph),
        Stage::Hidden => {}
    }

    let text = stage_text(ui, stage);
    let tx = ix + icon + gap;
    let ty = py + ph * 0.5;
    draw_text(
        pixmap,
        font,
        text,
        tx,
        ty,
        chip::LABEL_PX * scale * pulse,
        rgba(colors.fg),
    );

    // Elapsed timer on live capture when `[ui.stages].show_timer` is true.
    if stage == Stage::Recording && ui.stages.show_timer {
        let meta = format!("{}:{:02}", rec_secs / 60, rec_secs % 60);
        let meta_size = chip::META_PX * scale * pulse;
        let tw = text_width(font, &meta, meta_size);
        draw_text(
            pixmap,
            font,
            &meta,
            px + pw - pad_x - tw,
            ty,
            meta_size,
            rgba(colors.meta),
        );
    }
}

/// Drop shadow: rounded-rect alpha mask, box-blurred, tinted with palette shadow.
fn draw_shadow(
    pixmap: &mut SkPixmap,
    mask: &mut SkPixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    shadow: [u8; 4],
    scale: f32,
) {
    mask.fill(Color::from_rgba8(0, 0, 0, 0));
    draw_round_rect(
        mask,
        x,
        y + chip::SHADOW_DY * scale,
        w,
        h,
        h * 0.5,
        rgba(shadow),
    );
    box_blur_alpha(mask, (chip::SHADOW_BLUR * scale).max(1.0) as u32);
    pixmap.draw_pixmap(
        0,
        0,
        mask.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
}

/// Separable box blur over the alpha channel (premultiplied black, so all
/// channels scale with alpha), 3 passes ≈ gaussian. Prefix-sum windows
/// with constant divisor: edges fade to transparent, no lopsidedness.
fn box_blur_alpha(pm: &mut SkPixmap, radius: u32) {
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    if radius == 0 || w < 2 || h < 2 {
        return;
    }
    let mut buf: Vec<u32> = pm.pixels().iter().map(|p| p.alpha() as u32).collect();
    let mut tmp = vec![0u32; buf.len()];
    let r = radius as usize;
    let n = (2 * r + 1) as u32;
    let mut ps = vec![0u32; w.max(h) + 1];
    for _ in 0..3 {
        // Horizontal.
        for row in 0..h {
            let base = row * w;
            ps[0] = 0;
            for i in 0..w {
                ps[i + 1] = ps[i] + buf[base + i];
            }
            for col in 0..w {
                let lo = col.saturating_sub(r);
                let hi = (col + r).min(w - 1);
                tmp[base + col] = (ps[hi + 1] - ps[lo]) / n;
            }
        }
        // Vertical.
        for col in 0..w {
            ps[0] = 0;
            for i in 0..h {
                ps[i + 1] = ps[i] + tmp[i * w + col];
            }
            for row in 0..h {
                let lo = row.saturating_sub(r);
                let hi = (row + r).min(h - 1);
                buf[row * w + col] = (ps[hi + 1] - ps[lo]) / n;
            }
        }
    }
    for (px, a) in pm.pixels_mut().iter_mut().zip(buf.iter()) {
        *px = PremultipliedColorU8::from_rgba(0, 0, 0, (*a).min(255) as u8)
            .unwrap_or_else(|| PremultipliedColorU8::from_rgba(0, 0, 0, 0).unwrap());
    }
}

fn draw_wave(pixmap: &mut SkPixmap, ix: f32, iy: f32, icon: f32, t: f32, color: Color) {
    let cx = ix + icon * 0.5;
    let cy = iy + icon * 0.5;
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    for i in 0..4 {
        let phase = t * 6.0 + i as f32 * 0.7;
        let h = icon * (0.18 + 0.28 * (phase.sin() * 0.5 + 0.5));
        let w = icon * 0.08;
        let x = cx - icon * 0.28 + i as f32 * icon * 0.18;
        let y = cy - h * 0.5;
        if let Some(path) = round_rect_path(x, y, w, h, w * 0.5) {
            pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
}

fn draw_spinner(pixmap: &mut SkPixmap, ix: f32, iy: f32, icon: f32, t: f32, color: Color) {
    let cx = ix + icon * 0.5;
    let cy = iy + icon * 0.5;
    let r = icon * 0.28;
    let a0 = t * 6.0;
    let a1 = a0 + PI * 1.35;
    if let Some(path) = arc_path(cx, cy, r, a0, a1) {
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        if let Some(ribbon) = arc_ribbon(cx, cy, r, a0, a1, icon * 0.07) {
            pixmap.fill_path(
                &ribbon,
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                None,
            );
        } else {
            pixmap.stroke_path(
                &path,
                &paint,
                &tiny_skia::Stroke {
                    width: icon * 0.07,
                    ..tiny_skia::Stroke::default()
                },
                Transform::identity(),
                None,
            );
        }
    }
}

fn draw_check(pixmap: &mut SkPixmap, ix: f32, iy: f32, icon: f32, age: f32, color: Color) {
    let cx = ix + icon * 0.5;
    let cy = iy + icon * 0.5;
    let progress = (age / 0.25).clamp(0.0, 1.0);
    let mut pb = PathBuilder::new();
    let x0 = cx - icon * 0.18;
    let y0 = cy + icon * 0.02;
    let x1 = cx - icon * 0.04;
    let y1 = cy + icon * 0.16;
    let x2 = cx + icon * 0.20;
    let y2 = cy - icon * 0.14;
    pb.move_to(x0, y0);
    if progress < 0.45 {
        let t = progress / 0.45;
        pb.line_to(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
    } else {
        pb.line_to(x1, y1);
        let t = ((progress - 0.45) / 0.55).clamp(0.0, 1.0);
        pb.line_to(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t);
    }
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        pixmap.stroke_path(
            &path,
            &paint,
            &tiny_skia::Stroke {
                width: icon * 0.08,
                line_cap: tiny_skia::LineCap::Round,
                line_join: tiny_skia::LineJoin::Round,
                ..tiny_skia::Stroke::default()
            },
            Transform::identity(),
            None,
        );
    }
}

fn draw_x(pixmap: &mut SkPixmap, ix: f32, iy: f32, icon: f32, color: Color) {
    let cx = ix + icon * 0.5;
    let cy = iy + icon * 0.5;
    let o = icon * 0.16;
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let stroke = tiny_skia::Stroke {
        width: icon * 0.08,
        line_cap: tiny_skia::LineCap::Round,
        ..tiny_skia::Stroke::default()
    };
    for ((ax, ay), (bx, by)) in [
        ((cx - o, cy - o), (cx + o, cy + o)),
        ((cx + o, cy - o), (cx - o, cy + o)),
    ] {
        let mut pb = PathBuilder::new();
        pb.move_to(ax, ay);
        pb.line_to(bx, by);
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }
}


fn draw_round_rect(
    pixmap: &mut SkPixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    color: Color,
) {
    if let Some(path) = round_rect_path(x, y, w, h, radius) {
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn stroke_round_rect(
    pixmap: &mut SkPixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    color: Color,
    width: f32,
) {
    if let Some(path) = round_rect_path(x, y, w, h, radius) {
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        pixmap.stroke_path(
            &path,
            &paint,
            &tiny_skia::Stroke {
                width,
                ..tiny_skia::Stroke::default()
            },
            Transform::identity(),
            None,
        );
    }
}

fn round_rect_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<SkPath> {
    let r = radius.min(w * 0.5).min(h * 0.5).max(0.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

fn fill_circle(pixmap: &mut SkPixmap, cx: f32, cy: f32, r: f32, color: Color) {
    if let Some(path) = circle_path(cx, cy, r) {
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn circle_path(cx: f32, cy: f32, r: f32) -> Option<SkPath> {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    pb.finish()
}

fn arc_path(cx: f32, cy: f32, r: f32, a0: f32, a1: f32) -> Option<SkPath> {
    let mut pb = PathBuilder::new();
    let steps = 24usize;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let a = a0 + (a1 - a0) * t;
        let x = cx + r * a.cos();
        let y = cy + r * a.sin();
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.finish()
}

fn arc_ribbon(cx: f32, cy: f32, r: f32, a0: f32, a1: f32, thickness: f32) -> Option<SkPath> {
    let mut pb = PathBuilder::new();
    let steps = 24usize;
    let r0 = (r - thickness * 0.5).max(0.5);
    let r1 = r + thickness * 0.5;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let a = a0 + (a1 - a0) * t;
        let x = cx + r1 * a.cos();
        let y = cy + r1 * a.sin();
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    for i in (0..=steps).rev() {
        let t = i as f32 / steps as f32;
        let a = a0 + (a1 - a0) * t;
        pb.line_to(cx + r0 * a.cos(), cy + r0 * a.sin());
    }
    pb.close();
    pb.finish()
}

fn text_width(font: &Font, text: &str, size: f32) -> f32 {
    text.chars()
        .map(|ch| font.metrics(ch, size).advance_width)
        .sum()
}

fn draw_text(
    pixmap: &mut SkPixmap,
    font: &Font,
    text: &str,
    x: f32,
    center_y: f32,
    size: f32,
    color: Color,
) {
    let baseline = if let Some(m) = font.horizontal_line_metrics(size) {
        center_y + (m.ascent + m.descent) * 0.5
    } else {
        center_y + size * 0.35
    };
    let cr = (color.red() * 255.0) as u16;
    let cg = (color.green() * 255.0) as u16;
    let cb = (color.blue() * 255.0) as u16;
    let ca = (color.alpha() * 255.0) as u16;
    let mut pen_x = x;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size);
        if !bitmap.is_empty() && metrics.width > 0 && metrics.height > 0 {
            if let Some(mut glyph) = SkPixmap::new(metrics.width as u32, metrics.height as u32) {
                for (i, coverage) in bitmap.iter().enumerate() {
                    let a = (*coverage as u16 * ca + 127) / 255;
                    let r = (cr * a + 127) / 255;
                    let g = (cg * a + 127) / 255;
                    let b = (cb * a + 127) / 255;
                    glyph.pixels_mut()[i] =
                        PremultipliedColorU8::from_rgba(r as u8, g as u8, b as u8, a as u8)
                            .unwrap_or(PremultipliedColorU8::TRANSPARENT);
                }
                let gx = pen_x + metrics.xmin as f32;
                let gy = baseline - metrics.ymin as f32 - metrics.height as f32;
                pixmap.draw_pixmap(
                    gx as i32,
                    gy as i32,
                    glyph.as_ref(),
                    &tiny_skia::PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
            }
        }
        pen_x += metrics.advance_width;
    }
}


#[cfg(test)]
mod overlay_tests {
    use super::*;

    #[test]
    fn create_with_overlay_false_is_null() {
        let cfg = UiConfig {
            overlay: false,
            ..UiConfig::default()
        };
        let ov = create(&cfg);
        ov.set(Stage::Recording);
        ov.flash(0);
        assert!(!ov.active());
    }

    #[test]
    fn create_theme_null_aliases_are_null() {
        for theme in ["null", "none", "off"] {
            let cfg = UiConfig {
                theme: theme.to_string(),
                ..UiConfig::default()
            };
            let ov = create(&cfg);
            ov.set(Stage::Done);
            assert!(!ov.active(), "theme {theme}");
        }
    }

    #[test]
    fn stage_labels_match_linux() {
        let ui = resolve_ui(&UiConfig::default());
        assert_eq!(stage_text(&ui, Stage::Recording), "Transcribing");
        assert_eq!(stage_text(&ui, Stage::Transcribing), "Processing");
        assert_eq!(stage_text(&ui, Stage::Done), "Done");
        assert_eq!(stage_text(&ui, Stage::Error), "Error");
        assert_eq!(stage_text(&ui, Stage::Hidden), "");
    }

    #[test]
    fn show_timer_flag_round_trips_resolve() {
        assert!(resolve_ui(&UiConfig::default()).stages.show_timer);
        let mut stages = steno_core::config::UiStages::default();
        stages.show_timer = false;
        let cfg = UiConfig {
            stages,
            ..UiConfig::default()
        };
        assert!(!resolve_ui(&cfg).stages.show_timer);
    }

    #[test]
    fn box_blur_alpha_softens_opaque_center() {
        // WHY: soft shadow must not remain a hard-edged rect after blur.
        let mut pm = tiny_skia::Pixmap::new(64, 64).expect("pixmap");
        pm.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 0));
        let mut paint = tiny_skia::Paint::default();
        paint.set_color(tiny_skia::Color::from_rgba8(0, 0, 0, 255));
        paint.anti_alias = false;
        let mut pb = tiny_skia::PathBuilder::new();
        pb.push_circle(32.0, 32.0, 10.0);
        let path = pb.finish().expect("circle");
        pm.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
        super::box_blur_alpha(&mut pm, 4);
        let center = pm.pixel(32, 32).expect("center").alpha();
        let edge = pm.pixel(32, 20).expect("edge").alpha();
        assert!(center > edge, "center {center} should exceed edge {edge}");
        assert!(edge > 0, "blur must spill alpha past the hard disc");
    }
}
