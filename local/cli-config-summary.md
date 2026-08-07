# CLI config / model / theme subcommands

## Owned files
- `crates/dictate/src/main.rs` — clap `Command` wiring (`config` / `model` / `theme`), global `--config`, `--type` help documents persistent arming via `dictate config set type_output true`
- `crates/dictate/src/config_cmd.rs` — handlers + unit tests (temp-file set/get roundtrip; unknown key / theme rejection)
- No `Cargo.toml` dep changes; reused existing `dictate_core` helpers

## Subcommands
1. `dictate config show` — effective path, settable keys (effective values), resolved model path, `dict.overrides` count (not the map)
2. `dictate config get <key>` — `config_get`; clear error if unset/missing
3. `dictate config set <key> <value>` — `config_set` on default or `--config`; creates file when missing; refuses unknown keys via `list_settable_keys`
4. `dictate model list` — dirs under `default_model_dir`; marks current `model_path` / resolved
5. `dictate model use <name-or-path>` — bare name joins models dir when present; `expand_tilde`; writes `model_path` (optional `--provider`)
6. `dictate theme list` — `list_themes()` + null|none|off note
7. `dictate theme set <name>` — validates against `list_themes` or null|none|off; sets `ui.theme`

## Typing fail-closed
- No new flag arms typing.
- Persistent path only: `dictate config set type_output true` (documented on `--type` and `config set --help`).
- Note: core currently includes `type_output` in `list_settable_keys` (verified); that is intentional file-edit arming, not a CLI bypass.

## Verification
- `cargo check -p dictate` OK
- unit tests in `config_cmd` OK (set/get roundtrip, unknown key, theme null alias)
- binary help exposes `config` / `model` / `theme`; `theme list` / `config get --help` work
- Did not start/restart daemon; did not edit `dictate-core`
