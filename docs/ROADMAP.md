# Roadmap (execution order)

## Phase 1 — land in-tree (agents)
- [ ] Single config (`[dict.overrides]` + legacy dictionary.toml import)
- [ ] IPC protocol + Unix socket skeleton
- [ ] OverlayBackend trait + NullOverlay + theme field

## Phase 2 — integrate
- [ ] Wire API server into daemon (api.enabled)
- [ ] Migrate call sites to Box<dyn OverlayBackend>
- [ ] Merge user dictionary.toml into config.toml on disk (operator step)
- [ ] `dictate api` / `dictate ping` CLI helpers

## Phase 3 — workspace + embed API
- [ ] Split dictate-core / dictate-platform / dictate
- [ ] Engine + Session public API
- [ ] Platform traits; Linux X11 behind them
- [ ] Null backends for headless

## Phase 4 — cross-platform
- [x] Windows stubs compile → SendInput + RegisterHotKey + layered window
- [x] macOS stubs compile → CGEvent + CGEventTap + NSPanel
- [ ] provider = cuda|cpu config; CPU CI path

## Phase 5 — harden
- [ ] GPU soak (100× en.wav) + nvidia-smi memory delta
- [ ] Socket framing fuzz / partial lines
- [ ] Caps Lock restore on SIGKILL documentation + Drop tests
- [ ] axiomexec remote verify (no local live typing)
- [ ] README + EMBEDDING.md finalized
