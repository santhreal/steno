# steno - expansion architecture

Target: a **minimal, offline, embeddable** speech engine + CLI/daemon that
works cross-platform, stores **all** user config in one file, exposes a
daemon API, and lets host apps swap the status UI.

## Current state (2026-08-07, Phase 6 polish on master)

Legend: **Verified** = exercised in this tree (unit/e2e or prior remote proof).
**Unverified** = code present but not re-proven on a live desktop / soak host.

| Piece | Status |
| --- | --- |
| Workspace split (`steno-core` / `steno-platform` / `steno`) | **Verified**: builds as a Cargo workspace |
| `Engine` + `Session` public API | **Verified**: unit-tested; `from_parts` / `with_pipeline` / `process_text` for embedders; GPU load not in unit tests |
| Single config + `[refine.dictionary]` (legacy `[dict.overrides]` merged) | **Verified**: unit tests; legacy `dictionary.toml` import-in-memory (default/XDG only; `--config` uses sibling only) |
| Parakeet TDT v3 via sherpa-onnx | **Verified** earlier on CUDA (JFK wav GPU smoke, ~498 MiB VRAM). `provider = "cuda"\|"cpu"` is honored by `Engine` / `Transcriber::load` (fail-closed; no silent fallback). Daemon hot-path passes `cfg.provider` (**Implemented/Verified**). |
| Caps Lock PTT + cancel-any-key | **Verified** on X11 (axiomexec earlier); **Unverified** on this operator workstation after cutover |
| Dictionary phrase overrides | **Verified** in unit tests; daemon needs restart after edits |
| Typing (`type_output`) | Fail-closed in code; **Unverified** on live session after engine cutover |
| Daemon NDJSON API (`ping` / `status` / `transcribe` / `shutdown`) | **Verified** in unit/socket tests; live daemon path **Unverified** here |
| `utterance.*` streaming ops | **Implemented** (text-only on stop; never types). `Event::UtteranceDone` reserved/emitted. Live daemon path **Unverified** here |
| OverlayBackend + theme palettes (`pill|mono|dusk|dawn|contrast`) + `resolve_ui` | **Verified** unit tests in core/platform; live X11 pill **Unverified** here after cutover |
| Cross-platform | Linux X11 real + Wayland MVP typing (`wtype`); Windows / macOS **hotkey + typing + status chip implemented** (HWND / NSPanel); chips consume `ResolvedUi`. Not live-UI verified on this Linux host |
| Embeddable lib | **Yes**: depend on `steno-core` (+ optional `steno-platform`) |
| Daemon IPC | **Yes**: Unix domain socket, NDJSON |
| Daemon soak / crash recovery | Thin: needs Phase 5 hardening |
| `provider = "cuda" \| "cpu"` | Config + `Engine`/`Transcriber` honor it (default `"cuda"`). CPU CI: `.github/workflows/ci-cpu.yml` / `scripts/ci-cpu.sh` |

---

## Operator testing policy

**No live-session testing on the operator workstation.**

Agents and local development must not:

- run `steno start` / `restart` against the logged-in desktop
- grab Caps Lock, inject keystrokes, or drive the live X11/GNOME session
- run GPU/nvidia-smi soaks or decode through the resident daemon on this machine

Hotkey, typing, overlay, and soak verification belong on **axiomexec** (Tailscale)
or a **disposable VM** only. Unit/`cargo test` and clippy on this host are fine
when they stay off the live session and do not install over `~/.cargo/bin`
unless the main agent asks.

---

## Locked decisions

0. **Unified Refinement Architecture**: `STT -> Commands -> Refinement (GEC + Vocabulary) -> Formatting`. Default `RuleRefine` + vocabulary overrides. Embedders can swap `RefineBackend` for heavier GEC; no network.

