//! Windows OS backends: Caps Lock push-to-talk, SendInput typing, status chip.
//!
//! Hotkey uses a low-level keyboard hook (`WH_KEYBOARD_LL`) so Caps Lock
//! hold/release matches Linux semantics and the caps toggle is swallowed.
//! Typing uses `SendInput` Unicode (Enter via `VK_RETURN`).
//!
//! ## Overlay
//! Layered topmost HWND status chip (`UpdateLayeredWindow` + tiny-skia).
//! Implements [`OverlayBackend`] stages. Visuals are a simplified always-on-top
//! rounded chip (stage label + basic icon animation)  -  not a pixel-perfect
//! port of the Linux X11 pill (flat shadow only — no soft blur; coarser motion).
//! Recording timer honors `[ui.stages].show_timer`. Colors/labels come from
//! [`dictate_core::resolve_ui`].
//! `UiConfig.overlay = false` / theme `null|none|off` still select
//! [`NullOverlay`] via [`create`]. Fail-open: spawn/HWND/font errors
//! disable the chip without affecting dictation.

use anyhow::{Context, Result, bail, ensure};
use dictate_core::config::UiConfig;
use dictate_core::overlay::{NullOverlay, OverlayBackend, Stage};
use dictate_core::{InjectTyper, ResolvedUi, resolve_ui};
use fontdue::{Font, FontSettings};
use std::f32::consts::PI;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tiny_skia::{Color, Paint, Path as SkPath, PathBuilder, Pixmap as SkPixmap, PremultipliedColorU8, Transform};

use crate::traits::{HotkeySource, Typer};

use windows_sys::Win32::Foundation::{
    GetLastError, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    MonitorFromPoint, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTOPRIMARY, RGBQUAD,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
    VK_CAPITAL, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_NUMLOCK,
    VK_RCONTROL, VK_RETURN, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SCROLL, VK_SHIFT, VIRTUAL_KEY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, GetSystemMetrics, HHOOK, HWND_TOPMOST, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG,
    PeekMessageW, PostQuitMessage, PostThreadMessageW, RegisterClassW, SetWindowPos,
    SetWindowsHookExW, ShowWindow, TranslateMessage, ULW_ALPHA, UnhookWindowsHookEx,
    UpdateLayeredWindow, HC_ACTION, PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, WH_KEYBOARD_LL,
    WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};

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

/// A cancel keypress within this window after the activating press is
/// treated as auto-repeat / bounce, not a deliberate cancel (mirrors Linux).
const CANCEL_GRACE: Duration = Duration::from_millis(150);

struct HookState {
    tx: SyncSender<HotkeyEvent>,
    held: bool,
    press_at: Option<Instant>,
}

static HOOK_STATE: Mutex<Option<HookState>> = Mutex::new(None);
// Stored as usize so the static is Send+Sync; only touched on the hook thread
// and from the low-level hook callback.
static HOOK_PTR: OnceLock<Mutex<usize>> = OnceLock::new();
static HOOK_OWNED: AtomicBool = AtomicBool::new(false);

fn hook_ptr_slot() -> &'static Mutex<usize> {
    HOOK_PTR.get_or_init(|| Mutex::new(0))
}

/// Global Caps Lock hold via `WH_KEYBOARD_LL`.
///
/// Hold = record, release = stop. While held, any non-modifier physical key
/// cancels (injected `SendInput` keystrokes are ignored via `LLKHF_INJECTED`).
/// Caps Lock itself is swallowed so the Lock toggle never latches.
pub struct Hotkey {
    rx: Receiver<HotkeyEvent>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
    source_held: bool,
}

