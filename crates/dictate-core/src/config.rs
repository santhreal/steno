//! Configuration: built-in defaults, then `~/.config/dictate/config.toml`,
//! then CLI flags (merged by `main.rs`).
//!
//! Dictionary overrides live under `[dict.overrides]` in the same file.
//! A legacy `~/.config/dictate/dictionary.toml` is imported into memory
//! once when that table is empty (never rewritten to disk).

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::dsp::{DspConfig, VadConfig};
use crate::text::{Dictionary, RefineConfig, TextConfig};

/// Status overlay section (`[ui]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Show the bottom-center status overlay (X11 only).
    pub overlay: bool,
    /// How long the "done"/"error" stage stays visible before hide.
    pub done_flash_ms: u64,
    /// Built-in overlay theme selected by platform `create`.
    ///
    /// Known values: `"pill"` (default X11 pill), `"null"` / `"none"` /
    /// `"off"` (no-op). Unknown themes log a warning and fall back to the
    /// pill — UI is fail-open.
    #[serde(default = "default_theme")]
    pub theme: String,
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
    /// `~/.local/share/dictate/models/`.
    pub model_path: Option<PathBuf>,
    /// Decode threads. Defaults to half the logical CPUs.
    pub n_threads: u32,
    /// sherpa-onnx execution provider: `"cuda"` (default) or `"cpu"`.
    /// CPU is for CI/headless hosts without NVIDIA. Unknown values fail
    /// closed at load — there is no silent fallback between providers.
    pub provider: String,
    /// Hard cap on one recording.
    pub max_record_secs: u64,
    /// ARMS typing: when true, results are typed into the focused window
    /// via xdotool. This is the ONLY way typing can be enabled — a
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
    /// Socket path. Empty / unset → `$XDG_RUNTIME_DIR/dictate/dictate.sock`
    /// (else `~/.cache/dictate/dictate.sock`).
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
    /// path is an error — a silent typo would be worse. A malformed file
    /// is an error with the offending line context.
    ///
    /// When `[dict.overrides]` is empty, a legacy
    /// `~/.config/dictate/dictionary.toml` is imported into memory (loud
    /// deprecation warning). The on-disk config is never rewritten.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let (path, explicit) = match path {
            Some(p) => (expand_tilde(p)?, true),
            None => (default_config_path()?, false),
        };
        let mut cfg = if !path.exists() {
            if explicit {
                bail!(
                    "config file '{}' does not exist — fix the path or remove the flag",
                    path.display()
                );
            }
            Self::default()
        } else {
            if path.is_dir() {
                bail!(
                    "config path '{}' is a directory — pass a TOML file",
                    path.display()
                );
            }
            let raw = fs::read_to_string(&path).with_context(|| {
                format!(
                    "cannot read config {} — check its permissions",
                    path.display()
                )
            })?;
            let cfg: Self = toml::from_str(&raw)
                .with_context(|| format!("invalid TOML in config {}", path.display()))?;
            cfg
        };
        cfg.migrate_legacy_dictionary()?;
        // Validate against the config path we tried to load (or the
        // default path when using built-in defaults).
        cfg.validate(&path)?;
        Ok(cfg)
    }

    /// Import `~/.config/dictate/dictionary.toml` into `dict.overrides`
    /// when that table is empty. Read-only: never writes config.toml.
    fn migrate_legacy_dictionary(&mut self) -> Result<()> {
        if !self.dict.overrides.is_empty() {
            return Ok(());
        }
        let legacy = default_dictionary_path()?;
        if !legacy.exists() {
            return Ok(());
        }
        if legacy.is_dir() {
            bail!(
                "legacy dictionary path '{}' is a directory — replace it with a TOML file, or move overrides under [dict.overrides] in config.toml",
                legacy.display()
            );
        }
        self.dict.overrides = Dictionary::load(Some(&legacy))?.to_map();
        log::warn!(
            "{} is deprecated; dictionary overrides now live under [dict.overrides] in config.toml — copy them there and remove the old file",
            legacy.display()
        );
        Ok(())
    }

    /// Reject values that parse but break downstream (integer truncation
    /// at the STT boundary, a zero recording cap).
    fn validate(&self, path: &Path) -> Result<()> {
        ensure!(
            (1..=(i32::MAX as u32)).contains(&self.n_threads),
            "invalid n_threads = {} in {} — set it between 1 and {}",
            self.n_threads,
            path.display(),
            i32::MAX
        );
        ensure!(
            matches!(self.provider.as_str(), "cuda" | "cpu"),
            "invalid provider = {:?} in {} — set it to \"cuda\" or \"cpu\"",
            self.provider,
            path.display()
        );
        ensure!(
            self.max_record_secs >= 1,
            "invalid max_record_secs = 0 in {} — set it to at least 1 second",
            path.display()
        );
        ensure!(
            self.ui.done_flash_ms <= 10_000,
            "invalid done_flash_ms = {} in {} — set it to at most 10000 (10 seconds)",
            self.ui.done_flash_ms,
            path.display()
        );
        Ok(())
    }
}

