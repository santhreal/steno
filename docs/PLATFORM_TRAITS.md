# Platform traits

Owned shapes as of the workspace split. `OverlayBackend` / `Stage` /
`NullOverlay` live in `steno-core` and are re-exported from
`steno-platform`.

```rust
// steno_platform::traits
pub trait HotkeySource: Send {
    fn next_event(&mut self) -> anyhow::Result<HotkeyEvent>;
    fn drain_pending(&mut self);
}

pub trait Typer: Send {
    fn type_text(&mut self, text: &str) -> anyhow::Result<()>;
}

// steno_core::session::InjectTyper (re-exported by steno-platform)
pub trait InjectTyper: Send {
    fn type_text(&mut self, text: &str) -> anyhow::Result<()>;
}

// steno_core::overlay (re-exported by steno-platform)
pub trait OverlayBackend: Send {
    fn set(&self, stage: Stage);
    fn flash(&self, ms: u64);
    fn active(&self) -> bool;
}

pub enum HotkeyEvent { Press, Release, Cancel, Shutdown }
pub enum Stage { Hidden, Recording, Transcribing, Done, Error }
```
## Top-Level Factory Helpers

`steno-platform` exports top-level factory helpers that construct OS-native backends without requiring host applications to import OS-specific modules (`linux`, `windows`, `macos`):

```rust
use steno_platform::{
    create_hotkey, create_typer, create_platform_backends, create_overlay,
    InjectTyper, OutputMode, PlatformBackends,
};
```

- **`create_hotkey() -> anyhow::Result<Box<dyn HotkeySource>>`**: Constructs the platform hotkey source bound to Caps Lock hold-to-talk.
- **`create_typer(mode: OutputMode) -> Box<dyn InjectTyper>`**: Constructs a platform keystroke injector (`Emitter`) wrapped as a `Box<dyn InjectTyper>` for the given [`OutputMode`] (`Type` or `Stdout`).
- **`create_platform_backends(mode: OutputMode, ui_cfg: &UiConfig) -> anyhow::Result<PlatformBackends>`**: Assembles all three platform backends (hotkey, typer, status overlay) in a single call:

```rust
use steno_core::{
    Config, Dictionary, Engine, RefineBackend, RuleRefine, Session, TextPipeline,
};
use steno_platform::{
    create_platform_backends, OutputMode, PlatformBackends,
};

pub struct PlatformBackends {
    pub hotkey: Box<dyn HotkeySource>,
    pub typer: Box<dyn InjectTyper>,
    pub overlay: Box<dyn OverlayBackend>,
}

// Build engine with RuleRefine (or a custom RefineBackend implementation):
let cfg = Config::load(None)?;
let pipeline = TextPipeline::with_refine(cfg.text, cfg.refine.make_backend());
let engine = Engine::load(&cfg)?.with_pipeline(pipeline);

// Assemble platform backends and attach to Session:
let backends = create_platform_backends(OutputMode::Type, &cfg.ui)?;
let session = Session::builder(engine)
    .from_config(&cfg)
    .typer(backends.typer)
    .overlay(backends.overlay)
    .build();
```

### Core-Platform Decoupling (`InjectTyper`)

`InjectTyper` is defined in `steno-core` and re-exported top-level by `steno-platform`. Platform `Emitter` and `NullTyper` implement `InjectTyper`, enabling `Session` builders to receive platform typing sinks while keeping `steno-core` free of OS-specific dependencies.

## Feature Flags & Build Configuration

`steno-platform` requires **no Cargo feature flags**. Target platform backends are selected automatically at compile time using standard Rust OS target conditionals (`cfg(target_os = "linux")`, `cfg(target_os = "windows")`, `cfg(target_os = "macos")`).

Linux: `linux` facade selects X11 vs Wayland. X11 path
`linux_x11::{hotkey, overlay, output}`: real Caps Lock grab, pill overlay
(`create(&UiConfig)`), xdotool typing via `Emitter` in `OutputMode::Type`.
`Emitter` implements both `Typer` and `steno_core::InjectTyper`. Pure Wayland
uses `linux_wayland::Emitter` (`wtype` / `ydotool`); see below.