impl Hotkey {
    /// Install a process-wide Caps Lock low-level keyboard hook.
    pub fn grab_caps_lock() -> Result<Self> {
        if HOOK_OWNED.swap(true, Ordering::SeqCst) {
            bail!(
                "Windows Caps Lock hotkey is already grabbed in this process  -  \
                 drop the existing Hotkey before grabbing again"
            );
        }

        let (tx, rx) = mpsc::sync_channel::<HotkeyEvent>(64);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32>>();

        let join = std::thread::Builder::new()
            .name("dictate-win-hotkey".into())
            .spawn(move || hotkey_thread_main(tx, ready_tx))
            .context("failed to spawn Windows hotkey thread")?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                rx,
                thread_id,
                join: Some(join),
                source_held: false,
            }),
            Ok(Err(e)) => {
                HOOK_OWNED.store(false, Ordering::SeqCst);
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                HOOK_OWNED.store(false, Ordering::SeqCst);
                let _ = join.join();
                bail!("Windows hotkey thread exited before reporting readiness")
            }
        }
    }

    /// Discard queued hotkey events (e.g. after typing so late cancels cannot leak).
    pub fn drain_pending(&mut self) {
        while self.rx.try_recv().is_ok() {}
    }

    /// Block until the next Press / Release / Cancel / Shutdown.
    /// Auto-repeats while held are collapsed into a single Press.
    pub fn next_event(&mut self, held: &mut bool) -> Result<HotkeyEvent> {
        loop {
            let ev = match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(HotkeyEvent::Shutdown),
            };
            match ev {
                HotkeyEvent::Press => {
                    if *held {
                        continue;
                    }
                    *held = true;
                    return Ok(HotkeyEvent::Press);
                }
                HotkeyEvent::Release => {
                    if !*held {
                        continue;
                    }
                    *held = false;
                    return Ok(HotkeyEvent::Release);
                }
                HotkeyEvent::Cancel => {
                    *held = false;
                    return Ok(HotkeyEvent::Cancel);
                }
                HotkeyEvent::Shutdown => return Ok(HotkeyEvent::Shutdown),
            }
        }
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        HOOK_OWNED.store(false, Ordering::SeqCst);
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

fn hotkey_thread_main(tx: SyncSender<HotkeyEvent>, ready_tx: mpsc::Sender<Result<u32>>) {
    let thread_id = unsafe { GetCurrentThreadId() };

    {
        let mut guard = match HOOK_STATE.lock() {
            Ok(g) => g,
            Err(_) => {
                let _ = ready_tx.send(Err(anyhow::anyhow!(
                    "Windows hotkey state mutex poisoned"
                )));
                return;
            }
        };
        *guard = Some(HookState {
            tx,
            held: false,
            press_at: None,
        });
    }

    let hmod: HINSTANCE = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(low_level_keyboard_proc),
            hmod,
            0,
        )
    };
    if hook.is_null() {
        let err = unsafe { GetLastError() };
        let _ = HOOK_STATE.lock().map(|mut g| g.take());
        let _ = ready_tx.send(Err(anyhow::anyhow!(
            "SetWindowsHookExW(WH_KEYBOARD_LL) failed (Win32 error {err})  -  \
             another accessibility hook may be blocking Caps Lock capture"
        )));
        return;
    }

    if let Ok(mut slot) = hook_ptr_slot().lock() {
        *slot = hook as usize;
    }

    if ready_tx.send(Ok(thread_id)).is_err() {
        unsafe {
            let _ = UnhookWindowsHookEx(hook);
        }
        let _ = HOOK_STATE.lock().map(|mut g| g.take());
        if let Ok(mut slot) = hook_ptr_slot().lock() {
            *slot = 0;
        }
        return;
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {}

    unsafe {
        let _ = UnhookWindowsHookEx(hook);
    }
    if let Ok(mut slot) = hook_ptr_slot().lock() {
        *slot = 0;
    }
    let _ = HOOK_STATE.lock().map(|mut g| g.take());
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code as u32 != HC_ACTION {
        return unsafe { CallNextHookEx(current_hook(), code, wparam, lparam) };
    }

    let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    let injected = (info.flags & LLKHF_INJECTED) != 0;
    let vk = info.vkCode as VIRTUAL_KEY;
    let is_up = wparam == WM_KEYUP as WPARAM || wparam == WM_SYSKEYUP as WPARAM;
    let is_down = wparam == WM_KEYDOWN as WPARAM || wparam == WM_SYSKEYDOWN as WPARAM;

    if vk == VK_CAPITAL && !injected {
        // Swallow Caps Lock entirely so Lock never toggles.
        if let Ok(mut guard) = HOOK_STATE.lock() {
            if let Some(state) = guard.as_mut() {
                if is_down {
                    if !state.held {
                        state.held = true;
                        state.press_at = Some(Instant::now());
                        let _ = state.tx.try_send(HotkeyEvent::Press);
                    }
                } else if is_up && state.held {
                    state.held = false;
                    state.press_at = None;
                    let _ = state.tx.try_send(HotkeyEvent::Release);
                }
            }
        }
        return 1;
    }

    // Cancel: any non-modifier physical key while held (after grace).
    if is_down && !injected && !is_modifier(vk) {
        if let Ok(mut guard) = HOOK_STATE.lock() {
            if let Some(state) = guard.as_mut() {
                if state.held {
                    let past_grace = state
                        .press_at
                        .is_none_or(|t| t.elapsed() >= CANCEL_GRACE);
                    if past_grace {
                        state.held = false;
                        state.press_at = None;
                        let _ = state.tx.try_send(HotkeyEvent::Cancel);
                    }
                }
            }
        }
    }

    unsafe { CallNextHookEx(current_hook(), code, wparam, lparam) }
}

