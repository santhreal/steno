//! Post-transcription text pipeline, applied in this exact order:
//! 1. voice commands  — "period", "new line", "scratch that", ...
//! 2. dictionary      — multi-word phrase overrides, empty replacement deletes
//! 3. formatting      — sentence capitalization, punctuation spacing
//! 4. refine — offline ASR cleanup (duplicate words, spaced
//!    contractions, space-before-punct); default on
//!
//! Dictionary replacements are inserted verbatim: the formatter spaces and
//! punctuates around them but never re-cases them, so a lowercase-branded
//! entry ("veyyon") stays lowercase even at a sentence start.
//!
//! Refine runs **after** format, on the final visible string (verbatim
//! markers already stripped). RuleRefine never re-cases tokens, so brand
//! replacements keep the casing format emitted. Limitation: once markers
//! are gone, refine cannot special-case dictionary spans beyond
//! case-preserving rules.
//!
//! Pure string logic; no I/O except legacy dictionary.toml parsing.

mod commands;
mod dictionary;
mod format;
mod refine;

pub use commands::COMMANDS;
pub use dictionary::Dictionary;
pub use format::FmtState;
pub use refine::{NullRefine, RefineBackend, RefineConfig, RuleRefine, rule_refine};

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
    refine: Box<dyn RefineBackend>,
}

impl TextPipeline {
    /// Build with the default [`RuleRefine`] backend (matches
    /// `[refine] enabled = true`, `backend = "rules"`).
    pub fn new(cfg: TextConfig, dict: Dictionary) -> Self {
        Self::with_refine(cfg, dict, Box::new(RuleRefine))
    }

    /// Build with an explicit refine backend (from [`RefineConfig::make_backend`]).
    pub fn with_refine(cfg: TextConfig, dict: Dictionary, refine: Box<dyn RefineBackend>) -> Self {
        Self { cfg, dict, refine }
    }

    /// Streaming: process one decoded segment, carrying formatter state
    /// (capitalization, quote state) across segments — pass the returned
    /// state to the next call. `scratch that` can only delete within the
    /// current segment — earlier segments are already emitted.
    ///
    /// Refine runs last on the formatted (or dictionary-only) string.
    pub fn process_stream(&self, raw: &str, state: format::FmtState) -> (String, format::FmtState) {
        let mut s = raw.to_string();
        if self.cfg.commands {
            s = commands::apply(&s);
        }
        let (mut s, state) = if self.cfg.format {
            // Marked replacements let the formatter protect their case.
            s = self.dict.apply_marked(&s);
            format::format_with(&s, state)
        } else {
            s = self.dict.apply(&s);
            (s.trim().to_string(), state)
        };
        s = self.refine.refine(&s);
        (s, state)
    }
}

#[cfg(test)]
mod pipeline_refine_tests {
    //! WHY: pipeline must apply refine after format, and NullRefine must
    //! leave duplicates intact when refine is disabled.

    use super::*;
    use std::collections::HashMap;

    #[test]
    fn refine_runs_after_format_by_default() {
        let pipe = TextPipeline::new(TextConfig::default(), Dictionary::default());
        let (text, _) = pipe.process_stream("the the cat", Default::default());
        assert_eq!(text, "The cat");
    }

    #[test]
    fn disabled_refine_leaves_duplicates() {
        let pipe = TextPipeline::with_refine(
            TextConfig::default(),
            Dictionary::default(),
            Box::new(NullRefine),
        );
        let (text, _) = pipe.process_stream("the the cat", Default::default());
        // Format capitalizes; NullRefine keeps the duplicate.
        assert_eq!(text, "The the cat");
    }

    #[test]
    fn refine_preserves_dictionary_brand_case() {
        let mut map = HashMap::new();
        map.insert("vayon".into(), "veyyon".into());
        let pipe = TextPipeline::new(TextConfig::default(), Dictionary::from_map(map));
        let (text, _) = pipe.process_stream("try vayon now", Default::default());
        assert!(text.contains("veyyon"), "{text:?}");
        assert!(!text.to_lowercase().contains("vayon"), "{text:?}");
    }
}
