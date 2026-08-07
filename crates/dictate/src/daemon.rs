//! Background dictation daemon: keep the STT model resident in VRAM, grab
//! Caps Lock, hold-to-talk → type into the focused window.
//!
//! Lifecycle mirrors the old `speak` helper:
//!   dictate start   — spawn daemon, print "running" + hotkey
//!   dictate stop    — kill via pidfile
//!   dictate status  — pid / not running
//!   dictate restart — stop then start

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use dictate_core::api::{self, ApiError, ApiHandler, ApiResult, ServeOptions, UtteranceBuffer, decode_pcm_f32_le_b64};
use dictate_core::audio;
use dictate_core::config::{self, ApiConfig, Config};
use dictate_core::dsp::{self, DspConfig};
use dictate_core::stt::Transcriber;
use dictate_core::text::{self, RefineConfig, TextConfig, TextPipeline};
use dictate_platform::{Hotkey, HotkeyEvent, OutputMode, Stage, create as create_overlay, restore_caps_lock_mapping};
use crate::{Cli, emit_transcript};

pub fn cache_dir() -> Result<PathBuf> {
    // XDG Base Directory: `$XDG_CACHE_HOME/dictate`, else `~/.cache/dictate`.
    let dir = match std::env::var_os("XDG_CACHE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("dictate"),
        _ => {
            let home = std::env::var_os("HOME").context(
                "HOME is unset and XDG_CACHE_HOME is unset — export one of them, or use --foreground",
            )?;
            PathBuf::from(home).join(".cache/dictate")
        }
    };
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

fn pid_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("dictate.pid"))
}

fn ready_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("dictate.ready"))
}

fn log_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("dictate.log"))
}

fn read_pid(path: &Path) -> Option<u32> {
    let s = fs::read_to_string(path).ok()?;
    s.trim().parse().ok()
}

fn pid_alive(pid: u32) -> bool {
    // signal 0: existence check, no delivery. EPERM means the process
    // exists but belongs to another user — that is alive, not dead.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == 0 || unsafe { *libc::__errno_location() } == libc::EPERM
}

/// True only when the pid is actually a dictate process. Without this a
/// recycled pid from a stale pidfile would get our signals.
fn pid_is_dictate(pid: u32) -> bool {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|name| name == "dictate")
}

/// Serialize start/stop/restart across processes: two concurrent starts
/// must not race the pidfile and orphan an armed daemon.
fn lifecycle_lock() -> Result<File> {
    let path = cache_dir()?.join("dictate.lock");
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open lifecycle lock {}", path.display()))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot lock {}", path.display()));
    }
    Ok(f)
}

/// Last ~8 KiB of the append-only daemon log (never slurps the whole file).
fn log_tail() -> String {
    let Ok(path) = log_path() else { return String::new() };
    let Ok(mut f) = File::open(&path) else {
        return String::new();
    };
    use std::io::{Read, Seek, SeekFrom};
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(8192);
    let _ = f.seek(SeekFrom::Start(start));
    let mut buf = String::new();
    let _ = f.read_to_string(&mut buf);
    let last: Vec<&str> = buf.lines().collect();
    last.into_iter()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn is_running() -> Result<Option<u32>> {
    let path = pid_path()?;
    let Some(pid) = read_pid(&path) else {
        return Ok(None);
    };
    if pid_alive(pid) && pid_is_dictate(pid) {
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
            println!("Hotkey: hold Caps Lock to speak; any other key cancels.");
            println!("Log: {}", log_path()?.display());
        }
        None => println!("Dictation not running."),
    }
    Ok(())
}


/// Best-effort: if a prior SIGKILL left Caps Lock as NoSymbol, put it back.
fn repair_caps_lock_if_needed() {
    match restore_caps_lock_mapping() {
        Ok(true) => eprintln!(
            "dictate: restored Caps Lock mapping (it was left dead by a previous unclean exit)"
        ),
        Ok(false) => {}
        Err(e) => log::warn!("could not check/restore Caps Lock mapping: {e:#}"),
    }
}

