//! NDJSON IPC server (accept loop + ApiHandler dispatch).
//!
//! Accepts connections, frames by newline, dispatches to [`ApiHandler`], and
//! writes a [`Response`] line per request. Event fan-out is reserved for a
//! later pass (handlers can still encode [`Event`] lines themselves).
//!
//! On Unix this uses an `AF_UNIX` stream socket; on Windows it uses a named
//! pipe at `\\.\pipe\steno`. The public API (`serve_unix` / `serve_unix_until`
//! / `serve_unix_with`) is identical on both platforms.


use crate::api::protocol::{
    Event, Op, Request, Response, decode_line, encode_line, peek_request_id,
};
use anyhow::{Context, Result, bail};
use crate::engine::Engine;
use base64::Engine as _;
use serde_json::{Value, json};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_IO_PENDING,
    ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    PIPE_UNLIMITED_INSTANCES,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Error returned by handler methods. Mapped onto a failed [`Response`].
#[derive(Debug, Clone)]
pub struct ApiError {
    /// Error message describing what went wrong.
    pub error: String,
    /// Optional remediation hint shown to the caller.
    pub hint: Option<String>,
}

impl ApiError {
    /// Construct an error with a message and an optional hint.
    pub fn new(error: impl Into<String>, hint: impl Into<Option<String>>) -> Self {
        Self {
            error: error.into(),
            hint: hint.into(),
        }
    }

    /// Construct a standard "not implemented" error for the given op name.
    pub fn not_implemented(op: &str) -> Self {
        Self::new(
            format!("{op} is not implemented"),
            Some(
                "wire this op in the daemon ApiHandler (main thread) before calling it"
                    .into(),
            ),
        )
    }
}

/// Return type for handler methods: an optional JSON result or an error.
pub type ApiResult = std::result::Result<Option<Value>, ApiError>;

/// Hard cap on a single NDJSON request line (bytes). Prevents a same-uid
/// client from OOMing the daemon with a multi-GB `pcm_f32_b64` frame.
pub const MAX_API_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Hard cap on buffered utterance PCM samples (~3 minutes at 16 kHz mono).
pub const MAX_UTTERANCE_SAMPLES: usize = 16_000 * 180;


/// Sync callback surface matching each protocol op.
///
/// Default methods: `ping` / `status` succeed with static JSON; transcribe and
/// utterance ops fail closed with `"not implemented"`.
pub trait ApiHandler {
    /// Optional shared-secret check. Default: accept any token (including none).
    fn authorize(&self, _token: Option<&str>) -> Result<(), ApiError> {
        Ok(())
    }

    /// Handle `ping` operation: return health status.
    fn ping(&self) -> ApiResult {
        Ok(Some(json!({"pong": true})))
    }

    /// Handle `status` operation: return process metadata and state.
    fn status(&self) -> ApiResult {
        Ok(Some(json!({
            "pid": std::process::id(),
            "stage": "idle",
            "type_output_armed": false,
        })))
    }

    /// Handle `transcribe` operation: transcribe audio from file path or base64 PCM.
    fn transcribe(
        &self,
        _wav_path: Option<PathBuf>,
        _pcm_f32_b64: Option<String>,
    ) -> ApiResult {
        Err(ApiError::not_implemented("transcribe"))
    }

    /// Handle `utterance.start` operation: initialize streaming utterance session.
    fn utterance_start(&self) -> ApiResult {
        Err(ApiError::not_implemented("utterance.start"))
    }

    /// Handle `utterance.audio` operation: append audio data to active utterance session.
    fn utterance_audio(&self, _pcm_f32_b64: String) -> ApiResult {
        Err(ApiError::not_implemented("utterance.audio"))
    }

    /// Handle `utterance.stop` operation: finalize active utterance session and return transcript.
    fn utterance_stop(&self) -> ApiResult {
        Err(ApiError::not_implemented("utterance.stop"))
    }

    /// Handle `utterance.cancel` operation: discard active utterance session without transcribing.
    fn utterance_cancel(&self) -> ApiResult {
        Err(ApiError::not_implemented("utterance.cancel"))
    }

    /// Handle `shutdown` operation: request daemon server termination.
    fn shutdown(&self) -> ApiResult {
        Err(ApiError::not_implemented("shutdown"))
    }
}

/// Handler that relies entirely on [`ApiHandler`] default stubs.
#[derive(Debug, Default, Clone, Copy)]
pub struct StubHandler;

impl ApiHandler for StubHandler {}

/// Decode standard-base64 little-endian f32 PCM bytes (no DSP / normalize).
pub fn decode_pcm_f32_le_b64(b64: &str) -> Result<Vec<f32>, ApiError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim().as_bytes())
        .map_err(|e| {
            ApiError::new(
                format!("invalid pcm_f32_b64: {e}"),
                Some("send standard base64 of little-endian f32 PCM at 16 kHz mono".into()),
            )
        })?;
    if bytes.len() % 4 != 0 {
        return Err(ApiError::new(
            format!(
                "pcm_f32_b64 length {} is not a multiple of 4",
                bytes.len()
            ),
            Some("encode little-endian f32 samples (4 bytes each) before base64".into()),
        ));
    }
    let n = bytes.len() / 4;
    // Reject before allocating a multi-million-sample Vec when a single
    // NDJSON frame (up to MAX_API_LINE_BYTES) would exceed the utterance cap.
    if n > MAX_UTTERANCE_SAMPLES {
        return Err(ApiError::new(
            format!(
                "pcm_f32_b64 exceeds max {MAX_UTTERANCE_SAMPLES} samples (~3 min at 16 kHz)"
            ),
            Some("trim the PCM or send shorter utterance.audio chunks".into()),
        ));
    }
    let mut samples = Vec::with_capacity(n);
    for chunk in bytes.chunks_exact(4) {
        samples.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(samples)
}

/// In-memory PCM buffer for `utterance.start|audio|stop|cancel` (one active
/// utterance per handler / daemon session).
#[derive(Debug, Default)]
pub struct UtteranceBuffer {
    active: bool,
    samples: Vec<f32>,
}

impl UtteranceBuffer {
    /// Begin a new utterance session, clearing any prior samples.
    pub fn start(&mut self) {
        self.active = true;
        self.samples.clear();
    }

