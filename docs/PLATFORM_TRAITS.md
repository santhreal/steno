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

#### Overlay themes (all platforms)

`create(&UiConfig)` still maps `overlay = false` and `theme` `null|none|off` to
`NullOverlay`. Otherwise the platform overlay calls `resolve_ui` once at
`start` and paints from `ResolvedUi` (palette + stage labels + `show_timer` /
`pulse_ms`). Presets: `pill` (default), `mono`, `dusk`, `dawn`, `contrast`.
`[ui.colors]` hex overrides and `[ui.stages]` labels apply through the same
path. Unknown themes fall back to pill colors (fail-open).

#### Caps Lock (Linux X11)

Hold Caps Lock to record; release to stop. While the daemon runs, the Caps
Lock keycode is remapped to `NoSymbol` so XKB cannot latch Lock — a passive
`XGrabKey` alone does not prevent the toggle. `Hotkey`'s `Drop` restores the
original keysyms (ungrab + `ChangeKeyboardMapping`) on clean exit, panic
unwinding, or graceful stop.

**SIGKILL limitation.** `kill -9` / `SIGKILL` never runs `Drop`, so the
keycode stays mapped to `NoSymbol` and Caps Lock appears "dead" until
something remaps it. The next daemon start detects an all-`NoSymbol` mapping
and synthesizes plain `Caps_Lock` as the restore payload, so a subsequent
clean exit hands the key back. Manual recovery on a typical PC keyboard
(X11 keycode **66**):

```bash
xmodmap -e 'keycode 66 = Caps_Lock'
```

Restore helpers (`recover_orig_keysyms`, `nosymbol_mapping`,
`caps_lock_restore_keysyms`) are unit-tested without a live display.

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
- **Overlay** — layered topmost HWND status chip via `UpdateLayeredWindow`
  + tiny-skia/fontdue. `create(&UiConfig)` returns the real `Overlay` when
  `overlay = true` and theme is not `null|none|off`; those cases (and
  `overlay = false`) still select `NullOverlay`. Stage labels and palette
  come from `resolve_ui` (defaults match Linux: `Recording` → "Transcribing",
  `Transcribing` → "Processing"). Honors `pulse_ms` (0 disables). **Visual
  delta vs Linux X11 pill:** simplified rounded chip (stage label + basic
  icon animation: waveform / spinner / check / x), flat offset shadow only
  (no soft CSS blur), no recording timer meta (`show_timer` unused),
  no DPI scale factor beyond primary work-area placement. Fail-open on
  HWND/font/GDI errors. Not live-session verified on this Linux host
  (no local UI soak). Full `cargo check -p dictate-platform --target
  x86_64-pc-windows-gnu` is blocked by `dictate-core` Unix-socket API
  (`std::os::unix`); `windows.rs` itself typechecks green for that target
  in isolation.

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
- **Overlay** — minimal AppKit `NSPanel` status chip (`create(&UiConfig)`).
  `overlay = false` or `theme` `null`/`none`/`off` → `NullOverlay`; otherwise
  the chip. Labels/colors from `resolve_ui` (same defaults as Linux).
  **Visual delta:** Linux pill is an animated tiny-skia capsule (icon +
  waveform/spinner/check, shadow, recording timer); macOS is a simpler
  floating `NSPanel` + `NSTextField` label only (bg/fg from palette; no icon
  animation / timer / pulse). Fail-open.

Same public surface as Linux. Not live-session verified on this Linux host.

## Verification

| Backend | Status |
|---|---|
| Linux X11 hotkey / type / pill | Implemented; live-session re-verify on axiomexec only |
| Null* | Unit-tested / headless |
| macOS hotkey / type / NSPanel chip | Implemented in tree; needs Accessibility + AppKit session on a Mac to runtime-verify |
| Windows hotkey / type / status chip | Implemented in tree (layered HWND chip); needs a Windows host to runtime-verify |