1. **Single config file**: `~/.config/steno/config.toml` owns everything,
   including vocabulary overrides under `[refine.dictionary]`
   (legacy `[dict.overrides]` is merged into it on load). Legacy `dictionary.toml` is imported once
   on default/XDG load (loud log; an explicit `--config` only reads a sibling file
   beside that path). Once `[refine.dictionary]` is populated, the legacy file is
   ignored. No second config file. `steno` never rewrites the operator's config for them.
2. **Workspace crates**
   - `steno-core`: STT, DSP, audio, text pipeline, config, `Engine` /
     `Session`, overlay trait/`Stage`, IPC protocol + Unix client/server.
   - `steno-platform`: `HotkeySource`, `Typer`, OS backends (Linux X11
     primary + Wayland MVP typing; Win/macOS real). Re-exports
     `OverlayBackend` / `Stage` / `NullOverlay` from core.
   - `steno`: CLI + daemon process binary.
3. **Daemon API**: local Unix domain socket (Linux/macOS) /
   named pipe (Windows later). Newline-delimited JSON. No HTTP, no
   cloud. Socket path: `$XDG_RUNTIME_DIR/steno/steno.sock` (fallback
   `$XDG_CACHE_HOME/steno/steno.sock` else `~/.cache/steno/steno.sock`). Optional `[api].token` shared secret. `[api].require_same_uid` (default true) gates peers via `SO_PEERCRED`.
4. **Typing safety stays fail-closed**: `type_output = true` in the
   config file is the only arming path, including for API clients. API
   cannot enable typing by itself. `utterance.*` must not enable typing.
5. **Overlay**: `OverlayBackend` trait in `steno-core`. Theme
   resolution (`resolve_ui` / `stage_label` / `ThemePalette` / `ResolvedUi`)
   also lives in core; platforms call it once at overlay start and paint
   from `ResolvedUi`. Default Linux path = X11 pill via
   `steno_platform::create`. Hosts may ignore `ui.theme` and inject their
   own backend while still reading the palette if desired. Config can
   disable (`overlay = false`) or select `theme = "pill" | "mono" | "dusk" |
   "dawn" | "contrast"` / `"null"` (aliases `"none"` / `"off"`). Custom draw
   code is a trait impl, not a scripting language.
6. **STT stays Parakeet/sherpa**: no whisper shims. `provider = "cuda"`
   (default) or `"cpu"` in config; unknown values fail at config load /
   `Transcriber::load`. No silent fallback between providers.
7. **Text pipeline order**: `STT -> Commands -> Refinement (GEC + Vocabulary) -> Formatting`.
   Refine is wired and **on by default** (`[refine] enabled = true`,
   `backend = "rules"` → `RuleRefine`). Vocabulary overrides (`[refine.dictionary]`,
   alias `[refine.overrides]`) and GEC cleanup run inside the refinement stage before formatting.
   Config fields include `enabled`, `backend`, and `dictionary` (alias `overrides`). Offline only;
   embedders may inject a custom `RefineBackend`. `enabled = false` → `NullRefine`.
   RuleRefine scope: tiny fixed rule tables (space-before-punctuation stripping,
   duplicate words/clauses, contractions, ASR phrase maps, subject-verb map, a/an edges, fillers),
   no token re-casing, no network/LLM in-tree.

---

## Text pipeline

```text
raw STT text
  → voice commands
  → refinement (GEC + vocabulary via [refine.dictionary] + RuleRefine / RefineBackend)
  → formatting (sentence case; punctuation spacing)
  → stdout / typer (typer only when type_output armed)
```

---

## Crate layout

```text
steno/                      # workspace root
  Cargo.toml
  crates/
    steno-core/
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
    steno-platform/
      src/
        lib.rs
        traits.rs           # HotkeySource, Typer
        linux/              # facade: DISPLAY/WAYLAND_DISPLAY selection
        linux_x11/          # hotkey / overlay / xdotool backends
        linux_wayland/      # wtype (+ ydotool) typing MVP
        windows.rs          # Caps Lock + SendInput + HWND soft-blur chip (ResolvedUi)
        macos.rs            # Caps Lock + CGEvent + NSPanel tiny-skia chip (ResolvedUi)
        null.rs             # NullHotkey / NullTyper
    steno/
      src/
        main.rs
        api_cmd.rs          # steno ping | steno api status
        config_cmd.rs       # steno config|model|theme
        daemon.rs           # hotkey loop + API thread when [api].enabled
  docs/
```

