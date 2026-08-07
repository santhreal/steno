//! Unix-domain NDJSON socket server (accept loop + ApiHandler dispatch).
//!
//! Accepts connections, frames by newline, dispatches to [`ApiHandler`], and
//! writes a [`Response`] line per request. Event fan-out is reserved for a
//! later pass (handlers can still encode [`Event`] lines themselves).

// Public until daemon wiring; keep symbols for embedders/CLI.
#![allow(dead_code)]

use crate::api::protocol::{
    Event, Op, Request, Response, decode_line, encode_line, peek_request_id,
};
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Error returned by handler methods. Mapped onto a failed [`Response`].
#[derive(Debug, Clone)]
pub struct ApiError {
    pub error: String,
    pub hint: Option<String>,
}

impl ApiError {
    pub fn new(error: impl Into<String>, hint: impl Into<Option<String>>) -> Self {
        Self {
            error: error.into(),
            hint: hint.into(),
        }
    }

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

pub type ApiResult = std::result::Result<Option<Value>, ApiError>;

/// Sync callback surface matching each protocol op.
///
/// Default methods: `ping` / `status` succeed with static JSON; transcribe and
/// utterance ops fail closed with `"not implemented"`.
pub trait ApiHandler {
    /// Optional shared-secret check. Default: accept any token (including none).
    fn authorize(&self, _token: Option<&str>) -> Result<(), ApiError> {
        Ok(())
    }

    fn ping(&self) -> ApiResult {
        Ok(Some(json!({"pong": true})))
    }

    fn status(&self) -> ApiResult {
        Ok(Some(json!({
            "pid": std::process::id(),
            "stage": "idle",
            "type_output_armed": false,
        })))
    }

    fn transcribe(
        &self,
        _wav_path: Option<PathBuf>,
        _pcm_f32_b64: Option<String>,
    ) -> ApiResult {
        Err(ApiError::not_implemented("transcribe"))
    }

    fn utterance_start(&self) -> ApiResult {
        Err(ApiError::not_implemented("utterance.start"))
    }

    fn utterance_audio(&self, _pcm_f32_b64: String) -> ApiResult {
        Err(ApiError::not_implemented("utterance.audio"))
    }

    fn utterance_stop(&self) -> ApiResult {
        Err(ApiError::not_implemented("utterance.stop"))
    }

    fn utterance_cancel(&self) -> ApiResult {
        Err(ApiError::not_implemented("utterance.cancel"))
    }

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
    let mut samples = Vec::with_capacity(bytes.len() / 4);
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
    pub fn start(&mut self) {
        self.active = true;
        self.samples.clear();
    }

    pub fn append_b64(&mut self, pcm_f32_b64: &str) -> Result<(), ApiError> {
        if !self.active {
            return Err(ApiError::new(
                "no active utterance",
                Some("call utterance.start before utterance.audio".into()),
            ));
        }
        let chunk = decode_pcm_f32_le_b64(pcm_f32_b64)?;
        self.samples.extend_from_slice(&chunk);
        Ok(())
    }

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

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Decode hook used by [`UtteranceApiHandler`] so tests can inject a mock
/// transcoder without loading a GPU model.
pub trait PcmTranscoder: Send + Sync {
    fn transcribe_pcm(&self, samples: &[f32]) -> Result<String, ApiError>;
}

/// ApiHandler that implements utterance.* against an in-memory buffer and a
/// pluggable transcoder. Typing is never performed here — stop returns text only.
pub struct UtteranceApiHandler<T: PcmTranscoder> {
    buf: Mutex<UtteranceBuffer>,
    transcoder: T,
}

impl<T: PcmTranscoder> UtteranceApiHandler<T> {
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
    pub pid: u32,
    pub uid: u32,
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

/// Read peer credentials via Linux `SO_PEERCRED`. Non-Linux returns Unsupported.
pub fn peer_credentials(stream: &UnixStream) -> std::io::Result<PeerCred> {
    peer_credentials_fd(stream.as_raw_fd())
}

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

/// Default socket path: `$XDG_RUNTIME_DIR/dictate/dictate.sock`, else
/// `~/.cache/dictate/dictate.sock`.
pub fn default_socket_path() -> Result<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return Ok(PathBuf::from(runtime).join("dictate/dictate.sock"));
        }
    }
    let home = match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => bail!(
            "neither XDG_RUNTIME_DIR nor HOME is set — export one of them, or pass an explicit socket path"
        ),
    };
    Ok(home.join(".cache/dictate/dictate.sock"))
}

/// Bind `path`, remove a stale socket file, accept forever, and dispatch.
pub fn serve_unix(path: impl AsRef<Path>, handler: impl ApiHandler) -> Result<()> {
    serve_unix_until(path, handler, None)
}

/// Like [`serve_unix`], but exits the accept loop when `stop` is set.
pub fn serve_unix_until(
    path: impl AsRef<Path>,
    handler: impl ApiHandler,
    stop: Option<Arc<AtomicBool>>,
) -> Result<()> {
    serve_unix_with(path, handler, stop, ServeOptions::default())
}

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
    }
    if path.exists() {
        fs::remove_file(path).with_context(|| {
            format!(
                "failed to remove stale socket {} — stop the other dictate daemon or delete the file",
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

fn handle_connection(stream: UnixStream, handler: &impl ApiHandler) -> Result<()> {
    // Bound idle clients so daemon shutdown cannot wedge forever in read_until.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let writer_stream = stream.try_clone().context(
        "failed to clone Unix stream for reply writes — check ulimit / EMFILE",
    )?;
    let mut reader = BufReader::new(stream);
    let mut writer = writer_stream;
    let mut buf = Vec::new();

    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .context("failed reading NDJSON line from API client")?;
        if n == 0 {
            break;
        }
        if buf.iter().all(|b| matches!(b, b'\n' | b'\r' | b' ' | b'\t')) {
            continue;
        }

        let line = std::str::from_utf8(&buf).map_err(|_| {
            anyhow::anyhow!(
                "API client sent non-UTF-8 data — send UTF-8 NDJSON lines ending with \\n"
            )
        })?;

        let response = match decode_line::<Request>(line) {
            Ok(req) => dispatch(handler, req),
            Err(err) => match peek_request_id(line) {
                Some(id) => Response::err(
                    id,
                    format!("invalid request JSON: {err}"),
                    Some(
                        "send one JSON object per line with fields id (u64) and op (string)"
                            .into(),
                    ),
                ),
                None => {
                    // No id → cannot correlate a reply; drop the frame.
                    log::warn!("dropping unparseable API line without id: {err}");
                    continue;
                }
            },
        };

        write_response(&mut writer, &response)?;
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

fn write_response(writer: &mut UnixStream, response: &Response) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::ApiClient;
    use crate::api::protocol::{Op, Request};
    use std::time::Duration;

    #[test]
    fn default_socket_path_prefers_xdg_runtime() {
        let prev_rt = std::env::var_os("XDG_RUNTIME_DIR");
        // SAFETY: test process; we restore below.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        }
        let path = default_socket_path().unwrap();
        assert_eq!(path, PathBuf::from("/run/user/1000/dictate/dictate.sock"));
        unsafe {
            match prev_rt {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }

    fn temp_sock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dictate-api-{tag}-{}-{}.sock",
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
            if token == Some(self.token) {
                Ok(())
            } else {
                Err(ApiError::new(
                    "unauthorized",
                    Some("set request token to match [api].token in config.toml".into()),
                ))
            }
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
}
