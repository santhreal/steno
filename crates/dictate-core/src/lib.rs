//! Embeddable offline speech-to-text: config, audio, DSP, STT, text pipeline,
//! daemon API types, session, and the high-level [`Engine`].
//!
//! # Quick start
//!
//! ```ignore
//! use dictate_core::{Config, Engine};
//! let cfg = Config::load(None)?;
//! let engine = Engine::load(&cfg)?;
//! let text = engine.transcribe_f32(&pcm_16k)?;
//! ```
//!
//! See `docs/EMBEDDING.md` for Session, OverlayBackend, RefineBackend, themes,
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

pub use config::{
    ApiConfig, Config, DictConfig, UiColors, UiConfig, UiStages, config_get, config_set,
    default_config_path, default_dictionary_path, default_model_dir, expand_tilde,
    list_settable_keys, resolve_model,
};
pub use ui_theme::{
    ResolvedUi, Rgba, ThemePalette, list_themes, parse_rgba, resolve_ui, stage_label,
};
pub use engine::Engine;
pub use overlay::{NullOverlay, OverlayBackend, Stage};
pub use session::{InjectTyper, Session, SessionBuilder};
pub use stt::Transcriber;
pub use text::{
    COMMANDS, Dictionary, FmtState, NullRefine, RefineBackend, RefineConfig, RuleRefine,
    TextConfig, TextPipeline,
};