fn current_hook() -> HHOOK {
    hook_ptr_slot()
        .lock()
        .map(|g| *g as HHOOK)
        .unwrap_or(std::ptr::null_mut())
}

fn is_modifier(vk: VIRTUAL_KEY) -> bool {
    matches!(
        vk,
        VK_SHIFT
            | VK_CONTROL
            | VK_MENU
            | VK_LSHIFT
            | VK_RSHIFT
            | VK_LCONTROL
            | VK_RCONTROL
            | VK_LMENU
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
            | VK_CAPITAL
            | VK_NUMLOCK
            | VK_SCROLL
    )
}

/// Progressive emitter: stdout or `SendInput` Unicode keystrokes.
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

/// Join a chunk onto the stream: insert one space when the previous chunk
/// ended on a word/punctuation and the next begins on a word character.
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

    let mut inputs: Vec<INPUT> = Vec::with_capacity(text.len() * 2);
    for ch in text.chars() {
        if ch == '\n' {
            push_vk(&mut inputs, VK_RETURN, false);
            push_vk(&mut inputs, VK_RETURN, true);
            continue;
        }
        let mut buf = [0u16; 2];
        for unit in ch.encode_utf16(&mut buf).iter().copied() {
            push_unicode(&mut inputs, unit, false);
            push_unicode(&mut inputs, unit, true);
        }
    }

    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    ensure!(
        sent as usize == inputs.len(),
        "SendInput typed {sent}/{} events (Win32 error {})  -  focus a text field and retry",
        inputs.len(),
        unsafe { GetLastError() },
    );
    Ok(())
}

