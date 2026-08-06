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
        let language = validate_language(language)?;
        let n_threads = check_n_threads(n_threads)?;
        let path_str = model_path
            .to_str()
            .with_context(|| format!("model path is not UTF-8: {}", model_path.display()))?;
        let ctx = WhisperContext::new_with_params(path_str, WhisperContextParameters::default())
            .map_err(|e| {
                anyhow!(
                    "failed to load model {}: {e} — the file may be corrupt or not a ggml whisper model; re-download it",
                    model_path.display()
                )
            })?;
        if !ctx.is_multilingual()
            && let Some(lang) = &language
            && lang != "en"
        {
            log::warn!("model is English-only; language '{lang}' is ignored");
        }
        Ok(Self {
            ctx,
            language,
            n_threads,
        })
    }

    /// Transcribe 16 kHz mono f32 samples to raw (unformatted) text.
    /// Decode and invoke `sink` with each segment's raw text AS IT
    /// FINALIZES, so callers can stream output while decoding continues.
    /// Segments arrive in order; concatenating them equals the full
    /// transcript. Errors from the sink cannot propagate through the FFI
    /// callback — the caller records them in captured state.
    pub fn transcribe_streaming(
        &self,
        samples: &[f32],
        mut sink: impl FnMut(&str) + 'static,
    ) -> Result<()> {
        ensure!(
            !samples.is_empty(),
            "no audio to transcribe — the capture was empty; check the microphone level and VAD settings"
        );

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
        // Pin the silence defaults so all-silence audio yields no segments
        // instead of hallucinated text: a segment is dropped when its
        // no-speech probability exceeds 0.6 AND its logprob is below -1.0.
        params.set_no_speech_thold(0.6);
        params.set_logprob_thold(-1.0);
        // Greedy at t=0, retry with rising temperature on failed decodes.
        params.set_temperature(0.0);
        params.set_temperature_inc(0.2);

        // Upstream whisper-rs 0.16 notes (ReviewPipeline #4/#5): the
        // callback thunk is Box::into_raw'd per decode and never
        // reclaimed (~48 bytes per utterance — a slow leak, fixed
        // upstream only when we upgrade), and non-UTF-8 segments are
        // silently skipped (we would produce no text, not garbage).
        params.set_segment_callback_safe(move |data: whisper_rs::SegmentCallbackData| {
            sink(&data.text);
        });

        state
            .full(params, samples)
            .map_err(|e| anyhow!("transcription failed: {e}"))?;
        Ok(())
    }
}

/// Map the user's language setting to whisper's: `None` = auto-detect,
/// otherwise the canonical code ("en", "de", ...). An unknown language is
/// an error, not a silent garbage decode: whisper_lang_id() returns -1 for
/// it and whisper.cpp feeds token_sot itself as the language token.
/// Accepts full names ("english") like whisper_lang_id does.
fn validate_language(language: &str) -> Result<Option<String>> {
    match language.trim() {
        "" | "auto" => Ok(None),
        lang => {
            // whisper-rs does CString::new(...).expect() — reject NUL
            // ourselves or a config value like "\0" panics the process.
            ensure!(
                !lang.contains('\0'),
                "invalid language {lang:?} — use a language code like \"en\" or \"auto\""
            );
            let id = whisper_rs::get_lang_id(lang).with_context(|| {
                format!("unknown language {lang:?} — use a code like \"en\", \"de\", \"ja\", or \"auto\"")
            })?;
            Ok(whisper_rs::get_lang_str(id).map(str::to_string))
        }
    }
}

/// whisper takes the thread count as c_int; reject 0 and values that
/// would wrap negative in a `u32 as i32` cast.
fn check_n_threads(n: u32) -> Result<i32> {
    ensure!(
        (1..=(i32::MAX as u32)).contains(&n),
        "invalid n_threads = {n} — set it between 1 and {}",
        i32::MAX
    );
    Ok(n as i32)
}

#[cfg(test)]
mod tests {
    //! Regression tests for the pure validation logic. WHY: an unknown
    //! --language previously reached whisper.cpp as lang_id -1 (silently
    //! wrong output), a NUL-containing language from TOML panicked inside
    //! CString::new().expect(), and n_threads truncated via `as i32`.
    use super::*;

    #[test]
    fn auto_and_empty_mean_auto_detect() {
        assert_eq!(validate_language("auto").unwrap(), None);
        assert_eq!(validate_language("").unwrap(), None);
        assert_eq!(validate_language("  ").unwrap(), None);
    }

    #[test]
    fn valid_codes_and_names_are_canonicalized() {
        assert_eq!(validate_language("en").unwrap().as_deref(), Some("en"));
        assert_eq!(validate_language("de").unwrap().as_deref(), Some("de"));
        // whisper_lang_id also accepts full English names.
        assert_eq!(validate_language("english").unwrap().as_deref(), Some("en"));
    }

    #[test]
    fn garbage_language_is_rejected_with_fix() {
        let err = format!("{:#}", validate_language("klingon").unwrap_err());
        assert!(err.contains("unknown language"), "{err}");
        assert!(err.contains("\"en\""), "{err}");
        assert!(err.contains("auto"), "{err}");
    }

    #[test]
    fn nul_byte_language_errors_instead_of_panicking() {
        // WHY: TOML can express "en\0"; whisper-rs would .expect() on it.
        let err = format!("{:#}", validate_language("en\0x").unwrap_err());
        assert!(err.contains("invalid language"), "{err}");
    }

    #[test]
    fn n_threads_bounds() {
        assert_eq!(check_n_threads(1).unwrap(), 1);
        assert_eq!(check_n_threads(16).unwrap(), 16);
        assert!(check_n_threads(0).is_err());
        assert!(check_n_threads(u32::MAX).is_err());
        assert!(check_n_threads((i32::MAX as u32) + 1).is_err());
        assert_eq!(check_n_threads(i32::MAX as u32).unwrap(), i32::MAX);
    }
}
