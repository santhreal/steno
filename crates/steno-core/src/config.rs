//! Configuration: built-in defaults, then `~/.config/steno/config.toml`,
//! then CLI flags (merged by `main.rs`).
//!
//! Dictionary overrides live under `[dict.overrides]` in the same file.
//! A legacy `dictionary.toml` is imported into memory once when that table
//! is empty (never rewritten to disk):
//! - default / XDG config load → `~/.config/steno/dictionary.toml`
//! - explicit `--config` → only a sibling `dictionary.toml` beside that file
//!   (never the operator XDG path; keeps alternate configs isolated)

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::dsp::{DspConfig, VadConfig};
use crate::text::{Dictionary, RefineConfig, TextConfig};

/// Optional per-slot hex overrides under `[ui.colors]`.
///
/// Each field is `#RRGGBB` or `#RRGGBBAA`. Omitted fields keep the active
/// theme preset. Unknown keys are rejected (`deny_unknown_fields`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct UiColors {
    pub bg: Option<String>,
    pub fg: Option<String>,
    pub border: Option<String>,
    pub icon_bg: Option<String>,
    pub icon_fg: Option<String>,
    pub meta: Option<String>,
    pub shadow: Option<String>,
    pub accent: Option<String>,
    pub error: Option<String>,
}

/// Stage labels and transition knobs under `[ui.stages]`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct UiStages {
    /// Label for [`crate::overlay::Stage::Recording`] (default `"Transcribing"`).
    pub recording: String,
    /// Label for [`crate::overlay::Stage::Transcribing`] (default `"Processing"`).
    pub transcribing: String,
    pub done: String,
    pub error: String,
    /// Show the live recording timer in the meta slot.
    pub show_timer: bool,
    /// Stage-change scale pulse duration in milliseconds.
    pub pulse_ms: u64,
}

impl Default for UiStages {
    fn default() -> Self {
        Self {
            recording: "Transcribing".to_string(),
            transcribing: "Processing".to_string(),
            done: "Done".to_string(),
            error: "Error".to_string(),
            show_timer: true,
            pulse_ms: 180,
        }
    }
}

/// Status overlay section (`[ui]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Show the bottom-center status overlay (X11, macOS, and Windows).
    pub overlay: bool,
    /// How long the "done"/"error" stage stays visible before hide.
    pub done_flash_ms: u64,
    /// Built-in overlay theme selected by platform `create` / [`crate::ui_theme::resolve_ui`].
    ///
    /// Palette presets: `"pill"` (default), `"mono"`, `"dusk"`, `"dawn"`,
    /// `"contrast"`. Platform `create` maps `"null"` / `"none"` / `"off"` to a
    /// no-op overlay; resolve still returns the pill palette for those.
    /// Unknown themes log a warning and fall back to pill; UI is fail-open.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Optional hex color overrides layered on the theme preset.
    pub colors: UiColors,
    /// Configurable stage labels and transition timing.
    pub stages: UiStages,
}

fn default_theme() -> String {
    "pill".to_string()
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            overlay: true,
            // Matches the mock's quick Done celebration (~1.2s).
            done_flash_ms: 1200,
            theme: "pill".to_string(),
            colors: UiColors::default(),
            stages: UiStages::default(),
        }
    }
}

// deny_unknown_fields: a typo'd key must fail loudly, not be silently
// ignored. Every nested table sets it too.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Path to a sherpa-onnx model DIRECTORY (encoder/decoder/joiner
    /// ONNX + tokens.txt). Falls back to the single model directory in
    /// `~/.local/share/steno/models/`.
    pub model_path: Option<PathBuf>,
    /// Decode threads. Defaults to half the logical CPUs.
    pub n_threads: u32,
    /// sherpa-onnx execution provider: `"cuda"` (default) or `"cpu"`.
    /// CPU is for CI/headless hosts without NVIDIA. Unknown values fail
    /// closed at load: there is no silent fallback between providers.
    pub provider: String,
    /// Hard cap on one recording.
    pub max_record_secs: u64,
    /// ARMS typing: when true, results are typed into the focused window
    /// via platform keystroke emitter. This is the ONLY way typing can be enabled: a
    /// deliberate, persistent act. The `--type` CLI flag alone errors
    /// out, so no script or test can inject keystrokes into a live
    /// session without the user having armed this file first.
    pub type_output: bool,
    pub vad: VadConfig,
    pub dsp: DspConfig,
    pub text: TextConfig,
    pub ui: UiConfig,
    /// Phrase overrides applied after voice commands.
    pub dict: DictConfig,
    /// Post-format ASR cleanup (`[refine]`).
    pub refine: RefineConfig,
    /// Unix-socket NDJSON API (daemon only).
    pub api: ApiConfig,
}

/// Dictionary section of the single config file (`[dict]` / `[dict.overrides]`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DictConfig {
    pub overrides: HashMap<String, String>,
}

/// Daemon NDJSON socket API (`[api]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    /// Listen on the Unix socket when the daemon starts.
    pub enabled: bool,
    /// Socket path. Empty / unset → `$XDG_RUNTIME_DIR/steno/steno.sock`,
    /// else `$XDG_CACHE_HOME/steno/steno.sock`, else `~/.cache/steno/steno.sock`.
    pub path: Option<PathBuf>,
    /// If set (non-empty), every request must carry a matching `token`.
    pub token: Option<String>,
    /// When true (default), Linux `SO_PEERCRED` rejects peers whose uid
    /// differs from the daemon's. Fail closed if credentials cannot be read.
    pub require_same_uid: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            token: None,
            require_same_uid: true,
        }
    }
}

impl ApiConfig {
    /// Non-empty configured token, if any.
    pub fn required_token(&self) -> Option<&str> {
        self.token.as_deref().filter(|t| !t.is_empty())
    }

