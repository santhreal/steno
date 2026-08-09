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

use anyhow::{Context, Result, bail};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;

use super::{LlmRefineConfig, RefineBackend};

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
/// creates a fresh context (KV cache), applies the model's chat
/// template, runs a single generation pass, and returns the corrected
/// text.
pub struct LlmRefine {
    backend: LlamaBackend,
    model: LlamaModel,
    config: LlmRefineConfig,
    system_prompt: String,
    /// The model's built-in chat template (e.g. ChatML for Qwen).
    chat_template: Option<LlamaChatTemplate>,
    /// Tokens that signal "stop generating" (beyond EOS).
    stop_tokens: Vec<LlamaToken>,
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

        // Validate config fields.
        let n_threads = config.n_threads.clamp(1, 32) as i32;
        let max_tokens = config.max_tokens.clamp(1, 4096);
        let temperature = config.temperature.clamp(0.0, 2.0);

        let backend = LlamaBackend::init()
            .context("failed to initialize llama.cpp backend")?;

        // n_gpu_layers: -1 (all to GPU) maps to u32::MAX; 0 = CPU only.
        let n_gpu = if config.n_gpu_layers < 0 {
            u32::MAX
        } else {
            config.n_gpu_layers as u32
        };
        let model_params = LlamaModelParams::default()
            .with_n_gpu_layers(n_gpu);
        let model = LlamaModel::load_from_file(&backend, &model_path, &model_params)
            .with_context(|| format!("failed to load GGUF model from {}", model_path.display()))?;
        let system_prompt = if config.prompt.is_empty() {
            build_system_prompt(dictionary)
        } else {
            config.prompt.clone()
        };

        // Load the model's built-in chat template (e.g. ChatML for Qwen).
        // Falls back to None if the model has no template — the raw
        // prompt format will be used instead.
        let chat_template = model.chat_template(None).ok();
        if chat_template.is_none() {
            log::warn!(
                "LLM refine: model has no chat template; using raw prompt format. \
                 A chat model (e.g. Qwen3, Llama3) is recommended."
            );
        }

        // Collect stop tokens: EOS + common chat markers.
        let mut stop_tokens = vec![model.token_eos()];
        for stop_str in ["<|im_end|>", "</s>", "<|end|>", "<|eot_id|>"] {
            if let Ok(toks) = model.str_to_token(stop_str, AddBos::Never) {
                if let Some(&first) = toks.first() {
                    stop_tokens.push(first);
                }
            }
        }

        let config = LlmRefineConfig {
            n_threads: n_threads as u32,
            max_tokens,
            temperature,
            ..config.clone()
        };