    /// Decode and append base64 PCM samples to the active utterance.
    pub fn append_b64(&mut self, pcm_f32_b64: &str) -> Result<(), ApiError> {
        if !self.active {
            return Err(ApiError::new(
                "no active utterance",
                Some("call utterance.start before utterance.audio".into()),
            ));
        }
        let chunk = match decode_pcm_f32_le_b64(pcm_f32_b64) {
            Ok(c) => c,
            Err(e) => {
                if e.error.contains("exceeds max") {
                    self.active = false;
                    self.samples.clear();
                }
                return Err(e);
            }
        };
        let new_len = self.samples.len().saturating_add(chunk.len());
        if new_len > MAX_UTTERANCE_SAMPLES {
            self.active = false;
            self.samples.clear();
            return Err(ApiError::new(
                format!(
                    "utterance exceeds max {MAX_UTTERANCE_SAMPLES} samples (~3 min at 16 kHz)"
                ),
                Some("call utterance.cancel and send a shorter utterance".into()),
            ));
        }
        self.samples.extend_from_slice(&chunk);
        Ok(())
    }

    /// Discard the active utterance and clear buffered samples.
    pub fn cancel(&mut self) {
        self.active = false;
        self.samples.clear();
    }

    /// Ends the utterance and returns accumulated samples.
    pub fn stop(&mut self) -> Result<Vec<f32>, ApiError> {
        if !self.active {
            return Err(ApiError::new(
                "no active utterance",
                Some("call utterance.start before utterance.stop (cancel clears the buffer)".into()),
            ));
        }
        self.active = false;
        Ok(std::mem::take(&mut self.samples))
    }

    /// Return whether an utterance session is active.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Return the number of buffered samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Return whether the buffer holds no samples.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Decode hook used by [`UtteranceApiHandler`] so tests can inject a mock
/// transcoder without loading a GPU model.
pub trait PcmTranscoder: Send + Sync {
    /// Transcribe PCM samples and return the resulting text.
    fn transcribe_pcm(&self, samples: &[f32]) -> Result<String, ApiError>;
}

impl PcmTranscoder for Engine {
    fn transcribe_pcm(&self, samples: &[f32]) -> Result<String, ApiError> {
        self.transcribe_f32(samples)
            .map_err(|e| ApiError::new(e.to_string(), None))
    }
}

impl<F> PcmTranscoder for F
where
    F: Fn(&[f32]) -> Result<String, ApiError> + Send + Sync,
{
    fn transcribe_pcm(&self, samples: &[f32]) -> Result<String, ApiError> {
        (self)(samples)
    }
}

/// ApiHandler that implements utterance.* against an in-memory buffer and a
/// pluggable transcoder. Typing is never performed here: stop returns text only.
pub struct UtteranceApiHandler<T: PcmTranscoder> {
    buf: Mutex<UtteranceBuffer>,
    transcoder: T,
}

impl<T: PcmTranscoder> UtteranceApiHandler<T> {
    /// Construct a handler with the given PCM transcoder.
    pub fn new(transcoder: T) -> Self {
        Self {
            buf: Mutex::new(UtteranceBuffer::default()),
            transcoder,
        }
    }
}

impl<T: PcmTranscoder> ApiHandler for UtteranceApiHandler<T> {
    fn utterance_start(&self) -> ApiResult {
        let mut g = self
            .buf
            .lock()
            .map_err(|_| ApiError::new("utterance lock poisoned", None))?;
        g.start();
        Ok(Some(json!({"started": true})))
    }

    fn utterance_audio(&self, pcm_f32_b64: String) -> ApiResult {
        let mut g = self
            .buf
            .lock()
            .map_err(|_| ApiError::new("utterance lock poisoned", None))?;
        g.append_b64(&pcm_f32_b64)?;
        Ok(Some(json!({"buffered_samples": g.len()})))
    }

    fn utterance_stop(&self) -> ApiResult {
        let samples = {
            let mut g = self
                .buf
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
        let text = self.transcoder.transcribe_pcm(&samples)?;
        Ok(Some(json!({ "text": text })))
    }

    fn utterance_cancel(&self) -> ApiResult {
        let mut g = self
            .buf
            .lock()
            .map_err(|_| ApiError::new("utterance lock poisoned", None))?;
        g.cancel();
        Ok(Some(json!({"cancelled": true})))
    }
}

/// Linux `SO_PEERCRED` identity for an accepted Unix-stream peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    /// Process ID of the peer.
    pub pid: u32,
    /// User ID of the peer.
    pub uid: u32,
    /// Group ID of the peer.
    pub gid: u32,
}

/// Options for the accept loop.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Reject peers whose uid differs from the daemon uid (default true).
    pub require_same_uid: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            require_same_uid: true,
        }
    }
}

/// Pure peer-uid gate (unit-testable without `getsockopt`).
pub fn authorize_peer(
    peer: &PeerCred,
    require_same_uid: bool,
    local_uid: u32,
) -> Result<(), ApiError> {
    if require_same_uid && peer.uid != local_uid {
        return Err(ApiError::new(
            format!(
                "peer uid {} rejected (daemon uid {local_uid})",
                peer.uid
            ),
            Some(
                "connect as the same OS user, or set api.require_same_uid = false in config.toml"
                    .into(),
            ),
        ));
    }
    Ok(())
}

/// Shared-secret gate for optional `[api].token`.
///
/// When `expected` is `None`/empty, any provided token is accepted (including
/// none). Otherwise the provided token must match with a byte-wise
/// constant-time compare so timing does not leak which characters differ.
/// Length mismatches still touch every byte of `expected` so a missing token
/// is not dramatically cheaper than a wrong-length guess.
pub fn authorize_token(provided: Option<&str>, expected: Option<&str>) -> Result<(), ApiError> {
    let Some(expected) = expected.filter(|t| !t.is_empty()) else {
        return Ok(());
    };
    let got = provided.unwrap_or("");
    if !constant_time_eq(got.as_bytes(), expected.as_bytes()) {
        return Err(ApiError::new(
            "unauthorized",
            Some("set request token to match [api].token in config.toml".into()),
        ));
    }
    Ok(())
}