    /// True when `path` is set to a non-empty value.
    pub fn configured_path(&self) -> Option<&Path> {
        self.path.as_deref().filter(|p| !p.as_os_str().is_empty())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model_path: None,
            n_threads: (std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(4)
                / 2)
            .max(1),
            provider: "cuda".to_string(),
            max_record_secs: 120,
            type_output: false,
            vad: VadConfig::default(),
            dsp: DspConfig::default(),
            text: TextConfig::default(),
            ui: UiConfig::default(),
            dict: DictConfig::default(),
            refine: RefineConfig::default(),
            api: ApiConfig::default(),
        }
    }
}

impl Config {
    /// Load `path`, or the default config path when `None`. A missing
    /// DEFAULT file is not an error (defaults apply); a missing EXPLICIT
    /// path is an error; a silent typo would be worse. A malformed file
    /// is an error with the offending line context.
    ///
    /// When `[dict.overrides]` is empty, a legacy `dictionary.toml` is
    /// imported into memory (loud deprecation warning). Default loads use
    /// `~/.config/steno/dictionary.toml`; an explicit `--config` only
    /// considers a sibling `dictionary.toml` beside that file (never the
    /// operator XDG path). The on-disk config is never rewritten.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let (path, explicit) = match path {
            Some(p) => (expand_tilde(p)?, true),
            None => (default_config_path()?, false),
        };
        let mut cfg = if !path.exists() {
            if explicit {
                bail!(
                    "config file '{}' does not exist: fix the path or remove the flag",
                    path.display()
                );
            }
            Self::default()
        } else {
            if path.is_dir() {
                bail!(
                    "config path '{}' is a directory: pass a TOML file",
                    path.display()
                );
            }
            let raw = fs::read_to_string(&path).with_context(|| {
                format!(
                    "cannot read config {}: check its permissions",
                    path.display()
                )
            })?;
            let cfg: Self = toml::from_str(&raw)
                .with_context(|| format!("invalid TOML in config {}", path.display()))?;
            cfg
        };
        cfg.migrate_legacy_dictionary(&path, explicit)?;
        // Validate against the config path we tried to load (or the
        // default path when using built-in defaults).
        cfg.validate(&path)?;
        Ok(cfg)
    }

    /// Import a legacy `dictionary.toml` into `dict.overrides` when that
    /// table is empty. Read-only: never writes config.toml.
    ///
    /// - `explicit == false` (default path): `$XDG_CONFIG_HOME/steno/dictionary.toml`
    /// - `explicit == true`: sibling `dictionary.toml` next to `loaded_from` only,
    ///   unless `loaded_from` *is* the default config path (then XDG legacy applies)
    fn migrate_legacy_dictionary(&mut self, loaded_from: &Path, explicit: bool) -> Result<()> {
        // Merge [dict.overrides] into [refine.dictionary]. The pipeline only
        // reads refine.dictionary (via RefineConfig::make_backend), so entries
        // left solely in [dict.overrides] would be silently dropped when both
        // tables are populated. Refine takes precedence on key collision.
        if !self.dict.overrides.is_empty() {
            for (k, v) in &self.dict.overrides {
                self.refine.dictionary.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        if !self.refine.dictionary.is_empty() {
            return Ok(());
        }
        let legacy = legacy_dictionary_candidate(loaded_from, explicit)?;
        let Some(legacy) = legacy else {
            return Ok(());
        };
        if !legacy.exists() {
            return Ok(());
        }
        if legacy.is_dir() {
            bail!(
                    "legacy dictionary path '{}' is a directory: replace it with a TOML file, or move overrides under [refine.dictionary] in config.toml",
                legacy.display()
            );
        }
        let map = Dictionary::load(Some(&legacy))?.to_map();
        self.dict.overrides = map.clone();
        self.refine.dictionary = map;
        log::warn!(
            "{} is deprecated; dictionary overrides now live under [refine.dictionary] in config.toml: copy them there and remove the old file",
            legacy.display()
        );
        Ok(())
    }

    /// Reject values that parse but break downstream (integer truncation
    /// at the STT boundary, a zero recording cap).
    fn validate(&self, path: &Path) -> Result<()> {
        ensure!(
            (1..=(i32::MAX as u32)).contains(&self.n_threads),
            "invalid n_threads = {} in {}: set it between 1 and {}",
            self.n_threads,
            path.display(),
            i32::MAX
        );
        ensure!(
            matches!(self.provider.as_str(), "cuda" | "cpu"),
            "invalid provider = {:?} in {}: set it to \"cuda\" or \"cpu\"",
            self.provider,
            path.display()
        );
        ensure!(
            self.max_record_secs >= 1,
            "invalid max_record_secs = 0 in {}: set it to at least 1 second",
            path.display()
        );
        ensure!(
            self.ui.done_flash_ms <= 10_000,
            "invalid done_flash_ms = {} in {}: set it to at most 10000 (10 seconds)",
            self.ui.done_flash_ms,
            path.display()
        );
        ensure!(
            self.ui.stages.pulse_ms <= 5_000,
            "invalid ui.stages.pulse_ms = {} in {}: set it to at most 5000",
            self.ui.stages.pulse_ms,
            path.display()
        );
        crate::ui_theme::validate_color_overrides(&self.ui.colors, path)?;
        Ok(())
    }
}


/// Dotted keys accepted by [`config_get`] / [`config_set`].
///
/// Top-level: `model_path`, `provider`, `type_output`, `n_threads`, `max_record_secs`.
/// API: `api.enabled`, `api.path`, `api.token`.
/// UI: `ui.theme`, `ui.overlay`, `ui.done_flash_ms`.
/// Stages: `ui.stages.recording`, `ui.stages.transcribing`, `ui.stages.done`,
/// `ui.stages.error`, `ui.stages.show_timer`, `ui.stages.pulse_ms`.
/// Colors: `ui.colors.bg`, `ui.colors.fg`, `ui.colors.border`,
/// `ui.colors.icon_bg`, `ui.colors.icon_fg`, `ui.colors.meta`,
/// `ui.colors.shadow`, `ui.colors.accent`, `ui.colors.error`.
///
/// Unknown keys are rejected. Helpers edit surgically via `toml_edit` and
/// preserve unrelated keys/comments where the document allows. They never
/// rewrite `[dict.overrides]` blindly and do not alter typing fail-closed
/// semantics -- `type_output` is just another typed key.
pub fn list_settable_keys() -> &'static [&'static str] {
    &[
        "model_path",
        "provider",
        "type_output",
        "n_threads",
        "max_record_secs",
        "api.enabled",
        "api.path",
        "api.token",
        "refine.enabled",
        "refine.backend",
        "refine.dictionary.*",
        "ui.theme",
        "ui.overlay",
        "ui.done_flash_ms",
        "ui.stages.recording",
        "ui.stages.transcribing",
        "ui.stages.done",
        "ui.stages.error",
        "ui.stages.show_timer",
        "ui.stages.pulse_ms",
        "ui.colors.bg",
        "ui.colors.fg",
        "ui.colors.border",
        "ui.colors.icon_bg",
        "ui.colors.icon_fg",
        "ui.colors.meta",
        "ui.colors.shadow",
        "ui.colors.accent",
        "ui.colors.error",
    ]
}

