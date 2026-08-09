#!/usr/bin/env bash
# CPU CI gate for steno.
#
# What this runs (honest scope):
#   - fetch/link against CPU sherpa-onnx shared libs (never CUDA)
#   - cargo test -p steno-core --lib
#   - cargo test -p steno-platform --lib
#   - cargo clippy -p steno-core -p steno-platform -p steno --all-targets -- -D warnings
#
# What this never does:
#   - start the steno daemon
#   - touch DISPLAY / GNOME / live X
#   - GPU soak / nvidia-smi
#   - download ASR models or run e2e decode
#
# Provider coverage comes from unit tests in steno-core (config + Transcriber
# fail-closed validation for cpu|cuda). Linking uses the CPU shared archive
# that sherpa-onnx-sys would auto-download for linux x86_64 + `shared`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SHERPA_VER="${SHERPA_VER:-1.13.4}"
ARCHIVE="sherpa-onnx-v${SHERPA_VER}-linux-x64-shared-lib.tar.bz2"
URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/v${SHERPA_VER}/${ARCHIVE}"
CACHE_ROOT="${SHERPA_ONNX_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/steno/sherpa-onnx-cpu}"
EXTRACTED="${CACHE_ROOT}/sherpa-onnx-v${SHERPA_VER}-linux-x64-shared-lib"
LIB_DIR="${EXTRACTED}/lib"

mkdir -p "$CACHE_ROOT"

if [[ ! -d "$LIB_DIR" ]]; then
  archive_path="${CACHE_ROOT}/${ARCHIVE}"
  if [[ ! -f "$archive_path" ]]; then
    echo "downloading CPU sherpa-onnx shared libs: $URL"
    curl -fsSL -o "${archive_path}.partial" "$URL"
    mv "${archive_path}.partial" "$archive_path"
  fi
  echo "extracting $archive_path -> $CACHE_ROOT"
  tar -xjf "$archive_path" -C "$CACHE_ROOT"
fi

if [[ ! -d "$LIB_DIR" ]]; then
  echo "error: expected lib dir missing after extract: $LIB_DIR" >&2
  exit 1
fi

# Refuse accidental CUDA provider libs (host installs often put these in
# /usr/local/lib/sherpa-onnx). CPU CI must stay CPU-only.
if compgen -G "${LIB_DIR}/libonnxruntime_providers_cuda*" >/dev/null \
  || compgen -G "${LIB_DIR}/libonnxruntime_providers_tensorrt*" >/dev/null; then
  echo "error: refusing CUDA/TensorRT sherpa libs under $LIB_DIR" >&2
  exit 1
fi

export SHERPA_ONNX_LIB_DIR="$LIB_DIR"
export LD_LIBRARY_PATH="${LIB_DIR}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# Keep the gate headless even if the operator session has a display.
unset DISPLAY WAYLAND_DISPLAY

echo "SHERPA_ONNX_LIB_DIR=$SHERPA_ONNX_LIB_DIR"
echo "running CPU unit/clippy gate (no daemon, no GPU, no overlay X)"

cargo test -p steno-core --lib
cargo test -p steno-platform --lib
cargo test -p steno
cargo clippy -p steno-core -p steno-platform -p steno --all-targets -- -D warnings

echo "ci-cpu: OK"
