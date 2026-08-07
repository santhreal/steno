//! Thin Unix-socket NDJSON client for the dictate daemon API.

// Public until daemon wiring; keep symbols for embedders/CLI.
#![allow(dead_code)]

use crate::api::protocol::{Request, Response, decode_line, encode_line};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
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
                .read_until(b'\n', &mut buf)
                .context("failed reading API response from socket")?;
            if n == 0 {
                bail!(
                    "API socket closed before a response arrived — the daemon may have exited; check logs and restart with `dictate start`"
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
