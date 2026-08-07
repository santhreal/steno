//! Daemon NDJSON IPC: protocol types, Unix socket client, and server.
//!
//! Wired by `daemon::run_daemon` when `[api].enabled`.

#![allow(dead_code)]
#![allow(unused_imports)]

pub mod client;
pub mod protocol;
pub mod server;

pub use client::ApiClient;
pub use protocol::{Event, Op, Request, Response, decode_line, encode_line};
pub use server::{
    ApiError, ApiHandler, ApiResult, PeerCred, PcmTranscoder, ServeOptions, StubHandler,
    UtteranceApiHandler, UtteranceBuffer, MAX_API_LINE_BYTES, MAX_UTTERANCE_SAMPLES, authorize_peer, authorize_token, decode_pcm_f32_le_b64,
    default_socket_path, peer_credentials, serve_unix, serve_unix_until, serve_unix_with,
};
