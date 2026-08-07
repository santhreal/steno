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

Windows / macOS: compile stubs in `windows.rs` / `macos.rs`.
Same public surface as Linux (`Hotkey`, `HotkeyEvent`, `Overlay`/`create`,
`Emitter`, `OutputMode`). Capability methods `bail!` with a corrective hint
(`… not implemented yet — use Linux X11 or Null*`).
`create` always returns `NullOverlay` so headless embeds work.
Real backends next: RegisterHotKey + SendInput + layered window; CGEventTap +
CGEvent + NSPanel.

## Verification

| Backend | Status |
|---|---|
| Linux X11 hotkey / type / pill | Implemented; live-session re-verify on axiomexec only |
| Null* | Unit-tested / headless |
| Windows / macOS | Compile stubs only — not runtime-verified |
