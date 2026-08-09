//! Post-transcription text pipeline, applied in this exact order:
//! 1. voice commands  : "period", "new line", "scratch that", ...
//! 2. refine          : offline ASR cleanup and dictionary phrase overrides
//! 3. formatting      : sentence capitalization, punctuation spacing
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

/// Text pipeline configuration (`[text]`).
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


/// Post-transcription text processing pipeline.
pub struct TextPipeline {
    cfg: TextConfig,
    refine: Box<dyn RefineBackend>,
}

impl TextPipeline {
    /// Build with the default [`RuleRefine`] backend (always active,
    /// regardless of `[refine] enabled`). For config-driven backend
    /// selection (including `NullRefine` when disabled), use
    /// [`Self::with_refine`] with [`RefineConfig::make_backend`].
    pub fn new(cfg: TextConfig, dict: Dictionary) -> Self {
        Self::with_refine(cfg, Box::new(RuleRefine::new(dict)))
    }

    /// Build with an explicit refine backend (from [`RefineConfig::make_backend`]).
    pub fn with_refine(cfg: TextConfig, refine: Box<dyn RefineBackend>) -> Self {
        Self { cfg, refine }
    }

    /// One-shot: process `raw` with a fresh [`FmtState`].
    ///
    /// Equivalent to `process_stream(raw, FmtState::default()).0`. Prefer
    /// [`Self::process_stream`] when decoding streams segment-by-segment.
    pub fn process(&self, raw: &str) -> String {
        self.process_stream(raw, format::FmtState::default()).0
    }

    /// Streaming: process one decoded segment, carrying formatter state
    /// (capitalization, quote state) across segments: pass the returned
    /// state to the next call. `scratch that` can only delete within the
    /// current segment: earlier segments are already emitted.
    ///
    /// Text pipeline order: voice commands -> refine -> format.
    pub fn process_stream(&self, raw: &str, state: format::FmtState) -> (String, format::FmtState) {
        let mut s = raw.to_string();
        if self.cfg.commands {
            s = commands::apply(&s);
        }
        s = self.refine.refine(&s);
        let (s, state) = if self.cfg.format {
            format::format_with(&s, state)
        } else {
            (s.trim().to_string(), state)
        };
        (s, state)
    }
}

#[cfg(test)]
mod pipeline_refine_tests {
    //! WHY: pipeline must apply refine before format, and NullRefine must
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
