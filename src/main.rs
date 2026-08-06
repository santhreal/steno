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
mod overlay;
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
    /// of printing. SAFETY: requires `type_output = true` in the config
    /// file — typing is never enabled from the command line alone.
    #[arg(long)]
    r#type: bool,

    /// Print to stdout even when typing is armed in the config.
    #[arg(long)]
    stdout: bool,

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
    // Fail closed and fail FAST: a disarmed --type must error before the
    // microphone opens, the model loads, or xdotool is ever spawned.
    let mode = output_mode(cli.r#type, cli.stdout, cfg.type_output)?;

    let overlay = overlay::Overlay::start(&cfg.ui);

    // Warn about flags that have no effect in the chosen mode; silently
    // ignoring them would look like they worked.
    if cli.raw && cli.dictionary.is_some() {
        log::warn!("--raw skips the text pipeline; --dictionary is ignored");
    }
    if cli.input.is_some() && cli.device.is_some() {
        log::warn!("--device is ignored when transcribing a file");
    }

    let samples = match &cli.input {
        Some(path) => {
            anyhow::ensure!(
                !path.is_dir(),
                "'{}' is a directory — pass a WAV file",
                path.display()
            );
            let (raw, rate) = dsp::read_wav(path)?;
            let mut s = dsp::resample(&raw, rate, dsp::WHISPER_RATE)?;
            let mut dc = dsp::DcBlock::new(dsp::WHISPER_RATE);
            dc.process(&mut s);
            dsp::normalize(&mut s, cfg.dsp.target_rms, cfg.dsp.max_gain);
            s
        }
        None => {
            overlay.set(overlay::Stage::Recording);
            audio::record(&audio::RecordConfig {
                device: cli.device.clone(),
                max_duration: Duration::from_secs(cfg.max_record_secs),
                vad: cfg.vad,
                target_rms: cfg.dsp.target_rms,
                max_gain: cfg.dsp.max_gain,
            })?
        }
    };
    log::info!(
        "{:.1}s of audio captured",
        samples.len() as f32 / dsp::WHISPER_RATE as f32
    );
    overlay.set(overlay::Stage::Transcribing);

    let model = config::resolve_model(cli.model.as_ref(), &cfg)?;
    let transcriber = stt::Transcriber::load(&model, language, cfg.n_threads)?;

    // Stream: each finalized whisper segment goes through the pipeline
    // and out to the user immediately, while decode continues.
    let dict = text::Dictionary::load(
        config::resolve_dictionary(cli.dictionary.as_ref(), &cfg)?.as_deref(),
    )?;
    let pipeline = text::TextPipeline::new(cfg.text, dict);

    struct StreamCtx {
        emitter: output::Emitter,
        state: text::FmtState,
        /// Sink errors cannot cross the FFI callback; the first one lands here.
        error: Option<String>,
    }
    let ctx = std::rc::Rc::new(std::cell::RefCell::new(StreamCtx {
        emitter: output::Emitter::new(mode),
        state: text::FmtState::default(),
        error: None,
    }));
    let ctx2 = ctx.clone();
    let run_pipeline = move |raw: &str| {
        let mut c = ctx2.borrow_mut();
        if c.error.is_some() {
            return; // a dead emitter must not spam further errors
        }
        let (text, state) = if cli.raw {
            (raw.trim().to_string(), c.state)
        } else {
            pipeline.process_stream(raw, c.state)
        };
        c.state = state;
        if let Err(e) = c.emitter.push(&text) {
            log::error!("emit failed: {e}");
            c.error = Some(e.to_string());
        }
    };
    transcriber.transcribe_streaming(&samples, run_pipeline)?;

    // whisper-rs owns the callback after full(); borrow, don't unwrap.
    let mut ctx = ctx.borrow_mut();
    if let Some(e) = ctx.error.take() {
        overlay.set(overlay::Stage::Error);
        overlay.flash(cfg.ui.done_flash_ms); // keep the error stage visible
        anyhow::bail!("{e}");
    }
    let started = ctx.emitter.started();
    ctx.emitter.finish()?;
    if !started {
        log::debug!("empty transcript, nothing emitted");
    }
    overlay.set(overlay::Stage::Done);
    overlay.flash(cfg.ui.done_flash_ms);
    Ok(())
}

/// Decide where the transcript goes. Typing is fail-closed: the ONLY way
/// to enable it is `type_output = true` in the config file — a deliberate,
/// persistent act by the user. A bare `--type` flag is never sufficient,
/// so no script, test, or agent can make dictate inject keystrokes into a
/// live session without the user having armed their own config first.
fn output_mode(cli_type: bool, cli_stdout: bool, cfg_armed: bool) -> Result<output::OutputMode> {
    if cli_stdout {
        return Ok(output::OutputMode::Stdout);
    }
    if cli_type && !cfg_armed {
        anyhow::bail!(
            "typing is disarmed: set `type_output = true` in {} to arm it. \
             Typing injects real keystrokes into the focused window and is \
             deliberately not enableable from the command line alone.",
            config::default_config_path()?.display()
        );
    }
    Ok(if cli_type || cfg_armed {
        output::OutputMode::Type
    } else {
        output::OutputMode::Stdout
    })
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

#[cfg(test)]
mod tests {
    //! Regression tests for output-mode selection. WHY: typing injects
    //! real keystrokes into the focused window, so it must be fail-closed
    //! — armable only through the config file, never through a CLI flag.
    //! A test, script, or agent running `dictate --type` must error out
    //! before xdotool is spawned.
    use super::*;

    #[test]
    fn stdout_when_nothing_requests_typing() {
        assert!(matches!(
            output_mode(false, false, false).unwrap(),
            output::OutputMode::Stdout
        ));
    }

    #[test]
    fn bare_type_flag_is_refused_when_disarmed() {
        let err = output_mode(true, false, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("disarmed"),
            "error must name the blocker: {msg}"
        );
        assert!(
            msg.contains("type_output"),
            "error must name the arming key: {msg}"
        );
    }

    #[test]
    fn armed_config_types_with_or_without_flag() {
        assert!(matches!(
            output_mode(false, false, true).unwrap(),
            output::OutputMode::Type
        ));
        assert!(matches!(
            output_mode(true, false, true).unwrap(),
            output::OutputMode::Type
        ));
    }

    #[test]
    fn stdout_flag_overrides_armed_config() {
        assert!(matches!(
            output_mode(false, true, true).unwrap(),
            output::OutputMode::Stdout
        ));
        // --stdout also suppresses the disarmed-typing error: the user
        // explicitly asked for stdout, so nothing would be typed anyway.
        assert!(matches!(
            output_mode(true, true, false).unwrap(),
            output::OutputMode::Stdout
        ));
    }
}
