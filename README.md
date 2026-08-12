# steno

Minimal, fully offline speech-to-text dictation for Linux, macOS, and
Windows. Speak; text comes out. No cloud. Default decode uses CUDA on
Linux; set `provider = "cpu"` for CPU-only hosts, or `"metal"` on macOS.
One-shot or a background daemon.

`steno` records from your microphone, transcribes locally with
[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) (Parakeet TDT), cleans
the text up, and prints it - or types it into whatever window is focused.

Use it one-shot (`steno`) or as a system-wide daemon (`steno start`) that
keeps the model loaded and listens for **Caps Lock** (hold to talk).

```console
$ steno
This is a test of the Dictate System.
The quick brown fox jumps over the lazy dog?

$ steno meeting-note.wav
The budget is approved, see attached.
```

## Build

You need a Rust toolchain (1.85+), a C compiler, and the sherpa-onnx
shared libraries available at build time via `SHERPA_ONNX_LIB_DIR`.

### Linux (CUDA or CPU)

```bash
export SHERPA_ONNX_LIB_DIR=/usr/local/lib/sherpa-onnx
cargo build -p steno --release
cargo install --path crates/steno
```

For CUDA LLM offload (NVIDIA GPU), build with the `llm-cuda` feature.
nvcc 12.8 is incompatible with GCC 13's `_Float128` in system headers;
use g++-12 as the CUDA host compiler:

```bash
export CUDA_PATH=/usr/local/cuda-12.8
export CMAKE_CUDA_HOST_COMPILER=/usr/bin/g++-12
export CMAKE_CUDA_COMPILER=/usr/local/cuda-12.8/bin/nvcc
cargo build -p steno --release --features llm-cuda
cargo install --path crates/steno --features llm-cuda
```

For Vulkan LLM offload (AMD/Intel/NVIDIA):

```bash
cargo build -p steno --release --features llm-vulkan
```

### macOS (Metal or CPU)

```bash
export SHERPA_ONNX_LIB_DIR=/path/to/sherpa-onnx-macos-lib
cargo build -p steno --release --features llm-metal
cargo install --path crates/steno --features llm-metal
```

Set `provider = "metal"` in config for Metal GPU decode. Accessibility
permission is required for hotkey and typing: grant it in System Settings
> Privacy & Security > Accessibility.

### Windows

```bash
set SHERPA_ONNX_LIB_DIR=C:\path\to\sherpa-onnx-windows-lib
cargo build -p steno --release
cargo install --path crates/steno
```

The daemon API uses a named pipe at `\\.\pipe\steno` instead of a Unix
socket. Typing uses SendInput Unicode keystrokes. `curl` and `tar`
(included in Windows 10 1803+) are needed for `steno model download`.

### Cargo features

| Feature | Effect |
| --- | --- |
| (default) | STT only, no LLM refine |
| `llm` | CPU-only LLM refine via llama-cpp-2 |
| `llm-cuda` | LLM refine with NVIDIA CUDA offload |
| `llm-vulkan` | LLM refine with Vulkan offload (AMD/Intel/NVIDIA) |
| `llm-metal` | LLM refine with macOS Metal offload |
| `wayland` | Wayland overlay (layer-shell) and evdev hotkey backends |

There is no cargo `--features cuda` flag for STT: pick the execution
provider in config (`provider = "cuda"` default, `"cpu"`, or `"metal"`).
CUDA builds still need the system CUDA/cuDNN install the sherpa libs were
built against. Unknown provider values fail closed (no silent fallback).

### CI

GitHub Actions runs three workflows:

- **cpu-ci** (`.github/workflows/ci-cpu.yml`): unit tests + clippy with
  default, `llm`, and `wayland` features, across stable/beta/1.85. No
  daemon, DISPLAY, or GPU.
- **cross-compile** (`.github/workflows/ci-cross.yml`): `cargo check` +
  clippy for Windows and macOS targets (best-effort).
- **release-check** (`.github/workflows/release-check.yml`):
  `cargo publish --dry-run` for each crate.

## Get a model

`steno` uses a sherpa-onnx **model directory** (encoder/decoder/joiner
ONNX + `tokens.txt`). Recommended: NVIDIA Parakeet TDT v3 int8.

```bash
steno model download              # STT model (~150 MB)
steno model download --llm        # STT + LLM refine model (~1.6 GB extra)
```

Or download the STT model manually:

