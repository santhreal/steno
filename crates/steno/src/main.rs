//! steno: minimal offline speech-to-text dictation CLI
//! (sherpa-onnx Parakeet TDT, GPU-resident).
//!
//! Binary name: `steno`.
//!
//! One-shot: `steno` records one utterance (VAD endpoint), transcribes it
//! locally, and prints or types the result. `steno clip.wav` transcribes a
//! file. Daemon: `steno start` keeps the model loaded system-wide; hold
//! Caps Lock to dictate into the focused window; `steno stop` tears it down.

mod api_cmd;
mod config_cmd;
mod daemon;

use anyhow::Result;
use clap::{Parser, Subcommand};
use steno_core::config::{self, Config};
use steno_core::{Engine, audio, dsp, text};
use steno_platform::{self as platform, OverlayBackend, OutputMode, Stage, create as create_overlay};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "steno", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Transcribe this WAV file instead of recording from the microphone.
    input: Option<PathBuf>,

    /// Path to a sherpa-onnx model directory. Overrides config.
    #[arg(short, long, global = true)]
    pub model: Option<PathBuf>,

    /// Type the result into the focused window via platform keystroke emitter instead
    /// of printing. SAFETY: requires `type_output = true` in the config
    /// file: typing is never enabled from the command line alone.
    /// Arm it persistently with `steno config set type_output true`.
    #[arg(long)]
    r#type: bool,

    /// Print to stdout even when typing is armed in the config.
    #[arg(long)]
    stdout: bool,

    /// Skip the refinement pipeline (commands, dictionary overrides, formatting, rules).
    #[arg(long, global = true)]
    pub raw: bool,

    /// List microphone input devices and exit.
    #[arg(long)]
    list_devices: bool,

    /// Input device name substring (default: system default device).
    #[arg(long, global = true)]
    pub device: Option<String>,

    /// Config file path (default: ~/.config/steno/config.toml).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Print the voice command table and exit.
    #[arg(long)]
    list_commands: bool,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Load the model system-wide and listen for Caps Lock (hold to talk).
    Start {
        /// Run in this terminal instead of detaching.
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the background dictation daemon.
    Stop,
    /// Show whether the daemon is running (pidfile check).
    Status,
    /// Restart the background daemon.
    Restart {
        /// Run in this terminal instead of detaching.
        #[arg(long)]
        foreground: bool,
    },
    /// Repair Caps Lock key mapping if it was left dead by an unclean daemon exit.
    Repair {
        /// Force repair even if the daemon pidfile indicates it is running.
        #[arg(long)]
        force: bool,
    },
    Ping {
        /// Override API socket path (default: config / XDG runtime).
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Daemon NDJSON API helpers (`steno api status`).
    Api {
        #[command(subcommand)]
        command: ApiCommand,
    },
    /// Read or write persistent config keys (`steno config show`).
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// List or select sherpa-onnx model directories.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// List or set the overlay theme (`ui.theme`).
    Theme {
        #[command(subcommand)]
        command: ThemeCommand,
    },
    /// Internal worker process started by `steno start`.
    #[command(hide = true)]
    Daemon,
}

#[derive(Subcommand, Debug)]
enum ApiCommand {
    /// Print daemon API status JSON (stage, pid, model, …).
    Status {
        /// Override API socket path (default: config / XDG runtime).
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Probe the daemon NDJSON API socket (pong + latency).
    Ping {
        /// Override API socket path (default: config / XDG runtime).
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Print the config path, settable keys, and refine dictionary entries count.
    Show,
    /// Get one settable config key from the TOML file.
    Get {
        /// Dotted key (see `steno config set --help` / `list_settable_keys`).
        key: String,
    },
    /// Set one settable config key (creates the file with the key if missing).
    ///
    /// Arm typing with `steno config set type_output true`: the only
    /// persistent path; `--type` alone never enables keystroke injection.
    Set {
        /// Dotted key (`provider`, `ui.theme`, `type_output`, …).
        key: String,
        /// Value as text; booleans/integers are parsed by key.
        value: String,
    },
}

#[derive(Subcommand, Debug)]
enum ModelCommand {
    /// List model directories under the default models dir.
    List,
    /// Select the active model (writes `model_path`).
    Use {
        /// Model directory name under the models dir, or an absolute/`~/` path.
        name_or_path: String,
        /// Also write `provider` (`cuda` or `cpu`).
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ThemeCommand {
    /// List built-in overlay themes and null aliases.
    List,
    /// Set `ui.theme` (`pill`/`mono`/`dusk`/`dawn`/`contrast`, or `null`/`none`/`off`).
    Set {
        name: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_log(cli.verbose);

    match cli.command {
        Some(Command::Start { foreground }) => return daemon::start(&cli, foreground),
        Some(Command::Stop) => return daemon::stop(),
        Some(Command::Status) => return daemon::status(),
        Some(Command::Restart { foreground }) => return daemon::restart(&cli, foreground),
        Some(Command::Repair { force }) => {
            match daemon::repair_caps_lock(force) {
                Ok(true) => {
                    println!("Caps Lock mapping successfully restored.");
                    return Ok(());
                }
                Ok(false) => {
                    println!("Caps Lock mapping is normal (no repair needed).");
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
        Some(Command::Ping { socket }) => {
            return api_cmd::ping(cli.config.as_deref(), socket);
        }
        Some(Command::Api { command }) => {
            return match command {
                ApiCommand::Status { socket } => {
                    api_cmd::api_status(cli.config.as_deref(), socket)
                }
                ApiCommand::Ping { socket } => {
                    api_cmd::ping(cli.config.as_deref(), socket)
                }
            };
        }
        Some(Command::Config { command }) => {
            return match command {
                ConfigCommand::Show => config_cmd::config_show(cli.config.as_deref()),
                ConfigCommand::Get { key } => {
                    config_cmd::config_get_cmd(cli.config.as_deref(), &key)
                }
                ConfigCommand::Set { key, value } => {
                    config_cmd::config_set_cmd(cli.config.as_deref(), &key, &value)
                }
            };
        }
        Some(Command::Model { command }) => {
            return match command {
                ModelCommand::List => config_cmd::model_list(cli.config.as_deref()),
                ModelCommand::Use {
                    name_or_path,
                    provider,
                } => config_cmd::model_use(
                    cli.config.as_deref(),
                    &name_or_path,
                    provider.as_deref(),
                ),
            };
        }
        Some(Command::Theme { command }) => {
            return match command {
                ThemeCommand::List => config_cmd::theme_list(cli.config.as_deref()),
                ThemeCommand::Set { name } => {
                    config_cmd::theme_set(cli.config.as_deref(), &name)
                }
            };
        }
        Some(Command::Daemon) => return daemon::run_daemon(&cli),
        None => {}
    }

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

    let cfg = Config::load(cli.config.as_deref())?;
    // Fail closed and fail FAST: a disarmed --type must error before the
    // microphone opens, the model loads, or xdotool is ever spawned.
    let mode = output_mode(cli.r#type, cli.stdout, cfg.type_output)?;

    let overlay = create_overlay(&cfg.ui);
    log::debug!("overlay active={}", overlay.active());

    // Warn about flags that have no effect in the chosen mode; silently
    // ignoring them would look like they worked.
    if cli.raw && !cfg.dict.overrides.is_empty() {
        log::warn!("--raw skips the refinement pipeline; [refine] dictionary entries are ignored");
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
            let mut s = dsp::resample(&raw, rate, dsp::STT_RATE)?;
            let mut dc = dsp::DcBlock::new(dsp::STT_RATE);
            dc.process(&mut s);
            dsp::normalize(&mut s, cfg.dsp.target_rms, cfg.dsp.max_gain);
            s
        }
        None => {
            overlay.set(Stage::Recording);
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
        samples.len() as f32 / dsp::STT_RATE as f32
    );
    overlay.set(Stage::Transcribing);

    // One-shot: prefer Engine (model + dictionary + pipeline in one place).
    let mut eng_cfg = cfg.clone();
    if let Some(model) = cli.model.as_ref() {
        eng_cfg.model_path = Some(model.clone());
    }
    let engine = Engine::load(&eng_cfg)?;
    let text_out = if cli.raw {
        engine.transcribe_f32_raw(&samples)?
    } else {
        engine.transcribe_f32(&samples)?
    };
    let mut emitter = platform::Emitter::new(mode);
    if let Err(e) = emitter.push(&text_out) {
        overlay.set(Stage::Error);
        overlay.flash(cfg.ui.done_flash_ms);
        overlay.set(Stage::Hidden);
        return Err(e);
    }
    emitter.finish()?;
    if text_out.is_empty() {
        log::debug!("empty transcript, nothing emitted");
    }
    overlay.set(Stage::Done);
    overlay.flash(cfg.ui.done_flash_ms);
    overlay.set(Stage::Hidden);
    Ok(())
}

/// Run the text pipeline + emitter over `samples`. Shared by one-shot and daemon.
pub(crate) fn emit_transcript(
    samples: &[f32],
    transcriber: &steno_core::Transcriber,
    pipeline: text::TextPipeline,
    raw: bool,
    mode: OutputMode,
    overlay: &dyn OverlayBackend,
    flash_ms: u64,
) -> Result<()> {
    struct StreamCtx {
        emitter: platform::Emitter,
        state: text::FmtState,
        /// Sink errors cannot cross the FFI callback; the first one lands here.
        error: Option<String>,
    }
    let ctx = std::rc::Rc::new(std::cell::RefCell::new(StreamCtx {
        emitter: platform::Emitter::new(mode),
        state: text::FmtState::default(),
        error: None,
    }));
    let ctx2 = ctx.clone();
    let run_pipeline = move |chunk: &str| {
        let mut c = ctx2.borrow_mut();
        if c.error.is_some() {
            return; // a dead emitter must not spam further errors
        }
        let (text, state) = if raw {
            (chunk.trim().to_string(), c.state)
        } else {
            pipeline.process_stream(chunk, c.state)
        };
        c.state = state;
        if let Err(e) = c.emitter.push(&text) {
            log::error!("emit failed: {e}");
            c.error = Some(e.to_string());
        }
    };
    transcriber.transcribe_streaming(samples, run_pipeline)?;

    // The sink closure records its own errors; borrow, don't unwrap.
    let mut ctx = ctx.borrow_mut();
    if let Some(e) = ctx.error.take() {
        overlay.set(Stage::Error);
        overlay.flash(flash_ms);
        overlay.set(Stage::Hidden);
        anyhow::bail!("{e}");
    }
    let started = ctx.emitter.started();
    ctx.emitter.finish()?;
    if !started {
        log::debug!("empty transcript, nothing emitted");
    }
    overlay.set(Stage::Done);
    overlay.flash(flash_ms);
    overlay.set(Stage::Hidden);
    Ok(())
}

/// Decide where the transcript goes. Typing is fail-closed: the ONLY way
/// to enable it is `type_output = true` in the config file: a deliberate,
/// persistent act by the user. A bare `--type` flag is never sufficient,
/// so no script, test, or agent can make steno inject keystrokes into a
/// live session without the user having armed their own config first.
fn output_mode(cli_type: bool, cli_stdout: bool, cfg_armed: bool) -> Result<OutputMode> {
    if cli_stdout {
        return Ok(OutputMode::Stdout);
    }
    if cli_type && !cfg_armed {
        anyhow::bail!(
            "typing is disarmed: run `steno config set type_output true` \
             (writes {}) to arm it. Typing injects real keystrokes into the \
             focused window and is deliberately not enableable from the \
             command line alone.",
            config::default_config_path()?.display()
        );
    }
    Ok(if cli_type || cfg_armed {
        OutputMode::Type
    } else {
        OutputMode::Stdout
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
    //! -- armable only through the config file, never through a CLI flag.
    //! A test, script, or agent running `steno --type` must error out
    //! before xdotool is spawned.
    use super::*;

    #[test]
    fn stdout_when_nothing_requests_typing() {
        assert!(matches!(
            output_mode(false, false, false).unwrap(),
            OutputMode::Stdout
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
            OutputMode::Type
        ));
        assert!(matches!(
            output_mode(true, false, true).unwrap(),
            OutputMode::Type
        ));
    }

    #[test]
    fn stdout_flag_overrides_armed_config() {
        assert!(matches!(
            output_mode(false, true, true).unwrap(),
            OutputMode::Stdout
        ));
        // --stdout also suppresses the disarmed-typing error: the user
        // explicitly asked for stdout, so nothing would be typed anyway.
        assert!(matches!(
            output_mode(true, true, false).unwrap(),
            OutputMode::Stdout
        ));
    }

    #[test]
    fn clap_parses_ping_and_api_status() {
        // WHY: --help and subcommand wiring must expose ping / api status
        // without requiring a live daemon — parse-only regression.
        let ping = Cli::try_parse_from(["steno", "ping"]).expect("ping");
        assert!(matches!(
            ping.command,
            Some(Command::Ping { socket: None })
        ));

        let ping_sock =
            Cli::try_parse_from(["steno", "ping", "--socket", "/tmp/x.sock"]).expect("ping sock");
        assert!(matches!(
            &ping_sock.command,
            Some(Command::Ping {
                socket: Some(p)
            }) if p == std::path::Path::new("/tmp/x.sock")
        ));

        let api_ping = Cli::try_parse_from(["steno", "api", "ping"]).expect("api ping");
        assert!(matches!(
            &api_ping.command,
            Some(Command::Api {
                command: ApiCommand::Ping { socket: None }
            })
        ));

        let api_ping_sock =
            Cli::try_parse_from(["steno", "api", "ping", "--socket", "/tmp/x.sock"]).expect("api ping sock");
        assert!(matches!(
            &api_ping_sock.command,
            Some(Command::Api {
                command: ApiCommand::Ping {
                    socket: Some(p)
                }
            }) if p == std::path::Path::new("/tmp/x.sock")
        ));

        let status = Cli::try_parse_from(["steno", "api", "status"]).expect("api status");
        assert!(matches!(
            &status.command,
            Some(Command::Api {
                command: ApiCommand::Status { socket: None }
            })
        ));
    }

    #[test]
    fn clap_parses_config_model_theme() {
        // WHY: help/subcommand wiring must expose config/model/theme without
        // a daemon — parse-only regression for the persistent config surface.
        let show = Cli::try_parse_from(["steno", "config", "show"]).expect("config show");
        assert!(matches!(
            show.command,
            Some(Command::Config {
                command: ConfigCommand::Show
            })
        ));

        let get = Cli::try_parse_from(["steno", "config", "get", "provider"]).expect("get");
        assert!(matches!(
            get.command,
            Some(Command::Config {
                command: ConfigCommand::Get { ref key }
            }) if key == "provider"
        ));

        let set = Cli::try_parse_from([
            "steno",
            "config",
            "set",
            "type_output",
            "true",
        ])
        .expect("set");
        assert!(matches!(
            set.command,
            Some(Command::Config {
                command: ConfigCommand::Set { ref key, ref value }
            }) if key == "type_output" && value == "true"
        ));

        let themes = Cli::try_parse_from(["steno", "theme", "list"]).expect("theme list");
        assert!(matches!(
            themes.command,
            Some(Command::Theme {
                command: ThemeCommand::List
            })
        ));

        let model = Cli::try_parse_from([
            "steno",
            "model",
            "use",
            "parakeet",
            "--provider",
            "cpu",
        ])
        .expect("model use");
        assert!(matches!(
            model.command,
            Some(Command::Model {
                command: ModelCommand::Use {
                    ref name_or_path,
                    provider: Some(ref p),
                }
            }) if name_or_path == "parakeet" && p == "cpu"
        ));

        // Global --config still attaches to nested commands.
        let with_cfg = Cli::try_parse_from([
            "steno",
            "--config",
            "/tmp/steno-test.toml",
            "theme",
            "set",
            "dusk",
        ])
        .expect("theme set --config");
        assert_eq!(
            with_cfg.config.as_deref(),
            Some(std::path::Path::new("/tmp/steno-test.toml"))
        );
        assert!(matches!(
            with_cfg.command,
            Some(Command::Theme {
                command: ThemeCommand::Set { ref name }
            }) if name == "dusk"
        ));
    }
}
