# Core UI theme summary

## Delivered (dictate-core only)
- Extended `UiConfig` with `colors: UiColors` (optional `#RRGGBB`/`#RRGGBBAA` overrides) and `stages: UiStages` (labels + `show_timer` + `pulse_ms`). Kept `overlay` / `done_flash_ms` / `theme`.
- New `crates/dictate-core/src/ui_theme.rs`: presets `pill|mono|dusk|dawn|contrast`, `parse_rgba`, `resolve_ui`, `stage_label`, `list_themes`, `ThemePalette` / `ResolvedUi` / `Rgba`.
- Unknown theme → `log::warn` + pill palette (fail-open). `null|none|off` still resolve to pill colors for shared helpers; platform `create` continues to map those to NullOverlay.
- Surgical TOML helpers: `config_get` / `config_set` / `list_settable_keys` via `toml_edit`. Supported dotted keys documented on `list_settable_keys` (model_path, provider, type_output, n_threads, ui.theme/overlay/done_flash_ms, ui.stages.*, ui.colors.*). Typing fail-closed semantics untouched.
- Exports from `lib.rs`. Dep: `toml_edit = "0.22"` (+ workspace `Cargo.lock` refresh).

## Defaults preserved
- Bare `[ui]` / omitted `[ui.colors]`/`[ui.stages]` still load.
- Stage defaults: Recording→`Transcribing`, Transcribing→`Processing`, Done→`Done`, Error→`Error`; `show_timer=true`, `pulse_ms=180`.
- Pill RGBA matches current white mock (bg FFFFFF/F0, fg/border/icon 111…, meta 777, shadow 000/1C, error B00020).

## Verification
- `cargo test -p dictate-core --lib`: **199 passed** (incl. 8 `ui_theme::*` + 5 new `config::tests::ui_*` / `config_get*`).
- No platform overlay / CLI / docs edits. No git commit.

## Notes for sibling agents
- Overlay/CLI should call `resolve_ui(&cfg.ui)` + `stage_label(&cfg.ui, stage)` instead of hard-coded colors/labels.
- Bad hex in `[ui.colors]` fails at `Config` load (validate); unknown color field names still deny_unknown.