```bash
mkdir -p ~/.local/share/steno/models
cd ~/.local/share/steno/models
curl -LO https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2
tar xjf sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2
```

| Model dir | Size | Note |
| --- | --- | --- |
| `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8` | ~600 MB | Default; multilingual; GPU-fast |

When `--model` is not set, `steno` picks the single model directory under
`~/.local/share/steno/models`. With several, set `model_path` in the
config (or pass `--model /path/to/model-dir`).

## Use it

**Daemon (recommended for daily use).** Arm typing once, then start the
resident model:

```toml
# ~/.config/steno/config.toml
type_output = true
model_path = "~/.local/share/steno/models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
```

```console
$ steno start
Dictation running (PID 12345).
Hotkey: hold Caps Lock to speak.
Log: /home/you/.cache/steno/steno.log

$ steno status
Dictation running (PID 12345).
Hotkey: hold Caps Lock to speak.

$ steno stop
Dictation stopped.
```

Hold **Caps Lock**, speak, release. The daemon already has the model in
memory, so there is no cold-start per utterance. `steno restart` bounces
it; pass `--foreground` to `steno start` or `steno restart` to run in the terminal for debugging.
Daemon pid/ready/log files live under `$XDG_CACHE_HOME/steno/` when that
env is set, otherwise `~/.cache/steno/`. Make sure no other app (GNOME
custom shortcut, etc.) already owns Caps Lock.
If another application (GNOME custom shortcut, sxhkd, KDE) already owns
Caps Lock, the daemon errors with a corrective hint. Caps Lock is grabbed
with a SYNC passive grab that never modifies the keymap: if the daemon
dies for any reason (including `kill -9`), Caps Lock works again
instantly with no repair needed. Legacy `steno stop` still repairs
NoSymbol damage from older daemon builds.

A SYNC grab freezes the whole keyboard between the Caps Lock press and
the daemon's `XAllowEvents`. A dedicated thread owns the grab connection
and does nothing else, so that window is a few milliseconds regardless of
what the daemon is doing. Transcription, LLM refine, and typing cannot
hold the keyboard.

**Record and print (one-shot).** Run `steno`, speak, pause. Recording
stops after about a second of silence (configurable). Text streams to
stdout segment by segment as it is decoded, so it composes:
`steno | xclip -selection clipboard`.

**Record and type (one-shot).** Typing is fail-closed: it works only after
you arm it once in `~/.config/steno/config.toml` (`type_output = true`).
Then `steno` (or `steno --type`) types into the focused window via
`xdotool` on X11 (`sudo apt install xdotool`) or `wtype` on pure Wayland (`sudo apt install wtype`), and `steno --stdout`
prints instead for one run. Typed text streams as it is decoded; the
clipboard is never touched.

