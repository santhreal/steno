//! Post-transcription text pipeline, applied in this exact order:
//! 1. voice commands  — "period", "new line", "scratch that", ...
//! 2. dictionary      — multi-word phrase overrides, empty replacement deletes
//! 3. formatting      — sentence capitalization, punctuation spacing
//!
//! Pure string logic; no I/O except dictionary file loading.

mod commands;
mod dictionary;
mod format;

pub use commands::{COMMANDS, VoiceCommand};
pub use dictionary::Dictionary;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
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

    /// Raw whisper output → final text. Order is fixed: commands, then
    /// dictionary, then formatting.
    pub fn process(&self, raw: &str) -> String {
        let mut s = raw.to_string();
        if self.cfg.commands {
            s = commands::apply(&s);
        }
        s = self.dict.apply(&s);
        if self.cfg.format {
            s = format::format(&s);
        }
        s.trim().to_string()
    }
}