pub fn default_config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("dictate/config.toml"))
}

/// Legacy standalone dictionary path (migration only).
pub fn default_dictionary_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("dictate/dictionary.toml"))
}

pub fn default_model_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("dictate/models"))
}

/// Model resolution order: CLI flag, config file, the single sherpa-onnx
/// model directory under the default model directory. A model is a
/// DIRECTORY holding encoder/decoder/joiner ONNX files + tokens.txt.
/// Fails with the exact download command.
pub fn resolve_model(cli: Option<&PathBuf>, cfg: &Config) -> Result<PathBuf> {
    const DOWNLOAD_HINT: &str = "download a model, e.g.:\n  \
        cd ~/.local/share/dictate/models && \\\n        curl -LO https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2 && \\\n        tar xjf sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2";
    if let Some(p) = cli.or(cfg.model_path.as_ref()) {
        let dir = expand_tilde(p)?;
        ensure!(
            dir.is_dir(),
            "model path '{}' is not a sherpa-onnx model directory — {DOWNLOAD_HINT}\n\
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
                    "cannot list model directory '{}' — check its permissions",
                    dir.display()
                )
            })?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir() && looks_like_model(p))
            .collect();
        models.sort();
    }
    match models.len() {
        1 => Ok(models.into_iter().next().unwrap()),
        0 => bail!("no sherpa-onnx model found in '{}' — {DOWNLOAD_HINT}", dir.display()),
        _ => bail!(
            "multiple models in '{}': {} — set model_path in the config to pick one",
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
/// ONNX — the full per-file check happens at load (stt::model_files).
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
            "the HOME environment variable is not set — set HOME, or pass explicit paths (--config, --model)"
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
        let path = std::env::temp_dir().join(format!("dictate-test-{}-{name}", std::process::id()));
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
            "dictate-xdg-empty-{}-{}",
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
        // WHY: a typo like "gpu" must fail closed with a fix hint — never
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
        // WHY: daemon usefulness — [api] defaults to enabled so `dictate start`
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
enabled = false
path = "/tmp/dictate-test.sock"
token = "s3cret"
require_same_uid = false
"#,
        );
        let cfg = load_without_legacy(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert!(!cfg.api.enabled);
        assert!(!cfg.api.require_same_uid);
        assert_eq!(
            cfg.api.configured_path().map(|p| p.to_string_lossy().into_owned()),
            Some("/tmp/dictate-test.sock".into())
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
            "dictate-xdg-mig-{}-{}",
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
        let config_path = temp_file("mig-empty.toml", b"n_threads = 3\n");

        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: held under ENV_LOCK; restored before unlock.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        let cfg = Config::load(Some(&config_path));
        match &prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let cfg = cfg.expect("migration load");
        fs::remove_file(&config_path).ok();
        fs::remove_dir_all(&xdg).ok();

        assert_eq!(cfg.dict.overrides.get("veyyon").map(String::as_str), Some("veyyon"));
        assert_eq!(cfg.dict.overrides.get("um").map(String::as_str), Some(""));
        // Config file on disk was not rewritten (still just n_threads).
        // (We deleted config_path above after load; migration is in-memory only.)
        let dict = Dictionary::from_entries(cfg.dict.overrides.clone());
        assert_eq!(dict.apply("say veyyon um now"), "say veyyon now");
    }

    #[test]
    fn inline_dict_overrides_skip_legacy_file() {
        // WHY: once [dict.overrides] exists, the legacy file must not
        // silently replace or merge over it.
        let _guard = ENV_LOCK.lock().unwrap();
        let xdg = std::env::temp_dir().join(format!(
            "dictate-xdg-skip-{}-{}",
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
            "dictate-xdg-bad-{}-{}",
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
        let config_path = temp_file("mig-bad.toml", b"n_threads = 2\n");

        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        let err = error_of(Config::load(Some(&config_path)));
        match &prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        fs::remove_file(&config_path).ok();
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
}
