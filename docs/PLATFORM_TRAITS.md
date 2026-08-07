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

Linux: `linux` facade selects X11 vs Wayland. X11 path
`linux_x11::{hotkey, overlay, output}` — real Caps Lock grab, pill overlay
(`create(&UiConfig)`), xdotool typing via `Emitter` in `OutputMode::Type`.
`Emitter` implements both `Typer` and `dictate_core::InjectTyper`. Pure Wayland
uses `linux_wayland::Emitter` (`wtype` / `ydotool`); see below.

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
keycode stays mapped to `NoSymbol` and Caps Lock appears "dead" even with
the daemon gone. Looking up `Caps_Lock` by keysym alone also fails in that
state, so older builds could not self-heal on the next start.

Recovery order now:

1. `dictate stop` / `dictate start` call `restore_caps_lock_mapping()` —
   resolves the keycode via live keysym, then `~/.cache/dictate/caps_keycode`,
   then PC fallback **66**, and writes plain `Caps_Lock` when the slot is
   all-`NoSymbol`.
2. Next `grab_caps_lock()` uses the same resolver before remapping again.
3. Failed grabs after remap use an RAII guard so Caps Lock is restored even
   when `Hotkey` was never constructed.

Manual recovery on a typical PC keyboard (X11 keycode **66**):

```bash
xmodmap -e 'keycode 66 = Caps_Lock'
# or:
dictate stop
```

Restore helpers (`recover_orig_keysyms`, `nosymbol_mapping`,
`caps_lock_restore_keysyms`, `resolve_caps_trigger`) are unit-tested without
a live display. `dictate stop` waits longer before escalating to SIGKILL so
a clean `Drop` is more likely mid-transcription.


### Linux Wayland (`linux_wayland` + `linux` facade)

Runtime selection (`linux::selection`):

- **`DISPLAY` set** (X11 or hybrid XWayland): X11 remains primary — Caps Lock
  grab, xdotool typing, X11 pill overlay (unchanged).
- **Pure Wayland** (`WAYLAND_DISPLAY` set, `DISPLAY` unset/empty):
  - **Typing** — `wtype` (preferred) with optional `ydotool` fallback; same
    sanitize + fail-closed `Emitter`/`Typer`/`InjectTyper` surface as X11.
    Missing binaries error with `sudo apt install wtype` / `ydotool` hints.
  - **Hotkey** — fails loudly with corrective action (enable XWayland /
    set `DISPLAY`, or use stdout mode). No silent no-op.
  - **Overlay** — `NullOverlay` + warn; layer-shell status pill is a
    follow-up (avoided heavy Wayland client deps for this MVP).

Public re-exports on Linux still come from the `linux` facade:
`Hotkey`, `Emitter`, `OutputMode`, `Overlay`, `create`, `HotkeyEvent`.

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
  `Transcribing` → "Processing"). Honors `show_timer` and `pulse_ms`
  (0 disables pulse). **Visual delta vs Linux X11 pill:** same soft
  `box_blur_alpha` drop shadow (rounded-rect mask, 3-pass box blur) and
  icon animation (waveform / spinner / check / x), but not pixel-perfect —
  no Xft DPI scale factor beyond primary work-area placement; motion/timing
  remain coarser than the Linux mock. Fail-open on HWND/font/GDI errors.
  Not live-session verified on this Linux host (no local UI soak). Full
  `cargo check -p dictate-platform --target x86_64-pc-windows-gnu` is
  blocked by `dictate-core` Unix-socket API (`std::os::unix`); `windows.rs`
  itself typechecks green for that target in isolation.

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
- **Overlay** — AppKit `NSPanel` + tiny-skia `NSImageView` status chip
  (`create(&UiConfig)`). `overlay = false` or `theme` `null`/`none`/`off` →
  `NullOverlay`; otherwise the chip. Labels/colors from `resolve_ui` (same
  defaults as Linux). **Visual delta vs Linux X11 pill:** soft
  `box_blur_alpha` shadow, icon disc + waveform/spinner/check/x, recording
  timer (`show_timer`), and scale pulse (`pulse_ms`) — closer to Linux than
  the old `NSTextField` chip, but not pixel-perfect (no Xft DPI scale; AppKit
  panel host instead of X override-redirect; coarser motion). Fail-open.

Same public surface as Linux. Not live-session verified on this Linux host.

## Verification

| Backend | Status |
|---|---|
| Linux X11 hotkey / type / pill | Implemented; live-session re-verify on axiomexec only |
| Linux Wayland type (`wtype`) + selection | Implemented (pure Wayland); hotkey needs DISPLAY/XWayland; overlay = NullOverlay + warn (layer-shell follow-up). Not live-session verified |
| Null* | Unit-tested / headless |
| macOS hotkey / type / skia NSPanel chip | Implemented in tree (tiny-skia soft-shadow chip in NSImageView); needs Accessibility + AppKit session on a Mac to runtime-verify |
| Windows hotkey / type / status chip | Implemented in tree (layered HWND + soft `box_blur_alpha` shadow); needs a Windows host to runtime-verify |
