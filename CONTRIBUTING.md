# Contributing to steno

## Build

Install a Rust toolchain (1.85+) and the sherpa-onnx shared libraries.
Set `SHERPA_ONNX_LIB_DIR` to the lib directory before building.

```bash
export SHERPA_ONNX_LIB_DIR=/usr/local/lib/sherpa-onnx
cargo build
cargo test
```

For the CPU CI gate locally:

```bash
./scripts/ci-cpu.sh
```

This downloads CPU sherpa-onnx libs, runs unit tests and clippy with
default, `llm`, and `wayland` features, and refuses CUDA/TensorRT libs.

## Layout

- `crates/steno-core` — embeddable engine: audio, DSP, STT, text pipeline,
  config, API types. No OS or CLI dependencies.
- `crates/steno-platform` — OS backends: hotkey, overlay, typing for
  Linux (X11 + Wayland), Windows, macOS.
- `crates/steno` — CLI and daemon binary.

## Tests

Tests prove observable behavior. Write assertions against properties that
must always hold, not against source text or incidental output. Every test
must fail on a plausible bug. Match existing conventions; keep tests
deterministic and isolated.

The LLM smoke test (`llm_smoke_refine`) skips gracefully when no GGUF
model is present. Set `STENO_LLM_MODEL` to point it at a specific model.

## Commits

One concern per commit. Stage only the paths you changed, by name. Do not
use `git add -A`. Commit messages start with a lowercase summary
(`ci:`, `fix:`, `docs:`, `feat:`).

## CI

Three workflows run on push and pull requests:

- **cpu-ci**: unit tests + clippy with default, `llm`, and `wayland`
  features, across stable/beta/1.85.
- **cross-compile**: `cargo check` + clippy for Windows and macOS targets.
- **release-check**: `cargo publish --dry-run` for each crate.

CUDA, Metal, and Vulkan LLM features are not tested in CI (they require
GPU hardware). Build and test those locally before pushing changes to
LLM code.
