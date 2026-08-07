# Overlay themes summary

## Delivered
- Linux/Win/mac overlays call `resolve_ui` once and pass `ResolvedUi` into the render path.
- Hardcoded pill colors → `ThemePalette`; glyphs use `icon_fg`; Error can tint with `colors.error`.
- Stage labels from `resolved.stages` (not static Transcribing/Processing).
- Linux honors `show_timer` + `pulse_ms` (0 disables pulse); Windows honors `pulse_ms`; macOS applies bg/fg/error on NSTextField.
- `create()` still maps `null|none|off` → NullOverlay.

## Verification
- `cargo check/test -p dictate-platform --lib`: 22 passed.
- No dictate-core / CLI edits from this agent.