### Overlay themes (all platforms)

`create(&UiConfig)` still maps `overlay = false` and `theme` `null|none|off` to
`NullOverlay`. Otherwise the platform overlay calls `resolve_ui` once at
`start` and paints from `ResolvedUi` (palette + stage labels + `show_timer` /
`pulse_ms`). Presets: `pill` (default), `mono`, `dusk`, `dawn`, `contrast`.
`[ui.colors]` hex overrides and `[ui.stages]` labels apply through the same
path. Unknown themes fall back to pill colors (fail-open).

### Caps Lock (Linux X11)

Hold Caps Lock to record; release to stop. While the daemon runs, the Caps
Lock keycode is remapped to `NoSymbol` so XKB cannot latch Lock, as a passive
`XGrabKey` alone does not prevent the toggle. `Hotkey`'s `Drop` restores the
original keysyms (ungrab + `ChangeKeyboardMapping`) on clean exit, panic
unwinding, or graceful stop.

**SIGKILL limitation.** `kill -9` / `SIGKILL` never runs `Drop`, so the
keycode stays mapped to `NoSymbol` and Caps Lock appears "dead" even with
the daemon gone. Looking up `Caps_Lock` by keysym alone also fails in that
state, so older builds could not self-heal on the next start.

Recovery order now:

1. `steno stop` / `steno start` call `restore_caps_lock_mapping()` (skipped while a live daemon is detected, never while a live daemon intentionally holds NoSymbol):
   resolves the keycode via live keysym, then `~/.cache/steno/caps_keycode`,
   then PC fallback **66**, and writes plain `Caps_Lock` when the slot is
   all-`NoSymbol`.
2. Next `grab_caps_lock()` uses the same resolver before remapping again.
3. Failed grabs after remap use an RAII guard so Caps Lock is restored even
   when `Hotkey` was never constructed.

Manual recovery on a typical PC keyboard (X11 keycode **66**):

```bash
xmodmap -e 'keycode 66 = Caps_Lock'
# or:
steno stop
```

Restore helpers (`recover_orig_keysyms`, `nosymbol_mapping`,
`caps_lock_restore_keysyms`, `resolve_caps_trigger`) are unit-tested without
a live display. `steno stop` waits longer before escalating to SIGKILL so
a clean `Drop` is more likely mid-transcription.


### Linux Wayland (`linux_wayland` + `linux` facade)

Runtime selection (`linux::selection`):

- **`DISPLAY` set** (X11 or hybrid XWayland): X11 remains primary: Caps Lock
  grab, xdotool typing, X11 pill overlay (unchanged).
- **Pure Wayland** (`WAYLAND_DISPLAY` set, `DISPLAY` unset/empty):
  - **Typing**: `wtype` (preferred) with optional `ydotool` fallback; same
    sanitize + fail-closed `Emitter`/`Typer`/`InjectTyper` surface as X11.
    Missing binaries error with `sudo apt install wtype` / `ydotool` hints.
  - **Hotkey**: fails loudly with corrective action (enable XWayland /
    set `DISPLAY`, or use stdout mode). No silent no-op.
  - **Overlay**: `NullOverlay` + warn; layer-shell status pill is a
    follow-up (avoided heavy Wayland client deps for this MVP).

Public re-exports on Linux still come from the `linux` facade:
`Hotkey`, `Emitter`, `OutputMode`, `Overlay`, `create`, `HotkeyEvent`.

Null*: `NullHotkey` / `NullTyper` / `NullOverlay`: no-ops for tests and
headless embedders. `NullHotkey::next_event()` sleeps for 50ms and returns `HotkeyEvent::Shutdown` to terminate event loops in tests and headless mode.

### Windows (`windows.rs`)

Real minimal backends via `windows-sys`:

