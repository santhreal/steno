# Embedding dictate-core

Minimal offline STT for host applications. No cloud. No UI required.

## Add the dependency

```toml
[dependencies]
dictate-core = { path = "…/crates/dictate-core" }
# optional OS backends (hotkey / typing / X11 overlay):
dictate-platform = { path = "…/crates/dictate-platform" }
```

## One-shot transcription

```rust
use dictate_core::{Config, Engine};

let cfg = Config::load(None)?;                 // ~/.config/dictate/config.toml
let engine = Engine::load(&cfg)?;              // model resident (provider from cfg)
let pcm: Vec<f32> = /* 16 kHz mono */;
let text = engine.transcribe_f32(&pcm)?;       // commands → dictionary → format
```

Raw decode (skip text pipeline):

```rust
let text = engine.transcribe_f32_raw(&pcm)?;
```

## Session (engine + overlay)

`Session` wraps a loaded `Engine` and a `Box<dyn OverlayBackend>`. One-shot
PCM still goes through the engine; the session drives status stages around
decode:

`Stage::Recording` → `Stage::Transcribing` → `Stage::Done` (or `Error`).

(Product docs sometimes call these Listening / Thinking / Done.)

```rust
use dictate_core::{Config, Engine, NullOverlay, OverlayBackend, Session, Stage};

struct MyLoader;
impl OverlayBackend for MyLoader {
    fn set(&self, stage: Stage) { /* drive your animation */ }
    fn flash(&self, _ms: u64) {}
    fn active(&self) -> bool { true }
}

let cfg = Config::load(None)?;
let engine = Engine::load(&cfg)?;
let mut session = Session::builder(engine)
    .from_config(&cfg)                 // copies type_output + ui.done_flash_ms
    .overlay(MyLoader)                 // or NullOverlay / overlay_box(...)
    .build();                          // Session (not Result); default overlay = NullOverlay

let text = session.transcribe_f32(&pcm)?;
```

Use `NullOverlay` for servers/tests (no GPU / no DISPLAY).

Stage order without loading a model (tests / custom decode):

```rust
Session::drive_overlay_stages(&NullOverlay, 0, || Ok::<_, anyhow::Error>(()));
```

## Typing (fail-closed)

Typing is **fail-closed**. Keystrokes leave `Session` only when **both** are true:

1. `type_output = true` in the user's config (via `SessionBuilder::from_config` / `type_output(true)`), and
2. a typer was injected with `SessionBuilder::typer(...)`.

`dictate-core` exposes `InjectTyper` (same shape as platform `Typer`) so the
session crate does not depend on OS backends. On Linux X11:

```rust
use dictate_core::{InjectTyper, Session};
use dictate_platform::{Emitter, NullTyper, OutputMode, Typer};

// Never types (tests / headless):
let session = Session::builder(engine)
    .typer(NullTyper)
    .type_output(false)   // keep disarmed
    .build();

// Real typing — still requires type_output = true in config:
let emitter = Emitter::new(OutputMode::Type);
let session = Session::builder(engine)
    .from_config(&cfg)    // must have armed type_output
    .typer(emitter)       // Emitter: Typer + InjectTyper
    .build();
```

Host apps that never want typing omit `.typer(...)` and leave `type_output = false`.
If `type_output` is armed but no typer was injected, `transcribe_f32` returns an
error instead of typing.

Linux also implements `dictate_platform::HotkeySource` for `Hotkey` and
`dictate_platform::Typer` for `Emitter` (Type mode only; Stdout mode refuses).

## Daemon API from another process

`ApiClient` is a thin Unix-socket NDJSON client: `connect(path)` + `call(&Request)`.

```rust
use dictate_core::api::{ApiClient, Op, Request, default_socket_path};

let mut c = ApiClient::connect(default_socket_path()?)?;

let ping = c.call(&Request {
    id: 1,
    token: None,
    op: Op::Ping,
})?;
assert!(ping.ok);

let resp = c.call(&Request {
    id: 2,
    token: None,
    op: Op::Transcribe {
        wav_path: Some("/path/to/clip.wav".into()),
        pcm_f32_b64: None,
    },
})?;
assert!(resp.ok);
// resp.result is JSON, typically {"text":"..."}; API never types
```

Socket: `$XDG_RUNTIME_DIR/dictate/dictate.sock` (else `~/.cache/dictate/dictate.sock`).

Streaming: send `utterance.start` → `utterance.audio` (pcm_f32_b64) → `utterance.stop`.
Stop returns `{"text":…}`; server may also emit `{"event":"utterance.done","text":…}`.
CLI helpers: `dictate ping`, `dictate api status` (same socket / optional `--socket`).

Ops implemented by the daemon today: `ping`, `status`, `transcribe`, `shutdown`.
`utterance.*` (`start`/`audio`/`stop`/`cancel`) is implemented on the daemon: buffers PCM, returns text on stop (never types). Optional `[api].require_same_uid` (default true) rejects other-uid peers.

## Config

Everything lives in one TOML file (`~/.config/dictate/config.toml`).
Dictionary overrides are `[dict.overrides]`. See `docs/ARCHITECTURE.md`.

Relevant knobs for embedders:

```toml
type_output = false          # FAIL-CLOSED: must be true to type
n_threads = 8
provider = "cuda"            # or "cpu"; fail-closed, no silent fallback

[ui]
overlay = true
done_flash_ms = 1200
theme = "pill"               # hosts ignore this and inject OverlayBackend
```
