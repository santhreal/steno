//! Embeddable offline speech-to-text: config, audio, DSP, STT, text pipeline,
//! daemon API types, session, and the high-level [`Engine`].
//!
//! # Quick start
//!
//! ```ignore
//! use steno_core::{Config, Engine};
//! let cfg = Config::load(None)?;
//! let engine = Engine::load(&cfg)?;
//! let text = engine.transcribe_f32(&pcm_16k)?;
//! ```
//!
//! See `docs/EMBEDDING.md` for [`Session`], [`OverlayBackend`], [`RefineBackend`], themes,
//! and the daemon NDJSON API.

pub mod api;
pub mod audio;
pub mod config;
pub mod dsp;
pub mod engine;
pub mod overlay;
pub mod session;
pub mod stt;
pub mod text;
pub mod ui_theme;

pub use api::{
    ApiClient, ApiError, ApiHandler, ApiResult, Event, MAX_API_LINE_BYTES,
    MAX_UTTERANCE_SAMPLES, Op, PcmTranscoder, PeerCred, Request, Response, ServeOptions,
    StubHandler, UtteranceApiHandler, UtteranceBuffer, authorize_peer, authorize_token,
    decode_line, decode_pcm_f32_le_b64, default_socket_path, encode_line, peer_credentials,
    serve_unix, serve_unix_until, serve_unix_with,
};
pub use audio::{RecordConfig, list_input_devices, record, record_while};
pub use config::{
    ApiConfig, Config, DictConfig, MODEL_DOWNLOAD_HINT, UiColors, UiConfig, UiStages, config_get,
    config_set, default_config_path, default_dictionary_path, default_model_dir, expand_tilde,
    list_settable_keys, resolve_model,
};
pub use dsp::{
    DcBlock, DspConfig, Endpoint, STT_RATE, VadConfig, VadEvent, normalize, read_wav, resample,
};
pub use engine::Engine;
pub use overlay::{FnOverlay, NullOverlay, OverlayBackend, Stage};
pub use session::{InjectTyper, Session, SessionBuilder};
pub use stt::Transcriber;
pub use text::{
    COMMANDS, Dictionary, FmtState, LlmRefineConfig, NullRefine, RefineBackend, RefineConfig,
    RuleRefine, TextConfig, TextPipeline,
};
#[cfg(feature = "llm")]
pub use text::LlmRefine;
pub use ui_theme::{
    ResolvedUi, Rgba, ThemePalette, list_themes, parse_rgba, resolve_ui, stage_label,
};
