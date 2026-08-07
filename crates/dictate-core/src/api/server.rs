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
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
///
/// Linux note (later): after `accept`, `SO_PEERCRED` / `getsockopt(SO_PEERCRED)`
/// can enforce same-uid peers before honoring `token` or privileged ops.
pub fn serve_unix(path: impl AsRef<Path>, handler: impl ApiHandler) -> Result<()> {
    serve_unix_until(path, handler, None)
}

/// Like [`serve_unix`], but exits the accept loop when `stop` is set.
///
/// Uses a non-blocking accept poll so SIGTERM / daemon shutdown can tear
/// down the listener without waiting for the next client.
pub fn serve_unix_until(
    path: impl AsRef<Path>,
    handler: impl ApiHandler,
    stop: Option<Arc<AtomicBool>>,
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
    // Peer credentials (Linux): libc::getsockopt with SO_PEERCRED can be
    // checked here later for same-uid enforcement before dispatch.
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

    #[test]
    fn ping_round_trip_over_temp_socket() {
        let path = std::env::temp_dir().join(format!(
            "dictate-api-ping-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let serve_path = path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            let _ = serve_unix_until(&serve_path, StubHandler, Some(stop2));
        });

        let mut connected = None;
        for _ in 0..100 {
            if path.exists() {
                match ApiClient::connect(&path) {
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
        let mut client = connected.expect("server should accept within timeout");

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
        stop.store(true, Ordering::Relaxed);
        let _ = std::os::unix::net::UnixStream::connect(&path);
        let _ = fs::remove_file(&path);
        let _ = thread.join();
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
        let path = std::env::temp_dir().join(format!(
            "dictate-api-token-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let serve_path = path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            let _ = serve_unix_until(&serve_path, TokenHandler { token: "s3cret" }, Some(stop2));
        });

        let mut client = None;
        for _ in 0..100 {
            if path.exists() {
                if let Ok(c) = ApiClient::connect(&path) {
                    client = Some(c);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut client = client.expect("server up");
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

        // Drop the client first — serve_unix_until only checks `stop`
        // between connections; a live reader blocks shutdown.
        drop(client);
        stop.store(true, Ordering::Relaxed);
        let _ = std::os::unix::net::UnixStream::connect(&path);
        let _ = fs::remove_file(&path);
        let _ = thread.join();
    }

    #[test]
    fn encode_event_line() {
        let line = encode_event(&Event::Stage {
            stage: "listening".into(),
        })
        .unwrap();
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"event\":\"stage\""));
    }
}