---

## CLI subcommand summary

- `steno start` / `stop` / `status` / `restart`: daemon lifecycle management (`start` and `restart` accept `--foreground`)
- `steno config`: `show`, `get <key>`, `set <key> <val>` (inspect or set individual configuration keys validated by `list_settable_keys()`, including `max_record_secs`, `api.enabled`, `api.path`, `api.token`, `model_path`, `provider`, `type_output`, `n_threads`, `ui.theme`, `ui.overlay`, `ui.done_flash_ms`, `ui.stages.*`, and `ui.colors.*`)
- `steno model`: `list`, `use <name_or_path> [--provider cuda|cpu]` (list or select sherpa-onnx model directory)
- `steno theme`: `list`, `set <name>` (list built-in overlay themes or set `ui.theme`)
- `steno ping`: check daemon API socket connectivity and round-trip latency
- `steno api status`: query daemon API for process, model, stage, and arming status

---

## Single-config shape

```toml
model_path = "~/.local/share/steno/models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
type_output = false          # FAIL-CLOSED: must be true to type
n_threads = 8
max_record_secs = 120
provider = "cuda"            # or "cpu"; fail-closed; no silent fallback

[text]
commands = true
format = true

[refine]
enabled = true               # default; false → NullRefine
backend = "rules"            # RuleRefine; unknown → warn + rules

[refine.overrides]           # or [refine.dictionary]
"veyyon" = "veyyon"
"vayon" = "veyyon"
"mukund" = "Mukund"
"um" = ""
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

[dsp]
# …existing DspConfig fields…

```
`list_settable_keys()` in `steno-core` provides exact surgical key validation for `config_get` / `config_set` and `steno config set`. The full set of supported keys includes top-level options (`model_path`, `provider`, `type_output`, `n_threads`, `max_record_secs`), daemon API options (`api.enabled`, `api.path`, `api.token`), overlay settings (`ui.theme`, `ui.overlay`, `ui.done_flash_ms`), stage labels (`ui.stages.recording`, `ui.stages.transcribing`, `ui.stages.done`, `ui.stages.error`, `ui.stages.show_timer`, `ui.stages.pulse_ms`), and color overrides (`ui.colors.*`). Wildcard patterns are rejected.

---

## IPC protocol (NDJSON)

Client → server (one JSON object per line):

| op | purpose | Daemon today |
| --- | --- | --- |
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

Socket: `$XDG_RUNTIME_DIR/steno/steno.sock`, else `$XDG_CACHE_HOME/steno/steno.sock`, else `~/.cache/steno/steno.sock`. Daemon pid/ready/log live under `$XDG_CACHE_HOME/steno/` (else `~/.cache/steno/`).

---

## Embedder surface (`steno-core`)