**Status overlay.** A tiny animated status chip at the bottom center of
your primary monitor shows the stage (defaults: Transcribing with waveform
+ timer, Processing with spinner, Done with check). It takes no focus and
no input, and hides itself after Done. Pick a palette with `ui.theme`,
override colors / labels under `[ui.colors]` / `[ui.stages]`, or disable
with `overlay = false` / `theme = "null"`. See [Themes](#themes).

A bare `steno --type` without the config entry fails with an error:
typing is deliberately not enableable from a one-shot flag. Arm it once
with `steno config set type_output true` (or edit the TOML). Control
characters other than newline are stripped before typing, so a transcript
can never smuggle Tab or Escape keystrokes into the target.

**Transcribe a file.** `steno clip.wav` reads a WAV instead of recording (any
PCM or 32-bit float WAV, resampled to 16 kHz mono internally), also useful for
testing your setup without a microphone.

Useful flags: `--list-devices` and `--device <name>` pick a microphone,
`--raw` skips all text processing,
`--model`/`--config <path>` override auto-resolution,
`-v`/`-vv` shows what the pipeline is doing.

One-shot invocations load the model from disk each time (a few seconds of
startup). `steno start` keeps the model resident so hold-to-talk skips
that cost. Smaller models start faster either way.

## Voice commands

Spoken commands are replaced inline. `steno --list-commands` prints this
table:

| Say | Get |
| --- | --- |
| period / full stop | `.` |
| comma | `,` |
| question mark | `?` |
| exclamation mark / point | `!` |
| colon | `:` |
| semicolon | `;` |
| ellipsis / dot dot dot | `…` |
| open quote | `"` |
| close quote / end quote / unquote | `"` |
| open paren | `(` |
| close paren | `)` |
| percent sign | `%` |
| dollar sign | `$` |
| new line | line break |
| new paragraph | blank line |
| scratch that / delete that | delete back to the last sentence boundary |

Commands match whole words only. The recognizer often adds its own
punctuation around spoken commands ("bank, comma,"); duplicate punctuation
is collapsed during formatting, so you get "bank, " and not "bank,, ".

## Refinement & Vocabulary

Refinement cleans and rewrites text during processing (`STT -> Commands -> Refinement (GEC + Vocabulary) -> Formatting`). Vocabulary overrides handle names, jargon, and product terms the recognizer gets wrong. Put overrides in the same config file under `[refine.dictionary]` (alias `[refine.overrides]`):

```toml
# ~/.config/steno/config.toml
[refine]
enabled = true

[refine.dictionary]
"handy" = "Dictate"
"main street" = "Main Street"
"um" = ""                # empty replacement deletes the phrase
```

Matching is case-insensitive and whole-word; longer phrases win. The
replacement's case is used exactly as written.

If you still have a legacy `~/.config/steno/dictionary.toml`, it is
imported into memory once when loading the **default** config and
`[refine.dictionary]` (or legacy `[dict.overrides]`) is empty (with a deprecation warning). An explicit
`--config /path/to.toml` only considers a sibling `dictionary.toml` beside
that file, never the operator XDG path. Copy entries under
`[refine.dictionary]` and remove
the old file; `steno` never rewrites your config for you. Restart the
daemon after edits (`steno restart`).

### LLM refine

For higher-quality correction, swap the rule-based refiner for a local LLM.
Build with `--features llm` (CPU), `llm-cuda` (NVIDIA), `llm-vulkan`
(AMD/Intel/NVIDIA), or `llm-metal` (macOS). Then set `backend = "llm"`:

```toml
[refine]
backend = "llm"

[refine.llm]
model_path = "~/.local/share/steno/models/LFM2.5-2.6B-Q4_K_M.gguf"
n_gpu_layers = -1          # -1 = all layers to GPU, 0 = CPU only
n_threads = 4              # CPU threads for prompt processing
max_tokens = 512           # max generated tokens per utterance
temperature = 0.1          # 0.0 = greedy, >0 = sampled
n_ctx = 4096               # context window (must fit prompt + max_tokens)
# prompt = "..."           # override the built-in system prompt
# no_think = false         # set true for Qwen3 reasoning models to skip <think>
```

Download the default LLM model with `steno model download --llm`, or
supply any GGUF chat model. If the model cannot be loaded (missing file,
corrupt GGUF, OOM), steno logs the error and falls back to `RuleRefine`
so dictation keeps working. The LLM refine backend serializes generation
calls with a mutex; only one utterance is refined at a time.
## Configuration

Everything has a default; `~/.config/steno/config.toml` overrides it; CLI
flags override the file (`--config <path>` loads a different file). A full
config looks like:

```toml
model_path = "~/.local/share/steno/models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
n_threads = 8            # CPU threads for feature extraction; default: half your CPUs
max_record_secs = 120    # hard cap per recording
type_output = false      # arm typing (xdotool/wtype); the ONLY way to enable it
provider = "cuda"        # or "cpu"; fail-closed, no silent fallback

[vad]
silence_ms = 900         # stop after this much trailing silence
min_speech_ms = 250      # ignore shorter bursts (clicks)
start_timeout_secs = 10  # give up if no speech starts
speech_threshold = 0.01  # RMS of a 30 ms window that counts as speech

[dsp]
target_rms = 0.1         # normalize recordings to this loudness
max_gain = 8.0           # ...but never boost more than this

[text]
commands = true
format = true

[refine]
enabled = true         # offline refinement pipeline (default on)
backend = "rules"      # RuleRefine; unknown names warn and use rules

[refine.dictionary]   # or [refine.overrides]
"handy" = "Dictate"
"main street" = "Main Street"
"um" = ""

[refine.llm]            # only used when backend = "llm"
# model_path = "~/.local/share/steno/models/LFM2.5-2.6B-Q4_K_M.gguf"
n_gpu_layers = -1       # -1 = all to GPU, 0 = CPU only
n_threads = 4
max_tokens = 512
temperature = 0.1
n_ctx = 4096
# no_think = false       # set true for Qwen3 reasoning models
[ui]
overlay = true         # bottom-center status overlay
done_flash_ms = 1200   # how long done/error stays visible
theme = "dusk"         # pill | mono | dusk | dawn | contrast
                       # (or null | none | off → no-op overlay)

[ui.colors]            # optional #RRGGBB / #RRGGBBAA overrides
fg = "#ECECF0"

[ui.stages]
recording = "Listening"      # Stage::Recording label (default "Transcribing")
transcribing = "Thinking"    # Stage::Transcribing label (default "Processing")
done = "Done"
error = "Error"
show_timer = true
pulse_ms = 180

[api]
enabled = true         # daemon listens on a local Unix socket
# path = ""            # empty → $XDG_RUNTIME_DIR/steno/steno.sock
# token = ""           # optional shared secret on each request
# require_same_uid = true    # SO_PEERCRED same-uid gate (default true)


The **refine** section (`[refine]`) configures the unified refinement pipeline (`STT -> Commands -> Refinement (GEC + Vocabulary) -> Formatting`), combining GEC cleanup (`RuleRefine` / `RefineBackend`) and vocabulary / dictionary phrase overrides (`[refine.dictionary]`, alias `[refine.overrides]`). It collapses duplicate words / short repeated clauses, fixes spaced or split contractions, high-precision ASR phrase maps (homophones with tight context, doubled prepositions, common mishears), a small subject-verb map, a/an edges, and light leading/trailing fillers, then strips space-before punctuation. Config knobs are `enabled`, `backend = "rules"`, and overrides under `[refine.dictionary]` (alias `[refine.overrides]`); set `enabled = false` to skip it.
RuleRefine stays offline (fixed tables, no token re-casing, no network/LLM) and still cannot repair acoustic garble like `chromax`; embedders can swap a custom `RefineBackend` in-process for heavier GEC.
## Themes

Built-in overlay palettes (also listed by `steno theme list`):

| Theme | Role |
| --- | --- |
| `pill` | Default light monochrome chip |
| `mono` | High-contrast monochrome |
| `dusk` | Dark cool palette |
| `dawn` | Warm light palette |
| `contrast` | Strong fg/bg separation |
| `null` / `none` / `off` | No-op overlay (`NullOverlay`) |

Unknown theme names warn and fall back to the `pill` palette (UI is
fail-open). Platform `create` still maps `null|none|off` (and
`overlay = false`) to `NullOverlay`; palette resolution still returns pill
colors for those aliases so shared helpers stay usable.

Optional `[ui.colors]` overrides any palette slot with `#RRGGBB` or
`#RRGGBBAA` (`bg`, `fg`, `border`, `icon_bg`, `icon_fg`, `meta`, `shadow`,
`accent`, `error`). Bad hex fails at config load.

`[ui.stages]` renames the visible copy and controls the recording timer /
pulse. Defaults match the historical hard-coded labels
(`Recording` → `"Transcribing"`, `Transcribing` → `"Processing"`):

```toml
[ui]
theme = "dusk"

[ui.colors]
fg = "#ECECF0"
error = "#FF6B6B"

[ui.stages]
recording = "Listening"
transcribing = "Thinking"
done = "Done"
error = "Error"
show_timer = true
pulse_ms = 180
```

Restart the daemon after theme (or model) changes so the resident process
reloads config: `steno restart`.

## CLI config

Surgical helpers over the same TOML file (`--config` overrides the path):

```console
$ steno config show
$ steno config get ui.theme
$ steno config set ui.theme dusk
$ steno config set max_record_secs 180
$ steno config set api.enabled true
$ steno config set api.token mysecret
$ steno config set type_output true   # only persistent typing arm path

$ steno model list
$ steno model use sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8
$ steno model use ~/models/my-parakeet --provider cpu

$ steno theme list
$ steno theme set dusk
$ steno theme set null                # disable overlay via ui.theme
```

`steno config set` creates the file when missing and validates against exact key names returned by `list_settable_keys()` (top-level: `model_path`, `provider`, `type_output`, `n_threads`, `max_record_secs`; API: `api.enabled`, `api.path`, `api.token`; UI: `ui.theme`, `ui.overlay`, `ui.done_flash_ms`, `ui.stages.recording`, `ui.colors.bg`, etc.), not wildcard patterns (e.g. `ui.*` or `ui.stages.*`).
Typing stays fail-closed: `--type` alone never arms keystroke injection.

Theme and model writes update the file only: restart the daemon for them
to take effect in a running hold-to-talk session.

## Daemon API

When the daemon is running with `[api].enabled` (the default), it listens on a
Unix socket: `$XDG_RUNTIME_DIR/steno/steno.sock`, else `$XDG_CACHE_HOME/steno/steno.sock`,
else `~/.cache/steno/steno.sock`. Override with
`[api].path`. Optional `[api].token` requires every request to carry the same
`token` field.

One JSON object per line (NDJSON). The API never enables typing; `type_output`
in the config file remains the only arming path.

```bash
# ping
printf '%s\n' '{"id":1,"op":"ping"}' | nc -U "$XDG_RUNTIME_DIR/steno/steno.sock"

# status
printf '%s\n' '{"id":2,"op":"status"}' | nc -U "$XDG_RUNTIME_DIR/steno/steno.sock"

# transcribe a WAV (returns {"text":"..."}; does not type)
printf '%s\n' '{"id":3,"op":"transcribe","wav_path":"/path/to/clip.wav"}' \
  | nc -U "$XDG_RUNTIME_DIR/steno/steno.sock"
```

CLI helpers (same socket; optional `--socket`):

```console
$ steno ping
$ steno api status
```

Ops: `ping`, `status`, `transcribe` (`wav_path` **or** `pcm_f32_b64` little-endian
f32 @ 16 kHz mono), `utterance.start` / `utterance.audio` / `utterance.stop` /
`utterance.cancel`, `shutdown`. Streaming utterance returns text only (never
types). `[api].require_same_uid` defaults true (SO_PEERCRED).

## How it works

```text
mic ── capture (cpal/ALSA)
    ── resample to 16 kHz mono, DC-block, gain-normalize, trim silence
    ── sherpa-onnx decode on the GPU (Parakeet TDT)
    ── voice commands → refinement (GEC + vocabulary) → formatting
    ── streamed to stdout, or synthetic keystrokes (xdotool/wtype, when armed)
```

One-shot mode ends on an energy-VAD endpoint (silence after speech). Daemon
mode ends when you release Caps Lock. Either way each utterance gets a
fresh decode state: nothing leaks between them.

## Notes and limits

- Typing sends keystrokes to the **focused** window. That is the feature;
  it is also why you should not steno while a password field is focused.
  It is armed only via `type_output = true` in your config, never from a
  CLI flag (see above).
- **No live-session testing on the operator workstation.** Agents and local
  runs must not start/restart the daemon against the logged-in desktop, grab
  Caps Lock, inject keystrokes, or run GPU soaks here. Hotkey / typing /
  overlay / soak verification belongs on **axiomexec** (Tailscale) or a
  disposable VM (e.g. Firecracker) only. Unit tests stay off the live session.
- If no speech starts within `start_timeout_secs`, `steno` exits non-zero
  with an error, so scripts can tell silence apart from an empty result.
- Typing: X11/XWayland uses `xdotool`; pure Wayland (`WAYLAND_DISPLAY` without `DISPLAY`) uses `wtype` (optional `ydotool` fallback). Install with `sudo apt install wtype`. Caps Lock hotkey still needs `DISPLAY` (XWayland); otherwise the daemon errors with corrective actions. Overlay on pure Wayland is a no-op until layer-shell lands; use stdout mode or XWayland for the pill.
- X11 connection: `steno` connects via the filesystem socket in
  `/tmp/.X11-unix/` first, then falls back to the abstract Unix socket
  (`\0/tmp/.X11-unix/Xn`) used by GDM/GNOME XWayland sessions that do not
  create a filesystem socket. `XAUTHORITY` must be set (or
  `~/.Xauthority` must exist) for MIT-MAGIC-COOKIE authentication.
- Caps Lock grab: if another application (GNOME custom shortcut, sxhkd,
  KDE, or a custom window manager) already owns Caps Lock, the daemon
  errors with a corrective hint. The grab uses SYNC mode with no keymap
  modification, so `kill -9` or a crash leaves Caps Lock working
  instantly. Older builds that remapped to NoSymbol may have left
  damage; `steno stop` or `xmodmap -e 'keycode 66 = Caps_Lock'` repairs
  that. The SYNC grab freezes the whole keyboard until the daemon calls
  `XAllowEvents`; a dedicated thread owns that connection and runs no
  other work, so the freeze lasts milliseconds even while the daemon is
  transcribing or typing.
- Parakeet TDT v3 covers 25 languages and detects them on its own; there
  is no language flag.
