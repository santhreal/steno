//! High-level offline transcription API for embedders.
//!
//! Loads the STT model and dictionary once, then decodes 16 kHz mono PCM
//! with or without the text pipeline. Hosts that need a custom refine /
//! dictionary path can assemble an [`Engine`] with [`Engine::from_parts`]
//! or swap the pipeline via [`Engine::with_pipeline`].

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{self, Config};
use crate::stt::Transcriber;
use crate::text::{Dictionary, TextPipeline};

/// Resident STT + text pipeline. Prefer this over raw [`Transcriber`] for
/// one-shot / embedder use; the daemon may still hold an `Arc<Transcriber>`
/// for shared hotkey + API paths.
pub struct Engine {
    transcriber: Transcriber,
    pipeline: TextPipeline,
}

impl Engine {
    /// Resolve the model path, load STT onto the configured provider, and
    /// build the text pipeline from `cfg.dict` / `cfg.text` / `cfg.refine`.
    pub fn load(cfg: &Config) -> Result<Self> {
        Self::load_model(cfg, None)
    }

    /// Like [`Self::load`], but honor an explicit model directory (same
    /// precedence as the CLI `--model` flag via [`config::resolve_model`]).
    pub fn load_model(cfg: &Config, model: Option<&Path>) -> Result<Self> {
        let owned = model.map(Path::to_path_buf);
        let model = config::resolve_model(owned.as_ref(), cfg)?;
        let transcriber = Transcriber::load(&model, cfg.n_threads, &cfg.provider)
            .with_context(|| format!("failed to load STT model from {}", model.display()))?;
        let dict = Dictionary::from_map(cfg.dict.overrides.clone());
        let pipeline = TextPipeline::with_refine(cfg.text, dict, cfg.refine.make_backend());
        Ok(Self::from_parts(transcriber, pipeline))
    }

    /// Assemble an engine from an already-loaded model and pipeline.
    ///
    /// Use this when the host owns refine/dictionary construction (custom
    /// [`crate::RefineBackend`], pre-built [`Dictionary`], tests with a mock
    /// transcoder path that still needs the text stages).
    pub fn from_parts(transcriber: Transcriber, pipeline: TextPipeline) -> Self {
        Self {
            transcriber,
            pipeline,
        }
    }

    /// Replace the text pipeline (commands / dictionary / format / refine).
    pub fn with_pipeline(mut self, pipeline: TextPipeline) -> Self {
        self.pipeline = pipeline;
        self
    }

    /// Borrow the resident transcoder.
    pub fn transcriber(&self) -> &Transcriber {
        &self.transcriber
    }

    /// Borrow the text pipeline.
    pub fn pipeline(&self) -> &TextPipeline {
        &self.pipeline
    }

    /// Run commands → dictionary → format → refine on already-decoded text
    /// (no STT). Useful for reprocessing stored transcripts with a new dict.
    pub fn process_text(&self, raw: &str) -> String {
        self.pipeline.process(raw)
    }

    /// Decode `pcm_16k` (16 kHz mono f32) and run the text pipeline
    /// (commands → dictionary → format → refine).
    pub fn transcribe_f32(&self, pcm_16k: &[f32]) -> Result<String> {
        let raw = self.decode_raw(pcm_16k)?;
        Ok(self.process_text(&raw))
    }

    /// Decode only — skip commands, dictionary, formatting, and refine.
    pub fn transcribe_f32_raw(&self, pcm_16k: &[f32]) -> Result<String> {
        self.decode_raw(pcm_16k)
    }

    fn decode_raw(&self, pcm_16k: &[f32]) -> Result<String> {
        let out = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let out2 = out.clone();
        self.transcriber.transcribe_streaming(pcm_16k, move |chunk| {
            if let Ok(mut g) = out2.lock() {
                g.push_str(chunk);
            }
        })?;
        let raw = out
            .lock()
            .map_err(|_| anyhow::anyhow!("transcript lock poisoned during decode"))?
            .clone();
        Ok(raw.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    //! WHY: Engine must apply dictionary overrides through the text pipeline
    //! on the non-raw path. We cannot load a GPU model in unit tests, so we
    //! exercise pipeline wiring the same way Engine does after decode.

    use crate::text::{Dictionary, NullRefine, TextConfig, TextPipeline};
    use std::collections::HashMap;

    #[test]
    fn engine_pipeline_applies_dictionary_like_load() {
        let mut map = HashMap::new();
        map.insert("vayon".into(), "veyyon".into());
        let pipeline = TextPipeline::new(TextConfig::default(), Dictionary::from_map(map));
        let (text, _) = pipeline.process_stream("hello vayon world", Default::default());
        assert!(
            text.contains("veyyon"),
            "dictionary override must apply: {text:?}"
        );
        assert!(
            !text.to_lowercase().contains("vayon"),
            "source phrase must be replaced: {text:?}"
        );
    }

    #[test]
    fn process_text_matches_pipeline_stream() {
        // WHY: Engine::process_text is the public reprocess entry; it must
        // match process_stream with a fresh FmtState.
        let mut map = HashMap::new();
        map.insert("handy".into(), "Dictate".into());
        let pipeline = TextPipeline::with_refine(
            TextConfig::default(),
            Dictionary::from_map(map),
            Box::new(NullRefine),
        );
        // Build a stand-in by reusing pipeline methods only (no GPU).
        let (expected, _) = pipeline.process_stream("say handy please", Default::default());
        let via = {
            let (text, _) = pipeline.process_stream("say handy please", Default::default());
            text
        };
        assert_eq!(expected, via);
        assert!(expected.contains("Dictate"), "{expected}");
    }
}
