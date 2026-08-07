# Library deep-review polish

## Gaps closed (this commit)
- `Engine::load_model`, `from_parts`, `with_pipeline`, `process_text`, accessors
- Removed dead `raw_default` field
- `TextPipeline::process` one-shot helper
- Crate-root re-exports: path helpers, `COMMANDS`, `FmtState`
- `SessionBuilder::from_config` docs clarify overlay is separate
- `ApiConfig.path` docs match XDG_CACHE_HOME fallback
- `docs/EMBEDDING.md` rewritten with Engine composition + library map
- ARCHITECTURE / ROADMAP synced

## Verification
- `cargo test -p dictate-core --lib`: 203 passed
- `cargo test -p dictate-platform --lib`: 22 passed
- clippy `-D warnings` on core/platform/dictate: green
- No live GNOME / daemon restart

## Remaining (honest, deferred Phase 6)
- Wayland hotkey/type/overlay
- Stronger offline GEC behind RefineBackend
- Win/mac soft-blur / full pill visual parity
