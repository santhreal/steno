# light-dictate

Minimal, fully offline speech-to-text dictation for Linux. Speak; text comes
out. No cloud. Default decode uses CUDA; set `provider = "cpu"` for CPU-only
hosts. One-shot or a background daemon.

`dictate` records from your microphone, transcribes locally with
[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) (Parakeet TDT), cleans
the text up, and prints it — or types it into whatever window is focused.

Use it one-shot (`dictate`) or as a system-wide daemon (`dictate start`) that
keeps the model loaded and listens for **Caps Lock** (hold to talk).

```
$ dictate
This is a test of the Dictate System.
The quick brown fox jumps over the lazy dog?

$ dictate meeting-note.wav
The budget is approved, see attached.
```

## Build

You need a Rust toolchain, a C compiler, and the sherpa-onnx CUDA shared
libraries (or a CPU build) available at build time via
`SHERPA_ONNX_LIB_DIR` (e.g. `/usr/local/lib/sherpa-onnx`).

```
export SHERPA_ONNX_LIB_DIR=/usr/local/lib/sherpa-onnx
cargo build -p dictate --release
cargo install --path crates/dictate
```

Workspace crates: `dictate-core` (embeddable engine), `dictate-platform`
(OS backends), `dictate` (CLI/daemon binary).

There is no cargo `--features cuda` flag — pick the execution provider in
config (`provider = "cuda"` default, or `"cpu"`). CUDA builds still need the
system CUDA/cuDNN install the sherpa libs were built against. Unknown
provider values fail closed (no silent fallback).

## Get a model

`dictate` uses a sherpa-onnx **model directory** (encoder/decoder/joiner
ONNX + `tokens.txt`). Recommended: NVIDIA Parakeet TDT v3 int8.

```
mkdir -p ~/.local/share/dictate/models
cd ~/.local/share/dictate/models
curl -LO https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2
tar xjf sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2
```

| Model dir | Size | Note |
|---|---|---|
| `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8` | ~600 MB | Default; multilingual; GPU-fast |

When `--model` is not set, `dictate` picks the single model directory under
`~/.local/share/dictate/models`. With several, set `model_path` in the
config (or pass `--model /path/to/model-dir`).

## Use it

**Daemon (recommended for daily use).** Arm typing once, then start the
resident model:

```toml
# ~/.config/dictate/config.toml
type_output = true
model_path = "~/.local/share/dictate/models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
```

```
$ dictate start
Dictation running (PID 12345).
Hotkey: hold Caps Lock to speak.
Log: /home/you/.cache/dictate/dictate.log

$ dictate status
Dictation running (PID 12345).
Hotkey: hold Caps Lock to speak.

$ dictate stop
Dictation stopped.
```

Hold **Caps Lock**, speak, release. The daemon already has the model in
memory, so there is no cold-start per utterance. `dictate restart` bounces
it; `dictate start --foreground` runs in the terminal for debugging.
Make sure no other app (GNOME custom shortcut, etc.) already owns
Caps Lock.

**Record and print (one-shot).** Run `dictate`, speak, pause. Recording
stops after about a second of silence (configurable). Text streams to
stdout segment by segment as it is decoded, so it composes:
`dictate | xclip -selection clipboard`.

**Record and type (one-shot).** Typing is fail-closed: it works only after
you arm it once in `~/.config/dictate/config.toml` (`type_output = true`).
Then `dictate` (or `dictate --type`) types into the focused window via
`xdotool` (X11; `sudo apt install xdotool`), and `dictate --stdout`
prints instead for one run. Typed text streams as it is decoded; the
clipboard is never touched.

**Status pill.** A tiny animated monochrome pill at the bottom center of
your primary monitor shows the stage: Transcribing (waveform + timer),
Processing (spinner), Done (check). It takes no focus and no input, and
hides itself after Done. Disable it with `[ui] overlay = false`.

A bare `dictate --type` without the config entry fails with an error —
typing is deliberately not enableable from the command line, so no script,
test, or agent can inject keystrokes into your session unless you armed it
yourself. Control characters other than newline are stripped before
typing, so a transcript can never smuggle Tab or Escape keystrokes into
the target.

**Transcribe a file.** `dictate clip.wav` reads a WAV instead of recording —
any PCM or 32-bit float WAV, resampled to 16 kHz mono internally —
also useful for testing your setup without a microphone.

Useful flags: `--list-devices` and `--device <name>` pick a microphone,
`--raw` skips all text processing,
`--model`/`--config <path>` override auto-resolution,
`-v`/`-vv` shows what the pipeline is doing.

One-shot invocations load the model from disk each time (a few seconds of
startup). `dictate start` keeps the model resident so hold-to-talk skips
that cost. Smaller models start faster either way.

## Voice commands

Spoken commands are replaced inline. `dictate --list-commands` prints this
table:

| Say | Get |
|---|---|
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

## Dictionary

The dictionary rewrites phrases after commands run — names, jargon, product
terms the recognizer gets wrong. Put overrides in the same config file under
`[dict.overrides]`:

