//! LLM refine backend integration test.
//!
//! Loads a GGUF model and runs refinement on sample transcripts.
//! Usage:
//!   SHERPA_ONNX_LIB_DIR=... LD_LIBRARY_PATH=... \
//!   cargo run --features llm-cuda --example llm_refine_test -- \
//!   /path/to/model.gguf

use std::collections::HashMap;

use steno_core::text::{LlmRefineConfig, RefineConfig};

fn main() {
    env_logger::init();

    let model_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| {
            eprintln!("Usage: llm_refine_test <model.gguf>");
            std::process::exit(1);
        });

    let mut dict = HashMap::new();
    dict.insert("vayon".to_string(), "veyyon".to_string());
    dict.insert("um".to_string(), "".to_string());

    let cfg = RefineConfig {
        enabled: true,
        backend: "llm".to_string(),
        dictionary: dict,
        llm: LlmRefineConfig {
            model_path: Some(model_path.into()),
            n_gpu_layers: -1,
            n_threads: 4,
            max_tokens: 256,
            n_ctx: 4096,
            temperature: 0.1,
            prompt: String::new(),
        },
    };

    eprintln!("Loading LLM model...");
    let backend = cfg.make_backend();

    let inputs = [
        "hello world how are you doing today",
        "i went to the store yesterday and bought some milk",
        "the vayon project is really cool um i think it has potential",
        "what time is it",
        "this is a test of the dictate system period new line the quick brown fox jumps over the lazy dog question mark",
    ];

    for input in &inputs {
        eprintln!("\n---");
        eprintln!("Input:  {input}");
        let output = backend.refine(input);
        eprintln!("Output: {output}");
    }

    eprintln!("\n---");
    eprintln!("Testing fallback: invalid model path...");
    let bad_cfg = RefineConfig {
        enabled: true,
        backend: "llm".to_string(),
        dictionary: HashMap::new(),
        llm: LlmRefineConfig {
            model_path: Some("/nonexistent/model.gguf".into()),
            ..Default::default()
        },
    };
    let fallback = bad_cfg.make_backend();
    let result = fallback.refine("hello world");
    eprintln!("Fallback result: {result}");
    assert!(!result.is_empty(), "fallback should return original text");
    eprintln!("Fallback OK (returned original text)");
}
