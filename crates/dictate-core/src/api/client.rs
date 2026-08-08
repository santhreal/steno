//! Thin Unix-socket NDJSON client for the dictate daemon API.

// Public until daemon wiring; keep symbols for embedders/CLI.
#![allow(dead_code)]

use crate::api::protocol::{Request, Response, decode_line, encode_line};
use crate::api::server::MAX_API_LINE_BYTES;
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Blocking client: one request line → one response line.
pub struct ApiClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl ApiClient {
    /// Connect to a daemon socket at `path`.
    pub fn connect(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let stream = UnixStream::connect(path).with_context(|| {
            format!(
                "failed to connect to dictate API socket at {} — is the daemon running (`dictate start`), and is [api] enabled?",
                path.display()
            )
        })?;
        // Bound hung daemons so CLI callers fail instead of hanging forever.
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .ok();
        let writer = stream.try_clone().context(
            "failed to clone Unix stream for API client writes — check ulimit / EMFILE",
        )?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
        })
    }

    /// Send `request` and wait for the matching response line.
    pub fn call(&mut self, request: &Request) -> Result<Response> {
        let line = encode_line(request).context("failed to encode API request JSON")?;
        self.writer
            .write_all(line.as_bytes())
            .context("failed writing API request to socket")?;
        self.writer
            .flush()
            .context("failed flushing API request to socket")?;

        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = self
                .reader
                .by_ref()
                .take((MAX_API_LINE_BYTES + 1) as u64)
                .read_until(b'\n', &mut buf)
                .context("failed reading API response from socket")?;
            if n == 0 {
                bail!(
                    "API socket closed before a response arrived — the daemon may have exited; check logs and restart with `dictate start`"
                );
            }
            if buf.len() > MAX_API_LINE_BYTES {
                bail!(
                    "API response line exceeded maximum allowed length of {MAX_API_LINE_BYTES} bytes — rejecting oversized socket response"
                );
            }
            if buf.iter().all(|b| matches!(b, b'\n' | b'\r' | b' ' | b'\t')) {
                continue;
            }
            let text = std::str::from_utf8(&buf).context(
                "API response was not valid UTF-8 — expected a JSON object line from the daemon",
            )?;
            // Skip unsolicited event lines if a future server starts broadcasting
            // on the same connection; wait for a Response with an id.
            if let Ok(resp) = decode_line::<Response>(text) {
                return Ok(resp);
            }
            // Non-response line (e.g. event): keep reading.
            if text.contains("\"event\"") {
                continue;
            }
            bail!(
                "API response was not valid JSON Response: {} — expected {{\"id\", \"ok\", ...}}",
                text.trim()
            );
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::protocol::Op;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    fn temp_sock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dictate-client-test-{tag}-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn client_line_length_bounds_exceeded_bails() {
        let path = temp_sock("len");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind listener");

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept stream");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");

            // Respond with line exceeding MAX_API_LINE_BYTES (16MB + 10 bytes)
            let oversized = vec![b'a'; MAX_API_LINE_BYTES + 10];
            let _ = stream.write_all(&oversized);
            let _ = stream.write_all(b"\n");
            let _ = stream.flush();
        });

        let mut client = ApiClient::connect(&path).expect("connect client");
        let err = client
            .call(&Request {
                id: 1,
                token: None,
                op: Op::Ping,
            })
            .expect_err("oversized response must fail");

        assert!(
            err.to_string()
                .contains("exceeded maximum allowed length"),
            "unexpected error message: {err}"
        );

        let _ = handle.join();
        let _ = std::fs::remove_file(&path);
    }
}