fn ensure_settable(key: &str) -> Result<()> {
    if key.starts_with("refine.dictionary.") {
        return Ok(());
    }
    ensure!(
        list_settable_keys().contains(&key),
        "unsupported config key {key:?}: supported keys: {}",
        list_settable_keys().join(", ")
    );
    Ok(())
}

fn item_display(item: &toml_edit::Item) -> Option<String> {
    match item {
        toml_edit::Item::Value(v) => Some(match v {
            toml_edit::Value::String(s) => s.value().clone(),
            toml_edit::Value::Integer(i) => i.value().to_string(),
            toml_edit::Value::Boolean(b) => b.value().to_string(),
            toml_edit::Value::Float(f) => f.value().to_string(),
            other => other.to_string(),
        }),
        toml_edit::Item::None => None,
        _ => Some(item.to_string().trim().to_string()),
    }
}

fn get_dotted<'a>(doc: &'a toml_edit::DocumentMut, key: &str) -> Option<&'a toml_edit::Item> {
    let mut cur = doc.as_item();
    for part in key.split('.') {
        cur = cur.as_table_like()?.get(part)?;
    }
    if cur.is_none() {
        None
    } else {
        Some(cur)
    }
}

fn set_dotted(doc: &mut toml_edit::DocumentMut, key: &str, value: toml_edit::Item) -> Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    ensure!(!parts.is_empty(), "empty config key");
    let mut table = doc.as_table_mut();
    for part in &parts[..parts.len() - 1] {
        if table.get(part).map(|i| i.is_none()).unwrap_or(true) {
            table.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let item = table.get_mut(part).expect("just inserted");
        if !item.is_table() && !item.is_inline_table() {
            bail!(
                "cannot set {key:?}: {part:?} is not a table (found {})",
                item.type_name()
            );
        }
        if item.is_inline_table() {
            if let Some(inline) = item.as_inline_table().cloned() {
                *item = toml_edit::Item::Table(inline.into_table());
            }
        }
        table = item.as_table_mut().expect("checked table");
        // Prefer explicit `[ui.colors]` style over dotted keys when we create.
        table.set_implicit(false);
    }
    let leaf = parts[parts.len() - 1];
    table.insert(leaf, value);
    Ok(())
}

fn typed_toml_value(key: &str, raw: &str) -> Result<toml_edit::Item> {
    let v = match key {
        "type_output" | "ui.overlay" | "ui.stages.show_timer" | "api.enabled" | "refine.enabled" => {
            let b: bool = raw.parse().map_err(|_| {
                anyhow::anyhow!("value for {key} must be a boolean (true/false), got {raw:?}")
            })?;
            toml_edit::value(b)
        }
        "n_threads" | "ui.done_flash_ms" | "ui.stages.pulse_ms" | "max_record_secs" => {
            let n: i64 = raw.parse().map_err(|_| {
                anyhow::anyhow!("value for {key} must be an integer, got {raw:?}")
            })?;
            toml_edit::value(n)
        }
        _ => toml_edit::value(raw),
    };
    Ok(v)
}

/// Read one supported dotted key from a TOML config file.
///
/// Returns `Ok(None)` when the file or key is absent. Rejects unsupported keys.
pub fn config_get(path: &Path, key: &str) -> Result<Option<String>> {
    ensure_settable(key)?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).with_context(|| {
        format!(
            "cannot read config {}: check its permissions",
            path.display()
        )
    })?;
    let doc = raw
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("invalid TOML in {}", path.display()))?;
    Ok(get_dotted(&doc, key).and_then(item_display))
}

/// Set one supported dotted key in a TOML config file, preserving the rest.
///
/// Creates the file (and parent dirs) when missing. Value types follow the
/// key: booleans for `type_output` / `ui.overlay` / `ui.stages.show_timer`,
/// integers for `n_threads` / `ui.done_flash_ms` / `ui.stages.pulse_ms`,
/// strings otherwise.
pub fn config_set(path: &Path, key: &str, value: &str) -> Result<()> {
    ensure_settable(key)?;
    let mut doc = if path.exists() {
        let raw = fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read config {}: check its permissions",
                path.display()
            )
        })?;
        raw.parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("invalid TOML in {}", path.display()))?
    } else {
        toml_edit::DocumentMut::new()
    };
    set_dotted(&mut doc, key, typed_toml_value(key, value)?)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!("cannot create config directory {}", parent.display())
            })?;
        }
    }
    fs::write(path, doc.to_string()).with_context(|| {
        format!(
            "cannot write config {}: check its permissions",
            path.display()
        )
    })?;
    Ok(())
}

/// Default path for the steno configuration file (`~/.config/steno/config.toml`).
pub fn default_config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("steno/config.toml"))
}

/// Legacy standalone dictionary path (migration only).
pub fn default_dictionary_path() -> Result<PathBuf> {
    let dir = config_dir()?;
    let steno_path = dir.join("steno/dictionary.toml");
    if steno_path.exists() {
        return Ok(steno_path);
    }
    let dictate_path = dir.join("dictate/dictionary.toml");
    if dictate_path.exists() {
        return Ok(dictate_path);
    }
    Ok(steno_path)
}

