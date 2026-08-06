//! Configuration: built-in defaults, then `~/.config/dictate/config.toml`,
//! then CLI flags (merged by `main.rs`).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::dsp::{DspConfig, VadConfig};
use crate::text::TextConfig;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Path to a ggml whisper model. Falls back to the first `*.bin` in
    /// `~/.local/share/dictate/models/`.
    pub model_path: Option<PathBuf>,
    /// Dictionary TOML with an `[overrides]` table. Falls back to
    /// `~/.config/dictate/dictionary.toml` when present.
    pub dictionary_path: Option<PathBuf>,
    /// Spoken language code ("en", "de", ...) or "auto".
    pub language: String,
    /// Decode threads. Defaults to half the logical CPUs.
    pub n_threads: u32,
    /// Hard cap on one recording.
    pub max_record_secs: u64,
    /// Type into the focused window with xdotool instead of printing.
    pub type_output: bool,
    pub vad: VadConfig,
    pub dsp: DspConfig,
    pub text: TextConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model_path: None,
            dictionary_path: None,
            language: "auto".into(),
            n_threads: (std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(4)
                / 2)
            .max(1),
            max_record_secs: 120,
            type_output: false,
            vad: VadConfig::default(),
            dsp: DspConfig::default(),
            text: TextConfig::default(),
        }
    }
}

impl Config {
    /// Load `path`, or the default config path when `None`. A missing file is
    /// not an error: defaults apply. A malformed file is an error with the
    /// offending line context.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = match path {
            Some(p) => expand_tilde(p),
            None => default_config_path(),
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("invalid TOML in config {}", path.display()))
    }
}

pub fn default_config_path() -> PathBuf {
    config_dir().join("dictate/config.toml")
}

pub fn default_dictionary_path() -> PathBuf {
    config_dir().join("dictate/dictionary.toml")
}

pub fn default_model_dir() -> PathBuf {
    data_dir().join("dictate/models")
}

/// Model resolution order: CLI flag, config file, first `*.bin` in the
/// default model directory. Fails with the exact download command.
pub fn resolve_model(cli: Option<&PathBuf>, cfg: &Config) -> Result<PathBuf> {
    if let Some(p) = cli {
        return Ok(expand_tilde(p));
    }
    if let Some(p) = &cfg.model_path {
        return Ok(expand_tilde(p));
    }
    let dir = default_model_dir();
    if dir.is_dir() {
        let mut bins: Vec<PathBuf> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "bin"))
            .collect();
        bins.sort();
        if let Some(first) = bins.into_iter().next() {
            return Ok(first);
        }
    }
    bail!(
        "no whisper model found. Download one, e.g.:\n  \
         mkdir -p {dir} && curl -L -o {dir}/ggml-base.en.bin \\\n    \
         https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin\n\
         or pass --model /path/to/ggml-model.bin",
        dir = dir.display()
    )
}

/// Dictionary resolution order: CLI flag, config file, default path if it
/// exists, else `None` (empty dictionary).
pub fn resolve_dictionary(cli: Option<&PathBuf>, cfg: &Config) -> Option<PathBuf> {
    if let Some(p) = cli {
        return Some(expand_tilde(p));
    }
    if let Some(p) = &cfg.dictionary_path {
        return Some(expand_tilde(p));
    }
    let default = default_dictionary_path();
    default.exists().then_some(default)
}

pub fn expand_tilde(p: &Path) -> PathBuf {
    match p.to_str() {
        Some(s) if s.starts_with("~/") => {
            home_dir().join(s.trim_start_matches("~/"))
        }
        _ => p.to_path_buf(),
    }
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is set on Linux"))
}

fn config_dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) => PathBuf::from(d),
        None => home_dir().join(".config"),
    }
}

fn data_dir() -> PathBuf {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) => PathBuf::from(d),
        None => home_dir().join(".local/share"),
    }
}