pub fn stop() -> Result<()> {
    let _lock = lifecycle_lock()?;
    let path = pid_path()?;
    match is_running()? {
        Some(pid) => {
            if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EPERM) {
                    anyhow::bail!(
                        "cannot signal PID {pid}: permission denied — it is not your process; remove {} by hand if it is stale",
                        path.display()
                    );
                }
            }
            // Wait for a clean exit so Hotkey::Drop can restore Caps Lock.
            // Mid-transcription does not poll SHUTDOWN, so allow several seconds
            // before escalating to SIGKILL (which skips Drop).
            for _ in 0..100 {
                if !pid_alive(pid) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            if pid_alive(pid)
                && unsafe { libc::kill(pid as i32, libc::SIGKILL) } != 0
            {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EPERM) {
                    anyhow::bail!(
                        "cannot kill PID {pid}: permission denied — remove {} by hand if it is stale",
                        path.display()
                    );
                }
            }
            if pid_alive(pid) {
                anyhow::bail!(
                    "PID {pid} is still alive after SIGKILL — investigate manually; the pidfile {} was left in place",
                    path.display()
                );
            }
            let _ = fs::remove_file(&path);
            println!("Dictation stopped.");
        }
        None => {
            let _ = fs::remove_file(&path);
            println!("Dictation not running.");
        }
    }
    // SIGKILL (escalate below) skips Hotkey::Drop — repair NoSymbol Caps Lock.
    repair_caps_lock_if_needed();
    Ok(())
}