/// Resolve which legacy dictionary file (if any) to consider for migration.
fn legacy_dictionary_candidate(loaded_from: &Path, explicit: bool) -> Result<Option<PathBuf>> {
    if !explicit {
        return Ok(Some(default_dictionary_path()?));
    }
    let default_cfg = default_config_path()?;
    if loaded_from == default_cfg {
        return Ok(Some(default_dictionary_path()?));
    }
    // Alternate --config: never read the operator XDG dictionary.toml.
    Ok(loaded_from.parent().map(|dir| dir.join("dictionary.toml")))
}

/// Default directory for sherpa-onnx model storage (`~/.local/share/steno/models`).
pub fn default_model_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("steno/models"))
}

/// Hint showing how to download a sherpa-onnx model.
pub const MODEL_DOWNLOAD_HINT: &str = "download a model, e.g.:\n  \
    cd ~/.local/share/steno/models && \\\n  \
    curl -LO https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2 && \\\n  \
    tar xjf sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2";

/// Model resolution order: CLI flag, config file, the single sherpa-onnx
/// model directory under the default model directory. A model is a
/// DIRECTORY holding encoder/decoder/joiner ONNX files + tokens.txt.
/// Fails with the exact download command.
pub fn resolve_model(cli: Option<&PathBuf>, cfg: &Config) -> Result<PathBuf> {
    if let Some(p) = cli.or(cfg.model_path.as_ref()) {
        let dir = expand_tilde(p)?;
        ensure!(
            dir.is_dir(),
            "model path '{}' is not a sherpa-onnx model directory: {MODEL_DOWNLOAD_HINT}\n\
             or pass --model /path/to/model-dir",
            dir.display()
        );
        return Ok(dir);
    }
    let dir = default_model_dir()?;
    let mut models: Vec<PathBuf> = Vec::new();
    if dir.is_dir() {
        models = fs::read_dir(&dir)
            .with_context(|| {
                format!(
                    "cannot list model directory '{}': check its permissions",
                    dir.display()
                )
            })?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir() && looks_like_model(p))
            .collect();
        models.sort();
    }
    match models.len() {
        1 => Ok(models
            .into_iter()
            .next()
            .expect("single model directory guaranteed by len == 1 check")),
        0 => bail!("no sherpa-onnx model found in '{}': {MODEL_DOWNLOAD_HINT}", dir.display()),
        _ => bail!(
            "multiple models in '{}': {} -- set model_path in the config to pick one",
            dir.display(),
            models
                .iter()
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// A directory counts as a model when it holds tokens.txt and an encoder
/// ONNX -- the full per-file check happens at load (stt::model_files).
fn looks_like_model(dir: &Path) -> bool {
    dir.join("tokens.txt").is_file()
        && ["encoder.int8.onnx", "encoder.onnx", "encoder.fp16.onnx"]
            .iter()
            .any(|n| dir.join(n).is_file())
}

/// Expand a leading `~` or `~/` to $HOME. Non-UTF-8 paths and paths
/// without a tilde pass through untouched.
pub fn expand_tilde(p: &Path) -> Result<PathBuf> {
    match p.to_str() {
        Some("~") => home_dir(),
        Some(s) if s.starts_with("~/") => Ok(home_dir()?.join(&s[2..])),
        _ => Ok(p.to_path_buf()),
    }
}

fn home_dir() -> Result<PathBuf> {
    home_dir_from(std::env::var_os("HOME"))
}

fn home_dir_from(home: Option<std::ffi::OsString>) -> Result<PathBuf> {
    match home {
        Some(h) if !h.as_os_str().is_empty() => Ok(PathBuf::from(h)),
        _ => bail!(
            "the HOME environment variable is not set: set HOME, or pass explicit paths (--config, --model)"
        ),
    }
}

fn config_dir() -> Result<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.as_os_str().is_empty() => Ok(PathBuf::from(d)),
        _ => Ok(home_dir()?.join(".config")),
    }
}

fn data_dir() -> Result<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.as_os_str().is_empty() => Ok(PathBuf::from(d)),
        _ => Ok(home_dir()?.join(".local/share")),
    }
}

