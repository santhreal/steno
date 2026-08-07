# Platform traits (target)

```rust
pub trait HotkeySource: Send {
    fn next_event(&mut self) -> anyhow::Result<HotkeyEvent>;
    fn drain_pending(&mut self);
}

pub trait Typer: Send {
    fn type_text(&mut self, text: &str) -> anyhow::Result<()>;
}

pub trait OverlayBackend: Send {
    fn set_stage(&self, stage: Stage);
    fn flash(&self, ms: u64);
    fn is_failed(&self) -> bool;
}

pub enum HotkeyEvent { Press, Release, Cancel, Shutdown }
pub enum Stage { Hidden, Listening, Thinking, Done, Error }
```

Linux X11: current hotkey.rs / overlay.rs / output.rs::type_text.
Null*: no-ops for tests and headless embedders.
Windows/macOS: compile stubs first (`bail!("unsupported on …")`), then real.
