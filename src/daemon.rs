//! Background dictation daemon: keep the whisper model resident, grab
//! Ctrl+Space, hold-to-talk → type into the focused window.
//!
//! Lifecycle mirrors the old `speak` helper:
//!   dictate start   — spawn daemon, print "running" + hotkey
//!   dictate stop    — kill via pidfile
//!   dictate status  — pid / not running
//!   dictate restart — stop then start

use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::audio;
use crate::config::{self, Config};
use crate::hotkey::{Hotkey, HotkeyEvent};
use crate::output::OutputMode;
use crate::overlay::{Overlay, Stage};
use crate::stt::Transcriber;
use crate::text::{self, TextPipeline};
use crate::{Cli, emit_transcript};

pub fn cache_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context(
        "HOME is unset — cannot locate ~/.cache/dictate (pass a writable HOME or use --foreground)",
    )?;
    let dir = PathBuf::from(home).join(".cache/dictate");
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

fn pid_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("dictate.pid"))
}

fn log_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("dictate.log"))
}

fn read_pid(path: &Path) -> Option<u32> {
    let s = fs::read_to_string(path).ok()?;
    s.trim().parse().ok()
}

fn pid_alive(pid: u32) -> bool {
    // signal 0: existence check, no delivery.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

pub fn is_running() -> Result<Option<u32>> {
    let path = pid_path()?;
    let Some(pid) = read_pid(&path) else {
        return Ok(None);
    };
    if pid_alive(pid) {
        Ok(Some(pid))
    } else {
        let _ = fs::remove_file(&path);
        Ok(None)
    }
}

pub fn status() -> Result<()> {
    match is_running()? {
        Some(pid) => {
            println!("Dictation running (PID {pid}).");
            println!("Hotkey: hold Ctrl+Space to speak.");
            println!("Log: {}", log_path()?.display());
        }
        None => println!("Dictation not running."),
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    let path = pid_path()?;
    match is_running()? {
        Some(pid) => {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            // Wait briefly for a clean exit; escalate once.
            for _ in 0..20 {
                if !pid_alive(pid) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if pid_alive(pid) {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
            let _ = fs::remove_file(&path);
            println!("Dictation stopped.");
        }
        None => {
            let _ = fs::remove_file(&path);
            println!("Dictation not running.");
        }
    }
    Ok(())
}

/// Spawn the daemon worker (or run it in-process when `foreground`).
pub fn start(cli: &Cli, foreground: bool) -> Result<()> {
    if let Some(pid) = is_running()? {
        println!("Dictation already running (PID {pid}).");
        println!("Hotkey: hold Ctrl+Space to speak.");
        return Ok(());
    }

    // Fail before claiming "running" or writing a pidfile.
    preflight(cli)?;

    if foreground {
        // Write pid for `dictate stop` even in foreground mode.
        write_pid(std::process::id())?;
        let _guard = PidGuard;
        return run_daemon(cli);
    }

    let exe = std::env::current_exe().context("cannot resolve dictate binary path")?;
    let log_file = log_path()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .with_context(|| format!("cannot open log {}", log_file.display()))?;
    let log_err = log.try_clone()?;

    let mut cmd = Command::new(&exe);
    cmd.arg("daemon");
    forward_flags(&mut cmd, cli);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    // Detach from the controlling terminal so closing the shell does not
    // SIGHUP the daemon. `pre_exec` runs in the child after fork, before exec.
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }

    let child = cmd.spawn().context("failed to spawn dictate daemon")?;
    write_pid(child.id())?;
    // Give it a moment to fail fast (missing model / grab conflict).
    thread::sleep(Duration::from_millis(300));
    if !pid_alive(child.id()) {
        let log = log_path()?;
        let _ = fs::remove_file(pid_path()?);
        let tail = fs::read_to_string(&log).unwrap_or_default();
        let last: String = tail
            .lines()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("
");
        if last.is_empty() {
            bail!("daemon exited immediately — see {}", log.display());
        }
        bail!("daemon exited immediately — see {}:
{}", log.display(), last);
    }

    println!("Dictation running (PID {}).", child.id());
    println!("Hotkey: hold Ctrl+Space to speak.");
    println!("Log: {}", log_path()?.display());
    Ok(())
}

pub fn restart(cli: &Cli, foreground: bool) -> Result<()> {
    let _ = stop();
    thread::sleep(Duration::from_millis(200));
    start(cli, foreground)
}

fn write_pid(pid: u32) -> Result<()> {
    let path = pid_path()?;
    let mut f = File::create(&path).with_context(|| format!("cannot write {}", path.display()))?;
    writeln!(f, "{pid}")?;
    Ok(())
}

struct PidGuard;
impl Drop for PidGuard {
    fn drop(&mut self) {
        if let Ok(path) = pid_path() {
            let _ = fs::remove_file(path);
        }
    }
}

fn forward_flags(cmd: &mut Command, cli: &Cli) {
    if let Some(m) = &cli.model {
        cmd.arg("--model").arg(m);
    }
    if let Some(d) = &cli.dictionary {
        cmd.arg("--dictionary").arg(d);
    }
    if let Some(l) = &cli.language {
        cmd.arg("--language").arg(l);
    }
    if let Some(d) = &cli.device {
        cmd.arg("--device").arg(d);
    }
    if let Some(c) = &cli.config {
        cmd.arg("--config").arg(c);
    }
    if cli.raw {
        cmd.arg("--raw");
    }
    for _ in 0..cli.verbose {
        cmd.arg("-v");
    }
}

/// Validate config + model path before we advertise "running".
fn preflight(cli: &Cli) -> Result<()> {
    let cfg = Config::load(cli.config.as_deref())?;
    if !cfg.type_output {
        bail!(
            "daemon requires typing armed: set `type_output = true` in {} then re-run `dictate start`",
            config::default_config_path()?.display()
        );
    }
    let _ = config::resolve_model(cli.model.as_ref(), &cfg)?;
    if std::env::var_os("DISPLAY").is_none() {
        bail!("DISPLAY is unset — the daemon needs X11 for Ctrl+Space and typing");
    }
    Ok(())
}

/// Foreground worker: load model once, grab hotkey, loop utterances.
pub fn run_daemon(cli: &Cli) -> Result<()> {
    // Ignore SIGHUP in case we were started without setsid.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    let cfg = Config::load(cli.config.as_deref())?;
    // Daemon's job is to type into the focused window. Fail closed: must
    // be armed in config, same rule as `--type`.
    if !cfg.type_output {
        bail!(
            "daemon requires typing armed: set `type_output = true` in {} then re-run `dictate start`",
            config::default_config_path()?.display()
        );
    }
    let mode = OutputMode::Type;
    let language = cli.language.as_deref().unwrap_or(&cfg.language);
    let model = config::resolve_model(cli.model.as_ref(), &cfg)?;

    eprintln!(
        "dictate: loading model {} …",
        model.display()
    );
    let transcriber = Transcriber::load(&model, language, cfg.n_threads)?;
    let dict = text::Dictionary::load(
        config::resolve_dictionary(cli.dictionary.as_ref(), &cfg)?.as_deref(),
    )?;
    let text_cfg = cfg.text;
    let overlay = Overlay::start(&cfg.ui);

    let mut hotkey = Hotkey::grab_ctrl_space()?;
    println!(
        "Dictation running (PID {}). Hold Ctrl+Space to speak.",
        std::process::id()
    );
    eprintln!("dictate: model ready. Hotkey: hold Ctrl+Space.");

    let record_cfg = audio::RecordConfig {
        device: cli.device.clone(),
        max_duration: Duration::from_secs(cfg.max_record_secs),
        vad: cfg.vad,
        target_rms: cfg.dsp.target_rms,
        max_gain: cfg.dsp.max_gain,
    };

    let mut held = false;
    loop {
        match hotkey.next_event(&mut held)? {
            HotkeyEvent::Press => {
                overlay.set(Stage::Recording);
                let stop = Arc::new(AtomicBool::new(false));
                let stop2 = stop.clone();
                let cfg2 = record_cfg.clone();
                let handle = thread::Builder::new()
                    .name("dictate-ptt".into())
                    .spawn(move || audio::record_while(&cfg2, &stop2))
                    .context("cannot spawn push-to-talk capture thread")?;

                // Wait for release (or another press path shouldn't happen).
                loop {
                    match hotkey.next_event(&mut held)? {
                        HotkeyEvent::Release => break,
                        HotkeyEvent::Press => continue,
                    }
                }
                stop.store(true, Ordering::Relaxed);
                let samples = match handle.join() {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        log::error!("capture failed: {e}");
                        overlay.set(Stage::Error);
                        overlay.flash(cfg.ui.done_flash_ms);
                        continue;
                    }
                    Err(_) => {
                        log::error!("capture thread panicked");
                        overlay.set(Stage::Error);
                        overlay.flash(cfg.ui.done_flash_ms);
                        continue;
                    }
                };
                if samples.is_empty() {
                    log::debug!("empty hold — skipped");
                    overlay.set(Stage::Done);
                    overlay.flash(cfg.ui.done_flash_ms);
                    continue;
                }
                overlay.set(Stage::Transcribing);
                let pipeline = TextPipeline::new(text_cfg, dict.clone());
                if let Err(e) = emit_transcript(
                    &samples,
                    &transcriber,
                    pipeline,
                    cli.raw,
                    mode,
                    &overlay,
                    cfg.ui.done_flash_ms,
                ) {
                    log::error!("transcript failed: {e}");
                    overlay.set(Stage::Error);
                    overlay.flash(cfg.ui.done_flash_ms);
                }
            }
            HotkeyEvent::Release => {
                // Spurious release with no press — ignore.
            }
        }
    }
}
