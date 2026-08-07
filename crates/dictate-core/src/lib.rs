//! Embeddable offline speech-to-text: config, audio, DSP, STT, text pipeline,
//! daemon API types, session, and the high-level [`Engine`].

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
    list_settable_keys, resolve_model,
};
pub use ui_theme::{
    ResolvedUi, Rgba, ThemePalette, list_themes, parse_rgba, resolve_ui, stage_label,
};
pub use engine::Engine;
pub use overlay::{NullOverlay, OverlayBackend, Stage};
pub use session::{InjectTyper, Session, SessionBuilder};
pub use stt::Transcriber;
pub use text::{Dictionary, NullRefine, RefineBackend, RefineConfig, RuleRefine, TextConfig, TextPipeline};
