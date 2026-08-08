//! Background dictation daemon: keep the STT model resident in VRAM, grab
//! Caps Lock, hold-to-talk → type into the focused window.
//!
//! Lifecycle mirrors the old `speak` helper:
//!   steno start   : spawn daemon, print "running" + hotkey
//!   steno stop    : kill via pidfile
//!   steno status  : pid / not running
//!   steno restart : stop then start

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

use steno_core::api::{self, ApiError, ApiHandler, ApiResult, ServeOptions, UtteranceBuffer, authorize_token, decode_pcm_f32_le_b64};
use steno_core::audio;
use steno_core::config::{self, ApiConfig, Config};
use steno_core::dsp::{self, DspConfig};
use steno_core::stt::Transcriber;
use steno_core::text::{self, RefineConfig, TextConfig, TextPipeline};
use steno_platform::{Hotkey, HotkeyEvent, OutputMode, Stage, create as create_overlay, restore_caps_lock_mapping};
use crate::{Cli, emit_transcript};

pub fn cache_dir() -> Result<PathBuf> {
    // XDG Base Directory: `$XDG_CACHE_HOME/steno`, else `~/.cache/steno`.
    let dir = match std::env::var_os("XDG_CACHE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("steno"),
        _ => {
            let home = std::env::var_os("HOME").context(
                "HOME is unset and XDG_CACHE_HOME is unset — export one of them, or use --foreground",
            )?;
            PathBuf::from(home).join(".cache/steno")
        }
    };
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

fn pid_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("steno.pid"))
}

fn ready_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("steno.ready"))
}

fn log_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("steno.log"))
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

/// True only when the pid is actually a steno process. Without this a
/// recycled pid from a stale pidfile would get our signals.
fn pid_is_steno(pid: u32) -> bool {
    // After `cargo install` replaces a running binary, Linux reports
    // `steno (deleted)` — still our process; must not drop the pidfile.
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|name| {
            let base = name.strip_suffix(" (deleted)").unwrap_or(name.as_str());
            base == "steno"
        })
}

