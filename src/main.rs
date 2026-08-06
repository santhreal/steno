//! dictate — minimal offline speech-to-text dictation CLI (whisper.cpp).
//!
//! `dictate` records one utterance from the microphone (stopping at the end
//! of speech), transcribes it locally, applies the text pipeline, and prints
//! the result. `dictate clip.wav` transcribes a file instead. `--type` types
//! the result into the focused window instead of printing.

mod audio;
mod config;
mod dsp;
mod output;
mod stt;
mod text;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "dictate", version, about, long_about = None)]
struct Cli {
    /// Transcribe this WAV file instead of recording from the microphone.
    input: Option<PathBuf>,

    /// Path to a ggml whisper model (.bin). Overrides config.
    #[arg(short, long)]
    model: Option<PathBuf>,

    /// Path to a dictionary TOML with an [overrides] table.
    #[arg(short, long)]
    dictionary: Option<PathBuf>,

    /// Spoken language code (e.g. "en") or "auto".
    #[arg(short, long)]
    language: Option<String>,

    /// Type the result into the focused window via xdotool (X11) instead
    /// of printing. Bind `dictate --type` to a desktop shortcut for
    /// real dictation.
    #[arg(long)]
    r#type: bool,

    /// Skip the text pipeline (commands, dictionary, formatting).
    #[arg(long)]
    raw: bool,

    /// List microphone input devices and exit.
    #[arg(long)]
    list_devices: bool,

    /// Input device name substring (default: system default device).
    #[arg(long)]
    device: Option<String>,

    /// Config file path (default: ~/.config/dictate/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Print the voice command table and exit.
    #[arg(long)]
    list_commands: bool,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_log(cli.verbose);
    // Route whisper.cpp/ggml chatter through `log` (quiet by default, -v shows it).
    whisper_rs::install_logging_hooks();

    if cli.list_commands {
        for c in text::COMMANDS {
            println!("{}", c.doc);
        }
        return Ok(());
    }
    if cli.list_devices {
        for d in audio::list_input_devices()? {
            println!("{d}");
        }
        return Ok(());
    }

    let cfg = config::Config::load(cli.config.as_deref())?;
    let language = cli.language.as_deref().unwrap_or(&cfg.language);

    let samples = match &cli.input {
        Some(path) => {
            let (raw, rate) = dsp::read_wav(path)?;
            let mut s = dsp::resample(&raw, rate, dsp::WHISPER_RATE)?;
            let mut dc = dsp::DcBlock::new(dsp::WHISPER_RATE);
            dc.process(&mut s);
            dsp::normalize(&mut s, cfg.dsp.target_rms, cfg.dsp.max_gain);
            s
        }
        None => audio::record(&audio::RecordConfig {
            device: cli.device.clone(),
            max_duration: Duration::from_secs(cfg.max_record_secs),
            vad: cfg.vad,
            target_rms: cfg.dsp.target_rms,
            max_gain: cfg.dsp.max_gain,
        })?,
    };
    log::info!("{:.1}s of audio captured", samples.len() as f32 / dsp::WHISPER_RATE as f32);

    let model = config::resolve_model(cli.model.as_ref(), &cfg)?;
    let transcriber = stt::Transcriber::load(&model, language, cfg.n_threads)?;
    let transcript = transcriber.transcribe(&samples)?;
    log::debug!("raw transcript: {transcript:?}");

    let out = if cli.raw {
        transcript
    } else {
        let dict = text::Dictionary::load(config::resolve_dictionary(cli.dictionary.as_ref(), &cfg).as_deref())?;
        text::TextPipeline::new(cfg.text, dict).process(&transcript)
    };

    output::emit(
        &out,
        if cli.r#type {
            output::OutputMode::Type
        } else {
            output::OutputMode::Stdout
        },
    )
}

fn init_log(verbosity: u8) {
    let level = match verbosity {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Info,
        2 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level.as_str()))
        .init();
}
