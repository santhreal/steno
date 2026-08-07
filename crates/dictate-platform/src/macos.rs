//! macOS OS backends: CGEvent typing, CGEventTap Caps Lock hotkey, AppKit overlay.
//!
//! ## Overlay
//! Minimal bottom-center [`NSPanel`] status chip (`objc2-app-kit`) implementing
//! [`OverlayBackend`]. [`create`] returns [`NullOverlay`] when `ui.overlay =
//! false` or `theme` is `null`/`none`/`off`; otherwise the chip.
//!
//! **Visual delta vs Linux X11 pill:** Linux draws an animated tiny-skia capsule
//! (icon disc + waveform/spinner/check/x, soft shadow, recording timer). macOS
//! ships a simpler AppKit chip — borderless floating `NSPanel` + `NSTextField`
//! stage label (optional recording timer via show_timer; no icon animation; system window shadow).
//! Colors/labels come from [`dictate_core::resolve_ui`]. Same bottom-center
//! placement; fail-open like Linux.
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
use dictate_core::{InjectTyper, ResolvedUi, resolve_ui};
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

fn ns_rgba(c: [u8; 4]) -> (f64, f64, f64, f64) {
    (
        c[0] as f64 / 255.0,
        c[1] as f64 / 255.0,
        c[2] as f64 / 255.0,
        c[3] as f64 / 255.0,
    )
}

/// Minimal AppKit status chip (`NSPanel` + `NSTextField`).
///
/// Pure display: nonactivating floating panel, ignores mouse, takes no focus.
/// Cosmetic and fail-open — AppKit/init failures disable the overlay without
/// affecting dictation.
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
            .name("dictate-overlay".into())
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

/// Logical padding / placement for the status chip (not a pixel-match of the
/// Linux pill — see module docs for the visual delta).
mod chip {
    pub const PAD_X: f64 = 14.0;
    pub const PAD_Y: f64 = 8.0;
    pub const LABEL_PX: f64 = 13.0;
    pub const BOTTOM_MARGIN: f64 = 48.0;
}

fn run_overlay(rx: Receiver<Stage>, failed: Arc<AtomicBool>, ui: ResolvedUi) {
    if let Err(e) = run_overlay_inner(rx, &ui) {
        log::debug!("overlay disabled: {e}");
        failed.store(true, Ordering::Relaxed);
    }
}

fn run_overlay_inner(rx: Receiver<Stage>, ui: &ResolvedUi) -> Result<()> {
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEventMask,
        NSFont, NSPanel, NSStatusWindowLevel, NSTextAlignment, NSTextField,
        NSWindowCollectionBehavior, NSWindowStyleMask,
    };
    use objc2_foundation::{
        NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, ns_string,
    };

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
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(120.0, 36.0)),
        style,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: panel is retained by this thread until we order it out and drop.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(true);
    panel.setLevel(NSStatusWindowLevel);
    panel.setIgnoresMouseEvents(true);
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );

    let label_view = NSTextField::labelWithString(ns_string!(""), mtm);
    label_view.setEditable(false);
    label_view.setSelectable(false);
    label_view.setBezeled(false);
    label_view.setDrawsBackground(true);
    let (br, bg, bb, ba) = ns_rgba(ui.colors.bg);
    label_view.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        br, bg, bb, ba,
    )));
    let (fr, fg, fb, fa) = ns_rgba(ui.colors.fg);
    label_view.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        fr, fg, fb, fa,
    )));
    label_view.setAlignment(NSTextAlignment::Center);
    label_view.setFont(Some(&NSFont::systemFontOfSize(chip::LABEL_PX)));

    let content = panel
        .contentView()
        .ok_or_else(|| anyhow::anyhow!("NSPanel missing contentView"))?;
    content.setWantsLayer(true);
    // label_view is an NSTextField (NSView subclass); retained by content.
    content.addSubview(&label_view);

    let mut current = Stage::Hidden;
    let mut recording_started = Instant::now();
    apply_stage(&panel, &label_view, mtm, current, ui, 0)?;

    loop {
        match rx.recv_timeout(Duration::from_millis(16)) {
            Ok(stage) => {
                if stage == Stage::Recording && current != Stage::Recording {
                    recording_started = Instant::now();
                }
                current = stage;
                while let Ok(more) = rx.try_recv() {
                    if more == Stage::Recording && current != Stage::Recording {
                        recording_started = Instant::now();
                    }
                    current = more;
                }
                let rec_secs = recording_started.elapsed().as_secs();
                apply_stage(&panel, &label_view, mtm, current, ui, rec_secs)?;
            }
            Err(RecvTimeoutError::Timeout) => {
                // Keep the recording timer label live while held.
                if current == Stage::Recording && ui.stages.show_timer {
                    let rec_secs = recording_started.elapsed().as_secs();
                    apply_stage(&panel, &label_view, mtm, current, ui, rec_secs)?;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
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
    }

    panel.orderOut(None);
    drop(panel);
    Ok(())
}

fn apply_stage(
    panel: &objc2_app_kit::NSPanel,
    label_view: &objc2_app_kit::NSTextField,
    mtm: objc2::MainThreadMarker,
    stage: Stage,
    ui: &ResolvedUi,
    rec_secs: u64,
) -> Result<()> {
    use objc2_app_kit::NSScreen;
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    if stage == Stage::Hidden {
        panel.orderOut(None);
        return Ok(());
    }

    let text = if stage == Stage::Recording && ui.stages.show_timer {
        format!(
            "{}  {}:{:02}",
            stage_text(ui, stage),
            rec_secs / 60,
            rec_secs % 60
        )
    } else {
        stage_text(ui, stage).to_string()
    };
    let ns = NSString::from_str(&text);
    label_view.setStringValue(&ns);
    // Error stage uses palette error tint for the label; others stay on fg.
    {
        use objc2_app_kit::NSColor;
        let (r, g, b, a) = if stage == Stage::Error {
            ns_rgba(ui.colors.error)
        } else {
            ns_rgba(ui.colors.fg)
        };
        label_view.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a)));
    }
    label_view.sizeToFit();

    let lf = label_view.frame();
    let chip_w = lf.size.width + chip::PAD_X * 2.0;
    let chip_h = lf.size.height + chip::PAD_Y * 2.0;
    label_view.setFrame(NSRect::new(
        NSPoint::new(chip::PAD_X, chip::PAD_Y),
        lf.size,
    ));

    let screen = NSScreen::mainScreen(mtm)
        .ok_or_else(|| anyhow::anyhow!("no main NSScreen for overlay"))?;
    let vis = screen.visibleFrame();
    let x = vis.origin.x + (vis.size.width - chip_w) * 0.5;
    let y = vis.origin.y + chip::BOTTOM_MARGIN;
    panel.setFrame_display(
        NSRect::new(NSPoint::new(x, y), NSSize::new(chip_w, chip_h)),
        true,
    );
    panel.orderFrontRegardless();
    Ok(())
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
}