fn push_unicode(inputs: &mut Vec<INPUT>, unit: u16, up: bool) {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    inputs.push(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
}

fn push_vk(inputs: &mut Vec<INPUT>, vk: VIRTUAL_KEY, up: bool) {
    let flags = if up { KEYEVENTF_KEYUP } else { 0 };
    inputs.push(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
}

/// Only '\n' is typed (voice "new line"); every other control char is stripped.
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

/// Layered HWND status chip. Prefer [`create`] for `UiConfig`-aware selection.
///
/// Visual delta vs Linux X11 pill: simpler rounded chip (label + icon
/// animation), flat offset shadow (no soft blur); recording timer honors show_timer. Same
/// stage API (`set` / `flash` / `active`).
pub struct Overlay {
    tx: Option<Sender<Stage>>,
    /// Set when the overlay thread failed (no HWND / font / GDI error).
    failed: std::sync::Arc<AtomicBool>,
}

impl Overlay {
    /// Start the overlay thread, or a no-op handle when disabled/unavailable.
    pub fn start(cfg: &UiConfig) -> Self {
        let failed = std::sync::Arc::new(AtomicBool::new(false));
        if !cfg.overlay {
            return Self { tx: None, failed };
        }
        let ui = resolve_ui(cfg);
        let (tx, rx) = mpsc::channel::<Stage>();
        let failed2 = failed.clone();
        match thread::Builder::new()
            .name("dictate-win-overlay".into())
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
/// - otherwise → layered HWND [`Overlay`] (palette via [`resolve_ui`])
pub fn create(cfg: &UiConfig) -> Box<dyn OverlayBackend> {
    if !cfg.overlay {
        return Box::new(NullOverlay);
    }
    match cfg.theme.as_str() {
        "null" | "none" | "off" => Box::new(NullOverlay),
        _ => Box::new(Overlay::start(cfg)),
    }
}

fn rgba(c: [u8; 4]) -> Color {
    Color::from_rgba8(c[0], c[1], c[2], c[3])
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

/// Logical design metrics for the simplified Windows chip.
mod chip {
    pub const WIN_W: u32 = 220;
    pub const WIN_H: u32 = 72;
    pub const PILL_H: f32 = 40.0;
    pub const ICON: f32 = 22.0;
    pub const PAD_X: f32 = 12.0;
    pub const GAP: f32 = 10.0;
    pub const LABEL_PX: f32 = 13.0;
    pub const META_PX: f32 = 11.0;
    pub const BOTTOM_MARGIN: i32 = 48;
    pub const TOP_PAD: f32 = 12.0;
}

const CLASS_NAME: &[u16] = &[
    b'D' as u16, b'i' as u16, b'c' as u16, b't' as u16, b'a' as u16, b't' as u16, b'e' as u16,
    b'S' as u16, b't' as u16, b'a' as u16, b't' as u16, b'u' as u16, b's' as u16, b'C' as u16,
    b'h' as u16, b'i' as u16, b'p' as u16, 0,
];

fn run_overlay(rx: Receiver<Stage>, failed: std::sync::Arc<AtomicBool>, ui: ResolvedUi) {
    if let Err(e) = run_overlay_inner(rx, &ui) {
        log::debug!("Windows overlay disabled: {e:#}");
        failed.store(true, Ordering::Relaxed);
    }
}

fn run_overlay_inner(rx: Receiver<Stage>, ui: &ResolvedUi) -> Result<()> {
    let font = load_font().context("overlay font")?;
    let hwnd = unsafe { create_chip_window() }.context("create status chip HWND")?;
    let mut layer = unsafe { LayerBuffer::new(chip::WIN_W as i32, chip::WIN_H as i32) }
        .context("CreateDIBSection for status chip")?;
    let mut pixmap = SkPixmap::new(chip::WIN_W, chip::WIN_H)
        .ok_or_else(|| anyhow::anyhow!("tiny-skia pixmap alloc failed"))?;

    let mut stage = Stage::Hidden;
    let mut stage_changed_at = Instant::now();
    let mut recording_started = Instant::now();
    let anim_start = Instant::now();
    let mut visible = false;

    loop {
        // Pump Win32 messages (Destroy / quit).
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    destroy_chip(hwnd, &mut layer);
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Drain stage updates; keep the latest.
        let mut got = false;
        loop {
            match rx.try_recv() {
                Ok(s) => {
                    if s == Stage::Recording && stage != Stage::Recording {
                        recording_started = Instant::now();
                    }
                    stage = s;
                    stage_changed_at = Instant::now();
                    got = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    destroy_chip(hwnd, &mut layer);
                    return Ok(());
                }
            }
        }

        if stage == Stage::Hidden {
            if visible {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                visible = false;
            }
            // Idle wait: block briefly for the next stage without spinning.
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(s) => {
                    if s == Stage::Recording && stage != Stage::Recording {
                        recording_started = Instant::now();
                    }
                    stage = s;
                    stage_changed_at = Instant::now();
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    destroy_chip(hwnd, &mut layer);
                    return Ok(());
                }
            }
            if stage == Stage::Hidden {
                continue;
            }
        }

        let anim_t = anim_start.elapsed().as_secs_f32();
        let stage_age = stage_changed_at.elapsed().as_secs_f32();
        let rec_secs = recording_started.elapsed().as_secs();
        draw_chip(&mut pixmap, &font, stage, anim_t, stage_age, rec_secs, ui);
        unsafe {
            layer.blit_skia(&pixmap)?;
            present_chip(hwnd, &layer)?;
            if !visible {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                visible = true;
            }
        }

        // ~30 fps while animating; skip sleep if we just got a stage change.
        if !got {
            thread::sleep(Duration::from_millis(33));
        }
    }
}

unsafe fn create_chip_window() -> Result<HWND> {
    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(std::ptr::null()) };
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(chip_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: std::ptr::null_mut(),
        hCursor: std::ptr::null_mut(),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: CLASS_NAME.as_ptr(),
    };
    let atom = unsafe { RegisterClassW(&wc) };
    if atom == 0 {
        let err = unsafe { GetLastError() };
        // Already registered in this process is fine.
        if err != 1410 {
            bail!("RegisterClassW(DictateStatusChip) failed (Win32 error {err})");
        }
    }

    let ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;
    let hwnd = unsafe {
        CreateWindowExW(
            ex,
            CLASS_NAME.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            0,
            0,
            chip::WIN_W as i32,
            chip::WIN_H as i32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        let err = unsafe { GetLastError() };
        bail!("CreateWindowExW(DictateStatusChip) failed (Win32 error {err})");
    }
    Ok(hwnd)
}

unsafe extern "system" fn chip_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn destroy_chip(hwnd: HWND, layer: &mut LayerBuffer) {
    layer.destroy();
    if !hwnd.is_null() {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

unsafe fn present_chip(hwnd: HWND, layer: &LayerBuffer) -> Result<()> {
    let (wx, wy, ww, wh) = primary_work_area();
    let x = wx + (ww - chip::WIN_W as i32) / 2;
    let y = wy + wh - chip::WIN_H as i32 - chip::BOTTOM_MARGIN;
    let dst = POINT { x, y };
    let size = SIZE {
        cx: layer.w,
        cy: layer.h,
    };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let ok = unsafe {
        UpdateLayeredWindow(
            hwnd,
            std::ptr::null_mut(),
            &dst,
            &size,
            layer.hdc,
            &src,
            0 as COLORREF,
            &blend,
            ULW_ALPHA,
        )
    };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        bail!("UpdateLayeredWindow failed (Win32 error {err})");
    }
    // Keep topmost without activating.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
    Ok(())
}

fn primary_work_area() -> (i32, i32, i32, i32) {
    unsafe {
        let pt = POINT { x: 0, y: 0 };
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
        if !mon.is_null() {
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(mon, &mut mi) != 0 {
                let r = mi.rcWork;
                return (r.left, r.top, r.right - r.left, r.bottom - r.top);
            }
        }
        (
            0,
            0,
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
        )
    }
}

struct LayerBuffer {
    hdc: HDC,
    hbmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl LayerBuffer {
    unsafe fn new(w: i32, h: i32) -> Result<Self> {
        let screen = unsafe { GetDC(std::ptr::null_mut()) };
        if screen.is_null() {
            bail!("GetDC failed for status chip");
        }
        let hdc = unsafe { CreateCompatibleDC(screen) };
        unsafe {
            let _ = ReleaseDC(std::ptr::null_mut(), screen);
        }
        if hdc.is_null() {
            bail!("CreateCompatibleDC failed for status chip");
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbReserved: 0,
            }],
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbmp = unsafe {
            CreateDIBSection(
                hdc,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            )
        };
        if hbmp.is_null() || bits.is_null() {
            unsafe {
                let _ = DeleteDC(hdc);
            }
            let err = unsafe { GetLastError() };
            bail!("CreateDIBSection failed (Win32 error {err})");
        }
        let old = unsafe { SelectObject(hdc, hbmp) };
        Ok(Self {
            hdc,
            hbmp,
            old,
            bits: bits as *mut u8,
            w,
            h,
        })
    }

    unsafe fn blit_skia(&mut self, pixmap: &SkPixmap) -> Result<()> {
        ensure!(
            pixmap.width() as i32 == self.w && pixmap.height() as i32 == self.h,
            "overlay pixmap size mismatch"
        );
        let src = pixmap.data();
        let dst = unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h * 4) as usize) };
        // tiny-skia premultiplied RGBA → Win32 premultiplied BGRA
        for (i, px) in src.chunks_exact(4).enumerate() {
            let o = i * 4;
            dst[o] = px[2];
            dst[o + 1] = px[1];
            dst[o + 2] = px[0];
            dst[o + 3] = px[3];
        }
        Ok(())
    }

    fn destroy(&mut self) {
        if !self.hdc.is_null() {
            unsafe {
                if !self.old.is_null() {
                    let _ = SelectObject(self.hdc, self.old);
                }
                if !self.hbmp.is_null() {
                    let _ = DeleteObject(self.hbmp);
                }
                let _ = DeleteDC(self.hdc);
            }
            self.hdc = std::ptr::null_mut();
            self.hbmp = std::ptr::null_mut();
            self.old = std::ptr::null_mut();
            self.bits = std::ptr::null_mut();
        }
    }
}

impl Drop for LayerBuffer {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn load_font() -> Result<Font> {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\segoeuib.ttf",
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\arialbd.ttf",
        r"C:\Windows\Fonts\arial.ttf",
        r"C:\Windows\Fonts\calibrib.ttf",
        r"C:\Windows\Fonts\calibri.ttf",
        // Cross-check hosts / Wine:
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
    bail!("no usable overlay font (tried Segoe UI / Arial / Calibri / DejaVu)")
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

fn draw_chip(
    pixmap: &mut SkPixmap,
    font: &Font,
    stage: Stage,
    anim_t: f32,
    stage_age: f32,
    rec_secs: u64,
    ui: &ResolvedUi,
) {
    pixmap.fill(Color::from_rgba8(0, 0, 0, 0));
    if stage == Stage::Hidden {
        return;
    }

    let colors = &ui.colors;
    let pill_w = pill_width(stage);
    let pill_h = chip::PILL_H;
    let x = (chip::WIN_W as f32 - pill_w) * 0.5;
    let y = chip::TOP_PAD;

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

    // Flat shadow (no blur — documented delta vs Linux).
    draw_round_rect(
        pixmap,
        px + 2.0,
        py + 3.0,
        pw,
        ph,
        ph * 0.5,
        rgba(colors.shadow),
    );

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

    let icon = chip::ICON * pulse;
    let pad_x = chip::PAD_X * pulse;
    let gap = chip::GAP * pulse;
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
        chip::LABEL_PX * pulse,
        rgba(colors.fg),
    );

    // Elapsed timer on live capture when `[ui.stages].show_timer` is true.
    if stage == Stage::Recording && ui.stages.show_timer {
        let meta = format!("{}:{:02}", rec_secs / 60, rec_secs % 60);
        let meta_size = chip::META_PX * pulse;
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
mod tests {
    use super::{create, join, sanitize_for_typing, stage_text};
    use dictate_core::config::UiConfig;
    use dictate_core::overlay::{OverlayBackend, Stage};
    use dictate_core::resolve_ui;

    #[test]
    fn sanitize_keeps_newline_strips_other_controls() {
        assert_eq!(sanitize_for_typing("ab\t\nc\u{7}"), "ab\nc");
    }

    #[test]
    fn join_inserts_space_between_words() {
        assert_eq!(join(Some('a'), "b"), " b");
        assert_eq!(join(None, "b"), "b");
        assert_eq!(join(Some(' '), "b"), "b");
    }

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
    fn create_enabled_returns_real_overlay_handle() {
        // WHY: overlay=true must not silently select NullOverlay; the HWND
        // chip may fail-open later (active=false) but create still returns
        // the real Overlay backend type wired through OverlayBackend.
        let cfg = UiConfig::default();
        assert!(cfg.overlay);
        let ov = create(&cfg);
        // Real backend accepts stage traffic without panicking. active() may
        // be false on hosts without a desktop session / fonts.
        ov.set(Stage::Recording);
        ov.set(Stage::Transcribing);
        ov.set(Stage::Done);
        ov.set(Stage::Error);
        ov.set(Stage::Hidden);
        ov.flash(0);
    }

    #[test]
    fn stage_labels_match_linux_pill() {
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
        let cfg = UiConfig {
            stages: dictate_core::config::UiStages {
                show_timer: false,
                ..Default::default()
            },
            ..UiConfig::default()
        };
        assert!(!resolve_ui(&cfg).stages.show_timer);
    }
}
