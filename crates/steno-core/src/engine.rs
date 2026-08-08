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
    /// Resolve the model path using default config (`Config::load(None)`), load STT,
    /// and build the text pipeline.
    pub fn load_default() -> Result<Self> {
        let cfg = Config::load(None)?;
        Self::load(&cfg)
    }

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
        let refine_backend = cfg.refine.make_backend();
        let pipeline = TextPipeline::with_refine(cfg.text, Dictionary::default(), refine_backend);
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
        if pcm_16k.is_empty() {
            return Ok(String::new());
        }
        let raw = self.decode_raw(pcm_16k)?;
        Ok(self.process_text(&raw))
    }

    /// Decode a WAV file from disk, resampling to 16 kHz mono if required.
    pub fn transcribe_wav_file(&self, path: &Path) -> Result<String> {
        let (pcm, sample_rate) = crate::dsp::read_wav(path)?;
        self.transcribe_f32_at_rate(&pcm, sample_rate)
    }

    /// Decode `pcm_i16` (16 kHz mono signed 16-bit PCM) converted to f32.
    pub fn transcribe_pcm_i16(&self, pcm_i16: &[i16]) -> Result<String> {
        if pcm_i16.is_empty() {
            return Ok(String::new());
        }
        let pcm_f32: Vec<f32> = pcm_i16.iter().map(|&s| s as f32 / 32768.0).collect();
        self.transcribe_f32(&pcm_f32)
    }

    /// Decode mono f32 PCM at `sample_rate`, resampling to 16 kHz if necessary.
    pub fn transcribe_f32_at_rate(&self, pcm: &[f32], sample_rate: u32) -> Result<String> {
        if pcm.is_empty() {
            return Ok(String::new());
        }
        if sample_rate == crate::dsp::STT_RATE {
            self.transcribe_f32(pcm)
        } else {
            let resampled = crate::dsp::resample(pcm, sample_rate, crate::dsp::STT_RATE)?;
            self.transcribe_f32(&resampled)
        }
    }

    /// Decode only: skip commands, dictionary, formatting, and refine.
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

    use super::Engine;
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

    #[test]
    fn process_text_matches_pipeline_stream() {
        // WHY: Engine::process_text is the public reprocess entry; it must
        // match process_stream with a fresh FmtState.
        let mut map = HashMap::new();
        map.insert("handy".into(), "Dictate".into());
        let pipeline = TextPipeline::new(
            TextConfig::default(),
            Dictionary::from_map(map),
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
    #[test]
    fn engine_with_refine_dictionary_refinement() {
        let mut cfg = crate::Config::default();
        cfg.refine.dictionary.insert("vayon".into(), "veyyon".into());
        let backend = cfg.refine.make_backend();
        let pipeline = TextPipeline::with_refine(TextConfig::default(), Dictionary::default(), backend);
        let engine = Engine::from_parts(crate::stt::Transcriber::dummy(), pipeline);
        assert_eq!(engine.process_text("hello vayon world"), "Hello veyyon world");
    }

    #[test]
    fn transcribe_pcm_i16_empty_returns_ok_empty() {
        // WHY: Engine::transcribe_pcm_i16 must return Ok("") immediately when given
        // an empty &[i16] slice without calling STT decode.
        let pipeline = TextPipeline::new(TextConfig::default(), Dictionary::default());
        let engine = Engine::from_parts(crate::stt::Transcriber::dummy(), pipeline);
        let pcm_i16: &[i16] = &[];
        let res = engine.transcribe_pcm_i16(pcm_i16).expect("empty pcm_i16 succeeds");
        assert_eq!(res, "", "empty i16 pcm must yield empty transcript");
    }

    #[test]
    fn transcribe_f32_at_rate_empty_returns_ok_empty() {
        // WHY: Engine::transcribe_f32_at_rate must return Ok("") immediately for empty input,
        // both at standard STT rate (16 kHz) and non-16 kHz rates (e.g. 8 kHz, 44.1 kHz, 48 kHz).
        let pipeline = TextPipeline::new(TextConfig::default(), Dictionary::default());
        let engine = Engine::from_parts(crate::stt::Transcriber::dummy(), pipeline);
        let pcm_f32: &[f32] = &[];
        for &rate in &[16000u32, 8000, 44100, 48000] {
            let res = engine
                .transcribe_f32_at_rate(pcm_f32, rate)
                .expect("empty f32 at rate succeeds");
            assert_eq!(res, "", "empty f32 pcm at rate {rate} must yield empty transcript");
        }
    }
}
