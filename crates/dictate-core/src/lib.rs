//! Embeddable offline speech-to-text: config, audio, DSP, STT, text pipeline,
//! daemon API types, and the high-level [`Engine`].

pub mod api;
pub mod audio;
pub mod config;
pub mod dsp;
pub mod engine;
pub mod overlay;
pub mod stt;
pub mod text;

pub use config::{ApiConfig, Config, DictConfig, UiConfig, resolve_model};
pub use engine::Engine;
pub use overlay::{NullOverlay, OverlayBackend, Stage};
pub use stt::Transcriber;
pub use text::{Dictionary, TextConfig, TextPipeline};
