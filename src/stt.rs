//! whisper.cpp transcription via whisper-rs. One `Transcriber` per model;
//! a fresh decode state per call so utterances never leak context.

use anyhow::{Context, Result, anyhow, ensure};
use std::path::Path;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub struct Transcriber {
    ctx: WhisperContext,
    language: Option<String>,
    n_threads: i32,
}

impl Transcriber {
    pub fn load(model_path: &Path, language: &str, n_threads: u32) -> Result<Self> {
        let path_str = model_path
            .to_str()
            .with_context(|| format!("model path is not UTF-8: {}", model_path.display()))?;
        let ctx = WhisperContext::new_with_params(path_str, WhisperContextParameters::default())
            .map_err(|e| anyhow!("failed to load model {}: {e}", model_path.display()))?;
        Ok(Self {
            ctx,
            language: (language != "auto").then(|| language.to_string()),
            n_threads: n_threads as i32,
        })
    }

    /// Transcribe 16 kHz mono f32 samples to raw (unformatted) text.
    pub fn transcribe(&self, samples: &[f32]) -> Result<String> {
        ensure!(!samples.is_empty(), "no audio to transcribe");

        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| anyhow!("failed to create decode state: {e}"))?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(self.language.as_deref());
        params.set_n_threads(self.n_threads);
        params.set_translate(false);
        // Each utterance is independent: no prompt carry-over between calls.
        params.set_no_context(true);
        params.set_single_segment(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        // Greedy at t=0, retry with rising temperature on failed decodes.
        params.set_temperature(0.0);
        params.set_temperature_inc(0.2);

        state
            .full(params, samples)
            .map_err(|e| anyhow!("transcription failed: {e}"))?;

        let n = state.full_n_segments();
        let mut out = String::new();
        for i in 0..n {
            let seg = state
                .get_segment(i)
                .with_context(|| format!("segment {i} of {n} vanished"))?;
            let text = seg
                .to_str_lossy()
                .map_err(|e| anyhow!("segment {i} is not valid text: {e}"))?;
            out.push_str(&text);
        }
        Ok(out.trim().to_string())
    }
}
