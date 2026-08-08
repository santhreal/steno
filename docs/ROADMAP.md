# Roadmap (execution order)

## Phase 1 - land in-tree (agents)
- [x] Single config (`[dict.overrides]` + legacy dictionary.toml import)
- [x] IPC protocol + Unix socket skeleton
- [x] OverlayBackend trait + NullOverlay + theme field

## Phase 2 - integrate
- [x] Wire API server into daemon (api.enabled)
- [x] `api.require_same_uid` + SO_PEERCRED gate (default true)
- [x] Migrate call sites to Box<dyn OverlayBackend>
- [x] Merge user dictionary.toml into config.toml on disk (operator step; in-memory import + docs; `steno` never rewrites the file)
- [x] `steno ping` / `steno api status` CLI helpers

## Phase 3 - workspace + embed API
- [x] Split steno-core / steno-platform / steno
- [x] Engine + Session public API
- [x] Platform traits; Linux X11 behind them
- [x] Null backends for headless

## Phase 4 - cross-platform
- [x] Windows: Caps Lock hotkey (`WH_KEYBOARD_LL`) + `SendInput` typing + layered HWND status chip
- [x] macOS: Caps Lock hotkey (`CGEventTap`) + `CGEvent` typing + NSPanel status chip
- [x] `provider = cuda|cpu` honored end-to-end (Config / Engine / Transcriber / daemon; fail-closed)
- [x] CPU CI job (`.github/workflows/ci-cpu.yml` + `scripts/ci-cpu.sh`; unit/clippy vs CPU sherpa)

## Phase 5 - harden
- [x] RuleRefine post-STT + RefineBackend hook (offline; expanded ASR/grammar tables; not full LLM GEC)
- [x] GPU soak (100× en.wav) + nvidia-smi memory delta - **axiomexec / disposable VM only** (not operator workstation)
- [x] Socket framing fuzz / partial lines (`api::server` framing_* tests)
- [x] Caps Lock restore Drop helpers + SIGKILL recovery docs (keycode 66)
- [x] axiomexec remote Xvfb verify: **PASS** on `axiomexec@192.168.0.135` (Tailscale SSH still interactive-auth blocked)
- [x] `utterance.*` streaming API (DaemonHandler text-only stop + `Event::UtteranceDone`)
- [x] README + EMBEDDING.md synced for single config / refine / API / embed hooks
- [x] Theme palettes (`pill|mono|dusk|dawn|contrast`) + `[ui.colors]` / `[ui.stages]` + `resolve_ui` in steno-core; platforms consume `ResolvedUi`
- [x] CLI `steno config show|get|set`, `steno model list|use [--provider]`, `steno theme list|set`
- [x] Configurable stage labels (`[ui.stages]`; defaults Transcribing/Processing/Done/Error)
- [x] Library polish: `Engine::{from_parts,with_pipeline,process_text,load_model}`, path helper re-exports, EMBEDDING rewrite

## Policy (standing)

No live-session testing on the operator workstation. Hotkey / typing /
overlay / soak verification only on axiomexec (LAN or Tailscale) or a disposable VM.

## Phase 6 - polish / platform depth
- [x] Honor `XDG_CACHE_HOME` for daemon pid/ready/log + API socket fallback
- [x] Win/mac recording timer via `[ui.stages].show_timer` (Win chip + macOS label)
- [x] Wayland MVP: runtime selection + `wtype` typing (+ `ydotool` fallback); hotkey via XWayland/X11 when `DISPLAY` set (pure Wayland fails loudly); overlay still NullOverlay + warn (layer-shell follow-up); X11 remains primary when `DISPLAY` works
- [x] Stronger offline RuleRefine GEC (expanded ASR/agreement/a-an/contractions/trailing fillers; still no LLM; RuleRefine stays default)
- [x] Win/mac overlay closer to Linux pill (soft `box_blur_alpha` shadow; macOS tiny-skia chip + icons; not pixel-perfect)
- [x] Phase 6 remote Xvfb re-verify: **PASS** on `axiomexec` (0057c06; XDG_CACHE_HOME, API socket, PTT hotkey, ping)

## Phase 7 - platform depth & extensions (future)
- [ ] Wayland native status overlay (`zwlr-layer-shell-v1` status pill)
- [ ] Wayland native global hotkey (XDG Global Shortcuts portal)
- [ ] Windows native named pipe IPC backend for daemon API (`\\.\pipe\steno`)
- [ ] High-DPI awareness & scale factor support for Windows HWND & macOS NSPanel overlays
- [ ] macOS Metal execution provider support in `stt.rs` (`provider = "metal"`)
- [ ] External / LLM `RefineBackend` plugin integration
- [ ] Daemon supervisor / auto-restart on unhandled panics + socket/pidfile cleanup
- [ ] Native audio capture failover / re-initialization on device disconnect

