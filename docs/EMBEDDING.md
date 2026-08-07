# Embedding dictate-core

Minimal offline STT for host applications. No cloud. No UI required.

## Add the dependency

```toml
# once the workspace split lands:
dictate-core = { path = "…/crates/dictate-core" }
# optional OS backends:
dictate-platform = { path = "…/crates/dictate-platform" }
```

Until the split lands, the types below are the target surface — implement
against this document.

## One-shot transcription

```rust
use dictate_core::{Config, Engine};

let cfg = Config::load(None)?;                 // single config.toml
let engine = Engine::load(&cfg)?;              // model resident (GPU/CPU)
let pcm: Vec<f32> = /* 16 kHz mono */;
let text = engine.transcribe_f32(&pcm)?;       // dictionary + commands + format applied
```

Raw decode (skip text pipeline):

```rust
let text = engine.transcribe_f32_raw(&pcm)?;
```

## Custom status UI

```rust
use dictate_platform::{OverlayBackend, Stage, NullOverlay};

struct MyLoader;
impl OverlayBackend for MyLoader {
    fn set_stage(&self, stage: Stage) { /* drive your animation */ }
    fn flash(&self, _ms: u64) {}
    fn is_failed(&self) -> bool { false }
}
```

Pass `Box::new(MyLoader)` into `Session::builder`. Use `NullOverlay` for
servers/tests.

## Typing

Typing is **fail-closed**. Even if you supply a real `Typer`, keystrokes
only flow when `type_output = true` in the user's config file. Host apps
that never want typing use `NullTyper` and leave `type_output = false`.

## Daemon API from another process

```rust
use dictate_core::api::ApiClient;

let mut c = ApiClient::connect_default()?;
let resp = c.ping()?;
assert!(resp.ok);
let text = c.transcribe_wav("/path/to/clip.wav")?;
```

Socket: `$XDG_RUNTIME_DIR/dictate/dictate.sock`.

## Config

Everything lives in one TOML file. Dictionary overrides are
`[dict.overrides]`. See docs/ARCHITECTURE.md.
