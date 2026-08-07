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
- [x] Windows stubs compile → SendInput + RegisterHotKey + layered window
- [x] macOS stubs compile → CGEvent + CGEventTap + NSPanel
- [ ] provider = cuda|cpu end-to-end (Config/`Engine`/`Transcriber` done; daemon hot-path must pass `cfg.provider`; CPU CI path still open)

## Phase 5 — harden
- [x] RuleRefine post-STT + RefineBackend hook (offline; not full LLM GEC)

- [ ] GPU soak (100× en.wav) + nvidia-smi memory delta — **axiomexec / disposable VM only**
- [ ] Socket framing fuzz / partial lines
- [ ] Caps Lock restore on SIGKILL documentation + Drop tests
- [ ] axiomexec remote verify (no local live typing)
- [x] `utterance.*` streaming API (DaemonHandler text-only stop + `Event::UtteranceDone`; live path Unverified)
- [ ] README + EMBEDDING.md finalized after Phase 5 proofs

## Policy (standing)

No live-session testing on the operator workstation. Hotkey / typing /
overlay / soak verification only on axiomexec (Tailscale) or a disposable VM.
