# Roadmap (execution order)

## Phase 1 — land in-tree (agents)
- [x] Single config (`[dict.overrides]` + legacy dictionary.toml import)
- [x] IPC protocol + Unix socket skeleton
- [x] OverlayBackend trait + NullOverlay + theme field

## Phase 2 — integrate
- [x] Wire API server into daemon (api.enabled)
- [x] `api.require_same_uid` + SO_PEERCRED gate (default true)
- [x] Migrate call sites to Box<dyn OverlayBackend>
- [x] Merge user dictionary.toml into config.toml on disk (operator step; in-memory import + docs; `dictate` never rewrites the file)
- [x] `dictate ping` / `dictate api status` CLI helpers

## Phase 3 — workspace + embed API
- [x] Split dictate-core / dictate-platform / dictate
- [x] Engine + Session public API
- [x] Platform traits; Linux X11 behind them
- [x] Null backends for headless

## Phase 4 — cross-platform
- [x] Windows: Caps Lock hotkey (`WH_KEYBOARD_LL`) + `SendInput` typing + layered HWND status chip
- [x] macOS: Caps Lock hotkey (`CGEventTap`) + `CGEvent` typing + NSPanel status chip
- [x] `provider = cuda|cpu` honored end-to-end (Config / Engine / Transcriber / daemon; fail-closed)
- [x] CPU CI job (`.github/workflows/ci-cpu.yml` + `scripts/ci-cpu.sh`; unit/clippy vs CPU sherpa)

## Phase 5 — harden
- [x] RuleRefine post-STT + RefineBackend hook (offline; expanded ASR/grammar tables; not full LLM GEC)
- [x] GPU soak (100× en.wav) + nvidia-smi memory delta — **axiomexec / disposable VM only** (not operator workstation)
- [x] Socket framing fuzz / partial lines (`api::server` framing_* tests)
- [x] Caps Lock restore Drop helpers + SIGKILL recovery docs (keycode 66)
- [x] axiomexec remote Xvfb verify: **PASS** on `axiomexec@192.168.0.135` (Tailscale SSH still interactive-auth blocked)
- [x] `utterance.*` streaming API (DaemonHandler text-only stop + `Event::UtteranceDone`)
- [x] README + EMBEDDING.md synced for single config / refine / API / embed hooks
- [x] Theme palettes (`pill|mono|dusk|dawn|contrast`) + `[ui.colors]` / `[ui.stages]` + `resolve_ui` in dictate-core; platforms consume `ResolvedUi`
- [x] CLI `dictate config show|get|set`, `dictate model list|use [--provider]`, `dictate theme list|set`
- [x] Configurable stage labels (`[ui.stages]`; defaults Transcribing/Processing/Done/Error)

## Policy (standing)

No live-session testing on the operator workstation. Hotkey / typing /
overlay / soak verification only on axiomexec (LAN or Tailscale) or a disposable VM.