/// Constant-time equality for equal-length slices. On length mismatch, folds
/// every byte of `expected` then returns false (length itself may still leak).
fn constant_time_eq(got: &[u8], expected: &[u8]) -> bool {
    let mut diff = 0u8;
    if got.len() != expected.len() {
        for &b in expected {
            diff |= b;
        }
        std::hint::black_box(diff);
        return false;
    }
    for (a, b) in got.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Read peer credentials via Linux `SO_PEERCRED`. Non-Linux returns Unsupported.
#[cfg(unix)]
pub fn peer_credentials(stream: &UnixStream) -> std::io::Result<PeerCred> {
    peer_credentials_fd(stream.as_raw_fd())
}

/// Read peer credentials on Windows. Named pipes do not expose `SO_PEERCRED`;
/// returns `Unsupported`.
#[cfg(target_os = "windows")]
pub fn peer_credentials(_stream: &NamedPipeStream) -> std::io::Result<PeerCred> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "peer credentials are not available on Windows named pipes",
    ))
}

#[cfg(unix)]
fn peer_credentials_fd(fd: libc::c_int) -> std::io::Result<PeerCred> {
    #[cfg(target_os = "linux")]
    {
        #[repr(C)]
        struct UCred {
            pid: libc::pid_t,
            uid: libc::uid_t,
            gid: libc::gid_t,
        }
        let mut cred = UCred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<UCred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(PeerCred {
            pid: cred.pid as u32,
            uid: cred.uid,
            gid: cred.gid,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = fd;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "SO_PEERCRED is only available on Linux",
        ))
    }
}

#[cfg(unix)]
fn gate_accepted_peer(stream: &UnixStream, require_same_uid: bool) -> Result<(), ApiError> {
    match peer_credentials(stream) {
        Ok(cred) => {
            log::info!(
                "api peer pid={} uid={} gid={}",
                cred.pid,
                cred.uid,
                cred.gid
            );
            let local_uid = unsafe { libc::getuid() } as u32;
            authorize_peer(&cred, require_same_uid, local_uid)
        }
        Err(err) => {
            if require_same_uid {
                Err(ApiError::new(
                    format!("cannot read peer credentials: {err}"),
                    Some(
                        "SO_PEERCRED failed — refuse connection while api.require_same_uid is true"
                            .into(),
                    ),
                ))
            } else {
                log::warn!("api SO_PEERCRED unavailable ({err}); continuing without uid check");
                Ok(())
            }
        }
    }
}

/// Windows: no `SO_PEERCRED` equivalent. The uid check is always skipped;
/// rely on the named pipe namespace ACL for access control.
#[cfg(target_os = "windows")]
fn gate_accepted_peer(_stream: &NamedPipeStream, _require_same_uid: bool) -> Result<(), ApiError> {
    Ok(())
}

/// Default socket path: `$XDG_RUNTIME_DIR/steno/steno.sock`, else
/// `$XDG_CACHE_HOME/steno/steno.sock`, else `~/.cache/steno/steno.sock`.
#[cfg(unix)]
pub fn default_socket_path() -> Result<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return Ok(PathBuf::from(runtime).join("steno/steno.sock"));
        }
    }
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        if !cache.is_empty() {
            return Ok(PathBuf::from(cache).join("steno/steno.sock"));
        }
    }
    let home = match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => bail!(
            "neither XDG_RUNTIME_DIR, XDG_CACHE_HOME, nor HOME is set — export one of them, or pass an explicit socket path"
        ),
    };
    Ok(home.join(".cache/steno/steno.sock"))
}

/// Default named pipe path on Windows: `\\.\pipe\steno`.
#[cfg(target_os = "windows")]
pub fn default_socket_path() -> Result<PathBuf> {
    Ok(PathBuf::from(r"\\.\pipe\steno"))
}

#[cfg(unix)]
/// Bind `path`, remove a stale socket file, accept forever, and dispatch.
pub fn serve_unix(path: impl AsRef<Path>, handler: impl ApiHandler) -> Result<()> {
    serve_unix_until(path, handler, None)
}

#[cfg(unix)]
/// Like [`serve_unix`], but exits the accept loop when `stop` is set.
pub fn serve_unix_until(
    path: impl AsRef<Path>,
    handler: impl ApiHandler,
    stop: Option<Arc<AtomicBool>>,
) -> Result<()> {
    serve_unix_with(path, handler, stop, ServeOptions::default())
}

