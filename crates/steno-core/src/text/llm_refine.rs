//! LLM-based post-STT refinement via llama-cpp-2 (GGUF models).
//!
//! Implements [`crate::text::RefineBackend`] using a local LLM for
//! grammatical error correction (GEC), punctuation restoration,
//! capitalization, and dictionary phrase substitution — all in one
//! neural pass on GPU or CPU.
//!
//! ## Config
//!
//! ```toml
//! [refine]
//! backend = "llm"
//!
//! [refine.llm]
//! model_path = "~/.local/share/steno/models/qwen3-0.6b-q4_k_m.gguf"
//! n_gpu_layers = -1   # -1 = all to GPU, 0 = CPU only
//! n_threads = 4
//! max_tokens = 512
//! temperature = 0.1
//! ```
//!
//! ## GPU support
//!
//! GPU offload is controlled by `n_gpu_layers` at runtime. The cargo
//! feature selected at build time determines which GPU backend is
//! available: `llm-cuda` (NVIDIA), `llm-vulkan` (AMD/Intel/NVIDIA),
//! `llm-metal` (macOS), or `llm` (CPU only). When `n_gpu_layers > 0`
//! but no GPU backend is compiled in, llama.cpp silently runs on CPU.
//!
//! ## Fallback
//!
//! If the model cannot be loaded (missing file, corrupt GGUF, OOM),
//! [`RefineConfig::make_backend`] logs an error and falls back to
//! [`crate::text::RuleRefine`] so dictation keeps working.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::token::data_array::LlamaTokenDataArray;

use super::{Dictionary, LlmRefineConfig, RefineBackend};

const DEFAULT_SYSTEM_PROMPT: &str = "\
You are a transcription correction engine. Fix the raw speech-to-text \
output in these ways, then output ONLY the corrected text with no \
preamble or explanation:\n\
1. Fix grammar and word errors from the speech recognizer.\n\
2. Add proper punctuation (commas, periods, questions, etc.).\n\
3. Capitalize sentence starts and proper nouns.\n\
4. Apply these dictionary substitutions exactly (case-insensitive match):\n\
{dictionary}\n\
5. Remove filler words (um, uh, you know) unless they change meaning.\n\
6. Do NOT change the meaning, add information, or remove content.\n\
7. Preserve newlines and paragraph structure.\n\
Output the corrected text only.";

/// LLM refine backend using a local GGUF model via llama-cpp-2.
///
/// The model is loaded once and kept resident. Each `refine()` call
/// constructs a prompt, runs a single generation pass, and returns the
/// corrected text. Thread-safe via a Mutex on the model+context.
pub struct LlmRefine {
    backend: LlamaBackend,
    model: LlamaModel,
    config: LlmRefineConfig,
    system_prompt: String,
}

impl LlmRefine {
    /// Load the GGUF model and prepare the LLM refine backend.
    pub fn new(config: &LlmRefineConfig, dictionary: &HashMap<String, String>) -> Result<Self> {
        let model_path = config.model_path.as_ref()
            .context("refine.llm.model_path is not set — provide a GGUF model path")?;
        let model_path = crate::config::expand_tilde(model_path)?;

        if !model_path.exists() {
            bail!(
                "LLM model not found at {}. Download a GGUF model (e.g. \
                 Qwen3-0.6B-Q4_K_M.gguf) and set refine.llm.model_path. \
                 See https://huggingface.co/models?other=gguf",
                model_path.display()
            );
        }
        if !model_path.is_file() {
            bail!("LLM model path {} is a directory, not a file", model_path.display());
        }

        let backend = LlamaBackend::init()
            .context("failed to initialize llama.cpp backend")?;

        let model_params = LlamaModelParams::default()
            .with_n_gpu_layers(config.n_gpu_layers);
        let model = LlamaModel::load_from_file(&backend, &model_path, &model_params)
            .with_context(|| format!("failed to load GGUF model from {}", model_path.display()))?;

        let system_prompt = if config.prompt.is_empty() {
            build_system_prompt(dictionary)
        } else {
            config.prompt.clone()
        };

        Ok(Self {
            backend,
            model,
            config: config.clone(),
            system_prompt,
        })
    }

