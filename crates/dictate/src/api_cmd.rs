//! CLI for the daemon NDJSON API: `dictate ping` and `dictate api status`.
//!
//! Uses [`dictate_core::api::ApiClient`]. Does not start or stop the daemon.

use anyhow::{Context, Result, bail};
use dictate_core::api::{ApiClient, Op, Request, default_socket_path};
use dictate_core::config::{self, Config};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Resolve the API socket: `--socket` wins, else `[api].path`, else default.
pub fn resolve_socket(
    config_path: Option<&Path>,
    socket_override: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(p) = socket_override {
        return config::expand_tilde(&p);
    }
    let cfg = Config::load(config_path)?;
    match cfg.api.configured_path() {
        Some(p) => config::expand_tilde(p),
        None => default_socket_path(),
    }
}

fn load_token(config_path: Option<&Path>) -> Result<Option<String>> {
    let cfg = Config::load(config_path)?;
    Ok(cfg.api.required_token().map(str::to_owned))
}

fn ensure_api_enabled(config_path: Option<&Path>, socket_override: &Option<PathBuf>) -> Result<()> {
    // Explicit `--socket` is for tests / diagnostics; skip the config gate.
    if socket_override.is_some() {
        return Ok(());
    }
    let cfg = Config::load(config_path)?;
    if !cfg.api.enabled {
        bail!(
            "[api].enabled is false in config — set enabled = true and run `dictate start`"
        );
    }
    Ok(())
}

fn connect(
    config_path: Option<&Path>,
    socket_override: Option<PathBuf>,
) -> Result<(ApiClient, PathBuf, Option<String>)> {
    ensure_api_enabled(config_path, &socket_override)?;
    let path = resolve_socket(config_path, socket_override)?;
    let token = load_token(config_path)?;
    let client = ApiClient::connect(&path).with_context(|| {
        format!(
            "daemon API unreachable at {} — start it with `dictate start` (and ensure [api] is enabled)",
            path.display()
        )
    })?;
    Ok((client, path, token))
}

/// `dictate ping` — RTT to the daemon API; exit nonzero if down.
pub fn ping(config_path: Option<&Path>, socket: Option<PathBuf>) -> Result<()> {
    let (mut client, path, token) = connect(config_path, socket)?;
    let start = Instant::now();
    let resp = client
        .call(&Request {
            id: 1,
            token,
            op: Op::Ping,
        })
        .with_context(|| {
            format!(
                "ping failed talking to {} — restart with `dictate start`",
                path.display()
            )
        })?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;

    if !resp.ok {
        let err = resp.error.as_deref().unwrap_or("unknown error");
        match resp.hint.as_deref() {
            Some(hint) => bail!("ping rejected: {err} — {hint}"),
            None => bail!("ping rejected: {err}"),
        }
    }

    let pong = resp
        .result
        .as_ref()
        .and_then(|v| v.get("pong"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if pong {
        println!("pong  {ms:.1} ms");
    } else {
        println!("ok  {ms:.1} ms");
        if let Some(result) = resp.result {
            println!("{result}");
        }
    }
    Ok(())
}

/// `dictate api status` — print daemon status JSON from the API socket.
pub fn api_status(config_path: Option<&Path>, socket: Option<PathBuf>) -> Result<()> {
    let (mut client, path, token) = connect(config_path, socket)?;
    let resp = client
        .call(&Request {
            id: 1,
            token,
            op: Op::Status,
        })
        .with_context(|| {
            format!(
                "status failed talking to {} — restart with `dictate start`",
                path.display()
            )
        })?;

    if !resp.ok {
        let err = resp.error.as_deref().unwrap_or("unknown error");
        match resp.hint.as_deref() {
            Some(hint) => bail!("api status rejected: {err} — {hint}"),
            None => bail!("api status rejected: {err}"),
        }
    }

    match resp.result {
        Some(value) => {
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        None => {
            println!("{{}}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! WHY: CLI API helpers must work against a temp mock socket without a
    //! live daemon, and must fail closed with a `dictate start` hint when the
    //! socket is absent — so `cargo test` never depends on the host daemon.

    use super::*;
    use dictate_core::api::{StubHandler, serve_unix_until};
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    fn temp_sock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dictate-cli-{tag}-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn spawn_stub(path: PathBuf) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let serve_path = path.clone();
        let handle = thread::spawn(move || {
            let _ = serve_unix_until(&serve_path, StubHandler, Some(stop2));
        });
        // Wait until the listener is accept-ready.
        for _ in 0..100 {
            if path.exists() && ApiClient::connect(&path).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        (stop, handle)
    }

    fn stop_server(path: &Path, stop: &AtomicBool, handle: thread::JoinHandle<()>) {
        stop.store(true, Ordering::Relaxed);
        let _ = std::os::unix::net::UnixStream::connect(path);
        let _ = fs::remove_file(path);
        let _ = handle.join();
    }

    #[test]
    fn ping_against_mock_socket_prints_pong() {
        let path = temp_sock("ping");
        let (stop, handle) = spawn_stub(path.clone());

        ping(None, Some(path.clone())).expect("ping against mock");

        stop_server(&path, &stop, handle);
    }

    #[test]
    fn api_status_against_mock_socket_ok() {
        let path = temp_sock("status");
        let (stop, handle) = spawn_stub(path.clone());

        api_status(None, Some(path.clone())).expect("status against mock");

        stop_server(&path, &stop, handle);
    }

    #[test]
    fn ping_missing_socket_mentions_dictate_start() {
        let missing = temp_sock("missing");
        let _ = fs::remove_file(&missing);
        let err = ping(None, Some(missing)).unwrap_err().to_string();
        assert!(
            err.contains("dictate start"),
            "error must carry corrective action: {err}"
        );
    }
}
