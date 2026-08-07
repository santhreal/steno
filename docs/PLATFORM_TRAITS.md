# Platform traits

Owned shapes as of the workspace split. `OverlayBackend` / `Stage` /
`NullOverlay` live in `dictate-core` and are re-exported from
`dictate-platform`.

```rust
// dictate_platform::traits
pub trait HotkeySource: Send {
    fn next_event(&mut self) -> anyhow::Result<HotkeyEvent>;
    fn drain_pending(&mut self);
}

pub trait Typer: Send {
    fn type_text(&mut self, text: &str) -> anyhow::Result<()>;
}

// dictate_core::overlay (re-exported by dictate-platform)
pub trait OverlayBackend: Send {
    fn set(&self, stage: Stage);
    fn flash(&self, ms: u64);
    fn active(&self) -> bool;
}

pub enum HotkeyEvent { Press, Release, Cancel, Shutdown }
pub enum Stage { Hidden, Recording, Transcribing, Done, Error }
```

Linux X11: `linux_x11::{hotkey, overlay, output}` — real Caps Lock grab, pill
overlay (`create(&UiConfig)`), xdotool typing via `Emitter` in `OutputMode::Type`.
`Emitter` implements both `Typer` and `dictate_core::InjectTyper`.

Null*: `NullHotkey` / `NullTyper` / `NullOverlay` — no-ops for tests and
headless embedders.

### Windows (`windows.rs`)

Real minimal backends via `windows-sys`:

- **Hotkey** — `WH_KEYBOARD_LL` Caps Lock hold (press/release). Caps Lock is
  swallowed so Lock does not latch. Non-modifier physical keys while held
  emit `Cancel` (injected `SendInput` keystrokes ignored via `LLKHF_INJECTED`).
- **Typer / Emitter** — `SendInput` Unicode (`KEYEVENTF_UNICODE`); `'\n'`
  uses `VK_RETURN`. Other control characters are stripped. `OutputMode::Type`
  only; stdout mode refuses typing (fail-closed). Arming stays in core/session.
- **Overlay** — **NullOverlay only for v1** (no layered HWND pill yet).
  `create` returns `NullOverlay`; loud module docs mark the gap.

Same public surface as Linux. Not live-session verified on this Linux host.

### macOS (`macos.rs`)

Real minimal backends (Accessibility required):

- **Hotkey** — `CGEventTap` at HID for Caps Lock hold (KeyDown/KeyUp). Caps
  Lock events are swallowed so Lock does not latch. Non-modifier keys while
  held emit `Cancel`. Errors tell you to grant Accessibility to the
  terminal/app under System Settings → Privacy & Security → Accessibility.
- **Typer / Emitter** — `CGEvent` keyboard events with
  `CGEventKeyboardSetUnicodeString` (no clipboard). `'\n'` uses Return;
  other control characters are stripped. `OutputMode::Type` only; stdout
  mode refuses typing (fail-closed). Arming stays in core/session.
- **Overlay** — **NullOverlay only for v1** (no NSPanel yet). `create`
  returns `NullOverlay`; loud module docs mark the gap.

Same public surface as Linux. Not live-session verified on this Linux host.

## Verification

| Backend | Status |
|---|---|
| Linux X11 hotkey / type / pill | Implemented; live-session re-verify on axiomexec only |
| Null* | Unit-tested / headless |
| macOS hotkey / type / NullOverlay | Implemented in tree; needs Accessibility on a Mac to runtime-verify |
| Windows hotkey / type / NullOverlay | Implemented in tree; needs a Windows host to runtime-verify |
