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
    ApiError, ApiHandler, ApiResult, StubHandler, default_socket_path, serve_unix,
    serve_unix_until,
};
