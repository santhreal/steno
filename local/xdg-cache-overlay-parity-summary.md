# XDG_CACHE_HOME + overlay timer parity

## Delivered
- `crates/dictate/src/daemon.rs`: `cache_dir()` uses `$XDG_CACHE_HOME/dictate` else `~/.cache/dictate` (pid/ready/log).
- `crates/dictate-core/src/api/server.rs`: `default_socket_path` falls back to `$XDG_CACHE_HOME/dictate/dictate.sock` before `~/.cache/...`.
- Windows chip: recording elapsed timer honors `[ui.stages].show_timer`.
- macOS NSPanel label: appends `m:ss` while Recording when `show_timer`.
- Docs/ROADMAP Phase 6: Wayland + stronger GEC + soft-blur parity still open.

## Verification
- Unit: socket XDG_CACHE fallback, daemon cache_dir, platform 22, core 202.
- Clippy `-D warnings` green on workspace crates.
- No live GNOME daemon restart.