```rust
use steno_core::{Config, Engine, NullOverlay, Session, resolve_ui, stage_label, Stage};

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

---

## Overlay theming

`OverlayBackend` (in `steno-core`, re-exported by `steno-platform`):

- `fn set(&self, stage: Stage)`
- `fn flash(&self, ms: u64)`
- `fn active(&self) -> bool` (fail-open UIs may return false)

`Stage`: `Hidden`, `Recording`, `Transcribing`, `Done`, `Error`.
Default labels: `"Transcribing"` / `"Processing"` / `"Done"` / `"Error"`
(overridable via `[ui.stages]`, e.g. Listening / Thinking).

**Resolution lives in `steno-core`.** `resolve_ui(&UiConfig) -> ResolvedUi`
picks a preset palette (`pill|mono|dusk|dawn|contrast`), applies optional
`[ui.colors]` hex overrides into `ThemePalette`, and copies stage knobs.
`stage_label` / `list_themes` / surgical `config_get` / `config_set` are
exported from the same crate. Unknown themes warn and fall back to pill;
`null|none|off` still resolve to pill colors for shared helpers.

**Platforms consume `ResolvedUi`.** `steno_platform::create(&UiConfig)`
maps `overlay = false` and `theme` `null|none|off` to `NullOverlay`;
otherwise Linux (X11 pill), Windows (layered HWND chip), and macOS
(`NSPanel` chip) call `resolve_ui` once at start and paint from the
resolved palette + labels. Embedders may ignore `ui.theme` and inject a
custom `OverlayBackend` while optionally still reading the palette:
no plugin ABI; compile-time injection only.

---

## Cross-platform roadmap

| Capability | Linux X11 | Linux Wayland | Windows | macOS |
| --- | --- | --- | --- | --- |
| Hotkey | done | evdev direct input (`/dev/input/event*` Caps Lock; `wayland` feature) or XWayland/X11 when `DISPLAY` set | WH_KEYBOARD_LL Caps Lock | CGEventTap Caps Lock |
| Type | xdotool | `wtype` (+ `ydotool` fallback) on pure Wayland | SendInput | CGEvent |
| Overlay | done (ResolvedUi) | layer-shell pill (`zwlr-layer-shell-v1`; `wayland` feature, smithay-client-toolkit; verified on sway headless) | HWND + soft `box_blur_alpha` chip (ResolvedUi; no local UI soak) | NSPanel + tiny-skia soft-shadow chip (ResolvedUi; no local UI soak) |
| IPC | Unix socket | Unix socket | named pipe later | Unix socket |
| Audio | cpal | cpal | cpal | cpal |
| STT | sherpa cuda\|cpu | same | sherpa CPU/(CUDA) | sherpa CPU/(Metal later) |

---

## Robustness checklist

- [x] Daemon GPU soak: 50 sequential decodes without VRAM leak (axiomexec RTX 4090, 546 MiB stable)
- [x] Socket reconnect + partial-line framing (framing_* tests in api::server)
- [x] Model load errors always include corrective action
- [x] Typing remains config-armed through API (handler never arms typing)
- [x] Crash → pidfile/socket cleaned; Caps Lock keysyms restored (watchdog thread + auto-repair on CLI + supervisor restart)
- [x] Config migrate: `dictionary.toml` → `[refine.dictionary]` (in-memory merge; `[dict.overrides]` also merged on load)
- [x] `cargo test` / clippy clean across workspace (323 tests, 6 suites, clippy -D warnings; 232 with llm-cuda on axiomexec)
- [x] Remote X verification on axiomexec: daemon start/stop/ping/api status on Xvfb :42 (RTX 4090, CUDA 12.0)
- [x] Policy: no live-session typing / hotkey / soak from local agent runs
- [x] Daemon supervisor: auto-restart on crash with exponential backoff
- [x] Audio failover: fresh device open per utterance; error overlay + continue
- [x] LLM refine backend: llama-cpp-2 GGUF, GPU/CPU via config + cargo features (llm + llm-cuda verified on axiomexec)
- [x] Wayland layer-shell overlay: `zwlr-layer-shell-v1` status pill (smithay-client-toolkit 0.21; `wayland` feature; verified on sway headless)
- [x] Wayland evdev hotkey: Caps Lock hold-to-talk via `/dev/input/event*` (`wayland` feature; requires `input` group)

---

## Non-goals (v0.2)

- Cloud STT, telemetry, auto-update
- Plugin dylib ABI for themes (trait injection only)
- HTTP/WebSocket API
- Streaming partial tokens from Parakeet (offline full-utterance)
- ~~Full Wayland hotkey/overlay parity~~ — **done**: evdev hotkey + layer-shell overlay (optional `wayland` feature)
- Live-session testing on the operator workstation