- **Hotkey**: `WH_KEYBOARD_LL` Caps Lock hold (press/release). Caps Lock is
  swallowed so Lock does not latch. Non-modifier physical keys while held
  emit `Cancel` (injected `SendInput` keystrokes ignored via `LLKHF_INJECTED`).
- **Typer / Emitter**: `SendInput` Unicode (`KEYEVENTF_UNICODE`); `'\n'`
  uses `VK_RETURN`. Other control characters are stripped. `OutputMode::Type`
  only; stdout mode refuses typing (fail-closed). Arming stays in core/session.
- **Overlay**: layered topmost HWND status chip via `UpdateLayeredWindow`
  + tiny-skia/fontdue. `create(&UiConfig)` returns the real `Overlay` when
  `overlay = true` and theme is not `null|none|off`; those cases (and
  `overlay = false`) still select `NullOverlay`. Stage labels and palette
  come from `resolve_ui` (defaults match Linux: `Recording` → "Transcribing",
  `Transcribing` → "Processing"). Honors `show_timer` and `pulse_ms`
  (0 disables pulse). **Visual delta vs Linux X11 pill:** same soft
  `box_blur_alpha` drop shadow (rounded-rect mask, 3-pass box blur) and
  icon animation (waveform / spinner / check / x), but not pixel-perfect:
  no Xft DPI scale factor beyond primary work-area placement; motion/timing
  remain coarser than the Linux mock. Fail-open on HWND/font/GDI errors.
  Not live-session verified on this Linux host (no local UI soak). Full
  `cargo check -p steno-platform --target x86_64-pc-windows-gnu` is
  blocked by `steno-core` Unix-socket API (`std::os::unix`); `windows.rs`
  itself typechecks green for that target in isolation.

Same public surface as Linux. Not live-session verified on this Linux host.

### macOS (`macos.rs`)

Real minimal backends (Accessibility required):

- **Hotkey**: `CGEventTap` at HID for Caps Lock hold (KeyDown/KeyUp). Caps
  Lock events are swallowed so Lock does not latch. Non-modifier keys while
  held emit `Cancel`. Errors tell you to grant Accessibility to the
  terminal/app under System Settings → Privacy & Security → Accessibility.
- **Typer / Emitter**: `CGEvent` keyboard events with
  `CGEventKeyboardSetUnicodeString` (no clipboard). `'\n'` uses Return;
  other control characters are stripped. `OutputMode::Type` only; stdout
  mode refuses typing (fail-closed). Arming stays in core/session.
- **Overlay**: AppKit `NSPanel` + tiny-skia `NSImageView` status chip
  (`create(&UiConfig)`). `overlay = false` or `theme` `null`/`none`/`off` →
  `NullOverlay`; otherwise the chip. Labels/colors from `resolve_ui` (same
  defaults as Linux). **Visual delta vs Linux X11 pill:** soft
  `box_blur_alpha` shadow, icon disc + waveform/spinner/check/x, recording
  timer (`show_timer`), and scale pulse (`pulse_ms`), closer to Linux than
  the old `NSTextField` chip, but not pixel-perfect (no Xft DPI scale; AppKit
  panel host instead of X override-redirect; coarser motion). Fail-open.

Same public surface as Linux. Not live-session verified on this Linux host.

## Verification

| Backend | Status |
| --- | --- |
| Linux X11 hotkey / type / pill | Implemented; live-session re-verify on axiomexec only |
| Linux Wayland type (`wtype`) + selection | Implemented (pure Wayland); hotkey needs DISPLAY/XWayland; overlay = NullOverlay + warn (layer-shell follow-up). Not live-session verified |
| Null* | Unit-tested / headless |
| macOS hotkey / type / skia NSPanel chip | Implemented in tree (tiny-skia soft-shadow chip in NSImageView); needs Accessibility + AppKit session on a Mac to runtime-verify |
| Windows hotkey / type / status chip | Implemented in tree (layered HWND + soft `box_blur_alpha` shadow); needs a Windows host to runtime-verify |
