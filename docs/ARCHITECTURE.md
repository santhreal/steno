# light-dictate — expansion architecture

Target: a **minimal, offline, embeddable** speech engine + CLI/daemon that
works cross-platform, stores **all** user config in one file, exposes a
daemon API, and lets host apps swap the status UI.

## Current state (2026-08-07, HEAD `d3bddd3` + in-tree expansion)

Legend: **Verified** = exercised in this tree (unit/e2e or prior remote proof).
**Unverified** = code present but not re-proven on a live desktop / soak host.

| Piece | Status |
|---|---|
| Workspace split (`dictate-core` / `dictate-platform` / `dictate`) | **Verified** — builds as a Cargo workspace |
| `Engine` + `Session` public API | **Verified** — unit-tested pipeline/session wiring; GPU load not in unit tests |
| Single config + `[dict.overrides]` | **Verified** — unit tests; legacy `dictionary.toml` import-in-memory (default/XDG only; `--config` uses sibling only) |
| Parakeet TDT v3 via sherpa-onnx | **Verified** earlier on CUDA (JFK wav GPU smoke, ~498 MiB VRAM). `provider = "cuda"\|"cpu"` is honored by `Engine` / `Transcriber::load` (fail-closed; no silent fallback). Daemon hot-path must pass `cfg.provider` (see ROADMAP). |
| Caps Lock PTT + cancel-any-key | **Verified** on X11 (axiomexec earlier); **Unverified** on this operator workstation after cutover |
| Dictionary + verbatim case protection | **Verified** in unit tests; daemon needs restart after edits |
| Typing (`type_output`) | Fail-closed in code; **Unverified** on live session after engine cutover |
| Daemon NDJSON API (`ping` / `status` / `transcribe` / `shutdown`) | **Verified** in unit/socket tests; live daemon path **Unverified** here |
| `utterance.*` streaming ops | **Implemented** (text-only on stop; never types). `Event::UtteranceDone` reserved/emitted. Live daemon path **Unverified** here |
| OverlayBackend + theme palettes (`pill|mono|dusk|dawn|contrast`) + `resolve_ui` | **Verified** unit tests in core/platform; live X11 pill **Unverified** here after cutover |
| Cross-platform | Linux X11 real; Windows / macOS **hotkey + typing + status chip implemented** (HWND / NSPanel); chips consume `ResolvedUi`. Not live-UI verified on this Linux host |
| Embeddable lib | **Yes** — depend on `dictate-core` (+ optional `dictate-platform`) |
| Daemon IPC | **Yes** — Unix domain socket, NDJSON |
| Daemon soak / crash recovery | Thin — needs Phase 5 hardening |
| `provider = cuda\|cpu` | Config + `Engine`/`Transcriber` honor it (default `"cuda"`). CPU CI: `.github/workflows/ci-cpu.yml` / `scripts/ci-cpu.sh` |

## Operator testing policy

**No live-session testing on the operator workstation.**

Agents and local development must not:

- run `dictate start` / `restart` against the logged-in desktop
- grab Caps Lock, inject keystrokes, or drive the live X11/GNOME session
- run GPU/nvidia-smi soaks or decode through the resident daemon on this machine

Hotkey, typing, overlay, and soak verification belong on **axiomexec** (Tailscale)
or a **disposable VM** only. Unit/`cargo test` and clippy on this host are fine
when they stay off the live session and do not install over `~/.cargo/bin`
unless the main agent asks.

## Locked decisions

0. **Post-STT refine** — `commands → dictionary → format → refine`. Default `RuleRefine` (offline ASR cleanup). Embedders can swap `RefineBackend` for heavier GEC; no network.


1. **Single config file** — `~/.config/dictate/config.toml` owns everything,
   including dictionary overrides under `[dict.overrides]`. Legacy
   `dictionary.toml` is imported once on default/XDG load (loud log; an
   explicit `--config` only reads a sibling file beside that path). Once
   `[dict.overrides]` is populated, the legacy file is ignored. No second
   config file. `dictate` never rewrites the operator's config for them.
