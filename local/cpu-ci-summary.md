# CPU CI summary

## What landed
- `.github/workflows/ci-cpu.yml` — ubuntu-latest job: apt build deps (ALSA + X11 headers only), stable Rust + clippy, cache, then `./scripts/ci-cpu.sh`.
- `scripts/ci-cpu.sh` — downloads CPU sherpa-onnx `linux-x64-shared-lib` v1.13.4 (~9.5 MB), refuses CUDA/TensorRT provider `.so`s, unsets `DISPLAY`/`WAYLAND_DISPLAY`, runs:
  - `cargo test -p dictate-core --lib`
  - `cargo test -p dictate-platform --lib`
  - `cargo clippy -p dictate-core -p dictate-platform -p dictate --all-targets -- -D warnings`
- Docs: ROADMAP Phase 4 CPU CI checkbox checked; README Build section + ARCHITECTURE provider row note the gate.

## Sherpa libs
- Crate: `sherpa-onnx` 1.13.4 with `shared` (via `dictate-core`).
- Link path: `SHERPA_ONNX_LIB_DIR` or auto-download of `sherpa-onnx-v1.13.4-linux-x64-shared-lib.tar.bz2` (CPU; separate from `*-gpu*` / CUDA archives).
- CI/script sets `SHERPA_ONNX_LIB_DIR` to the extracted CPU `lib/` (onnxruntime + sherpa c/cxx api only).

## Verification
- Ran `SHERPA_ONNX_CACHE_DIR=/tmp/sherpa-cpu-probe ./scripts/ci-cpu.sh` against real CPU shared libs.
- Result: dictate-core 186 tests OK; dictate-platform 19 tests OK; clippy `-D warnings` OK; finished in ~5s warm (well under 10 min cold budget).
- Provider unit coverage exercised: `provider_defaults_to_cuda`, `provider_cuda_and_cpu_parse`, `unknown_provider_is_rejected`, `load_rejects_unknown_provider_before_model_io`, `load_accepts_cpu_and_cuda_provider_strings`.

## Honesty / non-goals
- No daemon start, no GNOME/live X, no GPU soak, no ASR model download, no e2e decode.
- Clippy `--all-targets` compiles `dictate` integration/e2e targets but does not execute them.
- Not committed (Main commits).