/// Spawn the daemon worker (or run it in-process when `foreground`).
pub fn start(cli: &Cli, foreground: bool) -> Result<()> {
    let _lock = lifecycle_lock()?;
    if let Some(pid) = is_running()? {
        println!("Dictation already running (PID {pid}).");
        println!("Hotkey: hold Caps Lock to speak; any other key cancels.");
        return Ok(());
    }

    // Heal Caps Lock if a previous SIGKILL left it as NoSymbol.
    repair_caps_lock_if_needed();

    // Fail before claiming "running" or writing a pidfile.
    preflight(cli)?;

    if foreground {
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

    // Readiness handshake: the worker writes the ready file only after
    // the model is loaded AND the hotkey is grabbed. A fixed sleep would
    // report "running" for a daemon about to die.
    let ready = ready_path()?;
    let _ = fs::remove_file(&ready);
    let child = cmd.spawn().context("failed to spawn dictate daemon")?;
    let mut ok = false;
    for _ in 0..600 {
        if read_pid(&ready) == Some(child.id()) {
            ok = true;
            break;
        }
        if !pid_alive(child.id()) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if !ok {
        unsafe {
            libc::kill(child.id() as i32, libc::SIGKILL);
        }
        // Child may have remapped Caps Lock before dying / before ready.
        repair_caps_lock_if_needed();
        let tail = log_tail();
        if tail.is_empty() {
            bail!("daemon failed to become ready in 60s — see {}", log_file.display());
        }
        bail!("daemon failed to become ready — see {}:\n{tail}", log_file.display());
    }
    let _ = fs::remove_file(&ready);

    println!("Dictation running (PID {}).", child.id());
    println!("Hotkey: hold Caps Lock to speak; any other key cancels.");
    println!("Log: {}", log_path()?.display());
    Ok(())
}

pub fn restart(cli: &Cli, foreground: bool) -> Result<()> {
    stop()?;
    start(cli, foreground)
}

fn write_pid(pid: u32) -> Result<()> {
    let path = pid_path()?;
    let mut f = File::create(&path).with_context(|| format!("cannot write {}", path.display()))?;
    writeln!(f, "{pid}")?;
    Ok(())
}

struct PidGuard {
    socket: Option<PathBuf>,
    api_stop: Option<Arc<AtomicBool>>,
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        if let Some(stop) = self.api_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(sock) = self.socket.take() {
            // Unblock a non-blocking accept loop waiting on WouldBlock / connect.
            let _ = std::os::unix::net::UnixStream::connect(&sock);
            let _ = fs::remove_file(&sock);
        }
        if let Ok(path) = pid_path() {
            let _ = fs::remove_file(path);
        }
    }
}

fn forward_flags(cmd: &mut Command, cli: &Cli) {
    if let Some(m) = &cli.model {
        cmd.arg("--model").arg(m);
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
    // Legacy dictionary.toml parse errors (and other config issues) already
    // fail inside Config::load above — surface them before advertising
    // "running".
    let _ = text::Dictionary::from_map(cfg.dict.overrides.clone());
    if std::env::var_os("DISPLAY").is_none() {
        bail!("DISPLAY is unset — the daemon needs X11 for Caps Lock and typing");
    }
    Ok(())
}

/// Set on SIGTERM: the event loop checks it and exits gracefully so
/// Drop impls (grab release, pidfile removal) run.
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigterm(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

fn resolve_api_socket(api: &ApiConfig) -> Result<PathBuf> {
    match api.configured_path() {
        Some(p) => config::expand_tilde(p),
        None => api::default_socket_path(),
    }
}

/// NDJSON handler backed by the resident model. Typing is never armed here —
/// API clients only receive text; `type_output` stays config-only.
///
/// `utterance.*` and `transcribe` return JSON text only. The daemon process
/// still requires `type_output = true` to start (hotkey PTT path), but the
/// Emitter/typer is never invoked from these API ops — fail-closed for API.
struct DaemonHandler {
    transcriber: Arc<Transcriber>,
    text_cfg: TextConfig,
    refine: RefineConfig,
    dict: text::Dictionary,
    dsp: DspConfig,
    model: PathBuf,
    type_output_armed: bool,
    token: Option<String>,
    raw: bool,
    stage: Arc<Mutex<String>>,
    utterance: Mutex<UtteranceBuffer>,
}

impl DaemonHandler {
    fn load_wav(&self, path: &Path) -> Result<Vec<f32>, ApiError> {
        let (raw, rate) = dsp::read_wav(path).map_err(|e| {
            ApiError::new(
                format!("failed to read wav_path: {e:#}"),
                Some("pass a mono/stereo WAV file path readable by the daemon".into()),
            )
        })?;
        let mut samples = dsp::resample(&raw, rate, dsp::STT_RATE).map_err(|e| {
            ApiError::new(
                format!("failed to resample WAV to 16 kHz: {e:#}"),
                Some("re-export the WAV with a positive sample rate".into()),
            )
        })?;
        let mut dc = dsp::DcBlock::new(dsp::STT_RATE);
        dc.process(&mut samples);
        dsp::normalize(&mut samples, self.dsp.target_rms, self.dsp.max_gain);
        Ok(samples)
    }

    fn decode_pcm_b64(&self, b64: &str) -> Result<Vec<f32>, ApiError> {
        let mut samples = decode_pcm_f32_le_b64(b64)?;
        dsp::normalize(&mut samples, self.dsp.target_rms, self.dsp.max_gain);
        Ok(samples)
    }

    fn decode_samples(&self, samples: &[f32]) -> Result<String, ApiError> {
        let out = Arc::new(Mutex::new(String::new()));
        let out2 = out.clone();
        self.transcriber
            .transcribe_streaming(samples, move |chunk| {
                if let Ok(mut g) = out2.lock() {
                    g.push_str(chunk);
                }
            })
            .map_err(|e| {
                ApiError::new(
                    format!("transcription failed: {e:#}"),
                    Some("check GPU / model health and retry; see daemon log".into()),
                )
            })?;
        let raw = out
            .lock()
            .map_err(|_| ApiError::new("transcript lock poisoned", None))?
            .clone();
        if self.raw {
            return Ok(raw.trim().to_string());
        }
        let pipeline = TextPipeline::with_refine(
            self.text_cfg,
            self.dict.clone(),
            self.refine.make_backend(),
        );
        let (text, _) = pipeline.process_stream(&raw, text::FmtState::default());
        Ok(text)
    }
}

impl ApiHandler for DaemonHandler {
    fn authorize(&self, token: Option<&str>) -> Result<(), ApiError> {
        let Some(expected) = self.token.as_deref() else {
            return Ok(());
        };
        if token == Some(expected) {
            Ok(())
        } else {
            Err(ApiError::new(
                "unauthorized",
                Some("set request token to match [api].token in config.toml".into()),
            ))
        }
    }

    fn ping(&self) -> ApiResult {
        Ok(Some(json!({"pong": true})))
    }

    fn status(&self) -> ApiResult {
        let stage = self
            .stage
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "idle".into());
        Ok(Some(json!({
            "pid": std::process::id(),
            "model": self.model.display().to_string(),
            "type_output_armed": self.type_output_armed,
            "stage": stage,
            "api": true,
        })))
    }

    fn transcribe(
        &self,
        wav_path: Option<PathBuf>,
        pcm_f32_b64: Option<String>,
    ) -> ApiResult {
        let has_wav = wav_path.as_ref().is_some_and(|p| !p.as_os_str().is_empty());
        let has_pcm = pcm_f32_b64.as_ref().is_some_and(|s| !s.is_empty());
        let samples = match (has_wav, has_pcm) {
            (true, false) => self.load_wav(wav_path.as_ref().unwrap())?,
            (false, true) => self.decode_pcm_b64(pcm_f32_b64.as_ref().unwrap())?,
            (true, true) => {
                return Err(ApiError::new(
                    "transcribe requires exactly one of wav_path or pcm_f32_b64",
                    Some("omit one payload field; do not send both".into()),
                ));
            }
            (false, false) => {
                return Err(ApiError::new(
                    "transcribe requires wav_path or pcm_f32_b64",
                    Some(
                        r#"send {"op":"transcribe","wav_path":"/path/file.wav"} or pcm_f32_b64"#
                            .into(),
                    ),
                ));
            }
        };
        let text = self.decode_samples(&samples)?;
        Ok(Some(json!({ "text": text })))
    }

    fn utterance_start(&self) -> ApiResult {
        let mut g = self
            .utterance
            .lock()
            .map_err(|_| ApiError::new("utterance lock poisoned", None))?;
        g.start();
        Ok(Some(json!({"started": true})))
    }

    fn utterance_audio(&self, pcm_f32_b64: String) -> ApiResult {
        let mut g = self
            .utterance
            .lock()
            .map_err(|_| ApiError::new("utterance lock poisoned", None))?;
        g.append_b64(&pcm_f32_b64)?;
        Ok(Some(json!({"buffered_samples": g.len()})))
    }

    fn utterance_stop(&self) -> ApiResult {
        let mut samples = {
            let mut g = self
                .utterance
                .lock()
                .map_err(|_| ApiError::new("utterance lock poisoned", None))?;
            g.stop()?
        };
        if samples.is_empty() {
            return Err(ApiError::new(
                "utterance is empty",
                Some("send utterance.audio with pcm_f32_b64 before utterance.stop".into()),
            ));
        }
        dsp::normalize(&mut samples, self.dsp.target_rms, self.dsp.max_gain);
        // Return text only — never type from API utterance ops (Emitter stays
        // on the hotkey path, which is gated by config type_output at daemon start).
        let text = self.decode_samples(&samples)?;
        Ok(Some(json!({ "text": text })))
    }

    fn utterance_cancel(&self) -> ApiResult {
        let mut g = self
            .utterance
            .lock()
            .map_err(|_| ApiError::new("utterance lock poisoned", None))?;
        g.cancel();
        Ok(Some(json!({"cancelled": true})))
    }

    fn shutdown(&self) -> ApiResult {
        SHUTDOWN.store(true, Ordering::Relaxed);
        Ok(Some(json!({"stopping": true})))
    }
}

/// Foreground worker: load model once, grab hotkey, loop utterances.
pub fn run_daemon(cli: &Cli) -> Result<()> {
    // Ignore SIGHUP in case we were started without setsid.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
    }

    // The worker owns its pidfile: the parent must not publish the pid
    // before grab + model load succeed, and a second worker must not
    // clobber a live one's entry.
    write_pid(std::process::id())?;
    let mut pid_guard = PidGuard {
        socket: None,
        api_stop: None,
    };

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
    let model = config::resolve_model(cli.model.as_ref(), &cfg)?;

    eprintln!(
        "dictate: loading model {} …",
        model.display()
    );
    let transcriber = Arc::new(Transcriber::load(&model, cfg.n_threads, &cfg.provider)?);
    let dict = text::Dictionary::from_map(cfg.dict.overrides.clone());
    if !dict.is_empty() {
        eprintln!(
            "dictate: dictionary ({} overrides) loaded from {}; edits apply after `dictate restart`",
            cfg.dict.overrides.len(),
            config::default_config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "~/.config/dictate/config.toml".into())
        );
    }
    let text_cfg = cfg.text;
    let overlay = create_overlay(&cfg.ui);
    log::debug!("overlay active={}", overlay.active());
    let stage = Arc::new(Mutex::new(String::from("idle")));

    // API socket: spawn before hotkey loop so clients can ping while Caps Lock
    // is idle. Typing remains fail-closed (config-only); handler never types.
    if cfg.api.enabled {
        let sock = resolve_api_socket(&cfg.api)?;
        let api_stop = Arc::new(AtomicBool::new(false));
        let require_same_uid = cfg.api.require_same_uid;
        let handler = DaemonHandler {
            transcriber: Arc::clone(&transcriber),
            text_cfg,
            refine: cfg.refine.clone(),
            dict: dict.clone(),
            dsp: cfg.dsp,
            model: model.clone(),
            type_output_armed: cfg.type_output,
            token: cfg.api.required_token().map(str::to_owned),
            raw: cli.raw,
            stage: Arc::clone(&stage),
            utterance: Mutex::new(UtteranceBuffer::default()),
        };
        let serve_path = sock.clone();
        let stop2 = Arc::clone(&api_stop);
        thread::Builder::new()
            .name("dictate-api".into())
            .spawn(move || {
                if let Err(e) = api::serve_unix_with(
                    &serve_path,
                    handler,
                    Some(stop2),
                    ServeOptions { require_same_uid },
                ) {
                    log::error!("api server exited: {e:#}");
                }
            })
            .context("cannot spawn API socket thread")?;
        eprintln!("dictate: api socket {}", sock.display());
        pid_guard.socket = Some(sock);
        pid_guard.api_stop = Some(api_stop);
    }

    let mut hotkey = Hotkey::grab_caps_lock()?;
    // Ready: model loaded AND hotkey grabbed. Tell the parent.
    if let Ok(ready) = ready_path() {
        let _ = fs::write(&ready, format!("{}", std::process::id()));
    }
    println!(
        "Dictation running (PID {}). Hold Caps Lock to speak.",
        std::process::id()
    );
    eprintln!("dictate: model ready. Hotkey: hold Caps Lock.");

    let record_cfg = audio::RecordConfig {
        device: cli.device.clone(),
        max_duration: Duration::from_secs(cfg.max_record_secs),
        vad: cfg.vad,
        target_rms: cfg.dsp.target_rms,
        max_gain: cfg.dsp.max_gain,
    };

    let mut held = false;
    loop {
        match hotkey.next_event_debug(&mut held, false, &SHUTDOWN)? {
            HotkeyEvent::Press => {
                if let Ok(mut g) = stage.lock() {
                    *g = "listening".into();
                }
                overlay.set(Stage::Recording);
                let stop = Arc::new(AtomicBool::new(false));
                let discard = Arc::new(AtomicBool::new(false));
                let stop2 = stop.clone();
                let discard2 = discard.clone();
                let cfg2 = record_cfg.clone();
                let handle = thread::Builder::new()
                    .name("dictate-ptt".into())
                    .spawn(move || audio::record_while(&cfg2, &stop2, &discard2))
                    .context("cannot spawn push-to-talk capture thread")?;

                // Wait for release (normal end) or any other key (cancel).
                let mut cancelled = false;
                loop {
                    match hotkey.next_event_debug(&mut held, false, &SHUTDOWN)? {
                        HotkeyEvent::Release => break,
                        HotkeyEvent::Press => continue,
                        HotkeyEvent::Cancel => {
                            cancelled = true;
                            break;
                        }
                        HotkeyEvent::Shutdown => {
                            cancelled = true;
                            break;
                        }
                    }
                }
                stop.store(true, Ordering::Relaxed);
                if cancelled {
                    discard.store(true, Ordering::Relaxed);
                }
                let samples = match handle.join() {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        log::error!("capture failed: {e}");
                        overlay.set(Stage::Error);
                        overlay.flash(cfg.ui.done_flash_ms);
                        overlay.set(Stage::Hidden);
                        if let Ok(mut g) = stage.lock() {
                            *g = "idle".into();
                        }
                        continue;
                    }
                    Err(_) => {
                        log::error!("capture thread panicked");
                        overlay.set(Stage::Error);
                        overlay.flash(cfg.ui.done_flash_ms);
                        overlay.set(Stage::Hidden);
                        if let Ok(mut g) = stage.lock() {
                            *g = "idle".into();
                        }
                        continue;
                    }
                };
                if cancelled {
                    // Drop the utterance: no transcription, no typing.
                    log::info!("utterance cancelled by keypress");
                    overlay.set(Stage::Hidden);
                    if let Ok(mut g) = stage.lock() {
                        *g = "idle".into();
                    }
                    if SHUTDOWN.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    continue;
                }
                if samples.is_empty() {
                    log::debug!("empty hold — skipped");
                    overlay.set(Stage::Hidden);
                    if let Ok(mut g) = stage.lock() {
                        *g = "idle".into();
                    }
                    continue;
                }
                if let Ok(mut g) = stage.lock() {
                    *g = "transcribing".into();
                }
                overlay.set(Stage::Transcribing);
                let pipeline = TextPipeline::with_refine(
                    text_cfg,
                    dict.clone(),
                    cfg.refine.make_backend(),
                );
                if let Err(e) = emit_transcript(
                    &samples,
                    &transcriber,
                    pipeline,
                    cli.raw,
                    mode,
                    overlay.as_ref(),
                    cfg.ui.done_flash_ms,
                ) {
                    log::error!("transcript failed: {e}");
                    overlay.set(Stage::Error);
                    overlay.flash(cfg.ui.done_flash_ms);
                    overlay.set(Stage::Hidden);
                }
                if let Ok(mut g) = stage.lock() {
                    *g = "idle".into();
                }
                // Typing just injected keys: drop any late raw events so
                // they cannot cancel the next utterance.
                hotkey.drain_pending();
            }
            HotkeyEvent::Release | HotkeyEvent::Cancel => {
                // Spurious release/cancel with no press — ignore.
            }
            HotkeyEvent::Shutdown => return Ok(()),
        }
    }
}