2. **Workspace crates**
   - `dictate-core` — STT, DSP, audio, text pipeline, config, `Engine` /
     `Session`, overlay trait/`Stage`, IPC protocol + Unix client/server.
   - `dictate-platform` — `HotkeySource`, `Typer`, OS backends (Linux X11
     first; Win/macOS stubs). Re-exports `OverlayBackend` / `Stage` /
     `NullOverlay` from core.
   - `dictate` — CLI + daemon process binary.
3. **Daemon API** — local Unix domain socket (Linux/macOS) /
   named pipe (Windows later). Newline-delimited JSON. No HTTP, no
   cloud. Socket path: `$XDG_RUNTIME_DIR/dictate/dictate.sock` (fallback
   `$XDG_CACHE_HOME/dictate/dictate.sock` else `~/.cache/dictate/dictate.sock`). Optional `[api].token` shared secret. `[api].require_same_uid` (default true) gates peers via `SO_PEERCRED`.
4. **Typing safety stays fail-closed** — `type_output = true` in the
   config file is the only arming path, including for API clients. API
   cannot enable typing by itself. `utterance.*` must not enable typing.
5. **Overlay** — `OverlayBackend` trait in `dictate-core`. Theme
   resolution (`resolve_ui` / `stage_label` / `ThemePalette` / `ResolvedUi`)
   also lives in core; platforms call it once at overlay start and paint
   from `ResolvedUi`. Default Linux path = X11 pill via
   `dictate_platform::create`. Hosts may ignore `ui.theme` and inject their
   own backend while still reading the palette if desired. Config can
   disable (`overlay = false`) or select `theme = "pill"|"mono"|"dusk"|
   "dawn"|"contrast"` / `"null"` (aliases `"none"` / `"off"`). Custom draw
   code is a trait impl, not a scripting language.
6. **STT stays Parakeet/sherpa** — no whisper shims. `provider = "cuda"`
   (default) or `"cpu"` in config; unknown values fail at config load /
   `Transcriber::load`. No silent fallback between providers.
7. **Text pipeline order** — commands → dictionary → format → refine.
   Refine is wired and **on by default** (`[refine] enabled = true`,
   `backend = "rules"` → `RuleRefine`). Config fields are only `enabled` +
   `backend` (no max_* keys). Offline only; embedders may inject a custom
   `RefineBackend`. `enabled = false` → `NullRefine`. RuleRefine scope: tiny
   fixed rule tables, no token re-casing, no network/LLM in-tree.

## Text pipeline

```
raw STT text
  → voice commands
  → dictionary ([dict.overrides]; legacy dictionary.toml import-in-memory)
  → format (sentence case; verbatim markers protect dictionary casing)
  → refine (RuleRefine by default; RefineBackend hook; NullRefine if disabled)
  → stdout / typer (typer only when type_output armed)
```

## Crate layout

```
light-dictate/                      # workspace root
  Cargo.toml
  crates/
    dictate-core/
      src/
        lib.rs
        config.rs
        audio.rs dsp.rs stt.rs
        text/…              # commands, dictionary, format, refine
        engine.rs           # Engine::load / transcribe_f32[_raw]
        session.rs          # Session + InjectTyper (fail-closed typing)
        overlay.rs          # OverlayBackend + Stage + NullOverlay
        ui_theme.rs         # resolve_ui / stage_label / ThemePalette / ResolvedUi
        api/
          protocol.rs       # request/response/event types (serde)
          client.rs         # ApiClient::connect / call
          server.rs         # serve_unix[_until], ApiHandler
    dictate-platform/
      src/
        lib.rs
        traits.rs           # HotkeySource, Typer
        linux_x11/          # hotkey / overlay / type backends
        windows.rs          # Caps Lock + SendInput + HWND chip (ResolvedUi)
        macos.rs            # Caps Lock + CGEvent + NSPanel chip (ResolvedUi)
        null.rs             # NullHotkey / NullTyper
    dictate/
      src/
        main.rs
        config_cmd.rs       # dictate config|model|theme
        daemon.rs           # hotkey loop + API thread when [api].enabled
  docs/
```