#[cfg(test)]
mod tests {
    //! Regression tests for config parsing, validation, and path
    //! resolution. WHY: these guards exist because a typo'd config key was
    //! silently ignored, `n_threads` could truncate to a negative i32 at
    //! the STT boundary, and a missing/empty model produced a cryptic
    //! load error instead of an actionable message.
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// Serialize tests that mutate process-wide env (XDG_CONFIG_HOME).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Write `content` to a uniquely named temp file and return its path.
    fn temp_file(name: &str, content: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("steno-test-{}-{name}", std::process::id()));
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content).unwrap();
        path
    }

    fn error_of<T>(r: Result<T>) -> String {
        format!("{:#}", r.err().expect("expected an error"))
    }

    /// Unique empty XDG config home so a developer's real dictionary.toml
    /// cannot leak into tests that expect an empty `[dict.overrides]`.
    fn empty_xdg() -> PathBuf {
        let xdg = std::env::temp_dir().join(format!(
            "steno-xdg-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&xdg).unwrap();
        xdg
    }

    /// `Config::load` with XDG_CONFIG_HOME pointed at an empty dir (no legacy file).
    fn load_without_legacy(path: Option<&Path>) -> Result<Config> {
        let _guard = ENV_LOCK.lock().unwrap();
        let xdg = empty_xdg();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: held under ENV_LOCK; restored before unlock.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        let result = Config::load(path);
        match &prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        fs::remove_dir_all(&xdg).ok();
        result
    }

    #[test]
    fn unknown_config_key_is_rejected() {
        let path = temp_file("unknown-key.toml", b"languge = \"en\"\n");
        let err = error_of(load_without_legacy(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("languge"), "error names the typo'd key: {err}");
    }

    #[test]
    fn known_keys_still_parse() {
        let path = temp_file(
            "known-keys.toml",
            b"n_threads = 4\nmax_record_secs = 30\ntype_output = true\n",
        );
        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(cfg.n_threads, 4);
        assert_eq!(cfg.max_record_secs, 30);
        assert!(cfg.type_output);
        assert!(cfg.dict.overrides.is_empty());
        assert!(cfg.model_path.is_none());
        assert_eq!(cfg.provider, "cuda");
    }

    #[test]
    fn provider_defaults_to_cuda() {
        // WHY: omitted provider must stay cuda so existing configs keep
        // GPU decode; CPU is opt-in for CI/headless.
        let cfg = Config::default();
        assert_eq!(cfg.provider, "cuda");
        let path = temp_file("provider-default.toml", b"n_threads = 2\n");
        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(cfg.provider, "cuda");
    }

    #[test]
    fn provider_cuda_and_cpu_parse() {
        for value in ["cuda", "cpu"] {
            let path = temp_file(
                &format!("provider-{value}.toml"),
                format!("provider = \"{value}\"\n").as_bytes(),
            );
            let cfg = load_without_legacy(Some(&path)).unwrap();
            fs::remove_file(&path).ok();
            assert_eq!(cfg.provider, value);
        }
    }

    #[test]
    fn unknown_provider_is_rejected() {
        // WHY: a typo like "gpu" must fail closed with a fix hint: never
        // silently map onto cuda or cpu.
        let path = temp_file("provider-bad.toml", b"provider = \"gpu\"\n");
        let err = error_of(load_without_legacy(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("provider"), "{err}");
        assert!(err.contains("gpu"), "{err}");
        assert!(
            err.contains("\"cuda\"") || err.contains("cuda"),
            "error must name allowed values: {err}"
        );
        assert!(
            err.contains("\"cpu\"") || err.contains("cpu"),
            "error must name allowed values: {err}"
        );
    }

    #[test]
    fn refine_config_defaults_enabled_rules() {
        // WHY: [refine] defaults on with rules so post-STT cleanup is
        // active without config; disable via enabled = false.
        let cfg = Config::default();
        assert!(cfg.refine.enabled);
        assert_eq!(cfg.refine.backend, "rules");
    }

    #[test]
    fn refine_section_parses_and_unknown_key_rejected() {
        let path = temp_file(
            "refine-ok.toml",
            br#"
[refine]
enabled = false
backend = "rules"
"#,
        );
        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert!(!cfg.refine.enabled);
        assert_eq!(cfg.refine.backend, "rules");
        assert_eq!(cfg.refine.make_backend().refine("the the"), "the the");

        let bad = temp_file("refine-bad.toml", b"[refine]\nextra = true\n");
        let err = error_of(load_without_legacy(Some(&bad)));
        fs::remove_file(&bad).ok();
        assert!(err.contains("extra") || err.contains("unknown"), "{err}");
    }

    #[test]
    fn api_config_defaults_enabled() {
        // WHY: daemon usefulness -- [api] defaults to enabled so `steno start`
        // exposes the socket without extra config. require_same_uid defaults true
        // (fail-closed peer uid gate).
        let cfg = Config::default();
        assert!(cfg.api.enabled);
        assert!(cfg.api.path.is_none());
        assert!(cfg.api.token.is_none());
        assert!(cfg.api.require_same_uid);
        assert!(cfg.api.required_token().is_none());
        assert!(cfg.api.configured_path().is_none());
    }

    #[test]
    fn api_section_parses_and_unknown_key_rejected() {
        let path = temp_file(
            "api-ok.toml",
            br#"
[api]
enabled = true
path = "/tmp/steno-test.sock"
token = "s3cret"
require_same_uid = false
"#,
        );
        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert!(cfg.api.enabled);
        assert!(!cfg.api.require_same_uid);
        assert_eq!(
            cfg.api.configured_path().map(|p| p.to_string_lossy().into_owned()),
            Some("/tmp/steno-test.sock".into())
        );
        assert_eq!(cfg.api.required_token(), Some("s3cret"));

        let bad = temp_file("api-bad.toml", b"[api]\nextra = true\n");
        let err = error_of(load_without_legacy(Some(&bad)));
        fs::remove_file(&bad).ok();
        assert!(err.contains("extra"), "error names the typo\'d key: {err}");
    }

    #[test]
    fn api_empty_token_and_path_mean_unset() {
        let path = temp_file(
            "api-empty.toml",
            br#"
[api]
path = ""
token = ""
"#,
        );
        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert!(cfg.api.enabled);
        assert!(cfg.api.require_same_uid);
        assert!(cfg.api.configured_path().is_none());
        assert!(cfg.api.required_token().is_none());
    }

    #[test]
    fn dict_overrides_load_and_dictionary_applies() {
        // WHY: overrides must live in the single config file and feed
        // Dictionary without a separate dictionary.toml.
        let path = temp_file(
            "dict-overrides.toml",
            br#"
n_threads = 2

[dict.overrides]
"mukund" = "Mukund"
"um" = ""
"main street" = "Main Street"
"#,
        );
        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(cfg.dict.overrides.get("mukund").map(String::as_str), Some("Mukund"));
        assert_eq!(cfg.dict.overrides.get("um").map(String::as_str), Some(""));
        let dict = Dictionary::from_map(cfg.dict.overrides.clone());
        assert_eq!(dict.apply("hello mukund um on main street"), "hello Mukund on Main Street");
    }

    #[test]
    fn unknown_dict_key_is_rejected() {
        let path = temp_file(
            "dict-unknown.toml",
            b"[dict]\nextra = 1\n",
        );
        let err = error_of(load_without_legacy(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("extra"), "error names the typo'd key: {err}");
    }

    #[test]
    fn legacy_dictionary_toml_migrates_when_overrides_empty() {
        // WHY: users with a pre-single-config dictionary.toml must keep
        // working without rewriting config.toml on disk.
        let _guard = ENV_LOCK.lock().unwrap();
        let xdg = std::env::temp_dir().join(format!(
            "steno-xdg-mig-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dictate_dir = xdg.join("dictate");
        fs::create_dir_all(&dictate_dir).unwrap();
        fs::write(
            dictate_dir.join("dictionary.toml"),
            b"[overrides]\n\"veyyon\" = \"veyyon\"\n\"um\" = \"\"\n",
        )
        .unwrap();
        // Default config path under this XDG home (load(None) migration path).
        let config_path = dictate_dir.join("config.toml");
        fs::write(&config_path, b"n_threads = 3\n").unwrap();

        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: held under ENV_LOCK; restored before unlock.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        let cfg = Config::load(None);
        match &prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let cfg = cfg.expect("migration load");
        let on_disk = fs::read_to_string(&config_path).unwrap();
        fs::remove_dir_all(&xdg).ok();

        assert_eq!(cfg.dict.overrides.get("veyyon").map(String::as_str), Some("veyyon"));
        assert_eq!(cfg.dict.overrides.get("um").map(String::as_str), Some(""));
        // Config file on disk was not rewritten (still just n_threads).
        assert_eq!(on_disk, "n_threads = 3\n");
        let dict = Dictionary::from_entries(cfg.dict.overrides.clone());
        assert_eq!(dict.apply("say veyyon um now"), "say veyyon now");
    }

    #[test]
    fn explicit_config_does_not_import_xdg_dictionary() {
        // WHY: `steno --config /tmp/foo.toml config show` must not bleed
        // the operator's ~/.config/dictate/dictionary.toml into the report.
        let _guard = ENV_LOCK.lock().unwrap();
        let xdg = std::env::temp_dir().join(format!(
            "steno-xdg-iso-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dictate_dir = xdg.join("dictate");
        fs::create_dir_all(&dictate_dir).unwrap();
        fs::write(
            dictate_dir.join("dictionary.toml"),
            b"[overrides]\n\"legacy\" = \"LEGACY\"\n",
        )
        .unwrap();
        let config_path = temp_file("iso-empty.toml", b"n_threads = 3\n");

        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        let cfg = Config::load(Some(&config_path));
        match &prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let cfg = cfg.expect("isolated load");
        fs::remove_file(&config_path).ok();
        fs::remove_dir_all(&xdg).ok();

        assert!(
            cfg.dict.overrides.is_empty(),
            "explicit --config must not import XDG dictionary.toml: {:?}",
            cfg.dict.overrides
        );
    }

    #[test]
    fn explicit_config_imports_sibling_dictionary_only() {
        let dir = std::env::temp_dir().join(format!(
            "steno-sib-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("dictionary.toml"),
            b"[overrides]\n\"sib\" = \"SIB\"\n",
        )
        .unwrap();
        let config_path = dir.join("config.toml");
        fs::write(&config_path, b"n_threads = 4\n").unwrap();

        let cfg = Config::load(Some(&config_path)).expect("sibling migrate");
        fs::remove_dir_all(&dir).ok();

        assert_eq!(cfg.dict.overrides.get("sib").map(String::as_str), Some("SIB"));
    }

    #[test]
    fn inline_dict_overrides_skip_legacy_file() {
        // WHY: once [dict.overrides] exists, the legacy file must not
        // silently replace or merge over it.
        let _guard = ENV_LOCK.lock().unwrap();
        let xdg = std::env::temp_dir().join(format!(
            "steno-xdg-skip-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dictate_dir = xdg.join("dictate");
        fs::create_dir_all(&dictate_dir).unwrap();
        fs::write(
            dictate_dir.join("dictionary.toml"),
            b"[overrides]\n\"legacy\" = \"LEGACY\"\n",
        )
        .unwrap();
        let config_path = temp_file(
            "mig-skip.toml",
            b"[dict.overrides]\n\"inline\" = \"INLINE\"\n",
        );

        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        let cfg = Config::load(Some(&config_path));
        match &prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let cfg = cfg.expect("load");
        fs::remove_file(&config_path).ok();
        fs::remove_dir_all(&xdg).ok();

        assert_eq!(cfg.dict.overrides.get("inline").map(String::as_str), Some("INLINE"));
        assert!(!cfg.dict.overrides.contains_key("legacy"));
    }

    #[test]
    fn malformed_legacy_dictionary_errors_naming_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let xdg = std::env::temp_dir().join(format!(
            "steno-xdg-bad-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dictate_dir = xdg.join("dictate");
        fs::create_dir_all(&dictate_dir).unwrap();
        let legacy = dictate_dir.join("dictionary.toml");
        fs::write(&legacy, b"overrides = \"not-a-table\"\n").unwrap();
        fs::write(dictate_dir.join("config.toml"), b"n_threads = 2\n").unwrap();

        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        let err = error_of(Config::load(None));
        match &prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        fs::remove_dir_all(&xdg).ok();

        assert!(
            err.contains(legacy.file_name().unwrap().to_str().unwrap())
                || err.contains("dictionary.toml"),
            "error must name the legacy path: {err}"
        );
        assert!(
            err.contains("invalid dictionary") || err.contains("overrides"),
            "{err}"
        );
    }

    #[test]
    fn wrong_type_is_rejected() {
        let path = temp_file("wrong-type.toml", b"n_threads = \"lots\"\n");
        let err = error_of(load_without_legacy(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("invalid TOML"), "{err}");
    }

    #[test]
    fn negative_n_threads_is_rejected() {
        let path = temp_file("negative.toml", b"n_threads = -5\n");
        let err = error_of(load_without_legacy(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("invalid TOML"), "{err}");
    }

    #[test]
    fn zero_n_threads_is_rejected_with_fix() {
        // WHY: n_threads = 0 would reach the recognizer as 0 threads.
        let path = temp_file("zero-threads.toml", b"n_threads = 0\n");
        let err = error_of(load_without_legacy(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("n_threads"), "{err}");
        assert!(err.contains("between 1 and"), "{err}");
    }

    #[test]
    fn huge_n_threads_is_rejected_before_i32_truncation() {
        // WHY: 3_000_000_000 as i32 wraps negative; the recognizer would
        // get a negative thread count.
        let path = temp_file("huge-threads.toml", b"n_threads = 3000000000\n");
        let err = error_of(load_without_legacy(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("n_threads"), "{err}");
    }

    #[test]
    fn zero_max_record_secs_is_rejected() {
        let path = temp_file("zero-record.toml", b"max_record_secs = 0\n");
        let err = error_of(load_without_legacy(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("max_record_secs"), "{err}");
    }

    #[test]
    fn explicit_config_path_that_is_a_directory_errors() {
        let dir = std::env::temp_dir();
        let err = error_of(load_without_legacy(Some(&dir)));
        assert!(err.contains("is a directory"), "{err}");
    }

    #[test]
    fn missing_explicit_config_errors() {
        let path = std::env::temp_dir().join("dictate-test-does-not-exist.toml");
        let err = error_of(load_without_legacy(Some(&path)));
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn resolve_model_missing_path_says_what_to_do() {
        // WHY: an explicit path that does not exist must fail with the
        // download command, not an ONNX runtime error at load time.
        let cfg = Config::default();
        let path = PathBuf::from("/nonexistent/no-such-model-dir");
        let err = error_of(resolve_model(Some(&path), &cfg));
        assert!(err.contains("not a sherpa-onnx model directory"), "{err}");
        assert!(err.contains("sherpa-onnx-nemo-parakeet"), "{err}");
    }

    #[test]
    fn resolve_model_rejects_plain_file() {
        // WHY: a whisper-era ggml .bin pin must fail loudly at resolve
        // time with migration guidance, not deep inside onnxruntime.
        let cfg = Config::default();
        let path = temp_file("ggml-base.en.bin", b"old whisper model");
        let err = error_of(resolve_model(Some(&path), &cfg));
        fs::remove_file(&path).ok();
        assert!(err.contains("not a sherpa-onnx model directory"), "{err}");
    }

    #[test]
    fn resolve_model_accepts_model_dir() {
        let cfg = Config::default();
        let dir = std::env::temp_dir().join("dictate-cfg-model-dir");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tokens.txt"), b"x").unwrap();
        fs::write(dir.join("encoder.int8.onnx"), b"x").unwrap();
        let got = resolve_model(Some(&dir), &cfg).unwrap();
        fs::remove_dir_all(&dir).ok();
        assert_eq!(got, dir);
    }

    #[test]
    fn expand_tilde_variants() {
        let home = home_dir().unwrap();
        assert_eq!(expand_tilde(Path::new("~")).unwrap(), home);
        assert_eq!(
            expand_tilde(Path::new("~/x/y.bin")).unwrap(),
            home.join("x/y.bin")
        );
        assert_eq!(
            expand_tilde(Path::new("/abs/path")).unwrap(),
            PathBuf::from("/abs/path")
        );
        assert_eq!(
            expand_tilde(Path::new("relative/path")).unwrap(),
            PathBuf::from("relative/path")
        );
        // "~user" form is not expanded (no tilde-slash after ~).
        assert_eq!(
            expand_tilde(Path::new("~root/x")).unwrap(),
            PathBuf::from("~root/x")
        );
    }

    #[test]
    fn home_dir_from_unset_is_an_error_not_a_panic() {
        // WHY: this used to be .expect("HOME is set on Linux"), a panic
        // with no corrective action under e.g. systemd.
        let err = error_of(home_dir_from(None));
        assert!(err.contains("HOME"), "{err}");
        assert!(err.contains("--model"), "{err}");
        assert!(!err.contains("--dictionary"), "{err}");
        let err = error_of(home_dir_from(Some(std::ffi::OsString::new())));
        assert!(err.contains("HOME"), "{err}");
        assert_eq!(
            home_dir_from(Some(std::ffi::OsString::from("/home/u"))).unwrap(),
            PathBuf::from("/home/u")
        );
    }

    #[test]
    fn ui_section_defaults_without_colors_or_stages() {
        // WHY: existing configs with bare [ui] / no ui table must keep loading.
        let path = temp_file(
            "ui-bare.toml",
            br#"
[ui]
overlay = true
done_flash_ms = 900
theme = "pill"
"#,
        );
        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert!(cfg.ui.overlay);
        assert_eq!(cfg.ui.done_flash_ms, 900);
        assert_eq!(cfg.ui.theme, "pill");
        assert_eq!(cfg.ui.colors, UiColors::default());
        assert_eq!(cfg.ui.stages.recording, "Transcribing");
        assert_eq!(cfg.ui.stages.transcribing, "Processing");
        assert_eq!(cfg.ui.stages.done, "Done");
        assert_eq!(cfg.ui.stages.error, "Error");
        assert!(cfg.ui.stages.show_timer);
        assert_eq!(cfg.ui.stages.pulse_ms, 180);
    }

    #[test]
    fn ui_colors_and_stages_parse_and_unknown_color_key_rejected() {
        // WHY: deny_unknown_fields on [ui.colors] must catch typos like fgs.
        let path = temp_file(
            "ui-full.toml",
            br##"
[ui]
theme = "dusk"

[ui.colors]
fg = "#FF0000FF"

[ui.stages]
recording = "Listening"
pulse_ms = 90
"##,
        );
        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(cfg.ui.theme, "dusk");
        assert_eq!(cfg.ui.colors.fg.as_deref(), Some("#FF0000FF"));
        assert!(cfg.ui.colors.bg.is_none());
        assert_eq!(cfg.ui.stages.recording, "Listening");
        assert_eq!(cfg.ui.stages.pulse_ms, 90);

        let bad = temp_file("ui-colors-typo.toml", b"[ui.colors]\nfgs = \"#fff\"\n");
        let err = error_of(load_without_legacy(Some(&bad)));
        fs::remove_file(&bad).ok();
        assert!(err.contains("fgs") || err.contains("unknown"), "{err}");
    }

    #[test]
    fn ui_colors_bad_hex_is_rejected_at_load() {
        let path = temp_file(
            "ui-bad-hex.toml",
            b"[ui.colors]\nbg = \"nope\"\n",
        );
        let err = error_of(load_without_legacy(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("ui.colors.bg"), "{err}");
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn config_get_set_round_trip_preserves_other_keys() {
        // WHY: CLI helpers must surgically edit known keys without wiping dict.
        let path = temp_file(
            "getset.toml",
            br#"
n_threads = 4
type_output = false

[dict.overrides]
"um" = ""

[ui]
theme = "pill"
"#,
        );
        assert_eq!(config_get(&path, "n_threads").unwrap().as_deref(), Some("4"));
        assert_eq!(config_get(&path, "ui.theme").unwrap().as_deref(), Some("pill"));
        assert!(config_get(&path, "ui.colors.fg").unwrap().is_none());

        config_set(&path, "ui.theme", "dusk").unwrap();
        config_set(&path, "ui.colors.fg", "#AABBCCFF").unwrap();
        config_set(&path, "ui.stages.recording", "Listening").unwrap();
        config_set(&path, "n_threads", "6").unwrap();

        assert_eq!(config_get(&path, "ui.theme").unwrap().as_deref(), Some("dusk"));
        assert_eq!(
            config_get(&path, "ui.colors.fg").unwrap().as_deref(),
            Some("#AABBCCFF")
        );
        assert_eq!(
            config_get(&path, "ui.stages.recording").unwrap().as_deref(),
            Some("Listening")
        );
        assert_eq!(config_get(&path, "n_threads").unwrap().as_deref(), Some("6"));

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[dict.overrides]"), "{raw}");
        assert!(raw.contains("\"um\"") || raw.contains("um"), "{raw}");
        assert!(raw.contains("type_output"), "{raw}");

        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(cfg.ui.theme, "dusk");
        assert_eq!(cfg.ui.colors.fg.as_deref(), Some("#AABBCCFF"));
        assert_eq!(cfg.ui.stages.recording, "Listening");
        assert_eq!(cfg.n_threads, 6);
        assert!(!cfg.type_output);
        assert_eq!(cfg.dict.overrides.get("um").map(String::as_str), Some(""));
    }

    #[test]
    fn config_set_rejects_unknown_key() {
        let path = temp_file("getset-bad.toml", b"n_threads = 2\n");
        let err = error_of(config_set(&path, "ui.colour", "x"));
        fs::remove_file(&path).ok();
        assert!(err.contains("unsupported config key"), "{err}");
    }

    #[test]
    fn set_dotted_converts_inline_table_to_standard_table() {
        // WHY: setting nested keys inside an inline table must convert it to a standard Table
        // rather than panicking on as_table_mut().
        let mut doc = "ui = { theme = \"pill\" }"
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        set_dotted(&mut doc, "ui.theme", toml_edit::value("dusk")).unwrap();
        set_dotted(&mut doc, "ui.colors.fg", toml_edit::value("#112233FF")).unwrap();

        assert_eq!(
            get_dotted(&doc, "ui.theme").and_then(item_display).as_deref(),
            Some("dusk")
        );
        assert_eq!(
            get_dotted(&doc, "ui.colors.fg").and_then(item_display).as_deref(),
            Some("#112233FF")
        );
    }

    #[test]
    fn config_set_handles_inline_tables_without_panic() {
        let path = temp_file("inline-table.toml", b"ui = { theme = \"pill\" }\n");
        config_set(&path, "ui.theme", "dusk").unwrap();
        config_set(&path, "ui.colors.fg", "#AABBCCFF").unwrap();

        assert_eq!(
            config_get(&path, "ui.theme").unwrap().as_deref(),
            Some("dusk")
        );
        assert_eq!(
            config_get(&path, "ui.colors.fg").unwrap().as_deref(),
            Some("#AABBCCFF")
        );

        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(cfg.ui.theme, "dusk");
        assert_eq!(cfg.ui.colors.fg.as_deref(), Some("#AABBCCFF"));
    }
    #[test]
    fn list_settable_keys_includes_api_and_max_record_secs() {
        let keys = list_settable_keys();
        assert!(keys.contains(&"api.enabled"));
        assert!(keys.contains(&"api.path"));
        assert!(keys.contains(&"api.token"));
        assert!(keys.contains(&"max_record_secs"));
    }

    #[test]
    fn config_set_get_new_settable_keys() {
        let path = temp_file("new-settable.toml", b"");
        config_set(&path, "api.enabled", "false").unwrap();
        config_set(&path, "api.path", "/tmp/dictate-test.sock").unwrap();
        config_set(&path, "api.token", "secret-token").unwrap();
        config_set(&path, "max_record_secs", "120").unwrap();

        assert_eq!(config_get(&path, "api.enabled").unwrap().as_deref(), Some("false"));
        assert_eq!(config_get(&path, "api.path").unwrap().as_deref(), Some("/tmp/dictate-test.sock"));
        assert_eq!(config_get(&path, "api.token").unwrap().as_deref(), Some("secret-token"));
        assert_eq!(config_get(&path, "max_record_secs").unwrap().as_deref(), Some("120"));

        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert!(!cfg.api.enabled);
        assert_eq!(cfg.api.path, Some(PathBuf::from("/tmp/dictate-test.sock")));
        assert_eq!(cfg.api.token.as_deref(), Some("secret-token"));
        assert_eq!(cfg.max_record_secs, 120);
    }
    #[test]
    fn config_set_get_refine_keys() {
        let path = temp_file("refine-settable.toml", b"");
        config_set(&path, "refine.enabled", "true").unwrap();
        config_set(&path, "refine.backend", "rules").unwrap();
        config_set(&path, "refine.dictionary.vayon", "veyyon").unwrap();

        assert_eq!(config_get(&path, "refine.enabled").unwrap().as_deref(), Some("true"));
        assert_eq!(config_get(&path, "refine.backend").unwrap().as_deref(), Some("rules"));
        assert_eq!(config_get(&path, "refine.dictionary.vayon").unwrap().as_deref(), Some("veyyon"));

        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert!(cfg.refine.enabled);
        assert_eq!(cfg.refine.backend, "rules");
        assert_eq!(cfg.refine.dictionary.get("vayon").map(String::as_str), Some("veyyon"));
    }
}