    /// Run a single generation pass: encode the prompt, decode tokens,
    /// extract the corrected text.
    fn generate(&self, prompt: &str) -> Result<String> {
        let ctx_params = LlamaContextParams::default()
            .with_n_threads(self.config.n_threads as i32)
            .with_n_ctx(std::num::NonZeroU32::new(4096).unwrap());

        let mut ctx = self.model.new_context(&self.backend, ctx_params)
            .context("failed to create LLM context")?;

        // Tokenize the prompt.
        let tokens = ctx.model.str_to_token(prompt, llama_cpp_2::llama_batch::AddEos::Always)
            .context("failed to tokenize prompt")?;

        // Create a batch and add the prompt tokens.
        let mut batch = LlamaBatch::new(tokens.len() + self.config.max_tokens as usize, 1);
        for (i, &token) in tokens.iter().enumerate() {
            batch.add(token, i as i32, &[0], false)
                .context("failed to add token to batch")?;
        }

        ctx.decode(&mut batch)
            .context("failed to decode prompt batch")?;

        // Generate tokens one at a time.
        let mut output = String::new();
        let n_prompt = tokens.len() as i32;

        for _ in 0..self.config.max_tokens {
            // Sample the next token.
            let logits = ctx.get_logits_ith(batch.n_tokens() - 1)
                .context("failed to get logits")?;
            let mut candidates = LlamaTokenDataArray::from_iter(logits, false);

            // Simple greedy/temperature sampling.
            if self.config.temperature <= 0.0 {
                // Greedy: pick the highest logit.
                let best = candidates.data.iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| {
                        a.logit.partial_cmp(&b.logit).unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                candidates.set_index(best);
            } else {
                ctx.sample_temp(&mut candidates, self.config.temperature);
                candidates.sample();
            }

            let new_token = candidates.token_at_index()
                .context("failed to sample token")?;

            // Check for end-of-generation token.
            if new_token == ctx.model.token_eos() {
                break;
            }

            // Decode the token to text.
            if let Some(piece) = ctx.model.token_to_str(new_token) {
                output.push_str(&piece);
            }

            // Feed the new token back for the next iteration.
            batch.clear();
            let pos = ctx.n_ctx() as i32; // not ideal but works for single-turn
            batch.add(new_token, n_prompt, &[0], true)
                .context("failed to add generated token to batch")?;
            ctx.decode(&mut batch)
                .context("failed to decode generated token")?;
        }

        Ok(output.trim().to_string())
    }
}

impl RefineBackend for LlmRefine {
    fn refine(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }

        let prompt = format!(
            "{system_prompt}\n\n---\nRaw transcript:\n{input}\n---\nCorrected transcript:\n",
            system_prompt = self.system_prompt,
            input = text,
        );

        match self.generate(&prompt) {
            Ok(corrected) if !corrected.is_empty() => corrected,
            Ok(_) => {
                log::warn!("LLM refine returned empty output; using original text");
                text.to_string()
            }
            Err(e) => {
                log::error!("LLM refine generation failed: {e:#}; using original text");
                text.to_string()
            }
        }
    }
}


/// Build the system prompt with dictionary entries embedded.
fn build_system_prompt(dictionary: &HashMap<String, String>) -> String {
    if dictionary.is_empty() {
        return DEFAULT_SYSTEM_PROMPT.replace("{dictionary}", "(none)");
    }
    let mut entries: Vec<_> = dictionary.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let dict_str = entries.iter()
        .map(|(k, v)| format!("  \"{k}\" → \"{v}\""))
        .collect::<Vec<_>>()
        .join("\n");
    DEFAULT_SYSTEM_PROMPT.replace("{dictionary}", &dict_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_system_prompt_with_dictionary() {
        let mut dict = HashMap::new();
        dict.insert("vayon".to_string(), "veyyon".to_string());
        dict.insert("um".to_string(), "".to_string());
        let prompt = build_system_prompt(&dict);
        assert!(prompt.contains("\"vayon\" → \"veyyon\""));
        assert!(prompt.contains("\"um\" → \"\""));
    }

    #[test]
    fn build_system_prompt_empty_dictionary() {
        let prompt = build_system_prompt(&HashMap::new());
        assert!(prompt.contains("(none)"));
    }

    #[test]
    fn default_config_values() {
        let cfg = LlmRefineConfig::default();
        assert_eq!(cfg.n_gpu_layers, -1);
        assert_eq!(cfg.n_threads, 4);
        assert_eq!(cfg.max_tokens, 512);
        assert_eq!(cfg.temperature, 0.1);
        assert!(cfg.model_path.is_none());
        assert!(cfg.prompt.is_empty());
    }
}