## Single-config shape

```toml
model_path = "~/.local/share/dictate/models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
type_output = false          # FAIL-CLOSED: must be true to type
n_threads = 8
max_record_secs = 120
provider = "cuda"            # or "cpu" — fail-closed; no silent fallback

[text]
commands = true
format = true

[refine]
enabled = true               # default; false → NullRefine
backend = "rules"            # RuleRefine; unknown → warn + rules

[ui]
overlay = true
done_flash_ms = 1200
theme = "dusk"               # pill|mono|dusk|dawn|contrast; null|none|off → NullOverlay
                             # hosts may ignore the string and inject OverlayBackend

[ui.colors]                  # optional #RRGGBB / #RRGGBBAA overrides
fg = "#ECECF0"

[ui.stages]
recording = "Listening"      # defaults: Transcribing / Processing / Done / Error
transcribing = "Thinking"
done = "Done"
error = "Error"
show_timer = true
pulse_ms = 180

[api]
enabled = true               # daemon listens on the socket
# path = ""                  # empty → default runtime path
# token = ""                 # empty → no shared-secret check
# require_same_uid = true    # SO_PEERCRED same-uid gate (default true)

[hotkey]
# key = "Caps_Lock"          # reserved; Linux X11 Caps Lock for now

[dsp]
# …existing DspConfig fields…

[dict.overrides]
"veyyon" = "veyyon"
"vayon" = "veyyon"
"mukund" = "Mukund"
"um" = ""
```

## IPC protocol (NDJSON)

Client → server (one JSON object per line):

| op | purpose | Daemon today |
|---|---|---|
| `ping` | liveness | Implemented |
| `status` | pid, model, stage, `type_output_armed`, `api` | Implemented |
| `transcribe` | `{ "pcm_f32_b64" \| "wav_path": … }` → final text | Implemented (never types) |
| `utterance.start` | begin push-to-talk from API (no hotkey) | Implemented (buffer clear) |
| `utterance.audio` | stream PCM frames while open | Implemented (append PCM f32 LE b64) |
| `utterance.stop` | end → decode → text result (+ `utterance.done`) | Implemented (never types) |
| `utterance.cancel` | drop buffer | Implemented |
| `shutdown` | graceful stop | Implemented |

Server → client:

```json
{"id":1,"ok":true,"result":{"text":"hello"}}
{"id":1,"ok":false,"error":"…","hint":"…"}
{"event":"stage","stage":"listening"}
{"event":"transcript","text":"hello","final":true}
{"event":"utterance.done","text":"hello"}
```

`utterance.done` is emitted when an `utterance.stop` completes (same text as the
response `result`). API utterance/transcribe paths never type.

Socket: `$XDG_RUNTIME_DIR/dictate/dictate.sock`, else `$XDG_CACHE_HOME/dictate/dictate.sock`, else `~/.cache/dictate/dictate.sock`. Daemon pid/ready/log live under `$XDG_CACHE_HOME/dictate/` (else `~/.cache/dictate/`).

## Embedder surface (`dictate-core`)

```rust
use dictate_core::{Config, Engine, NullOverlay, Session, resolve_ui, stage_label, Stage};

let cfg = Config::load(None)?;
let engine = Engine::load(&cfg)?;          // model resident
let text = engine.transcribe_f32(&pcm16k)?; // offline utterance

let ui = resolve_ui(&cfg.ui);              // ThemePalette + stages
let _ = stage_label(&cfg.ui, Stage::Recording);

// Interactive (optional platform / custom overlay):
let session = Session::builder(engine)
    .from_config(&cfg)                     // copies type_output + ui.done_flash_ms
    .overlay(NullOverlay)                  // or a custom OverlayBackend
    .build();                              // missing overlay → NullOverlay
let text = session.transcribe_f32(&pcm16k)?;
```