        Ok(Self {
            backend,
            model,
            config,
            system_prompt,
            chat_template,
            stop_tokens,
        })
    }

    /// Run a single generation pass: create a context, encode the
    /// prompt, decode tokens, extract the corrected text.
    fn generate(&self, prompt: &str) -> Result<String> {
        let ctx_params = LlamaContextParams::default()
            .with_n_threads(self.config.n_threads as i32)
            .with_n_threads_batch(self.config.n_threads as i32)
            .with_n_ctx(std::num::NonZeroU32::new(4096));
        let mut ctx = self.model.new_context(&self.backend, ctx_params)
            .context("failed to create LLM context")?;

        // Tokenize the prompt (adds BOS token).
        let tokens = self.model.str_to_token(prompt, AddBos::Always)
            .context("failed to tokenize prompt")?;

        // Ensure prompt fits in context window.
        let n_ctx = ctx.n_ctx() as usize;
        let max_tokens = self.config.max_tokens as usize;
        let (prompt_tokens, n_prompt) = if tokens.len() + max_tokens > n_ctx {
            let max_prompt = n_ctx.saturating_sub(max_tokens);
            let start = tokens.len().saturating_sub(max_prompt);
            log::warn!(
                "LLM refine: prompt ({} tokens) + max_tokens ({}) exceeds context ({}); \
                 truncating prompt to {} tokens",
                tokens.len(), max_tokens, n_ctx, max_prompt
            );
            (tokens[start..].to_vec(), (tokens.len() - start) as i32)
        } else {
            (tokens.clone(), tokens.len() as i32)
        };

        let mut batch = LlamaBatch::new(prompt_tokens.len() + max_tokens, 1);

        for (i, &token) in prompt_tokens.iter().enumerate() {
            let needs_logits = i == prompt_tokens.len() - 1;
            batch.add(token, i as i32, &[0], needs_logits)
                .map_err(|e| anyhow::anyhow!("failed to add token to batch: {e}"))?;
        }

        ctx.decode(&mut batch)
            .context("failed to decode prompt batch")?;

        // Build the sampler: temp → greedy (or temp → dist for stochastic).
        let sampler = if self.config.temperature <= 0.0 {
            LlamaSampler::greedy()
        } else {
            LlamaSampler::chain(
                [
                    LlamaSampler::temp(self.config.temperature),
                    LlamaSampler::dist(0),
                ],
                true,
            )
        };
        let mut sampler = sampler;

        let mut output = String::new();

        for n_cur in (n_prompt..).take(self.config.max_tokens as usize) {
            // Sample the next token from the last position.
            let new_token = sampler.sample(&ctx, batch.n_tokens() - 1);

            // Check for end-of-generation (EOS, EOG, or stop sequence).
            if self.stop_tokens.contains(&new_token)
                || self.model.is_eog_token(new_token)
            {
                break;
            }

            // Decode the token to text.
            match self.model.token_to_piece_bytes(new_token, 8, true, None) {
                Ok(bytes) => output.push_str(&String::from_utf8_lossy(&bytes)),
                Err(e) => log::warn!("failed to decode token: {e}"),
            }

            // Feed the new token back for the next iteration.
            batch.clear();
            batch.add(new_token, n_cur, &[0], true)
                .map_err(|e| anyhow::anyhow!("failed to add generated token: {e}"))?;
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

        // Build the prompt using the model's chat template if available
        // (correct format for chat-tuned models like Qwen3, Llama3).
        // Fall back to raw text format for base models.
        let prompt = match &self.chat_template {
            Some(tmpl) => {
                let messages = match (
                    LlamaChatMessage::new("system".into(), self.system_prompt.clone()),
                    LlamaChatMessage::new("user".into(), format!(
                        "/no_think\nCorrect this speech-to-text transcript. Output ONLY the corrected text, nothing else:\n\n{text}"
                    )),
                ) {
                    (Ok(sys), Ok(user)) => vec![sys, user],
                    _ => {
                        log::warn!("LLM refine: failed to create chat messages; using raw prompt");
                        return text.to_string();
                    }
                };
                match self.model.apply_chat_template(tmpl, &messages, true) {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("LLM refine: chat template failed: {e}; using raw prompt");
                        format!(
                            "{system_prompt}\n\n---\nRaw transcript:\n{input}\n---\nCorrected transcript:\n",
                            system_prompt = self.system_prompt,
                            input = text,
                        )
                    }
                }
            }
            None => format!(
                "{system_prompt}\n\n---\nRaw transcript:\n{input}\n---\nCorrected transcript:\n",
                system_prompt = self.system_prompt,
                input = text,
            ),
        };

        match self.generate(&prompt) {
            Ok(corrected) if !corrected.is_empty() => strip_think_blocks(&corrected),
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

/// Strip `<think>...</think>` blocks from Qwen3 reasoning model output.
/// If the block is unclosed (model still thinking when max_tokens hit),
/// strip everything from `<think>` to end. Returns the cleaned text,
/// or the original if no think block was found.
fn strip_think_blocks(text: &str) -> String {
    if !text.contains("<think>") {
        return text.trim().to_string();
    }
    // Remove all <think>...</think> blocks (closed or unclosed).
    // For unclosed blocks (model still thinking at max_tokens), discard
    // everything from <think> to end.
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        result.push_str(&rest[..start]);
        rest = &rest[start + "<think>".len()..];
        if let Some(end) = rest.find("</think>") {
            rest = &rest[end + "</think>".len()..];
        } else {
            // Unclosed think block — discard the rest.
            rest = "";
        }
    }
    result.push_str(rest);
    let cleaned = result.trim();
    if cleaned.is_empty() {
        String::new()
    } else {
        cleaned.to_string()
    }
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

    #[test]
    fn strip_think_blocks_removes_closed_block() {
        let open = "\u{3c}think\u{3e}";
        let close = "\u{3c}/think\u{3e}";
        let input = format!("{open}Let me think about this.{close}\n\nHello, world! How are you doing today?");
        let result = strip_think_blocks(&input);
        assert_eq!(result, "Hello, world! How are you doing today?");
    }

    #[test]
    fn strip_think_blocks_removes_unclosed_block() {
        let open = "\u{3c}think\u{3e}";
        let input = format!("{open}I'm still thinking...");
        let result = strip_think_blocks(&input);
        assert_eq!(result, "");
    }

    #[test]
    fn strip_think_blocks_no_block_returns_trimmed() {
        let input = "  Hello world  ";
        let result = strip_think_blocks(input);
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn strip_think_blocks_multiple_blocks() {
        let open = "\u{3c}think\u{3e}";
        let close = "\u{3c}/think\u{3e}";
        let input = format!("{open}first thinking block{close}\n{open}second{close}\n\nHello, world! How are you doing today?");
        let result = strip_think_blocks(&input);
        assert_eq!(result, "Hello, world! How are you doing today?");
    }

    #[test]
    fn strip_think_blocks_empty_after_strip() {
        let open = "\u{3c}think\u{3e}";
        let close = "\u{3c}/think\u{3e}";
        let input = format!("{open}just thinking...{close}");
        let result = strip_think_blocks(&input);
        assert_eq!(result, "");
    }
}