#[cfg(test)]
mod api_handler_tests {
    //! WHY: API must reject ambiguous/missing audio payloads and never treat
    //! typing as API-controllable — unit-test the pure validation path without
    //! loading a GPU model.
    use super::*;

    struct PayloadProbe;

    impl ApiHandler for PayloadProbe {
        fn transcribe(
            &self,
            wav_path: Option<PathBuf>,
            pcm_f32_b64: Option<String>,
        ) -> ApiResult {
            // Mirror DaemonHandler payload rules without STT.
            let has_wav = wav_path.as_ref().is_some_and(|p| !p.as_os_str().is_empty());
            let has_pcm = pcm_f32_b64.as_ref().is_some_and(|s| !s.is_empty());
            match (has_wav, has_pcm) {
                (true, false) | (false, true) => Ok(Some(json!({"ok": true}))),
                (true, true) => Err(ApiError::new(
                    "transcribe requires exactly one of wav_path or pcm_f32_b64",
                    Some("omit one payload field; do not send both".into()),
                )),
                (false, false) => Err(ApiError::new(
                    "transcribe requires wav_path or pcm_f32_b64",
                    Some(
                        r#"send {"op":"transcribe","wav_path":"/path/file.wav"} or pcm_f32_b64"#
                            .into(),
                    ),
                )),
            }
        }
    }