`OverlayBackend` methods: `set(Stage)`, `flash(ms)`, `active() -> bool`.
Hosts may ignore `ui.theme` and inject their own backend while still calling
`resolve_ui` for palette / labels.

Typing requires **both** `type_output` armed (via `from_config` / `type_output(true)`)
**and** a `SessionBuilder::typer(...)` implementing `InjectTyper`. See
`docs/EMBEDDING.md`.

## Overlay theming

`OverlayBackend` (in `dictate-core`, re-exported by `dictate-platform`):

- `fn set(&self, stage: Stage)`
- `fn flash(&self, ms: u64)`
- `fn active(&self) -> bool` (fail-open UIs may return false)

`Stage`: `Hidden`, `Recording`, `Transcribing`, `Done`, `Error`.
Default labels: `"Transcribing"` / `"Processing"` / `"Done"` / `"Error"`
(overridable via `[ui.stages]`, e.g. Listening / Thinking).

**Resolution lives in `dictate-core`.** `resolve_ui(&UiConfig) -> ResolvedUi`
picks a preset palette (`pill|mono|dusk|dawn|contrast`), applies optional
`[ui.colors]` hex overrides into `ThemePalette`, and copies stage knobs.
`stage_label` / `list_themes` / surgical `config_get` / `config_set` are
exported from the same crate. Unknown themes warn and fall back to pill;
`null|none|off` still resolve to pill colors for shared helpers.

**Platforms consume `ResolvedUi`.** `dictate_platform::create(&UiConfig)`
maps `overlay = false` and `theme` `null|none|off` to `NullOverlay`;
otherwise Linux (X11 pill), Windows (layered HWND chip), and macOS
(`NSPanel` chip) call `resolve_ui` once at start and paint from the
resolved palette + labels. Embedders may ignore `ui.theme` and inject a
custom `OverlayBackend` while optionally still reading the palette —
no plugin ABI; compile-time injection only.

## Cross-platform roadmap

| Capability | Linux X11 | Linux Wayland | Windows | macOS |
|---|---|---|---|---|
| Hotkey | done | evdev/portal later | WH_KEYBOARD_LL Caps Lock | CGEventTap Caps Lock |
| Type | xdotool | portal/ydotool later | SendInput | CGEvent |
| Overlay | done (ResolvedUi) | layer-shell later | HWND chip (ResolvedUi; no local UI soak) | NSPanel chip (ResolvedUi; no local UI soak) |
| IPC | Unix socket | Unix socket | named pipe later | Unix socket |
| Audio | cpal | cpal | cpal | cpal |
| STT | sherpa cuda\|cpu | same | sherpa CPU/(CUDA) | sherpa CPU/(Metal later) |

## Robustness checklist

- [ ] Daemon GPU soak: N sequential decodes without VRAM leak (axiomexec / VM only)
- [ ] Socket reconnect + partial-line framing
- [ ] Model load errors always include corrective action
- [x] Typing remains config-armed through API (handler never arms typing)
- [ ] Crash → pidfile/socket cleaned; Caps Lock keysyms restored
- [x] Config migrate: dictionary.toml → `[dict.overrides]` once (in-memory; operator copies to disk)
- [ ] `cargo test` / clippy clean across workspace (main runs final gates)
- [ ] Remote X verification on axiomexec after platform extract
- [x] Policy: no live-session typing / hotkey / soak from local agent runs

## Non-goals (v0.2)

- Cloud STT, telemetry, auto-update
- Plugin dylib ABI for themes (trait injection only)
- HTTP/WebSocket API
- Streaming partial tokens from Parakeet (offline full-utterance)
- Wayland production support in the first cross-platform cut
- Live-session testing on the operator workstation
