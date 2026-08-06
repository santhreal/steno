//! Configuration: built-in defaults, then `~/.config/dictate/config.toml`,
//! then CLI flags (merged by `main.rs`).

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::dsp::{DspConfig, VadConfig};
use crate::overlay::UiConfig;
use crate::text::TextConfig;

// deny_unknown_fields: a typo'd key must fail loudly, not be silently
// ignored. Every nested table sets it too.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
            ui: UiConfig::default(),
        }
    }
}

impl Config {
    /// Load `path`, or the default config path when `None`. A missing
    /// DEFAULT file is not an error (defaults apply); a missing EXPLICIT
    /// path is an error — a silent typo would be worse. A malformed file
    /// is an error with the offending line context.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let (path, explicit) = match path {
            Some(p) => (expand_tilde(p)?, true),
            None => (default_config_path()?, false),
        };
        if !path.exists() {
            if explicit {
                bail!(
                    "config file '{}' does not exist — fix the path or remove the flag",
                    path.display()
                );
            }
            return Ok(Self::default());
        }
        ensure!(
            !path.is_dir(),
            "config path '{}' is a directory — pass a TOML file",
            path.display()
        );
        let raw = fs::read_to_string(&path).with_context(|| {
            format!(
                "cannot read config {} — check its permissions",
                path.display()
            )
        })?;
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("invalid TOML in config {}", path.display()))?;
        cfg.validate(&path)?;
        Ok(cfg)
    }

    /// Reject values that parse but break downstream (integer truncation
    /// at the whisper boundary, a zero recording cap).
    fn validate(&self, path: &Path) -> Result<()> {
        ensure!(
            (1..=(i32::MAX as u32)).contains(&self.n_threads),
            "invalid n_threads = {} in {} — set it between 1 and {}",
            self.n_threads,
            path.display(),
            i32::MAX
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

pub fn default_dictionary_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("dictate/dictionary.toml"))
}

pub fn default_model_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("dictate/models"))
}

/// Model resolution order: CLI flag, config file, first `*.bin` in the
/// default model directory. Fails with the exact download command.
pub fn resolve_model(cli: Option<&PathBuf>, cfg: &Config) -> Result<PathBuf> {
    let candidate = if let Some(p) = cli {
        expand_tilde(p)?
    } else if let Some(p) = &cfg.model_path {
        expand_tilde(p)?
    } else {
        let dir = default_model_dir()?;
        let mut found = None;
        if dir.is_dir() {
            let mut bins: Vec<PathBuf> = fs::read_dir(&dir)
                .with_context(|| {
                    format!(
                        "cannot list model directory '{}' — check its permissions",
                        dir.display()
                    )
                })?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "bin"))
                .collect();
            bins.sort();
            found = bins.into_iter().next();
        }
        match found {
            Some(first) => first,
            None => bail!(
                "no whisper model found. Download one, e.g.:\n  \
                 mkdir -p {dir} && curl -L -o {dir}/ggml-base.en.bin \\\n    \
                 https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin\n\
                 or pass --model /path/to/ggml-model.bin",
                dir = dir.display()
            ),
        }
    };
    check_model_file(&candidate)?;
    Ok(candidate)
}

/// A chosen model path must be a readable, non-empty regular file.
/// whisper's own load errors are cryptic; say exactly what to fix.
/// Symlinks are fine (metadata follows them); broken ones report as missing.
fn check_model_file(path: &Path) -> Result<()> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "whisper model '{}' does not exist — fix the path, or download a model, e.g.:\n  \
             curl -L -o {} \
             https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
            path.display(),
            path.display()
        ),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "cannot access whisper model '{}' — check its permissions",
                    path.display()
                )
            });
        }
    };
    ensure!(
        meta.is_file(),
        "whisper model '{}' is not a regular file — pass the path to a .bin model file",
        path.display()
    );
    ensure!(
        meta.len() > 0,
        "whisper model '{}' is empty — the download likely failed; delete it and download again",
        path.display()
    );
    fs::File::open(path).with_context(|| {
        format!(
            "whisper model '{}' is not readable — fix its permissions (chmod +r)",
            path.display()
        )
    })?;
    Ok(())
}