    #[test]
    fn transcribe_rejects_both_and_neither() {
        let h = PayloadProbe;
        let both = h.transcribe(Some(PathBuf::from("/tmp/a.wav")), Some("AAAA".into()));
        assert!(both.is_err());
        let neither = h.transcribe(None, None);
        assert!(neither.is_err());
        let wav_only = h.transcribe(Some(PathBuf::from("/tmp/a.wav")), None);
        assert!(wav_only.is_ok());
    }

    #[test]
    fn api_config_enabled_by_default() {
        assert!(ApiConfig::default().enabled);
        assert!(ApiConfig::default().require_same_uid);
    }

    #[test]
    fn utterance_ops_are_wired_not_nyi() {
        // WHY: DaemonHandler must implement utterance.* (buffer + decode path).
        // We cannot construct DaemonHandler without a GPU model here, so assert
        // the shared UtteranceBuffer contract the handler embeds.
        let mut buf = UtteranceBuffer::default();
        buf.start();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0.5f32.to_le_bytes());
        let b64 = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        };
        buf.append_b64(&b64).unwrap();
        buf.cancel();
        let err = buf.stop().expect_err("stop after cancel");
        assert!(err.error.contains("no active utterance"));
    }
}

#[cfg(test)]
mod cache_dir_tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn cache_dir_honors_xdg_cache_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dictate-xdg-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let prev = std::env::var_os("XDG_CACHE_HOME");
        // SAFETY: serialized under ENV_LOCK; restored before unlock.
        unsafe { std::env::set_var("XDG_CACHE_HOME", &root) };
        let got = cache_dir();
        match &prev {
            Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
        let got = got.expect("cache_dir");
        assert_eq!(got, root.join("dictate"));
        assert!(got.is_dir(), "cache_dir must create {}", got.display());
        let _ = fs::remove_dir_all(&root);
    }
}

