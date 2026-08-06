//! Post-transcription text pipeline, applied in this exact order:
//! 1. voice commands  — "period", "new line", "scratch that", ...
//! 2. dictionary      — multi-word phrase overrides, empty replacement deletes
//! 3. formatting      — sentence capitalization, punctuation spacing
//!
//! Dictionary replacements are inserted verbatim: the formatter spaces and
//! punctuates around them but never re-cases them, so a lowercase-branded
//! entry ("veyyon") stays lowercase even at a sentence start.
//!
//! Streaming limit: each whisper segment is processed on its own, so a
//! multi-word dictionary phrase split across two segments never matches.
//! Keep overrides to phrases whisper emits within one breath, or make each
//! half its own entry.
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

/// Private-use sentinel pair wrapping each dictionary replacement between
/// stages 2 and 3. The formatter copies marked text through without case
/// transforms and strips the markers; they never reach emitted output.
/// Chosen from the Unicode private use area so real transcripts and
/// dictionary entries cannot contain them.
const VERBATIM_START: char = '\u{E000}';
const VERBATIM_END: char = '\u{E001}';

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
        if self.cfg.format {
            // Marked replacements let the formatter protect their case.
            s = self.dict.apply_marked(&s);
            format::format_with(&s, state)
        } else {
            s = self.dict.apply(&s);
            (s.trim().to_string(), state)
        }
    }
}