#[cfg(unix)]
/// Accept loop with explicit [`ServeOptions`] (peer-uid gate, etc.).
pub fn serve_unix_with(
    path: impl AsRef<Path>,
    handler: impl ApiHandler,
    stop: Option<Arc<AtomicBool>>,
    opts: ServeOptions,
) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create socket directory {} — check permissions or pick another path",
                parent.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    if path.exists() {
        // If something still answers on this path, do not steal the name
        // (unlink+bind TOCTOU against a live daemon).
        if UnixStream::connect(path).is_ok() {
            bail!(
                "API socket {} is still live — stop the other steno daemon before starting another",
                path.display()
            );
        }
        fs::remove_file(path).with_context(|| {
            format!(
                "failed to remove stale socket {} — stop the other steno daemon or delete the file",
                path.display()
            )
        })?;
    }

    let listener = UnixListener::bind(path).with_context(|| {
        format!(
            "failed to bind Unix socket at {} — another process may hold it, or the path is not writable",
            path.display()
        )
    })?;
    // Restrict to the owning uid; peer-uid gate is belt-and-suspenders.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    if stop.is_some() {
        listener
            .set_nonblocking(true)
            .context("failed to set API socket non-blocking for stoppable accept")?;
    }

    loop {
        if stop.as_ref().is_some_and(|s| s.load(Ordering::Relaxed)) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(err) = gate_accepted_peer(&stream, opts.require_same_uid) {
                    log::warn!(
                        "api rejecting connection: {}{}",
                        err.error,
                        err.hint
                            .as_deref()
                            .map(|h| format!(" — {h}"))
                            .unwrap_or_default()
                    );
                    continue;
                }
                if let Err(err) = handle_connection(stream, &handler) {
                    log::warn!("api connection closed with error: {err:#}");
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                log::warn!("api accept failed: {err}");
                if stop.as_ref().is_some_and(|s| s.load(Ordering::Relaxed)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Windows named-pipe server
// ---------------------------------------------------------------------------

/// Like [`serve_unix`] but backed by a Windows named pipe.
#[cfg(target_os = "windows")]
pub fn serve_unix(path: impl AsRef<Path>, handler: impl ApiHandler) -> Result<()> {
    serve_unix_until(path, handler, None)
}

/// Like [`serve_unix`], but exits the accept loop when `stop` is set.
#[cfg(target_os = "windows")]
pub fn serve_unix_until(
    path: impl AsRef<Path>,
    handler: impl ApiHandler,
    stop: Option<Arc<AtomicBool>>,
) -> Result<()> {
    serve_unix_with(path, handler, stop, ServeOptions::default())
}

/// Accept loop with explicit [`ServeOptions`] (peer-uid gate, etc.).
#[cfg(target_os = "windows")]
pub fn serve_unix_with(
    path: impl AsRef<Path>,
    handler: impl ApiHandler,
    stop: Option<Arc<AtomicBool>>,
    opts: ServeOptions,
) -> Result<()> {
    let path = path.as_ref();

    // If another daemon is already listening on this pipe name, a client
    // connect succeeds — refuse to steal the name.
    if let Some(h) = open_pipe_client(path) {
        unsafe { CloseHandle(h) };
        bail!(
            "API named pipe {} is already live — stop the other steno daemon before starting another",
            path.display()
        );
    }

    loop {
        if stop.as_ref().is_some_and(|s| s.load(Ordering::Relaxed)) {
            break;
        }

        let handle = create_named_pipe(path)?;

        if !accept_named_pipe(handle, stop.as_ref())? {
            // stop was requested while waiting for a client.
            unsafe { CloseHandle(handle) };
            break;
        }

        let stream = NamedPipeStream::new(handle)?;

        if let Err(err) = gate_accepted_peer(&stream, opts.require_same_uid) {
            log::warn!(
                "api rejecting connection: {}{}",
                err.error,
                err.hint
                    .as_deref()
                    .map(|h| format!(" — {h}"))
                    .unwrap_or_default()
            );
            continue;
        }

        if let Err(err) = handle_connection(stream, &handler) {
            log::warn!("api connection closed with error: {err:#}");
        }
    }

    Ok(())
}

/// Create a named pipe instance with overlapped I/O for duplex communication.
#[cfg(target_os = "windows")]
fn create_named_pipe(path: &Path) -> Result<HANDLE> {
    let name: String = path.to_string_lossy().into_owned();
    let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateNamedPipeW(
            name_wide.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        bail!(
            "failed to create named pipe {} — {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(handle)
}

/// Wait for a client to connect to a named pipe instance.
///
/// Returns `Ok(true)` when a client has connected, `Ok(false)` when `stop`
/// was set while waiting (the caller should close the handle and exit).
#[cfg(target_os = "windows")]
fn accept_named_pipe(handle: HANDLE, stop: Option<&Arc<AtomicBool>>) -> Result<bool> {
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    if event.is_null() {
        bail!(
            "CreateEventW failed: {}",
            std::io::Error::last_os_error()
        );
    }

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event;

    let connected = unsafe { ConnectNamedPipe(handle, &mut overlapped) };

    if connected == 0 {
        let err = unsafe { GetLastError() };
        if err == ERROR_IO_PENDING {
            // Poll for completion or stop every 50 ms.
            loop {
                if stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
                    unsafe { CancelIo(handle) };
                    let mut transferred: u32 = 0;
                    let _ = unsafe {
                        GetOverlappedResult(handle, &mut overlapped, &mut transferred, 1)
                    };
                    unsafe { CloseHandle(event) };
                    return Ok(false);
                }
                let result = unsafe { WaitForSingleObject(event, 50) };
                if result == WAIT_OBJECT_0 {
                    break;
                }
                // WAIT_TIMEOUT — keep polling.
            }
        } else if err == ERROR_PIPE_CONNECTED {
            // A client connected between CreateNamedPipeW and ConnectNamedPipe.
        } else {
            unsafe { CloseHandle(event) };
            bail!(
                "ConnectNamedPipe failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    unsafe { CloseHandle(event) };
    Ok(true)
}

/// Try to open a named pipe as a client. Returns the handle on success,
/// `None` if no pipe is listening. Used for the liveness probe.
#[cfg(target_os = "windows")]
fn open_pipe_client(path: &Path) -> Option<HANDLE> {
    let name: String = path.to_string_lossy().into_owned();
    let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            name_wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(handle)
    }
}

/// Duplex stream over a Windows named pipe handle. Implements `Read` + `Write`
/// via overlapped I/O with a 30-second timeout per operation.
#[cfg(target_os = "windows")]
pub struct NamedPipeStream {
    handle: HANDLE,
    event: HANDLE,
}

#[cfg(target_os = "windows")]
impl NamedPipeStream {
    fn new(handle: HANDLE) -> Result<Self> {
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            unsafe { CloseHandle(handle) };
            bail!("CreateEventW failed: {}", std::io::Error::last_os_error());
        }
        Ok(Self { handle, event })
    }

    /// Open a named pipe as a client. Uses overlapped I/O so the same
    /// 30-second timeout applies as on the server side.
    pub fn connect(path: &Path) -> std::io::Result<Self> {
        let name: String = path.to_string_lossy().into_owned();
        let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                name_wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            unsafe { CloseHandle(handle) };
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { handle, event })
    }

    /// Duplicate the underlying pipe handle so the caller can split
    /// read and write halves without sharing a single handle.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        let mut new_handle: HANDLE = std::ptr::null_mut();
        // INVALID_HANDLE_VALUE (-1) is the current-process pseudo-handle,
        // equivalent to GetCurrentProcess().
        let ok = unsafe {
            DuplicateHandle(
                INVALID_HANDLE_VALUE,
                self.handle,
                INVALID_HANDLE_VALUE,
                &mut new_handle,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            unsafe { CloseHandle(new_handle) };
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            handle: new_handle,
            event,
        })
    }
}

#[cfg(target_os = "windows")]
impl std::io::Read for NamedPipeStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = self.event;
        let mut bytes_read: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut bytes_read,
                &mut overlapped,
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_IO_PENDING {
                let result = unsafe { WaitForSingleObject(self.event, 30000) };
                if result == WAIT_TIMEOUT {
                    unsafe { CancelIo(self.handle) };
                    let mut transferred: u32 = 0;
                    let _ = unsafe {
                        GetOverlappedResult(self.handle, &mut overlapped, &mut transferred, 1)
                    };
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "named pipe read timed out (30s)",
                    ));
                }
                if result != WAIT_OBJECT_0 {
                    unsafe { CancelIo(self.handle) };
                    return Err(std::io::Error::last_os_error());
                }
                let mut transferred: u32 = 0;
                let ok2 = unsafe {
                    GetOverlappedResult(self.handle, &mut overlapped, &mut transferred, 1)
                };
                if ok2 == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                bytes_read = transferred;
            } else {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(bytes_read as usize)
    }
}

