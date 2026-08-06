# dictate

Minimal, fully offline speech-to-text dictation for Linux. Speak; text comes
out. No daemon, no cloud, no GPU required.

`dictate` records one utterance from your microphone, stops by itself when you
finish speaking, transcribes it locally with
[whisper.cpp](https://github.com/ggml-org/whisper.cpp), cleans the text up,
and prints it — or types it into whatever window is focused.

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

With exactly one `*.bin` in the models directory, `dictate` finds it
automatically. Otherwise pass `--model /path/to/model.bin`.

## Use it

**Record and print.** Run `dictate`, speak, pause. Recording stops after
about a second of silence (configurable). The text is printed, so it
composes: `dictate | xclip -selection clipboard`.

**Record and type.** `dictate --type` types the result into the currently
focused window via `xdotool` (X11). Bind it to a global shortcut for real
dictation: GNOME Settings → Keyboard → Custom Shortcuts, command
`/path/to/dictate --type`, e.g. Ctrl+Space. Click into any text field, press
the shortcut, speak.

**Transcribe a file.** `dictate clip.wav` reads a WAV instead of recording —
also useful for testing your setup without a microphone.

Useful flags: `--list-devices` and `--device <name>` pick a microphone,
`--language en` skips language detection, `--raw` skips all text processing,
`-v`/`-vv` shows what the pipeline is doing.

## Voice commands

Spoken commands are replaced inline. `dictate --list-commands` prints this
table:

| Say | Get |
|---|---|
| period / full stop | `.` |
| comma | `,` |
| question mark | `?` |
| exclamation mark / point | `!` |
| colon / semicolon | `:` / `;` |
| ellipsis / dot dot dot | `…` |
| open quote / close quote | `"` |
| open paren / close paren | `(` `)` |
| percent sign / dollar sign | `%` `$` |
| new line | line break |
| new paragraph | blank line |
| scratch that / delete that | deletes back to the last sentence boundary |

Commands match whole words only. whisper often adds its own punctuation
around spoken commands ("bank, comma,"); duplicate punctuation is collapsed
during formatting, so you get "bank, " and not "bank,, ".

## Dictionary

The dictionary rewrites phrases after commands run — names, jargon, product
terms whisper gets wrong. Create `~/.config/dictate/dictionary.toml`:

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
flags override the file. A full config looks like:

```toml
model_path = "~/.local/share/dictate/models/ggml-small.en.bin"
dictionary_path = "~/.config/dictate/dictionary.toml"
language = "auto"        # or "en", "de", ...
n_threads = 8            # decode threads; default: half your CPUs
max_record_secs = 120    # hard cap per recording
type_output = false      # --type by default

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
```

## How it works

```
mic ── capture (cpal/ALSA)
    ── resample to 16 kHz mono, DC-block, gain-normalize, trim silence
    ── whisper.cpp decode (greedy, temperature fallback)
    ── voice commands → dictionary → formatter
    ── stdout, or synthetic keystrokes (xdotool --type)
```

Recording ends on an energy-VAD endpoint (silence after speech), so there is
no button to press twice and no daemon to babysit. Each invocation is a
fresh process: state never leaks between utterances.

## Notes and limits

- `--type` sends keystrokes to the **focused** window. That is the feature;
  it is also why you should not run it while a password field is focused.
- X11 only for `--type` (xdotool). On Wayland, use stdout mode with your
  compositor's tooling.
- English `.en` models ignore `language`; multilingual models need it
  (`--language de`) or auto-detection kicks in.
