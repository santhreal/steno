//! Post-transcription text pipeline, applied in this exact order:
//! 1. voice commands  — "period", "new line", "scratch that", ...
//! 2. dictionary      — multi-word phrase overrides, empty replacement deletes
//! 3. formatting      — sentence capitalization, punctuation spacing
//!
//! Pure string logic; no I/O except dictionary file loading.

mod commands;
mod dictionary;
mod format;

pub use commands::COMMANDS;
pub use dictionary::Dictionary;
pub use format::FmtState;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TextConfig {
    /// Apply the voice command table.
    pub commands: bool,
    /// Apply sentence capitalization and punctuation spacing.
    pub format: bool,
}

impl Default for TextConfig {
    fn default() -> Self {
        Self {
            commands: true,
            format: true,
        }
    }
}

pub struct TextPipeline {
    cfg: TextConfig,
    dict: Dictionary,
}

impl TextPipeline {
    pub fn new(cfg: TextConfig, dict: Dictionary) -> Self {
        Self { cfg, dict }
    }

    /// Streaming: process one decoded segment, carrying formatter state
    /// (capitalization, quote state) across segments — pass the returned
    /// state to the next call. `scratch that` can only delete within the
    /// current segment — earlier segments are already emitted.
    pub fn process_stream(&self, raw: &str, state: format::FmtState) -> (String, format::FmtState) {
        let mut s = raw.to_string();
        if self.cfg.commands {
            s = commands::apply(&s);
        }
        s = self.dict.apply(&s);
        if self.cfg.format {
            format::format_with(&s, state)
        } else {
            (s.trim().to_string(), state)
        }
    }
}
