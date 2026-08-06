# light-dictate

Minimal, fully offline speech-to-text dictation for Linux. Speak; text comes
out. No cloud, no GPU required. One-shot or a background daemon.

`dictate` records from your microphone, transcribes locally with
[whisper.cpp](https://github.com/ggml-org/whisper.cpp), cleans the text up,
and prints it — or types it into whatever window is focused.

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

You need a Rust toolchain, a C compiler, and cmake (for whisper.cpp).

```
cargo build --release
# binary: target/release/dictate
cargo install --path .   # or: install `dictate` into ~/.cargo/bin
```

NVIDIA GPU acceleration is a compile-time feature (CUDA toolkit required):

```
cargo build --release --features cuda
```

## Get a model

`dictate` uses any ggml whisper model and never downloads anything itself.
One curl command is all it takes:

```
mkdir -p ~/.local/share/dictate/models
curl -L -o ~/.local/share/dictate/models/ggml-small.en.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin
```

Which model is a speed/accuracy tradeoff:

| Model | Size | Note |
|---|---|---|
| `ggml-base.en.bin` | 142 MB | Fast; English only |
| `ggml-small.en.bin` | 466 MB | Good default; English only |
| `ggml-medium.en.bin` | 1.5 GB | More accurate, slower |
| `ggml-large-v3-turbo.bin` | 1.6 GB | Best accuracy; multilingual |

`dictate` looks in `~/.local/share/dictate/models` when `--model` is not
passed and picks the first `*.bin` alphabetically. With one model there is
nothing to configure; with several, pass `--model /path/to/model.bin` (or
set `model_path` in the config) to choose.

## Use it

**Daemon (recommended for daily use).** Arm typing once, then start the
resident model:

```toml
# ~/.config/dictate/config.toml
type_output = true
model_path = "~/.local/share/dictate/models/ggml-small.en.bin"
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
`--language en` skips language detection, `--raw` skips all text processing,
`--model`/`--dictionary`/`--config <path>` override auto-resolution,
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

Commands match whole words only. whisper often adds its own punctuation
around spoken commands ("bank, comma,"); duplicate punctuation is collapsed
during formatting, so you get "bank, " and not "bank,, ".

## Dictionary

The dictionary rewrites phrases after commands run — names, jargon, product
terms whisper gets wrong. Create `~/.config/dictate/dictionary.toml`
(picked up automatically when it exists; `--dictionary <path>` points
elsewhere):

```toml
[overrides]
"handy" = "Dictate"
"main street" = "Main Street"
"um" = ""                # empty replacement deletes the phrase
```

Matching is case-insensitive and whole-word; longer phrases win. The
replacement's case is used exactly as written.

## Configuration

Everything has a default; `~/.config/dictate/config.toml` overrides it; CLI
flags override the file (`--config <path>` loads a different file). A full
config looks like:

```toml
model_path = "~/.local/share/dictate/models/ggml-small.en.bin"
dictionary_path = "~/.config/dictate/dictionary.toml"
language = "auto"        # or "en", "de", ...
n_threads = 8            # decode threads; default: half your CPUs
max_record_secs = 120    # hard cap per recording
type_output = false      # arm typing (xdotool); the ONLY way to enable it

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

[ui]
overlay = true         # bottom-center status pill (X11)
done_flash_ms = 1200    # how long done/error stays visible
```

## How it works

```
mic ── capture (cpal/ALSA)
    ── resample to 16 kHz mono, DC-block, gain-normalize, trim silence
    ── whisper.cpp decode (greedy, temperature fallback)
    ── voice commands → dictionary → formatter, per decoded segment
    ── streamed to stdout, or synthetic keystrokes (xdotool, when armed)
```

One-shot mode ends on an energy-VAD endpoint (silence after speech). Daemon
mode ends when you release Caps Lock. Either way each utterance gets a
fresh decode state — nothing leaks between them.

## Notes and limits

- Typing sends keystrokes to the **focused** window. That is the feature;
  it is also why you should not dictate while a password field is focused.
  It is armed only via `type_output = true` in your config — never from a
  CLI flag (see above). The test suite never types; real keystroke e2e
  belongs in a disposable microVM (e.g. Firecracker), not a live desktop.
- If no speech starts within `start_timeout_secs`, `dictate` exits non-zero
  with an error, so scripts can tell silence apart from an empty result.
- X11 only for typing (xdotool). On Wayland, use stdout mode with your
  compositor's tooling.
- English `.en` models ignore `language`; multilingual models need it
  (`--language de`) or auto-detection kicks in.