#[cfg(target_os = "windows")]
impl std::io::Write for NamedPipeStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = self.event;
        let mut bytes_written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr(),
                buf.len() as u32,
                &mut bytes_written,
                &mut overlapped,
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_IO_PENDING {
                let result = unsafe { WaitForSingleObject(self.event, 30000) };
                if result == WAIT_TIMEOUT {
                    unsafe { CancelIo(self.handle) };
                    let mut transferred: u32 = 0;
                    let _ = unsafe {
                        GetOverlappedResult(self.handle, &mut overlapped, &mut transferred, 1)
                    };
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "named pipe write timed out (30s)",
                    ));
                }
                if result != WAIT_OBJECT_0 {
                    unsafe { CancelIo(self.handle) };
                    return Err(std::io::Error::last_os_error());
                }
                let mut transferred: u32 = 0;
                let ok2 = unsafe {
                    GetOverlappedResult(self.handle, &mut overlapped, &mut transferred, 1)
                };
                if ok2 == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                bytes_written = transferred;
            } else {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(bytes_written as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "windows")]
impl Drop for NamedPipeStream {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.event);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(unix)]
fn handle_connection(stream: UnixStream, handler: &impl ApiHandler) -> Result<()> {
    // Bound idle clients so daemon shutdown cannot wedge forever in read_until.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let writer_stream = stream.try_clone().context(
        "failed to clone Unix stream for reply writes — check ulimit / EMFILE",
    )?;
    let mut reader = BufReader::new(stream);
    let mut writer = writer_stream;
    handle_conn_loop(&mut reader, &mut writer, handler)
}

#[cfg(target_os = "windows")]
fn handle_connection(stream: NamedPipeStream, handler: &impl ApiHandler) -> Result<()> {
    let writer_stream = stream
        .try_clone()
        .context("failed to duplicate named pipe handle for reply writes")?;
    let mut reader = BufReader::new(stream);
    let mut writer = writer_stream;
    handle_conn_loop(&mut reader, &mut writer, handler)
}

/// Shared NDJSON line-reading + dispatch loop. Platform-independent: the
/// caller provides a buffered reader and a writer for the accepted stream.
fn handle_conn_loop<R: Read, W: Write>(
    reader: &mut BufReader<R>,
    writer: &mut W,
    handler: &impl ApiHandler,
) -> Result<()> {
    let mut buf = Vec::new();

    loop {
        buf.clear();
        // Bound per-line growth: read byte-by-byte until newline or cap.
        let mut n = 0usize;
        loop {
            let mut byte = [0u8; 1];
            match reader.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    n += 1;
                    buf.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                    if buf.len() > MAX_API_LINE_BYTES {
                        anyhow::bail!(
                            "API client sent a line larger than {MAX_API_LINE_BYTES} bytes — \
                             reject oversized pcm_f32_b64 / JSON frames"
                        );
                    }
                }
                Err(e) => {
                    return Err(e).context("failed reading NDJSON line from API client");
                }
            }
        }
        if n == 0 {
            break;
        }
        if buf.iter().all(|b| matches!(b, b'\n' | b'\r' | b' ' | b'\t')) {
            continue;
        }

        let line = match std::str::from_utf8(&buf) {
            Ok(s) => s,
            Err(_) => {
                let id = peek_request_id(&String::from_utf8_lossy(&buf)).unwrap_or(0);
                let response = Response::err(
                    id,
                    "API client sent non-UTF-8 data — send UTF-8 NDJSON lines ending with \\n",
                    Some("send UTF-8 NDJSON lines ending with \\n".into()),
                );
                let _ = write_response(writer, &response);
                return Ok(());
            }
        };

        let response = match decode_line::<Request>(line) {
            Ok(req) => dispatch(handler, req),
            Err(err) => {
                let id = peek_request_id(line).unwrap_or(0);
                let response = Response::err(
                    id,
                    format!("invalid request JSON: {err}"),
                    Some(
                        "send one JSON object per line with fields id (u64) and op (string)"
                            .into(),
                    ),
                );
                let _ = write_response(writer, &response);
                return Ok(());
            }
        };
        write_response(writer, &response)?;
    }
    Ok(())
}

fn dispatch(handler: &impl ApiHandler, req: Request) -> Response {
    let id = req.id;
    if let Err(err) = handler.authorize(req.token.as_deref()) {
        return Response::err(id, err.error, err.hint);
    }
    let result = match req.op {
        Op::Ping => handler.ping(),
        Op::Status => handler.status(),
        Op::Transcribe {
            wav_path,
            pcm_f32_b64,
        } => handler.transcribe(wav_path, pcm_f32_b64),
        Op::UtteranceStart => handler.utterance_start(),
        Op::UtteranceAudio { pcm_f32_b64 } => handler.utterance_audio(pcm_f32_b64),
        Op::UtteranceStop => handler.utterance_stop(),
        Op::UtteranceCancel => handler.utterance_cancel(),
        Op::Shutdown => handler.shutdown(),
    };
    match result {
        Ok(value) => Response::ok(id, value),
        Err(err) => Response::err(id, err.error, err.hint),
    }
}

fn write_response(writer: &mut impl Write, response: &Response) -> Result<()> {
    let line = encode_line(response).context("failed to encode API response JSON")?;
    writer
        .write_all(line.as_bytes())
        .context("failed writing API response to client")?;
    writer
        .flush()
        .context("failed flushing API response to client")?;
    Ok(())
}

/// Encode an [`Event`] as an NDJSON line (helper for future fan-out).
pub fn encode_event(event: &Event) -> Result<String> {
    encode_line(event).context("failed to encode API event JSON")
}

