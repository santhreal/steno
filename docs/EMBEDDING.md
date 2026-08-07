# Embedding dictate-core

Minimal offline STT for host applications. No cloud. No UI required.

`dictate-core` is the embeddable library. `dictate-platform` is optional OS
glue (Caps Lock, typing, status chip). The `dictate` binary is one consumer of
both — hosts should depend on `dictate-core` (+ platform only if they want
native hotkey/overlay/typing).

## Add the dependency

```toml
[dependencies]
dictate-core = { path = "…/crates/dictate-core" }
# optional OS backends (hotkey / typing / status chip):
dictate-platform = { path = "…/crates/dictate-platform" }
```

## One-shot transcription

```rust
use dictate_core::{Config, Engine};

let cfg = Config::load(None)?;                 // ~/.config/dictate/config.toml
let engine = Engine::load(&cfg)?;              // model resident (provider from cfg)
let pcm: Vec<f32> = /* 16 kHz mono */;
let text = engine.transcribe_f32(&pcm)?;       // commands → dictionary → format → refine
```

Raw decode (skip text pipeline, including refine):

```rust
let text = engine.transcribe_f32_raw(&pcm)?;
```

Explicit model directory (same precedence as CLI `--model`):

```rust
use std::path::Path;
let engine = Engine::load_model(&cfg, Some(Path::new("/path/to/model-dir")))?;
```

Reprocess stored transcripts with the loaded dictionary / refine (no STT):

```rust
let cleaned = engine.process_text("hello vayon world");
```

### Engine composition (custom pipeline)

Hosts that inject a custom [`RefineBackend`], pre-built dictionary, or test
double assemble the engine explicitly:

```rust
use dictate_core::{
    Config, Dictionary, Engine, NullRefine, RefineBackend, TextConfig, TextPipeline,
    Transcriber,
};

struct MyRefine;
impl RefineBackend for MyRefine {
    fn refine(&self, text: &str) -> String { /* pure, offline */ text.to_string() }
}

let cfg = Config::load(None)?;
let model = dictate_core::resolve_model(None, &cfg)?;
let transcriber = Transcriber::load(&model, cfg.n_threads, &cfg.provider)?;
let dict = Dictionary::from_map(cfg.dict.overrides.clone());
let pipeline = TextPipeline::with_refine(cfg.text, dict, Box::new(MyRefine));
let engine = Engine::from_parts(transcriber, pipeline);

// Or swap after load:
let engine = Engine::load(&cfg)?.with_pipeline(
    TextPipeline::with_refine(cfg.text, Dictionary::from_map(Default::default()), Box::new(NullRefine)),
);
```

Accessors: `engine.transcriber()`, `engine.pipeline()`.

## Text pipeline

Order is fixed: **commands → dictionary → format → refine**.

```rust
use dictate_core::{Dictionary, FmtState, TextConfig, TextPipeline, COMMANDS};

let pipeline = TextPipeline::new(TextConfig::default(), Dictionary::from_map(overrides));
let one_shot = pipeline.process("bank comma next line");
let (chunk, state) = pipeline.process_stream("first segment", FmtState::default());
let (next, state) = pipeline.process_stream("second segment", state);
```

`COMMANDS` is the built-in voice-command table. Dictionary replacements are
verbatim (formatter never re-cases them).

## Refine (`RefineBackend`)

`Engine::load` builds `TextPipeline::with_refine(..., cfg.refine.make_backend())`.
Default `[refine] enabled = true`, `backend = "rules"` → `RuleRefine`
(duplicate/short-clause collapse, spaced/split contractions, high-precision
ASR phrase + subject–verb maps, a/an edges, light fillers, space-before-punct;
offline tables only — not acoustic-garble repair). `enabled = false` →
`NullRefine`.

`RefineBackend` must stay pure and offline — there is no network path in
`dictate-core`. Heavier offline GEC belongs behind the same trait.

## Session (engine + overlay + optional typer)

`Session` wraps a loaded `Engine` and a `Box<dyn OverlayBackend>`. One-shot
PCM still goes through the engine; the session drives status stages around
decode:

`Stage::Recording` → `Stage::Transcribing` → `Stage::Done` (or `Error`).

Default stage labels are `"Transcribing"` / `"Processing"` / `"Done"` /
`"Error"`; remap via `[ui.stages]` (for example Listening / Thinking).

```rust
use dictate_core::{Config, Engine, NullOverlay, OverlayBackend, Session, Stage};

struct MyLoader;
impl OverlayBackend for MyLoader {
    fn set(&self, stage: Stage) { /* drive your custom animation */ }
    fn flash(&self, _ms: u64) {}
    fn active(&self) -> bool { true }
}

let cfg = Config::load(None)?;
let engine = Engine::load(&cfg)?;
let mut session = Session::builder(engine)
    .from_config(&cfg)                 // copies type_output + ui.done_flash_ms only
    .overlay(MyLoader)                 // custom OverlayBackend animations
    .build();                          // default overlay = NullOverlay

let text = session.transcribe_f32(&pcm)?;
```