/// Dictionary resolution order: CLI flag, config file, default path if it
/// exists, else `None` (empty dictionary).
pub fn resolve_dictionary(cli: Option<&PathBuf>, cfg: &Config) -> Result<Option<PathBuf>> {
    if let Some(p) = cli {
        return Ok(Some(expand_tilde(p)?));
    }
    if let Some(p) = &cfg.dictionary_path {
        return Ok(Some(expand_tilde(p)?));
    }
    let default = default_dictionary_path()?;
    Ok(default.exists().then_some(default))
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
            "the HOME environment variable is not set — set HOME, or pass explicit paths (--config, --model, --dictionary)"
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
    //! the whisper boundary, and a missing/empty model produced a cryptic
    //! whisper load error instead of an actionable message.
    use super::*;
    use std::io::Write;

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

    #[test]
    fn unknown_config_key_is_rejected() {
        let path = temp_file("unknown-key.toml", b"languge = \"en\"\n");
        let err = error_of(Config::load(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("languge"), "error names the typo'd key: {err}");
    }

    #[test]
    fn known_keys_still_parse() {
        let path = temp_file(
            "known-keys.toml",
            b"language = \"en\"\nn_threads = 4\nmax_record_secs = 30\ntype_output = true\n",
        );
        let cfg = Config::load(Some(&path)).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(cfg.language, "en");
        assert_eq!(cfg.n_threads, 4);
        assert_eq!(cfg.max_record_secs, 30);
        assert!(cfg.type_output);
    }

    #[test]
    fn wrong_type_is_rejected() {
        let path = temp_file("wrong-type.toml", b"n_threads = \"lots\"\n");
        let err = error_of(Config::load(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("invalid TOML"), "{err}");
    }

    #[test]
    fn negative_n_threads_is_rejected() {
        let path = temp_file("negative.toml", b"n_threads = -5\n");
        let err = error_of(Config::load(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("invalid TOML"), "{err}");
    }

    #[test]
    fn zero_n_threads_is_rejected_with_fix() {
        // WHY: n_threads = 0 reaches whisper as 0 decode threads.
        let path = temp_file("zero-threads.toml", b"n_threads = 0\n");
        let err = error_of(Config::load(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("n_threads"), "{err}");
        assert!(err.contains("between 1 and"), "{err}");
    }

    #[test]
    fn huge_n_threads_is_rejected_before_i32_truncation() {
        // WHY: 3_000_000_000 as i32 wraps negative; whisper would get a
        // negative thread count.
        let path = temp_file("huge-threads.toml", b"n_threads = 3000000000\n");
        let err = error_of(Config::load(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("n_threads"), "{err}");
    }

    #[test]
    fn zero_max_record_secs_is_rejected() {
        let path = temp_file("zero-record.toml", b"max_record_secs = 0\n");
        let err = error_of(Config::load(Some(&path)));
        fs::remove_file(&path).ok();
        assert!(err.contains("max_record_secs"), "{err}");
    }

    #[test]
    fn explicit_config_path_that_is_a_directory_errors() {
        let dir = std::env::temp_dir();
        let err = error_of(Config::load(Some(&dir)));
        assert!(err.contains("is a directory"), "{err}");
    }

    #[test]
    fn missing_explicit_config_errors() {
        let path = std::env::temp_dir().join("dictate-test-does-not-exist.toml");
        let err = error_of(Config::load(Some(&path)));
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn resolve_model_missing_file_says_what_to_do() {
        let cfg = Config::default();
        let path = PathBuf::from("/nonexistent/ggml-nope.bin");
        let err = error_of(resolve_model(Some(&path), &cfg));
        assert!(err.contains("does not exist"), "{err}");
        assert!(err.contains("curl"), "{err}");
    }

    #[test]
    fn resolve_model_directory_is_rejected() {
        let cfg = Config::default();
        let dir = std::env::temp_dir();
        let err = error_of(resolve_model(Some(&dir), &cfg));
        assert!(err.contains("not a regular file"), "{err}");
    }

    #[test]
    fn resolve_model_empty_bin_is_rejected() {
        // WHY: a failed curl leaves a 0-byte .bin; whisper's load error
        // for it does not say the file is empty or to re-download.
        let cfg = Config::default();
        let path = temp_file("empty.bin", b"");
        let err = error_of(resolve_model(Some(&path), &cfg));
        fs::remove_file(&path).ok();
        assert!(err.contains("is empty"), "{err}");
        assert!(err.contains("download again"), "{err}");
    }

    #[test]
    fn resolve_model_accepts_nonempty_file() {
        let cfg = Config::default();
        let path = temp_file("ok.bin", b"not a real model, but nonempty");
        let got = resolve_model(Some(&path), &cfg).unwrap();
        fs::remove_file(&path).ok();
        assert_eq!(got, path);
    }

    #[test]
    fn resolve_model_accepts_symlinked_model() {
        let cfg = Config::default();
        let target = temp_file("real.bin", b"nonempty");
        let link =
            std::env::temp_dir().join(format!("dictate-test-{}-link.bin", std::process::id()));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let got = resolve_model(Some(&link), &cfg).unwrap();
        fs::remove_file(&link).ok();
        fs::remove_file(&target).ok();
        assert_eq!(got, link);
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
        let err = error_of(home_dir_from(Some(std::ffi::OsString::new())));
        assert!(err.contains("HOME"), "{err}");
        assert_eq!(
            home_dir_from(Some(std::ffi::OsString::from("/home/u"))).unwrap(),
            PathBuf::from("/home/u")
        );
    }
}
