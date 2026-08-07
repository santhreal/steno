//! Windows OS backends: Caps Lock push-to-talk, SendInput typing, NullOverlay.
//!
//! Hotkey uses a low-level keyboard hook (`WH_KEYBOARD_LL`) so Caps Lock
//! hold/release matches Linux semantics and the caps toggle is swallowed.
//! Typing uses `SendInput` Unicode (Enter via `VK_RETURN`). Overlay v1 is
//! intentionally [`NullOverlay`] — a layered HWND pill is deferred.

use anyhow::{Context, Result, bail, ensure};
use dictate_core::config::UiConfig;
use dictate_core::overlay::{NullOverlay, OverlayBackend, Stage};
use dictate_core::InjectTyper;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::traits::{HotkeySource, Typer};

use windows_sys::Win32::Foundation::{GetLastError, HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
    VK_CAPITAL, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_NUMLOCK,
    VK_RCONTROL, VK_RETURN, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SCROLL, VK_SHIFT, VIRTUAL_KEY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
    WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
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
static HOOK_PTR: OnceLock<Mutex<*mut core::ffi::c_void>> = OnceLock::new();
static HOOK_OWNED: AtomicBool = AtomicBool::new(false);

fn hook_ptr_slot() -> &'static Mutex<*mut core::ffi::c_void> {
    HOOK_PTR.get_or_init(|| Mutex::new(std::ptr::null_mut()))
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
                "Windows Caps Lock hotkey is already grabbed in this process — \
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
            "SetWindowsHookExW(WH_KEYBOARD_LL) failed (Win32 error {err}) — \
             another accessibility hook may be blocking Caps Lock capture"
        )));
        return;
    }

    if let Ok(mut slot) = hook_ptr_slot().lock() {
        *slot = hook;
    }

    if ready_tx.send(Ok(thread_id)).is_err() {
        unsafe {
            let _ = UnhookWindowsHookEx(hook);
        }
        let _ = HOOK_STATE.lock().map(|mut g| g.take());
        if let Ok(mut slot) = hook_ptr_slot().lock() {
            *slot = std::ptr::null_mut();
        }
        return;
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {}

    unsafe {
        let _ = UnhookWindowsHookEx(hook);
    }
    if let Ok(mut slot) = hook_ptr_slot().lock() {
        *slot = std::ptr::null_mut();
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
        .map(|g| *g)
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
        for &unit in ch.encode_utf16(&mut buf) {
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
        "SendInput typed {sent}/{} events (Win32 error {}) — focus a text field and retry",
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

/// Status overlay stub (no HWND). Prefer [`create`] which returns [`NullOverlay`].
///
/// A layered topmost pill is deferred; NullOverlay is the supported Windows
/// overlay path until that lands.
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

/// Windows overlay v1: always [`NullOverlay`].
///
/// Layered HWND status pill is not shipped yet; headless / embed builds keep
/// working. Hotkey + SendInput typing are the real Windows capabilities today.
pub fn create(_cfg: &UiConfig) -> Box<dyn OverlayBackend> {
    Box::new(NullOverlay)
}

#[cfg(test)]
mod tests {
    use super::{join, sanitize_for_typing};

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
}
