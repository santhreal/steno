//! High-level offline transcription API for embedders.
//!
//! Loads the STT model and dictionary once, then decodes 16 kHz mono PCM
//! with or without the text pipeline.

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
    raw_default: bool,
}

impl Engine {
    /// Resolve the model path, load STT onto the configured provider, and
    /// build the text pipeline from `cfg.dict` / `cfg.text`.
    pub fn load(cfg: &Config) -> Result<Self> {
        let model = config::resolve_model(None, cfg)?;
        let transcriber = Transcriber::load(&model, cfg.n_threads)
            .with_context(|| format!("failed to load STT model from {}", model.display()))?;
        let dict = Dictionary::from_map(cfg.dict.overrides.clone());
        let pipeline = TextPipeline::new(cfg.text, dict);
        Ok(Self {
            transcriber,
            pipeline,
            raw_default: false,
        })
    }

    /// Decode `pcm_16k` (16 kHz mono f32) and run the text pipeline
    /// (commands → dictionary → format), unless this engine was built
    /// with raw-default (not currently exposed).
    pub fn transcribe_f32(&self, pcm_16k: &[f32]) -> Result<String> {
        if self.raw_default {
            return self.transcribe_f32_raw(pcm_16k);
        }
        let raw = self.decode_raw(pcm_16k)?;
        let (text, _) = self
            .pipeline
            .process_stream(&raw, crate::text::FmtState::default());
        Ok(text)
    }

    /// Decode only — skip commands, dictionary, and formatting.
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

    use crate::text::{Dictionary, TextConfig, TextPipeline};
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
}