#[cfg(all(test, unix))]
mod tests {
    //! WHY: API server socket creation, token authentication, client session handling, and request
    //! dispatching must operate securely and reliably over IPC sockets.
    use super::*;
    use crate::api::client::ApiClient;
    use crate::api::protocol::{Op, Request};
    use std::io::{BufRead, BufReader, Write};
    use std::sync::Mutex;
    use std::time::Duration;

    /// Serialize tests that mutate process-global env (XDG_*).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn default_socket_path_prefers_xdg_runtime() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev_rt = std::env::var_os("XDG_RUNTIME_DIR");
        // SAFETY: held under ENV_LOCK; restored before unlock.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        }
        let path = default_socket_path().unwrap();
        assert_eq!(path, PathBuf::from("/run/user/1000/steno/steno.sock"));
        unsafe {
            match prev_rt {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }

    #[test]
    fn default_socket_path_falls_back_to_xdg_cache_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev_rt = std::env::var_os("XDG_RUNTIME_DIR");
        let prev_cache = std::env::var_os("XDG_CACHE_HOME");
        // SAFETY: held under ENV_LOCK; restored before unlock.
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
            std::env::set_var("XDG_CACHE_HOME", "/tmp/steno-cache-home");
        }
        let path = default_socket_path().unwrap();
        assert_eq!(
            path,
            PathBuf::from("/tmp/steno-cache-home/steno/steno.sock")
        );
        unsafe {
            match prev_rt {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
            match prev_cache {
                Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
        }
    }

    fn temp_sock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "steno-api-{tag}-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn connect_with_retry(path: &Path) -> ApiClient {
        let mut connected = None;
        for _ in 0..100 {
            if path.exists() {
                match ApiClient::connect(path) {
                    Ok(c) => {
                        connected = Some(c);
                        break;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        connected.expect("server should accept within timeout")
    }

    fn shutdown_server(path: &Path, stop: &Arc<AtomicBool>, thread: std::thread::JoinHandle<()>) {
        stop.store(true, Ordering::Relaxed);
        let _ = UnixStream::connect(path);
        let _ = fs::remove_file(path);
        let _ = thread.join();
    }

    #[test]
    fn ping_round_trip_over_temp_socket() {
        let path = temp_sock("ping");
        let serve_path = path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            let _ = serve_unix_until(&serve_path, StubHandler, Some(stop2));
        });

        let mut client = connect_with_retry(&path);

        let resp = client
            .call(&Request {
                id: 1,
                token: None,
                op: Op::Ping,
            })
            .expect("ping call");
        assert!(resp.ok, "ping should succeed: {resp:?}");
        assert_eq!(resp.id, 1);
        assert_eq!(resp.result, Some(json!({"pong": true})));

        let status = client
            .call(&Request {
                id: 2,
                token: None,
                op: Op::Status,
            })
            .expect("status call");
        assert!(status.ok);

        let unimpl = client
            .call(&Request {
                id: 3,
                token: None,
                op: Op::UtteranceStart,
            })
            .expect("utterance.start call");
        assert!(!unimpl.ok);
        assert!(
            unimpl
                .error
                .as_deref()
                .is_some_and(|e| e.contains("utterance.start") && e.contains("not implemented")),
            "unexpected error: {unimpl:?}"
        );

        drop(client);
        shutdown_server(&path, &stop, thread);
    }

    struct TokenHandler {
        token: &'static str,
    }

    impl ApiHandler for TokenHandler {
        fn authorize(&self, token: Option<&str>) -> Result<(), ApiError> {
            authorize_token(token, Some(self.token))
        }
    }

    #[test]
    fn authorize_rejects_missing_token() {
        let path = temp_sock("token");
        let serve_path = path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            let _ = serve_unix_until(&serve_path, TokenHandler { token: "s3cret" }, Some(stop2));
        });

        let mut client = connect_with_retry(&path);
        let denied = client
            .call(&Request {
                id: 1,
                token: None,
                op: Op::Ping,
            })
            .unwrap();
        assert!(!denied.ok);
        assert_eq!(denied.error.as_deref(), Some("unauthorized"));

        let ok = client
            .call(&Request {
                id: 2,
                token: Some("s3cret".into()),
                op: Op::Ping,
            })
            .unwrap();
        assert!(ok.ok);

        drop(client);
        shutdown_server(&path, &stop, thread);
    }