/// Serialize start/stop/restart across processes: two concurrent starts
/// must not race the pidfile and orphan an armed daemon.
fn lifecycle_lock() -> Result<File> {
    let path = cache_dir()?.join("steno.lock");
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
    if pid_alive(pid) && pid_is_steno(pid) {
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
    // Live daemon intentionally maps Caps → NoSymbol. Never "heal" under it.
    match is_running() {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(e) => {
            log::warn!("skipping Caps Lock repair; cannot check daemon pid: {e:#}");
            return;
        }
    }
    match restore_caps_lock_mapping() {
        Ok(true) => eprintln!(
            "steno: restored Caps Lock mapping (it was left dead by a previous unclean exit)"
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
            // Mid-transcription blocks in sherpa; allow several seconds before
            // escalating to SIGKILL (which skips Drop).
            for _ in 0..100 {
                if !pid_alive(pid) || !pid_is_steno(pid) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            // Re-validate identity before SIGKILL — PID recycle during the wait
            // must not kill an unrelated process.
            if pid_alive(pid)
                && pid_is_steno(pid)
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
            if pid_alive(pid) && pid_is_steno(pid) {
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
    let lock = lifecycle_lock()?;
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
        // Claim the pidfile under the lock so a concurrent `start` cannot
        // race past `is_running()` during the window before run_daemon's
        // write_pid. Then drop the flock — holding it across the daemon
        // lifetime would block `steno stop` forever.
        write_pid(std::process::id())?;
        drop(lock);
        return run_daemon(cli);
    }

    let exe = std::env::current_exe().context("cannot resolve steno binary path")?;
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
    let child = cmd.spawn().context("failed to spawn steno daemon")?;

    // Wait briefly for the pidfile under the lock so a concurrent `start`
    // cannot spawn a second worker, then release so `stop` can SIGTERM a
    // slow model load instead of blocking for up to 60s on the flock.
    let mut claimed = false;
    for _ in 0..50 {
        if read_pid(&pid_path()?) == Some(child.id()) && pid_is_steno(child.id()) {
            claimed = true;
            break;
        }
        if !pid_alive(child.id()) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    drop(lock);
    if !claimed && !pid_alive(child.id()) {
        let tail = log_tail();
        if tail.is_empty() {
            bail!("daemon exited before writing pidfile — see {}", log_file.display());
        }
        bail!("daemon exited before writing pidfile — see {}:
{tail}", log_file.display());
    }

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
        if pid_alive(child.id()) && pid_is_steno(child.id()) {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGKILL);
            }
        }
        // SIGKILL skips PidGuard — scrub pidfile / ready / possible API socket.
        let _ = fs::remove_file(pid_path()?);
        let _ = fs::remove_file(&ready);
        if let Ok(cfg) = Config::load(cli.config.as_deref()) {
            if cfg.api.enabled {
                if let Ok(sock) = resolve_api_socket(&cfg.api) {
                    let _ = fs::remove_file(&sock);
                }
            }
        }
        // Child may have remapped Caps Lock before dying / before ready.
        repair_caps_lock_if_needed();
        let tail = log_tail();
        if tail.is_empty() {
            bail!("daemon failed to become ready in 60s — see {}", log_file.display());
        }
        bail!("daemon failed to become ready — see {}:
{tail}", log_file.display());
    }
    let _ = fs::remove_file(&ready);

    println!("Dictation running (PID {}).", child.id());
    println!("Hotkey: hold Caps Lock to speak; any other key cancels.");
    println!("Log: {}", log_path()?.display());
    Ok(())
}

pub fn restart(cli: &Cli, foreground: bool) -> Result<()> {
    preflight(cli)?;
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
    ready: Option<PathBuf>,
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
        if let Some(ready) = self.ready.take() {
            let _ = fs::remove_file(ready);
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
            "daemon requires typing armed: run `steno config set type_output true` then `steno start`"
        );
    }
    let _ = config::resolve_model(cli.model.as_ref(), &cfg)?;
    // Legacy dictionary.toml parse errors (and other config issues) already
    // fail inside Config::load above — surface them before advertising
    // "running".
    let _ = text::Dictionary::from_map(cfg.dict.overrides.clone());
    let display = std::env::var_os("DISPLAY");
    let display_missing_or_empty = display.as_deref().is_none_or(std::ffi::OsStr::is_empty);
    if display_missing_or_empty {
        let wayland = std::env::var_os("WAYLAND_DISPLAY");
        let wayland_set = wayland.as_deref().is_some_and(|s| !s.is_empty());
        if wayland_set {
            #[cfg(target_os = "linux")]
            {
                bail!("{}", steno_platform::linux::selection::pure_wayland_hotkey_error());
            }
            #[cfg(not(target_os = "linux"))]
            {
                bail!(
                    "Caps Lock hotkey is unavailable on a pure Wayland session (WAYLAND_DISPLAY is set, DISPLAY is not)."
                );
            }
        } else {
            bail!("DISPLAY is unset or empty — the daemon needs X11 for Caps Lock and typing");
        }
    }
    Ok(())
}

/// Set on SIGTERM: the event loop checks it and exits gracefully so
/// Drop impls (grab release, pidfile removal) run.
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigterm(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

fn jail_wav_path(path: &Path) -> Result<PathBuf, ApiError> {
    let canon = path.canonicalize().map_err(|_| {
        ApiError::new(
            "wav_path not found",
            Some("pass an existing WAV under $HOME, $TMPDIR, or XDG cache/runtime".into()),
        )
    })?;
    if !canon.is_file() {
        return Err(ApiError::new(
            "wav_path is not a regular file",
            Some("pass a WAV file path, not a directory".into()),
        ));
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    for key in ["HOME", "TMPDIR", "XDG_RUNTIME_DIR", "XDG_CACHE_HOME", "XDG_CONFIG_HOME"] {
        if let Some(v) = std::env::var_os(key) {
            if !v.is_empty() {
                roots.push(PathBuf::from(v));
            }
        }
    }
    if roots.iter().all(|r| r.as_os_str() != "/tmp") {
        roots.push(PathBuf::from("/tmp"));
    }
    let allowed = roots.iter().any(|root| {
        root.canonicalize()
            .ok()
            .is_some_and(|root| canon.starts_with(root))
    });
    if !allowed {
        return Err(ApiError::new(
            "wav_path is outside the allowed directories",
            Some("place the WAV under $HOME or $TMPDIR (symlinks escaping those roots are rejected)".into()),
        ));
    }
    Ok(canon)
}

fn resolve_api_socket(api: &ApiConfig) -> Result<PathBuf> {
    match api.configured_path() {
        Some(p) => config::expand_tilde(p),
        None => api::default_socket_path(),
    }
}

/// NDJSON handler backed by the resident model. Typing is never armed here:
/// API clients only receive text; `type_output` stays config-only.
///
/// `utterance.*` and `transcribe` return JSON text only. The daemon process
/// still requires `type_output = true` to start (hotkey PTT path), but the
/// Emitter/typer is never invoked from these API ops: fail-closed for API.
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
        // Same-uid peers could otherwise point at any readable path. Jail to
        // HOME / TMPDIR / XDG_* after canonicalize (no symlink escape).
        let path = jail_wav_path(path)?;
        // Bound decode size so a huge WAV cannot OOM the resident daemon.
        if let Ok(meta) = fs::metadata(&path) {
            // ~3 min mono f32 @ 48 kHz ≈ 35 MB raw; reject absurd files early.
            const MAX_WAV_BYTES: u64 = 64 * 1024 * 1024;
            if meta.len() > MAX_WAV_BYTES {
                return Err(ApiError::new(
                    "wav_path exceeds 64 MiB",
                    Some("trim the WAV or send pcm_f32_b64 in smaller utterance.audio chunks".into()),
                ));
            }
        }
        let (raw, rate) = dsp::read_wav(&path).map_err(|_e| {
            ApiError::new(
                "failed to read wav_path",
                Some("pass a mono/stereo WAV under $HOME or $TMPDIR".into()),
            )
        })?;
        if raw.len() > api::MAX_UTTERANCE_SAMPLES.saturating_mul(4) {
            return Err(ApiError::new(
                "wav_path is too large to decode in-process",
                Some("trim the WAV or send pcm_f32_b64 in smaller utterance.audio chunks".into()),
            ));
        }
        let mut samples = dsp::resample(&raw, rate, dsp::STT_RATE).map_err(|e| {
            ApiError::new(
                format!("failed to resample WAV to 16 kHz: {e:#}"),
                Some("re-export the WAV with a positive sample rate".into()),
            )
        })?;
        if samples.len() > api::MAX_UTTERANCE_SAMPLES {
            return Err(ApiError::new(
                format!(
                    "wav exceeds max {} samples (~3 min at 16 kHz)",
                    api::MAX_UTTERANCE_SAMPLES
                ),
                Some("trim the recording before transcribe".into()),
            ));
        }
        let mut dc = dsp::DcBlock::new(dsp::STT_RATE);
        dc.process(&mut samples);
        dsp::normalize(&mut samples, self.dsp.target_rms, self.dsp.max_gain);
        Ok(samples)
    }

    fn decode_pcm_b64(&self, b64: &str) -> Result<Vec<f32>, ApiError> {
        let mut samples = decode_pcm_f32_le_b64(b64)?;
        if samples.len() > api::MAX_UTTERANCE_SAMPLES {
            return Err(ApiError::new(
                format!(
                    "pcm_f32_b64 exceeds max {} samples (~3 min at 16 kHz)",
                    api::MAX_UTTERANCE_SAMPLES
                ),
                Some("trim the PCM or send shorter utterance.audio chunks".into()),
            ));
        }
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
        authorize_token(token, self.token.as_deref())
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
        libc::signal(libc::SIGINT, on_sigterm as *const () as libc::sighandler_t);
        libc::signal(libc::SIGQUIT, on_sigterm as *const () as libc::sighandler_t);
    }

    // The worker owns its pidfile: the parent must not publish the pid
    // before grab + model load succeed, and a second worker must not
    // clobber a live one's entry.
    write_pid(std::process::id())?;
    let mut pid_guard = PidGuard {
        socket: None,
        ready: None,
        api_stop: None,
    };

    let cfg = Config::load(cli.config.as_deref())?;
    // Daemon's job is to type into the focused window. Fail closed: must
    // be armed in config, same rule as `--type`.
    if !cfg.type_output {
        bail!(
            "daemon requires typing armed: run `steno config set type_output true` then `steno start`"
        );
    }
    let mode = OutputMode::Type;
    let model = config::resolve_model(cli.model.as_ref(), &cfg)?;

    eprintln!(
        "steno: loading model {} …",
        model.display()
    );
    let transcriber = Arc::new(Transcriber::load(&model, cfg.n_threads, &cfg.provider)?);
    let dict = text::Dictionary::from_map(cfg.dict.overrides.clone());
    if !dict.is_empty() {
        eprintln!(
            "steno: dictionary ({} overrides) loaded from {}; edits apply after `steno restart`",
            cfg.dict.overrides.len(),
            config::default_config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "~/.config/steno/config.toml".into())
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
            .name("steno-api".into())
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
        eprintln!("steno: api socket {}", sock.display());
        pid_guard.socket = Some(sock);
        pid_guard.api_stop = Some(api_stop);
    }

    let mut hotkey = Hotkey::grab_caps_lock()?;
    // Ready: model loaded AND hotkey grabbed. Tell the parent.
    if let Ok(ready) = ready_path() {
        let _ = fs::write(&ready, format!("{}", std::process::id()));
        pid_guard.ready = Some(ready);
    }
    println!(
        "Dictation running (PID {}). Hold Caps Lock to speak.",
        std::process::id()
    );
    eprintln!("steno: model ready. Hotkey: hold Caps Lock.");

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
                    .name("steno-ptt".into())
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
                        if SHUTDOWN.load(Ordering::Relaxed) {
                            return Ok(());
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
                        if SHUTDOWN.load(Ordering::Relaxed) {
                            return Ok(());
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
                    if SHUTDOWN.load(Ordering::Relaxed) {
                        return Ok(());
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
                // Transcription is a blocking sherpa call; honor SIGTERM now.
                if SHUTDOWN.load(Ordering::Relaxed) {
                    return Ok(());
                }
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
    //! typing as API-controllable; unit-test the pure validation path without
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
            "steno-xdg-cache-{}-{}",
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
        assert_eq!(got, root.join("steno"));
        assert!(got.is_dir(), "cache_dir must create {}", got.display());
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod pid_guard_tests {
    use super::*;

    #[test]
    fn pid_guard_unlinks_ready_file_on_drop() {
        let tmp = std::env::temp_dir().join(format!(
            "steno-pidguard-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&tmp, "12345").unwrap();
        assert!(tmp.exists());

        {
            let _guard = PidGuard {
                socket: None,
                ready: Some(tmp.clone()),
                api_stop: None,
            };
        }

        assert!(!tmp.exists(), "ready file must be unlinked on drop");
    }
}

#[cfg(test)]
mod restart_tests {
    use super::*;

    #[test]
    fn restart_runs_preflight_before_stop() {
        let dir = std::env::temp_dir().join(format!(
            "steno-restart-preflight-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.toml");
        fs::write(&cfg_path, "type_output = false\n").unwrap();

        let cli = Cli {
            config: Some(cfg_path.clone()),
            model: None,
            device: None,
            raw: false,
            verbose: 0,
            r#type: false,
            stdout: false,
            input: None,
            list_devices: false,
            list_commands: false,
            command: None,
        };

        let res = restart(&cli, false);
        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("steno config set type_output true"),
            "restart error must offer exact actionable CLI command, got: {err_msg}"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
