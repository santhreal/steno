# Changelog

All notable changes to steno are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `steno model download [--llm]` subcommand: downloads the default STT
  model (Parakeet TDT v3 int8) and optionally the LLM refine model
  (LFM2.5-2.6B Q4_K_M GGUF) via `curl` + `tar`.
- LLM refine backend (`refine.backend = "llm"`) using llama-cpp-2 with
  CUDA, Vulkan, Metal, and CPU build features.
- `refine.llm.no_think` config flag for Qwen3 reasoning models.
- Abstract Unix socket fallback for X11 connections (GDM/GNOME XWayland).
- Platform-aware `data_dir()`: macOS `~/Library/Application Support`,
  Windows `%LOCALAPPDATA%`.
- CI: feature-gated test and clippy runs for `llm` and `wayland` features.
- CI: cross-compile check for Windows and macOS targets.
- CI: Rust version matrix (stable, beta, 1.85).
- CI: release dry-run workflow (`cargo publish --dry-run`).

### Fixed

- Caps Lock grab no longer blackholes the keyboard. The SYNC passive grab
  freezes the whole keyboard until `XAllowEvents` runs; it was serviced
  from the daemon's main loop, so a Caps Lock press that arrived during
  transcription, LLM refine, or typing held every key hostage until the
  daemon came back — indefinitely if it wedged. A dedicated thread now
  owns the grab connection and unfreezes on every key event before any
  classification.
- Token decode buffer increased from 8 to 64 bytes for multi-byte UTF-8.
- LLM refine prompt truncation now keeps the system prompt (first tokens)
  instead of the last tokens.
- LLM refine `generate()` calls serialized with a mutex for thread safety.
- Hotkey grab moved before model load to prevent sherpa-onnx C++ destructor
  heap corruption on X11 failure.
- X11 display scanning: tries `DISPLAY` first, then scans
  `/tmp/.X11-unix/` for available displays.
- Clippy `field_reassign_with_default` in LLM smoke test.

## [0.1.0] - 2026-01-15

### Added

- Offline speech-to-text dictation CLI powered by sherpa-onnx (Parakeet TDT).
- Caps Lock hold-to-talk daemon with X11, Wayland, Windows, and macOS backends.
- Text pipeline: voice commands, rule-based refinement, formatting.
- Status overlay with themeable palettes.
- Daemon API over Unix socket / Windows named pipe (NDJSON protocol).
- Three-crate workspace: `steno-core`, `steno-platform`, `steno`.