```toml
# ~/.config/dictate/config.toml
[dict.overrides]
"handy" = "Dictate"
"main street" = "Main Street"
"um" = ""                # empty replacement deletes the phrase
```

Matching is case-insensitive and whole-word; longer phrases win. The
replacement's case is used exactly as written.

If you still have a legacy `~/.config/dictate/dictionary.toml`, it is
imported into memory once when `[dict.overrides]` is empty (with a
deprecation warning). Copy the entries under `[dict.overrides]` and remove
the old file; `dictate` never rewrites your config for you. Restart the
daemon after edits (`dictate restart`).

## Configuration

Everything has a default; `~/.config/dictate/config.toml` overrides it; CLI
flags override the file (`--config <path>` loads a different file). A full
config looks like:

```toml
model_path = "~/.local/share/dictate/models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
n_threads = 8            # CPU threads for feature extraction; default: half your CPUs
max_record_secs = 120    # hard cap per recording
type_output = false      # arm typing (xdotool); the ONLY way to enable it
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
enabled = true         # post-format offline ASR cleanup (default on)
backend = "rules"      # RuleRefine; unknown names warn and use rules

[ui]
overlay = true         # bottom-center status pill (X11)
done_flash_ms = 1200    # how long done/error stays visible
theme = "pill"         # "pill" or "null"/"none"/"off"

[api]
enabled = true         # daemon listens on a local Unix socket
# path = ""            # empty → $XDG_RUNTIME_DIR/dictate/dictate.sock
# token = ""           # optional shared secret on each request

[dict.overrides]
"handy" = "Dictate"
"main street" = "Main Street"
"um" = ""
```

Post-format **refine** (`[refine]`) collapses duplicate words, fixes a tiny
set of spaced contractions / common ASR glitches, and strips space-before
punctuation. Config knobs are only `enabled` and `backend = "rules"`; set
`enabled = false` to skip it. RuleRefine stays offline (tiny fixed tables, no
token re-casing, no network/LLM) — embedders can swap a custom
`RefineBackend` in-process.

## Daemon API

When the daemon is running with `[api].enabled` (the default), it listens on a
Unix socket — `$XDG_RUNTIME_DIR/dictate/dictate.sock`, or
`~/.cache/dictate/dictate.sock` if `XDG_RUNTIME_DIR` is unset. Override with
`[api].path`. Optional `[api].token` requires every request to carry the same
`token` field.

One JSON object per line (NDJSON). The API never enables typing; `type_output`
in the config file remains the only arming path.

```bash
# ping
printf '%s\n' '{"id":1,"op":"ping"}' | nc -U "$XDG_RUNTIME_DIR/dictate/dictate.sock"

# status
printf '%s\n' '{"id":2,"op":"status"}' | nc -U "$XDG_RUNTIME_DIR/dictate/dictate.sock"

# transcribe a WAV (returns {"text":"..."}; does not type)
printf '%s\n' '{"id":3,"op":"transcribe","wav_path":"/path/to/clip.wav"}' \
  | nc -U "$XDG_RUNTIME_DIR/dictate/dictate.sock"
```

CLI helpers (same socket; optional `--socket`):

```
$ dictate ping
$ dictate api status
```

Ops: `ping`, `status`, `transcribe` (`wav_path` **or** `pcm_f32_b64` little-endian
f32 @ 16 kHz mono), `utterance.start` / `utterance.audio` / `utterance.stop` /
`utterance.cancel`, `shutdown`. Streaming utterance returns text only (never
types). `[api].require_same_uid` defaults true (SO_PEERCRED).

## How it works


```
mic ── capture (cpal/ALSA)
    ── resample to 16 kHz mono, DC-block, gain-normalize, trim silence
    ── sherpa-onnx decode on the GPU (Parakeet TDT)
    ── voice commands → dictionary → formatter → refine
    ── streamed to stdout, or synthetic keystrokes (xdotool, when armed)
```

One-shot mode ends on an energy-VAD endpoint (silence after speech). Daemon
mode ends when you release Caps Lock. Either way each utterance gets a
fresh decode state — nothing leaks between them.

## Notes and limits

- Typing sends keystrokes to the **focused** window. That is the feature;
  it is also why you should not dictate while a password field is focused.
  It is armed only via `type_output = true` in your config — never from a
  CLI flag (see above).
- **No live-session testing on the operator workstation.** Agents and local
  runs must not start/restart the daemon against the logged-in desktop, grab
  Caps Lock, inject keystrokes, or run GPU soaks here. Hotkey / typing /
  overlay / soak verification belongs on **axiomexec** (Tailscale) or a
  disposable VM (e.g. Firecracker) only. Unit tests stay off the live session.
- If no speech starts within `start_timeout_secs`, `dictate` exits non-zero
  with an error, so scripts can tell silence apart from an empty result.
- X11 only for typing (xdotool). On Wayland, use stdout mode with your
  compositor's tooling.
- Parakeet TDT v3 covers 25 languages and detects them on its own; there
  is no language flag.
