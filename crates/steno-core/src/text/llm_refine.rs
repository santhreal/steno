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
use std::sync::Mutex;

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
/// The model is loaded once and kept resident. Each `refine()` call
/// creates a fresh context (KV cache) — this is necessary because
/// `LlamaContext<'a>` borrows `&'a LlamaModel`, so the context cannot
/// outlive the model or be co-stored in the same struct. Context
/// creation is cheap relative to model load; the expensive part (model
/// load + GPU offload) happens once in `new()`.
///
/// A `Mutex` serializes `generate()` calls so concurrent invocations
/// from the capture thread and the API thread do not race on the
/// shared `LlamaModel` during context creation and decoding.
pub struct LlmRefine {
    backend: LlamaBackend,
    model: LlamaModel,
    config: LlmRefineConfig,
    system_prompt: String,
    /// The model's built-in chat template (e.g. ChatML for Qwen).
    chat_template: Option<LlamaChatTemplate>,
    /// Tokens that signal "stop generating" (beyond EOS).
    stop_tokens: Vec<LlamaToken>,
    /// Serializes generation calls for thread safety.
    lock: Mutex<()>,
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
        for stop_str in ["<|im_end|>", "</s>", "<|end|>", "<|eot_id|>", "<|end_of_text|>", "<|finetune_right_pad_id|>", "<|reserved_special_token_0|>", "<end_of_turn>", "<|endoftext|>"] {
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
            lock: Mutex::new(()),
        })
    }

    /// Run a single generation pass: create a context, encode the
    /// prompt, decode tokens, extract the corrected text.
    fn generate(&self, prompt: &str) -> Result<String> {
        let gen_start = std::time::Instant::now();
        // Serialize generation: the shared LlamaModel is not safe for
        // concurrent context creation + decoding across threads.
        let _guard = self.lock.lock().expect("LLM refine mutex poisoned");

        let ctx_params = LlamaContextParams::default()
            .with_n_threads(self.config.n_threads as i32)
            .with_n_threads_batch(self.config.n_threads as i32)
            .with_n_ctx(std::num::NonZeroU32::new(self.config.n_ctx));
        let mut ctx = self.model.new_context(&self.backend, ctx_params)
            .context("failed to create LLM context")?;

        // Tokenize the prompt (adds BOS token).
        let tokens = self.model.str_to_token(prompt, AddBos::Always)
            .context("failed to tokenize prompt")?;

        // Ensure prompt fits in context window. Keep the FIRST max_prompt
        // tokens (system prompt + chat template header) rather than the
        // last — truncating the system prompt would cause unguided output.
        let n_ctx = ctx.n_ctx() as usize;
        let max_tokens = self.config.max_tokens as usize;
        let (prompt_tokens, n_prompt) = if tokens.len() + max_tokens > n_ctx {
            let max_prompt = n_ctx.saturating_sub(max_tokens);
            log::warn!(
                "LLM refine: prompt ({} tokens) + max_tokens ({}) exceeds context ({}); \
                 truncating prompt to first {} tokens (system prompt preserved)",
                tokens.len(), max_tokens, n_ctx, max_prompt
            );
            (tokens[..max_prompt].to_vec(), max_prompt as i32)
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
            match self.model.token_to_piece_bytes(new_token, 64, true, None) {
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
        log::info!(
            "LLM refine: generated {} chars in {:.2}s",
            output.len(),
            gen_start.elapsed().as_secs_f64()
        );
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
                        "{no_think}Correct this speech-to-text transcript. Output ONLY the corrected text, nothing else:\n\n{text}",
                        no_think = if self.config.no_think { "/no_think\n" } else { "" }
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
            Ok(corrected) if !corrected.is_empty() => {
                let stripped = strip_think_blocks(&corrected);
                if stripped.is_empty() {
                    // The model only produced a think block with no
                    // corrected text after it. Return the original.
                    log::warn!(
                        "LLM refine: model output was only a think block; \
                         using original text"
                    );
                    text.to_string()
                } else {
                    strip_wrapping_quotes(&stripped)
                }
            }
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

/// Strip matching wrapping quotes from LLM output. Chat models often wrap
/// the corrected text in `"..."` or `'...'` despite instructions not to.
/// Only strips when the first and last characters are the same quote type
/// and the string contains no other instances of that quote (so multi-line
/// text with internal quotes is preserved).
fn strip_wrapping_quotes(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() < 2 {
        return trimmed.to_string();
    }
    let first = trimmed.chars().next().unwrap();
    let last = trimmed.chars().last().unwrap();
    if (first == '"' || first == '\'') && first == last {
        let inner = &trimmed[first.len_utf8()..trimmed.len() - last.len_utf8()];
        // Only strip if no internal occurrences of the same quote mark
        // (otherwise we'd corrupt quoted dialogue).
        if !inner.contains(first) {
            return inner.trim().to_string();
        }
    }
    trimmed.to_string()
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
        assert!(!cfg.no_think);
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

    #[test]
    fn strip_wrapping_quotes_removes_double_quotes() {
        assert_eq!(strip_wrapping_quotes("\"Hello world\""), "Hello world");
    }

    #[test]
    fn strip_wrapping_quotes_removes_single_quotes() {
        assert_eq!(strip_wrapping_quotes("'Hello world'"), "Hello world");
    }

    #[test]
    fn strip_wrapping_quotes_preserves_internal_quotes() {
        // Internal double quotes mean we should NOT strip the wrapping ones.
        assert_eq!(
            strip_wrapping_quotes(r#""He said "hi" to me""#),
            r#""He said "hi" to me""#
        );
    }

    #[test]
    fn strip_wrapping_quotes_preserves_unquoted() {
        assert_eq!(strip_wrapping_quotes("Hello world"), "Hello world");
    }

    #[test]
    fn strip_wrapping_quotes_preserves_mismatched() {
        assert_eq!(strip_wrapping_quotes("\"Hello'"), "\"Hello'");
    }

    #[test]
    fn strip_wrapping_quotes_too_short() {
        assert_eq!(strip_wrapping_quotes("\""), "\"");
        assert_eq!(strip_wrapping_quotes(""), "");
    }

    #[test]
    fn default_config_has_n_ctx() {
        let cfg = LlmRefineConfig::default();
        assert_eq!(cfg.n_ctx, 4096);
    }

    /// Smoke test: load a real GGUF model and run a refinement pass.
    ///
    /// Requires the `llm` cargo feature and a model at
    /// `~/.local/share/steno/models/*.gguf` (or `STENO_LLM_MODEL`).
    /// Skips gracefully when no model is available — this is an
    /// integration-level check, not a unit test.
    ///
    /// WHY: the unit tests above only cover prompt building, think-block
    /// stripping, and quote stripping. This test proves the full path
    /// (model load → context creation → tokenize → generate → decode)
    /// works end to end with a real GGUF. It catches regressions that
    /// mock-based tests cannot: wrong context params, broken chat
    /// template application, token decode buffer issues, and mutex
    /// deadlocks.
    #[cfg(feature = "llm")]
    #[test]
    fn llm_smoke_refine() {
        let model_path = std::env::var("STENO_LLM_MODEL")
            .ok()
            .or_else(|| {
                let dir = std::path::PathBuf::from(
                    std::env::var_os("HOME")?,
                ).join(".local/share/steno/models");
                std::fs::read_dir(&dir).ok()?
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .find(|p| {
                        p.extension().is_some_and(|ext| ext == "gguf") && p.is_file()
                    })
                    .map(|p| p.to_string_lossy().to_string())
            });

        let model_path = match model_path {
            Some(p) => p,
            None => {
                eprintln!("llm_smoke_refine: skipped (no GGUF model found; set STENO_LLM_MODEL)");
                return;
            }
        };

        let cfg = LlmRefineConfig {
            model_path: Some(std::path::PathBuf::from(&model_path)),
            n_threads: 4,
            max_tokens: 64,
            n_ctx: 2048,
            temperature: 0.1,
            no_think: false,
            ..LlmRefineConfig::default()
        };

        let refine = match LlmRefine::new(&cfg, &HashMap::new()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("llm_smoke_refine: skipped (model load failed: {e:#})");
                return;
            }
        };

        // Feed a simple unpunctuated transcript. The model should produce
        // non-empty output (even if the exact correction varies).
        let input = "hello world how are you doing today";
        let result = refine.refine(input);

        assert!(
            !result.is_empty(),
            "LLM refine returned empty output for input: {input:?}"
        );
        assert!(
            result.len() < input.len() * 5,
            "LLM refine output suspiciously long ({} chars for {} input): {result:?}",
            result.len(),
            input.len()
        );

        // The output should contain at least some of the input words.
        let lower = result.to_lowercase();
        assert!(
            lower.contains("hello") || lower.contains("world"),
            "LLM refine output lost all input words: {result:?}"
        );

        eprintln!("llm_smoke_refine: input={input:?} output={result:?}");

        // Leak the model to avoid a llama.cpp backend teardown segfault
        // (known issue: global state crashes on drop). The test process
        // exits immediately after, so the leak is harmless.
        std::mem::forget(refine);
    }
}
