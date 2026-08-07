# Windows overlay summary

## Delivered
- `crates/dictate-platform/src/windows.rs`: replaced NullOverlay-only path with a layered topmost HWND status chip (`WS_EX_LAYERED` + `UpdateLayeredWindow`, tiny-skia/fontdue). Implements `OverlayBackend` stages (`Hidden` / `Recording` / `Transcribing` / `Done` / `Error`) with Linux-matching labels (`Recording`→"Transcribing", `Transcribing`→"Processing"). Basic icon animation (wave / spinner / check / x). Fail-open on spawn/HWND/font/GDI errors.
- `create(&UiConfig)`: real `Overlay` when `overlay=true` and theme not `null|none|off`; those cases still return `NullOverlay`.
- `crates/dictate-platform/Cargo.toml`: added windows-only `Win32_Graphics_Gdi` feature. No `lib.rs` changes.
- `docs/PLATFORM_TRAITS.md`: Windows overlay subsection + verification row updated; visual delta vs Linux pill documented.

## Visual delta vs Linux X11 pill
Simplified always-on-top rounded chip — not pixel-perfect. Flat offset shadow only (no soft CSS blur), no recording timer meta, no stage-change scale pulse, no DPI scale beyond primary work-area placement.

## Verification
- `cargo check -p dictate-platform`: green on Linux host.
- Windows-target typecheck via scratch crate (`x86_64-pc-windows-gnu`): green after Send wrapper / encode_utf16 / unwrap_or_else fixes.
- No commit; no local live UI test (per acceptance).

## Coordination
- MacOverlay shipped NSPanel chip separately; IRC closed (message cap). No further shared-file edits from either side.
