#!/usr/bin/env bash
# GPU soak: N sequential decodes; print nvidia-smi memory before/after.
# Does NOT touch the display or type anywhere.
set -euo pipefail
N="${1:-20}"
WAV="${2:-$HOME/.local/share/steno/models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/test_wavs/en.wav}"
BIN="${DICTATE_BIN:-$(command -v steno)}"
export SHERPA_ONNX_LIB_DIR="${SHERPA_ONNX_LIB_DIR:-/usr/local/lib/sherpa-onnx}"

if [[ ! -f "$WAV" ]]; then
  echo "missing wav: $WAV" >&2
  exit 1
fi
if [[ ! -x "$BIN" ]]; then
  echo "missing steno binary: $BIN" >&2
  exit 1
fi

mem() { nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits | head -1; }

echo "soak N=$N bin=$BIN wav=$WAV"
before=$(mem)
echo "vram_before_mib=$before"
for i in $(seq 1 "$N"); do
  out=$("$BIN" --stdout "$WAV" 2>/dev/null | tail -1)
  echo "[$i/$N] $out"
done
after=$(mem)
echo "vram_after_mib=$after"
echo "vram_delta_mib=$((after - before))"
