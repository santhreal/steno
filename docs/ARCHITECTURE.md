# light-dictate — expansion architecture

Target: a **minimal, offline, embeddable** speech engine + CLI/daemon that
works cross-platform, stores **all** user config in one file, exposes a
daemon API, and lets host apps swap the status UI.

## Current state (2026-08-07)

| Piece | Status |
|---|---|
| Parakeet TDT v3 via sherpa-onnx CUDA | Working — GPU smoke (JFK wav, ~498 MiB VRAM) |
| Caps Lock PTT + cancel-any-key | Working on X11 (axiomexec verified earlier) |
| Dictionary + verbatim case protection | Working in unit tests; needs daemon restart after edits |
| Typing (`type_output`) | Fail-closed; **not** re-verified on live session after engine cutover |
| Daemon soak / crash recovery | Thin — needs hardening |
| Cross-platform | Linux X11 only (`x11rb`, `xdotool`) |
| Embeddable lib | No — single bin crate |
| Daemon IPC | No — only pidfile + CLI subcommands |
| Overlay theming | Hard-coded pill mock |

## Locked decisions

1. **Single config file** — `~/.config/dictate/config.toml` owns everything,
   including dictionary overrides under `[dict.overrides]`. Legacy
   `dictionary.toml` is imported once on load (loud log) then ignored once
   the merged table exists. No second config file.
2. **Workspace crates**
   - `dictate-core` — STT, DSP, audio, text pipeline, config, session, IPC
     protocol types. This is what embedders depend on.
   - `dictate-platform` — `Hotkey`, `Typer`, `OverlayBackend` traits + OS
     backends (Linux X11 first; Win/macOS stubs → real).
   - `dictate` — CLI + daemon process + socket server binary.
3. **Daemon API** — local Unix domain socket (Linux/macOS) /
   named pipe (Windows later). Newline-delimited JSON. No HTTP, no
   cloud. Socket path: `$XDG_RUNTIME_DIR/dictate/dictate.sock` (fallback
   under cache dir). Optional token file for multi-user hosts.
4. **Typing safety stays fail-closed** — `type_output = true` in the
   config file is the only arming path, including for API clients. API
   cannot enable typing by itself.
5. **Overlay** — `OverlayBackend` trait. Default = current pill. Hosts
   inject their own. Config can disable (`overlay = false`) or select a
   named built-in theme; custom draw code is a trait impl, not a scripting
   language.
6. **STT stays Parakeet/sherpa** — no whisper shims. Provider selectable
   (`cuda` / `cpu`) via config for non-NVIDIA hosts.

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
        text/…
        session.rs          # high-level: load → transcribe PCM → text
        api/
          protocol.rs       # request/response/event types (serde)
          client.rs         # thin client for other processes
    dictate-platform/
      src/
        lib.rs
        traits.rs           # HotkeySource, Typer, OverlayBackend
        linux_x11/          # current hotkey/overlay/type backends
        windows/            # stubs → SendInput / RegisterHotKey
        macos/              # stubs → CGEvent*
        null.rs             # headless (tests, servers)
    dictate/
      src/
        main.rs
        daemon.rs
        api_server.rs       # socket accept loop → session
  examples/
  docs/
```

## Single-config shape

```toml
model_path = "~/.local/share/dictate/models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
type_output = false          # FAIL-CLOSED: must be true to type
n_threads = 8
max_record_secs = 120
provider = "cuda"            # or "cpu"

[text]
commands = true
format = true

[ui]
overlay = true
done_flash_ms = 1200
theme = "pill"               # built-in; hosts ignore this and inject their own

[api]
enabled = true               # daemon listens on the socket
# path = ""                  # empty → default runtime path
# token = ""                 # empty → peer-cred only (same uid)

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

| op | purpose |
|---|---|
| `ping` | liveness |
| `status` | pid, model, stage, armed typing?, uptime |
| `transcribe` | `{ "pcm_f32_b64" \| "wav_path": … }` → final text |
| `utterance.start` | begin push-to-talk from API (no hotkey) |
| `utterance.audio` | stream PCM frames while open |
| `utterance.stop` | end → decode → emit event |
| `utterance.cancel` | drop buffer |
| `shutdown` | graceful stop (same uid / token) |

Server → client:

```json
{"id":1,"ok":true,"result":{"text":"hello"}}
{"id":1,"ok":false,"error":"…","hint":"…"}
{"event":"stage","stage":"listening"}
{"event":"transcript","text":"hello","final":true}
```

## Embedder surface (`dictate-core`)

```rust
use dictate_core::{Config, Engine, Session};

let cfg = Config::load(None)?;
let engine = Engine::load(&cfg)?;          // model resident
let text = engine.transcribe_f32(&pcm16k)?; // offline utterance

// Interactive (optional platform):
let session = Session::builder(engine)
    .overlay(MyOverlay::new())             # or NullOverlay
    .typer(NullTyper)                      # never types unless cfg armed + real Typer
    .build()?;
```

## Overlay theming

`OverlayBackend`:

- `fn set_stage(&self, stage: Stage)`
- `fn flash_done(&self, ms: u64)`
- `fn is_alive(&self) -> bool` (fail-open)

Built-ins: `pill` (current), `null`. Embedders ship their own loading
animations by implementing the trait — no plugin ABI in v0.2; compile-time
injection keeps it minimal and safe.

## Cross-platform roadmap

| Capability | Linux X11 | Linux Wayland | Windows | macOS |
|---|---|---|---|---|
| Hotkey | done | evdev/portal later | RegisterHotKey | CGEventTap |
| Type | xdotool → XTEST | portal/ydotool later | SendInput | CGEvent |
| Overlay | done | layer-shell later | layered HWND | NSPanel |
| IPC | Unix socket | Unix socket | named pipe | Unix socket |
| Audio | cpal | cpal | cpal | cpal |
| STT | sherpa CUDA/CPU | same | sherpa CPU/(CUDA) | sherpa CPU/(Metal later) |

## Robustness checklist

- [ ] Daemon GPU soak: N sequential decodes without VRAM leak
- [ ] Socket reconnect + partial-line framing
- [ ] Model load errors always include corrective action
- [ ] Typing remains config-armed through API
- [ ] Crash → pidfile/socket cleaned; Caps Lock keysyms restored
- [ ] Config migrate: dictionary.toml → `[dict.overrides]` once
- [ ] `cargo test` / clippy clean across workspace
- [ ] Remote X verification on axiomexec after platform extract
- [ ] No live-session typing from local agent runs

## Non-goals (v0.2)

- Cloud STT, telemetry, auto-update
- Plugin dylib ABI for themes (trait injection only)
- HTTP/WebSocket API
- Streaming partial tokens from Parakeet (offline full-utterance)
- Wayland production support in the first cross-platform cut
