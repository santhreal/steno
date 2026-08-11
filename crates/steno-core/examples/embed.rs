//! Embedding `steno-core` for one-shot transcription, session use, and
//! custom refine backends. Run with:
//!
//! ```sh
//! cargo run -p steno-core --example embed -- /path/to/model-dir recording.wav
//! ```

use std::collections::HashMap;
use std::path::Path;

use steno_core::{
    Config, Dictionary, Engine, FnOverlay, NullOverlay, RefineBackend, Session, Stage,
    TextConfig, TextPipeline,
};

/// A custom refine backend that uppercases everything. Demonstrates the
/// `RefineBackend` trait for embedders who want their own GEC pipeline.
struct UppercaseRefine;

impl RefineBackend for UppercaseRefine {
    fn refine(&self, text: &str) -> String {
        text.to_uppercase()
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let model_dir = args
        .first()
        .map(Path::new)
        .expect("usage: embed <model-dir> [wav-file]");
    let wav = args.get(1).map(Path::new);

    // ── Pattern 1: load config from the default file path ─────────────
    // Config::load(None) reads ~/.config/steno/config.toml if it exists,
    // otherwise uses built-in defaults.
    let cfg = Config::load(None)?;

    // ── Pattern 2: load with an explicit model directory ─────────────
    // Same precedence as the CLI --model flag.
    let engine = Engine::load_model(&cfg, Some(model_dir))?;
    println!("Model loaded.");

    // ── Pattern 3: one-shot transcription from a WAV file ──────────────
    if let Some(wav) = wav {
        let text = engine.transcribe_wav_file(wav)?;
        println!("Transcript: {text}");
    }

    // ── Pattern 4: reprocess stored text (no STT) ─────────────────────
    let cleaned = engine.process_text("hello comma world period");
    println!("Processed: {cleaned}");

    // ── Pattern 5: custom RefineBackend ───────────────────────────────
    // Swap in a custom pipeline after load. The engine is consumed.
    let pipeline = TextPipeline::with_refine(TextConfig::default(), Box::new(UppercaseRefine));
    let engine = engine.with_pipeline(pipeline);
    if let Some(wav) = wav {
        let text = engine.transcribe_wav_file(wav)?;
        println!("Uppercased: {text}");
    }

    // ── Pattern 6: Session with overlay + fail-closed typing ──────────
    // Session wraps Engine, drives overlay stages, and optionally types.
    // Typing is fail-closed: requires type_output = true AND a typer.
    let mut session = Session::builder(engine)
        .from_config(&cfg)
        .overlay(FnOverlay(|stage| {
            if stage != Stage::Hidden {
                eprintln!("[stage] {stage:?}");
            }
        }))
        .build();

    // Session::transcribe_wav_file drives the overlay stages automatically:
    //   Recording -> Transcribing -> Done (or Error)
    if let Some(wav) = wav {
        let text = session.transcribe_wav_file(wav)?;
        println!("Session transcript: {text}");
    }

    // ── Pattern 7: programmatic config (no file needed) ───────────────
    // Config::default() gives built-in defaults; override fields directly.
    let mut prog_cfg = Config::default();
    prog_cfg.provider = "cpu".to_string();
    prog_cfg.n_threads = 4;
    prog_cfg.type_output = false;
    prog_cfg.refine.enabled = true;

    // Add a vocabulary override via a custom pipeline:
    let mut dict_map = HashMap::new();
    dict_map.insert("steno".to_string(), "Dictate".to_string());
    let dict = Dictionary::from_map(dict_map);
    let pipeline = TextPipeline::new(TextConfig::default(), dict);
    let _engine3 = Engine::load_model(&prog_cfg, Some(model_dir))?.with_pipeline(pipeline);
    println!("Programmatic config loaded, provider = {}", prog_cfg.provider);

    // ── Pattern 8: drive overlay stages without a model ───────────────
    // Useful for testing UI without loading a GPU model.
    Session::drive_overlay_stages(&NullOverlay, 0, || {
        Ok::<_, anyhow::Error>(())
    })?;

    println!("All embedding patterns demonstrated.");
    Ok(())
}