    #[test]
    fn encode_event_line() {
        let line = encode_event(&Event::Stage {
            stage: "listening".into(),
        })
        .unwrap();
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"event\":\"stage\""));

        let done = encode_event(&Event::UtteranceDone {
            text: "hi".into(),
        })
        .unwrap();
        assert!(done.contains("\"event\":\"utterance.done\""));
        assert!(done.contains("\"text\":\"hi\""));
    }

    #[test]
    fn authorize_token_accepts_when_unset() {
        assert!(authorize_token(None, None).is_ok());
        assert!(authorize_token(Some("x"), None).is_ok());
        assert!(authorize_token(None, Some("")).is_ok());
    }

    #[test]
    fn authorize_token_constant_time_match() {
        assert!(authorize_token(Some("s3cret"), Some("s3cret")).is_ok());
        assert!(authorize_token(None, Some("s3cret")).is_err());
        assert!(authorize_token(Some("wrong"), Some("s3cret")).is_err());
        assert!(authorize_token(Some("s3cre"), Some("s3cret")).is_err());
        assert!(authorize_token(Some("s3cret!"), Some("s3cret")).is_err());
    }

    #[test]
    fn constant_time_eq_rejects_mismatch() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"ab", b"abc"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn authorize_peer_same_uid_gate() {
        // WHY: PEERCRED enforcement must be unit-testable without getsockopt;
        // the seam is authorize_peer(local_uid, peer).
        let peer = PeerCred {
            pid: 42,
            uid: 1000,
            gid: 1000,
        };
        assert!(authorize_peer(&peer, true, 1000).is_ok());
        let err = authorize_peer(&peer, true, 1001).expect_err("uid mismatch");
        assert!(err.error.contains("peer uid 1000"));
        assert!(err.error.contains("daemon uid 1001"));
        assert!(authorize_peer(&peer, false, 1001).is_ok());
    }

    #[test]
    fn decode_pcm_f32_le_b64_round_trip() {
        let samples = [1.0f32, -0.5, 0.25];
        let mut bytes = Vec::new();
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let back = decode_pcm_f32_le_b64(&b64).unwrap();
        assert_eq!(back, samples);
        assert!(decode_pcm_f32_le_b64("!!!!").is_err());
        assert!(decode_pcm_f32_le_b64("YQ==").is_err()); // 1 byte after decode
    }

    struct MockTranscoder {
        label: &'static str,
    }

    impl PcmTranscoder for MockTranscoder {
        fn transcribe_pcm(&self, samples: &[f32]) -> Result<String, ApiError> {
            Ok(format!("{}:{}", self.label, samples.len()))
        }
    }

    #[test]
    fn utterance_start_audio_stop_returns_text_via_mock() {
        // WHY: utterance path must return text from a mock transcoder with no
        // GPU/mic — prove start→audio→stop contract over a real temp socket.
        let path = temp_sock("utt");
        let serve_path = path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            let handler = UtteranceApiHandler::new(MockTranscoder { label: "mock" });
            // Same-uid gate stays on; local client is the same uid.
            let _ = serve_unix_with(
                &serve_path,
                handler,
                Some(stop2),
                ServeOptions {
                    require_same_uid: true,
                },
            );
        });

        let mut client = connect_with_retry(&path);

        let start = client
            .call(&Request {
                id: 1,
                token: None,
                op: Op::UtteranceStart,
            })
            .unwrap();
        assert!(start.ok, "{start:?}");

        let mut bytes = Vec::new();
        for s in [0.1f32, 0.2, 0.3, 0.4] {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let audio = client
            .call(&Request {
                id: 2,
                token: None,
                op: Op::UtteranceAudio {
                    pcm_f32_b64: b64,
                },
            })
            .unwrap();
        assert!(audio.ok, "{audio:?}");
        assert_eq!(audio.result, Some(json!({"buffered_samples": 4})));

        let stop_resp = client
            .call(&Request {
                id: 3,
                token: None,
                op: Op::UtteranceStop,
            })
            .unwrap();
        assert!(stop_resp.ok, "{stop_resp:?}");
        assert_eq!(
            stop_resp.result,
            Some(json!({"text": "mock:4"}))
        );

        drop(client);
        shutdown_server(&path, &stop, thread);
    }

    #[test]
    fn utterance_append_rejects_over_max_samples() {
        let mut buf = UtteranceBuffer::default();
        buf.start();
        // 4 bytes per f32; build a chunk that alone exceeds the cap.
        let samples = vec![0.0f32; MAX_UTTERANCE_SAMPLES + 1];
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let err = buf.append_b64(&b64).expect_err("over-cap");
        assert!(err.error.contains("exceeds max"), "{err:?}");
        assert!(!buf.is_active());
    }

    #[test]
    fn decode_pcm_rejects_over_max_samples_before_alloc() {
        // 4 bytes/sample; one past the cap must fail closed without hanging.
        let n = MAX_UTTERANCE_SAMPLES + 1;
        let bytes = vec![0u8; n * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let err = decode_pcm_f32_le_b64(&b64).expect_err("oversize pcm");
        assert!(
            err.error.contains("exceeds max"),
            "error must mention the cap: {}",
            err.error
        );
    }

    #[test]
    fn utterance_cancel_then_stop_errors_cleanly() {

        let handler = UtteranceApiHandler::new(MockTranscoder { label: "mock" });
        assert!(handler.utterance_start().is_ok());
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert!(handler.utterance_audio(b64).is_ok());
        assert!(handler.utterance_cancel().is_ok());
        let err = handler.utterance_stop().expect_err("stop after cancel");
        assert!(
            err.error.contains("no active utterance"),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn utterance_audio_without_start_errors() {
        let handler = UtteranceApiHandler::new(MockTranscoder { label: "mock" });
        let err = handler
            .utterance_audio("AAAA".into())
            .expect_err("audio without start");
        assert!(err.error.contains("no active utterance"));
    }

    #[test]
    fn utterance_empty_stop_errors() {
        let handler = UtteranceApiHandler::new(MockTranscoder { label: "mock" });
        assert!(handler.utterance_start().is_ok());
        let err = handler.utterance_stop().expect_err("empty stop");
        assert!(err.error.contains("empty"));
    }

    fn spawn_stub_server(tag: &str) -> (PathBuf, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let path = temp_sock(tag);
        let serve_path = path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            // Keep SO_PEERCRED same-uid gate on; local client shares uid.
            let _ = serve_unix_with(
                &serve_path,
                StubHandler,
                Some(stop2),
                ServeOptions {
                    require_same_uid: true,
                },
            );
        });
        (path, stop, thread)
    }

    fn connect_raw_with_retry(path: &Path) -> UnixStream {
        for _ in 0..100 {
            if path.exists() {
                if let Ok(stream) = UnixStream::connect(path) {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    return stream;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("raw server connect timed out for {}", path.display());
    }

    fn read_response_line(reader: &mut BufReader<UnixStream>) -> Response {
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = reader
                .read_until(b'\n', &mut buf)
                .expect("read response line");
            assert!(n > 0, "API socket closed before a response arrived");
            if buf
                .iter()
                .all(|b| matches!(b, b'\n' | b'\r' | b' ' | b'\t'))
            {
                continue;
            }
            let text = std::str::from_utf8(&buf).expect("response utf-8");
            return decode_line::<Response>(text).unwrap_or_else(|e| {
                panic!("expected Response JSON, got {text:?}: {e}");
            });
        }
    }

    #[test]
    fn framing_partial_line_waits_for_newline() {
        // WHY: NDJSON is newline-delimited; a split write must not dispatch
        // until the terminating '\n' arrives (no premature decode).
        let (path, stop, thread) = spawn_stub_server("partial");
        let stream = connect_raw_with_retry(&path);
        let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
        let mut writer = stream.try_clone().expect("clone for write");
        let mut reader = BufReader::new(stream);

        writer
            .write_all(br#"{"id":1,"op":"pi"#)
            .expect("partial write");
        writer.flush().expect("flush partial");

        let mut buf = Vec::new();
        let err = reader
            .read_until(b'\n', &mut buf)
            .expect_err("partial frame must not yield a line yet");
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "expected timeout/would-block on partial line, got {err:?}"
        );
        assert!(buf.is_empty(), "no bytes should arrive before newline");

        let _ = reader.get_ref().set_read_timeout(Some(Duration::from_secs(2)));
        writer.write_all(b"ng\"}\n").expect("complete write");
        writer.flush().expect("flush complete");

        let resp = read_response_line(&mut reader);
        assert!(resp.ok, "{resp:?}");
        assert_eq!(resp.id, 1);
        assert_eq!(resp.result, Some(json!({"pong": true})));

        drop(writer);
        drop(reader);
        shutdown_server(&path, &stop, thread);
    }

    #[test]
    fn framing_multiple_requests_in_one_write() {
        // WHY: one Unix write may carry several NDJSON lines; each must get
        // its own correlated response without requiring a flush between.
        let (path, stop, thread) = spawn_stub_server("multi");
        let stream = connect_raw_with_retry(&path);
        let mut writer = stream.try_clone().expect("clone for write");
        let mut reader = BufReader::new(stream);

        writer
            .write_all(br#"{"id":1,"op":"ping"}
{"id":2,"op":"status"}
"#)
            .expect("multi-line write");
        writer.flush().expect("flush multi");

        let r1 = read_response_line(&mut reader);
        let r2 = read_response_line(&mut reader);
        assert!(r1.ok, "{r1:?}");
        assert_eq!(r1.id, 1);
        assert_eq!(r1.result, Some(json!({"pong": true})));
        assert!(r2.ok, "{r2:?}");
        assert_eq!(r2.id, 2);
        assert!(
            r2.result.as_ref().is_some_and(|v| {
                v.get("stage").and_then(|x| x.as_str()) == Some("idle")
                    && v.get("pid").is_some()
            }),
            "status payload: {r2:?}"
        );

        drop(writer);
        drop(reader);
        shutdown_server(&path, &stop, thread);
    }

    #[test]
    fn framing_empty_lines_ignored() {
        // WHY: blank / whitespace-only lines must be skipped so keepalives or
        // accidental blank writes do not poison framing or produce replies.
        let (path, stop, thread) = spawn_stub_server("empty");
        let stream = connect_raw_with_retry(&path);
        let mut writer = stream.try_clone().expect("clone for write");
        let mut reader = BufReader::new(stream);

        writer
            .write_all(b"\n\n  \n\t\r\n{\"id\":5,\"op\":\"ping\"}\n")
            .expect("empty+ping write");
        writer.flush().expect("flush empty+ping");

        let resp = read_response_line(&mut reader);
        assert!(resp.ok, "{resp:?}");
        assert_eq!(resp.id, 5);

        // No extra reply for the blanks: a short read after the ping reply
        // must time out rather than return another Response.
        let _ = reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(150)));
        let mut buf = Vec::new();
        let err = reader
            .read_until(b'\n', &mut buf)
            .expect_err("no spare response after empty lines");
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "unexpected: {err:?}"
        );

        drop(writer);
        drop(reader);
        shutdown_server(&path, &stop, thread);
    }

    #[test]
    fn framing_malformed_and_unknown_method_errors() {
        // WHY: unknown op, bad shape, or non-UTF-8 line must send an Error(id) response frame
        // and then close the socket connection.
        let (path, stop, thread) = spawn_stub_server("badframe");

        // 1. Unknown method with peekable id → err reply (id=9), then socket closed.
        let stream = connect_raw_with_retry(&path);
        let mut writer = stream.try_clone().expect("clone for write");
        let mut reader = BufReader::new(stream);

        writer
            .write_all(br#"{"id":9,"op":"nope"}
"#)
            .expect("unknown op write");
        writer.flush().expect("flush unknown");
        let unknown = read_response_line(&mut reader);
        assert!(!unknown.ok, "{unknown:?}");
        assert_eq!(unknown.id, 9);
        assert!(
            unknown
                .error
                .as_deref()
                .is_some_and(|e| e.contains("invalid request JSON")),
            "unexpected error: {unknown:?}"
        );
        assert!(
            unknown
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("id") && h.contains("op")),
            "hint should guide wire shape: {unknown:?}"
        );
        let mut eof_buf = Vec::new();
        assert_eq!(
            reader.read_until(b'\n', &mut eof_buf).expect("read eof"),
            0,
            "socket connection must close after sending error response"
        );
        drop(writer);
        drop(reader);

        // 2. Malformed JSON without peekable id → err reply (id=0), then socket closed.
        let stream2 = connect_raw_with_retry(&path);
        let mut writer2 = stream2.try_clone().expect("clone for write");
        let mut reader2 = BufReader::new(stream2);

        writer2
            .write_all(b"not-json-at-all\n")
            .expect("malformed write");
        writer2.flush().expect("flush malformed");
        let no_id = read_response_line(&mut reader2);
        assert!(!no_id.ok, "{no_id:?}");
        assert_eq!(no_id.id, 0);
        assert!(
            no_id
                .error
                .as_deref()
                .is_some_and(|e| e.contains("invalid request JSON")),
            "unexpected error: {no_id:?}"
        );
        let mut eof_buf2 = Vec::new();
        assert_eq!(
            reader2.read_until(b'\n', &mut eof_buf2).expect("read eof"),
            0,
            "socket connection must close after sending error response"
        );
        drop(writer2);
        drop(reader2);

        // 3. Invalid UTF-8 → err reply (id=0), then socket closed.
        let stream3 = connect_raw_with_retry(&path);
        let mut writer3 = stream3.try_clone().expect("clone for write");
        let mut reader3 = BufReader::new(stream3);

        writer3
            .write_all(b"\xFF\xFE\xFD\n")
            .expect("invalid utf-8 write");
        writer3.flush().expect("flush invalid utf-8");
        let bad_utf8 = read_response_line(&mut reader3);
        assert!(!bad_utf8.ok, "{bad_utf8:?}");
        assert_eq!(bad_utf8.id, 0);
        assert!(
            bad_utf8
                .error
                .as_deref()
                .is_some_and(|e| e.contains("non-UTF-8")),
            "unexpected error: {bad_utf8:?}"
        );
        let mut eof_buf3 = Vec::new();
        assert_eq!(
            reader3.read_until(b'\n', &mut eof_buf3).expect("read eof"),
            0,
            "socket connection must close after sending error response"
        );
        drop(writer3);
        drop(reader3);

        shutdown_server(&path, &stop, thread);
    }
}
