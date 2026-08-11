# Releasing steno

## Prerequisites

- Working tree clean, on `master`.
- All CI workflows green.
- `cargo test` and `cargo clippy -- -D warnings` pass locally with
  default, `llm`, and `wayland` features.

## Steps

1. Bump the workspace version in `Cargo.toml` (`[workspace.package]`).
2. Update `CHANGELOG.md` with the release date under the version heading.
3. Commit: `release: vX.Y.Z`.
4. Tag: `git tag vX.Y.Z`.
5. Build and verify the binary:
   ```bash
   cargo build -p steno --release
   ./target/release/steno --help
   ```
6. Publish crates in dependency order:
   ```bash
   cargo publish -p steno-core
   cargo publish -p steno-platform
   cargo publish -p steno
   ```
7. Push the tag: `git push origin master --tags`.

## Notes

- `steno-core` depends on `sherpa-onnx` which requires
  `SHERPA_ONNX_LIB_DIR` at build time. This is a known limitation for
  crates.io consumers; document it in the crate README.
- The `llm`, `llm-cuda`, `llm-vulkan`, `llm-metal`, and `wayland`
  features are optional and do not affect the default build.
- Binary release artifacts (`.deb`, `.rpm`, MSI, Homebrew formula) are
  not yet automated. Build from source or `cargo install` for now.