`from_config` does **not** pick a theme overlay — call
`dictate_platform::create(&cfg.ui)` when you want the built-in chip, or inject
your own backend. Theme palettes / labels stay available via `resolve_ui`.

Stage order without loading a model (tests / custom decode):

```rust
Session::drive_overlay_stages(&NullOverlay, 0, || Ok::<_, anyhow::Error>(()));
```

## UI resolution (`resolve_ui`)

Theme palettes and stage copy live in `dictate-core`. Platforms and host
apps share the same helpers:

```rust
use dictate_core::{resolve_ui, stage_label, list_themes, Stage};

let ui = resolve_ui(&cfg.ui);          // ResolvedUi { theme, colors, stages, … }
let label = stage_label(&cfg.ui, Stage::Recording);
let names = list_themes();             // pill | mono | dusk | dawn | contrast
```

- `resolve_ui` applies the preset (`pill` default) then optional
  `[ui.colors]` hex overrides into a `ThemePalette`, and copies
  `[ui.stages]` / `overlay` / `done_flash_ms` into `ResolvedUi`.
- Unknown themes warn and fall back to pill (fail-open). `null|none|off`
  still resolve to pill colors here; platform `create` maps those names to
  `NullOverlay`.
- Custom `OverlayBackend` remains valid: inject your own animation, ignore
  `ui.theme`, and optionally still call `resolve_ui` when you want the
  shared palette / stage labels.

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

Linux / Windows / macOS ship Caps Lock PTT + typing + a status chip (Linux =
animated X11 pill; Win/mac = simpler chips). All chips call `resolve_ui`.
`overlay = false` / `theme` `null|none|off` → `NullOverlay`.

## Capture / DSP (optional)

`dictate_core::audio` records 16 kHz mono (`record`, `record_while`,
`list_input_devices`). `dictate_core::dsp` covers resample / DC-block /
gain / trim used by the CLI. Most embedders feed their own PCM into
`Engine::transcribe_f32`.

## Config helpers

```rust
use dictate_core::{
    Config, config_get, config_set, default_config_path, default_model_dir,
    expand_tilde, list_settable_keys, resolve_model,
};

let path = default_config_path()?;
let cfg = Config::load(Some(&path))?;
let model = resolve_model(None, &cfg)?;
let _ = expand_tilde("~/.local/share/dictate/models".as_ref())?;
config_set(&path, "ui.theme", "dusk")?;
let theme = config_get(&path, "ui.theme")?;
```

Surgical keys are listed by `list_settable_keys()` (`model_path`, `provider`,
`type_output`, `n_threads`, `ui.*`, `ui.stages.*`, `ui.colors.*`). Typing stays
fail-closed: arm only via `type_output = true` in the file.

Relevant knobs:

```toml
type_output = false          # FAIL-CLOSED: must be true to type
n_threads = 8
provider = "cuda"            # or "cpu"; fail-closed, no silent fallback

[refine]
enabled = true               # default; false → NullRefine
backend = "rules"            # RuleRefine; unknown → warn + rules

[ui]
overlay = true
done_flash_ms = 1200
theme = "dusk"               # hint for platform create; hosts may ignore
                             # and inject OverlayBackend instead

[ui.colors]                  # optional #RRGGBB / #RRGGBBAA
fg = "#ECECF0"

[ui.stages]
recording = "Listening"      # defaults: Transcribing / Processing / Done / Error
transcribing = "Thinking"
done = "Done"
error = "Error"
show_timer = true
pulse_ms = 180

[dict.overrides]
"handy" = "Dictate"
```

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

Socket: `$XDG_RUNTIME_DIR/dictate/dictate.sock` (else `$XDG_CACHE_HOME/dictate/dictate.sock`, else `~/.cache/dictate/dictate.sock`).

Streaming: `utterance.start` → `utterance.audio` (pcm_f32_b64) → `utterance.stop`.
Stop returns `{"text":…}`; server may also emit `{"event":"utterance.done","text":…}`.
CLI: `dictate ping`, `dictate api status`.

Ops: `ping`, `status`, `transcribe`, `utterance.*`, `shutdown`. Typing is never
armed through the API. `[api].require_same_uid` (default true) rejects
other-uid peers.

## Library map

| Module | Role |
|--------|------|
| `config` | Single TOML + path helpers + surgical get/set |
| `ui_theme` | `resolve_ui` / palettes / stage labels |
| `engine` | High-level STT + pipeline for embedders |
| `session` | Engine + overlay + fail-closed typing |
| `stt` | Parakeet via sherpa-onnx (`Transcriber`) |
| `text` | Commands / dictionary / format / refine |
| `audio` / `dsp` | Mic capture + preprocess |
| `api` | NDJSON client + server types |
| `overlay` | `Stage` + `OverlayBackend` (no OS deps) |

Platform crate: `HotkeySource`, `Typer`, `create(&UiConfig)`, Linux/Win/mac backends.
