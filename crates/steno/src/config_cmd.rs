//! CLI for config / model / theme: `steno config|model|theme …`.
//!
//! Uses `steno_core` helpers (`config_get` / `config_set` /
//! `list_settable_keys` / `list_themes` / `resolve_model`). Does not
//! start or stop the daemon. Typing stays fail-closed: arm it only via
//! `steno config set type_output true` (never a one-shot CLI flag).

use anyhow::{Context, Result, bail, ensure};
use steno_core::config::{
    self, Config, MODEL_DOWNLOAD_HINT, config_get, config_set, default_config_path,
    default_model_dir, list_settable_keys, resolve_model,
};
use steno_core::{Rgba, UiConfig, list_themes, resolve_ui};
use std::fs;
use std::path::{Path, PathBuf};

/// Resolve `--config` or the default path (tilde-expanded). Does not require
/// the file to exist: `config set` creates it.
pub fn resolve_config_path(cli: Option<&Path>) -> Result<PathBuf> {
    match cli {
        Some(p) => config::expand_tilde(p),
        None => default_config_path(),
    }
}

/// `steno config show`: effective path, settable keys, resolved model,
/// and `[refine]` dictionary entries count (not the map itself).
pub fn config_show(config_path: Option<&Path>) -> Result<()> {
    let path = resolve_config_path(config_path)?;
    let cfg = load_for_show(config_path, &path)?;
    println!("config: {}", path.display());
    if path.exists() {
        println!("status: present");
    } else {
        println!("status: missing (showing built-in defaults)");
    }
    for key in list_settable_keys() {
        println!("{key} = {}", effective_value(&cfg, key));
    }
    match resolve_model(None, &cfg) {
        Ok(m) => println!("model (resolved) = {}", m.display()),
        Err(e) => println!("model (resolved) = (unavailable: {e})"),
    }
    println!("refine.dictionary = {} entries", cfg.refine.dictionary.len());
    Ok(())
}

/// `steno config get <key>`: file value when present, else the effective
/// default from `Config::load` (so omitted `provider` still prints `cuda`).
pub fn config_get_cmd(config_path: Option<&Path>, key: &str) -> Result<()> {
    let path = resolve_config_path(config_path)?;
    if !list_settable_keys().contains(&key) {
        bail!(
            "unsupported config key {key:?} — supported keys: {}",
            list_settable_keys().join(", ")
        );
    }
    match config_get(&path, key).with_context(|| format!("config get {key:?}"))? {
        Some(v) => {
            println!("{v}");
            Ok(())
        }
        None => {
            let cfg = Config::load(config_path)?;
            println!("{}", effective_value(&cfg, key));
            Ok(())
        }
    }
}

/// `steno config set <key> <value>`: surgical write; creates the file when
/// missing. Unknown keys are refused with the allowed list.
pub fn config_set_cmd(config_path: Option<&Path>, key: &str, value: &str) -> Result<()> {
    let path = resolve_config_path(config_path)?;
    // Refuse unknown keys up front so the error lists allowed keys clearly
    // even before any filesystem work (core also enforces this).
    if !list_settable_keys().contains(&key) {
        bail!(
            "unsupported config key {key:?} — supported keys: {}",
            list_settable_keys().join(", ")
        );
    }
    match key {
        "provider" => {
            ensure!(
                matches!(value, "cuda" | "cpu"),
                "invalid provider {value:?} — use \"cuda\" or \"cpu\""
            );
        }
        "ui.theme" => {
            let val = value.trim();
            ensure!(
                list_themes().contains(&val) || matches!(val, "null" | "none" | "off"),
                "unknown theme {value:?} — choose one of: {}, or null|none|off",
                list_themes().join(", ")
            );
        }
        "n_threads" => {
            let n: i64 = value.parse().map_err(|_| {
                anyhow::anyhow!("value for {key} must be an integer, got {value:?}")
            })?;
            ensure!(n > 0, "n_threads must be greater than 0, got {n}");
        }
        "refine.enabled" => {
            let _: bool = value.parse().map_err(|_| {
                anyhow::anyhow!("value for {key} must be a boolean (true/false), got {value:?}")
            })?;
        }
        "refine.backend" => {
            let val = value.trim();
            ensure!(
                matches!(val, "rules" | "llm"),
                "invalid refine backend {value:?} — use \"rules\" or \"llm\""
            );
        }
        k if k.starts_with("ui.colors.") => {
            steno_core::parse_rgba(value)?;
        }
        _ => {}
    }
    let created = !path.exists();
    config_set(&path, key, value)?;
    if created {
        println!("created {} and set {key} = {value}", path.display());
    } else {
        println!("set {key} = {value} in {}", path.display());
    }
    if key == "type_output" {
        println!(
            "note: typing is armed only through this persistent config key; \
             `--type` alone never enables keystroke injection"
        );
    }
    Ok(())
}

/// `steno model list`: directories under the default models dir; mark current.
pub fn model_list(config_path: Option<&Path>) -> Result<()> {
    let models_dir = default_model_dir()?;
    let cfg = Config::load(config_path)?;
    let current = cfg
        .model_path
        .as_ref()
        .and_then(|p| config::expand_tilde(p).ok())
        .or_else(|| resolve_model(None, &cfg).ok());

    println!("models dir: {}", models_dir.display());
    if let Some(ref c) = current {
        println!("current: {}", c.display());
    } else {
        println!("current: (unset / unresolved)");
    }

    if !models_dir.is_dir() {
        println!("(no models directory yet)");
        println!("{MODEL_DOWNLOAD_HINT}");
        return Ok(());
    }

    let mut entries: Vec<PathBuf> = fs::read_dir(&models_dir)
        .with_context(|| format!("cannot list {}", models_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();

    if entries.is_empty() {
        println!("(empty)");
        println!("{MODEL_DOWNLOAD_HINT}");
        return Ok(());
    }

    for dir in entries {
        let mark = match &current {
            Some(c) if same_path(c, &dir) => "*",
            _ => " ",
        };
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string());
        println!("{mark} {name}  ({})", dir.display());
    }
    Ok(())
}

/// `steno model use <name-or-path>`: write `model_path` (and optional provider).
pub fn model_use(
    config_path: Option<&Path>,
    name_or_path: &str,
    provider: Option<&str>,
) -> Result<()> {
    let path = resolve_model_arg(name_or_path)?;
    ensure!(
        path.is_dir(),
        "model path '{}' is not a directory — pass a sherpa-onnx model dir or a name under {}",
        path.display(),
        default_model_dir()?.display()
    );
    let cfg_path = resolve_config_path(config_path)?;
    let rendered = path.to_string_lossy();
    config_set(&cfg_path, "model_path", &rendered)?;
    println!("model_path = {}", path.display());
    if let Some(p) = provider {
        ensure!(
            matches!(p, "cuda" | "cpu"),
            "invalid provider {p:?} — use \"cuda\" or \"cpu\""
        );
        config_set(&cfg_path, "provider", p)?;
        println!("provider = {p}");
    }
    println!("wrote {}", cfg_path.display());
    Ok(())
}

/// URL for the default STT model (sherpa-onnx Parakeet TDT v3 int8).
const STT_MODEL_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2";

/// URL for the default LLM refine model (LFM2.5-2.6B Q4_K_M GGUF).
const LLM_MODEL_URL: &str = "https://huggingface.co/LiquidAI/LFM2-2.6B-GGUF/resolve/main/LFM2-2.6B-Q4_K_M.gguf";

/// `steno model download [--llm]`: download the default STT model and
/// optionally the LLM refine model into the default models directory.
/// Uses `curl` and `tar` as external commands — no Rust HTTP dependency.
pub fn model_download(config_path: Option<&Path>, download_llm: bool) -> Result<()> {
    let models_dir = default_model_dir()?;
    fs::create_dir_all(&models_dir)
        .with_context(|| format!("cannot create models dir {}", models_dir.display()))?;

    // --- STT model ---
    let stt_name = "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8";
    let stt_dir = models_dir.join(stt_name);
    if stt_dir.is_dir() && stt_dir.join("tokens.txt").is_file() {
        println!("STT model already present: {}", stt_dir.display());
    } else {
        println!("Downloading STT model (Parakeet TDT v3 int8, ~150 MB)...");
        let archive = models_dir.join(format!("{stt_name}.tar.bz2"));
        run_curl(STT_MODEL_URL, &archive)?;
        println!("Extracting...");
        run_tar_extract(&archive, &models_dir)?;
        fs::remove_file(&archive).ok();
        println!("STT model ready: {}", stt_dir.display());
    }

    // Write model_path if not already set.
    let cfg_path = resolve_config_path(config_path)?;
    if config_get(&cfg_path, "model_path")?.is_none() {
        let rendered = tilde_path(&stt_dir);
        config_set(&cfg_path, "model_path", &rendered)?;
        println!("Set model_path = {rendered}");
    }

    // --- LLM model ---
    if download_llm {
        let llm_name = "LFM2.5-2.6B-Q4_K_M.gguf";
        let llm_path = models_dir.join(llm_name);
        if llm_path.is_file() {
            println!("LLM model already present: {}", llm_path.display());
        } else {
            println!("Downloading LLM model (LFM2.5-2.6B Q4_K_M, ~1.6 GB)...");
            run_curl(LLM_MODEL_URL, &llm_path)?;
            println!("LLM model ready: {}", llm_path.display());
        }

        if config_get(&cfg_path, "refine.llm.model_path")?.is_none() {
            let rendered = tilde_path(&llm_path);
            config_set(&cfg_path, "refine.llm.model_path", &rendered)?;
            println!("Set refine.llm.model_path = {rendered}");
        }
        if config_get(&cfg_path, "refine.backend")?.is_none() {
            config_set(&cfg_path, "refine.backend", "llm")?;
            println!("Set refine.backend = llm");
        }
    }

    println!("\nDone. Run `steno` to start dictating.");
    Ok(())
}

/// Download a URL to a file using `curl`. Follows redirects, fails on
/// HTTP errors, shows a progress bar on terminals.
fn run_curl(url: &str, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("curl")
        .args(["-L", "--fail", "--progress-bar", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .context("failed to spawn curl — is it installed?")?;
    ensure!(status.success(), "curl failed with status {status} for {url}");
    Ok(())
}

/// Extract a tar.bz2 archive into a directory.
fn run_tar_extract(archive: &Path, dest_dir: &Path) -> Result<()> {
    let status = std::process::Command::new("tar")
        .args(["xjf"])
        .arg(archive)
        .arg("-C")
        .arg(dest_dir)
        .status()
        .context("failed to spawn tar — is it installed?")?;
    ensure!(status.success(), "tar failed with status {status} for {}", archive.display());
    Ok(())
}

/// Render an absolute path with the home directory prefix replaced by `~`
/// for human-readable config output. Falls back to the full path when
/// the home directory cannot be determined or the path is not under it.
fn tilde_path(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

/// `steno theme list`
pub fn theme_list(config_path: Option<&Path>) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let current_theme = cfg.ui.theme.trim();

    println!("current: {current_theme} (*)");
    println!("themes:");
    for name in list_themes() {
        let mark = if *name == current_theme { "*" } else { " " };
        let palette = resolve_ui(&UiConfig {
            theme: name.to_string(),
            ..Default::default()
        })
        .colors;
        println!(
            "{mark} {name:<8} bg={} fg={} border={} icon_bg={} icon_fg={} meta={} shadow={} accent={} error={}",
            format_rgba(palette.bg),
            format_rgba(palette.fg),
            format_rgba(palette.border),
            format_rgba(palette.icon_bg),
            format_rgba(palette.icon_fg),
            format_rgba(palette.meta),
            format_rgba(palette.shadow),
            format_rgba(palette.accent),
            format_rgba(palette.error),
        );
    }
    println!("null aliases (no-op overlay): null | none | off");
    Ok(())
}

/// `steno theme set <name>`: validates then writes `ui.theme`.
pub fn theme_set(config_path: Option<&Path>, name: &str) -> Result<()> {
    let name = name.trim();
    ensure!(
        list_themes().contains(&name) || matches!(name, "null" | "none" | "off"),
        "unknown theme {name:?} — choose one of: {}, or null|none|off",
        list_themes().join(", ")
    );
    let cfg_path = resolve_config_path(config_path)?;
    config_set(&cfg_path, "ui.theme", name)?;
    println!("ui.theme = {name} in {}", cfg_path.display());
    Ok(())
}

fn load_for_show(cli: Option<&Path>, resolved: &Path) -> Result<Config> {
    // Explicit `--config` that is missing should still fail closed (typo
    // protection). A missing default path shows built-in defaults.
    if cli.is_some() {
        return Config::load(cli);
    }
    if resolved.exists() {
        Config::load(None)
    } else {
        Ok(Config::default())
    }
}

fn effective_value(cfg: &Config, key: &str) -> String {
    match key {
        "model_path" => cfg
            .model_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unset)".into()),
        "provider" => cfg.provider.clone(),
        "type_output" => cfg.type_output.to_string(),
        "n_threads" => cfg.n_threads.to_string(),
        "max_record_secs" => cfg.max_record_secs.to_string(),
        "api.enabled" => cfg.api.enabled.to_string(),
        "api.path" => cfg
            .api
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unset)".into()),
        "api.token" => cfg
            .api
            .token
            .as_deref()
            .unwrap_or("(unset)")
            .to_string(),
        "refine.enabled" => cfg.refine.enabled.to_string(),
        "refine.backend" => cfg.refine.backend.clone(),
        "refine.llm.model_path" => cfg.refine.llm.model_path.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unset)".into()),
        "refine.llm.n_gpu_layers" => cfg.refine.llm.n_gpu_layers.to_string(),
        "refine.llm.n_threads" => cfg.refine.llm.n_threads.to_string(),
        "refine.llm.max_tokens" => cfg.refine.llm.max_tokens.to_string(),
        "refine.llm.temperature" => cfg.refine.llm.temperature.to_string(),
        "vad.silence_ms" => cfg.vad.silence_ms.to_string(),
        "vad.min_speech_ms" => cfg.vad.min_speech_ms.to_string(),
        "vad.start_timeout_secs" => cfg.vad.start_timeout_secs.to_string(),
        "vad.speech_threshold" => cfg.vad.speech_threshold.to_string(),
        "dsp.target_rms" => cfg.dsp.target_rms.to_string(),
        "dsp.max_gain" => cfg.dsp.max_gain.to_string(),
        "api.require_same_uid" => cfg.api.require_same_uid.to_string(),
        "ui.theme" => cfg.ui.theme.clone(),
        "ui.overlay" => cfg.ui.overlay.to_string(),
        "ui.done_flash_ms" => cfg.ui.done_flash_ms.to_string(),
        "ui.stages.recording" => cfg.ui.stages.recording.clone(),
        "ui.stages.transcribing" => cfg.ui.stages.transcribing.clone(),
        "ui.stages.done" => cfg.ui.stages.done.clone(),
        "ui.stages.error" => cfg.ui.stages.error.clone(),
        "ui.stages.show_timer" => cfg.ui.stages.show_timer.to_string(),
        "ui.stages.pulse_ms" => cfg.ui.stages.pulse_ms.to_string(),
        "ui.colors.bg" => opt_color(&cfg.ui.colors.bg),
        "ui.colors.fg" => opt_color(&cfg.ui.colors.fg),
        "ui.colors.border" => opt_color(&cfg.ui.colors.border),
        "ui.colors.icon_bg" => opt_color(&cfg.ui.colors.icon_bg),
        "ui.colors.icon_fg" => opt_color(&cfg.ui.colors.icon_fg),
        "ui.colors.meta" => opt_color(&cfg.ui.colors.meta),
        "ui.colors.shadow" => opt_color(&cfg.ui.colors.shadow),
        "ui.colors.accent" => opt_color(&cfg.ui.colors.accent),
        "ui.colors.error" => opt_color(&cfg.ui.colors.error),
        other => format!("(internal: unhandled key {other})"),
    }
}

fn format_rgba([r, g, b, a]: Rgba) -> String {
    if a == 0xff {
        format!("#{:02X}{:02X}{:02X}", r, g, b)
    } else {
        format!("#{:02X}{:02X}{:02X}{:02X}", r, g, b, a)
    }
}
fn opt_color(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| "(theme default)".into())
}

/// Resolve a model name or path: expand `~`; bare names join under the
/// default models directory when that entry exists.
pub fn resolve_model_arg(name_or_path: &str) -> Result<PathBuf> {
    let raw = PathBuf::from(name_or_path);
    let expanded = config::expand_tilde(&raw)?;
    if expanded.exists() {
        return Ok(expanded);
    }

    let bare = !name_or_path.contains('/')
        && !name_or_path.contains('\\')
        && !name_or_path.starts_with('~');
    if bare {
        let under = default_model_dir()?.join(name_or_path);
        if under.exists() {
            return Ok(under);
        }
        bail!(
            "model {name_or_path:?} not found under {} — pass an absolute path or install the model directory first",
            default_model_dir()?.display()
        );
    }

    // Explicit path that does not exist yet: still return the expanded
    // form so the caller can produce a clear "not a directory" error.
    Ok(expanded)
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    //! WHY: config set/get must round-trip through a real TOML file without
    //! a daemon, and unknown keys must fail closed with the allowed list.
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_cfg(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "steno-cli-cfg-{}-{}-{name}.toml",
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn set_get_roundtrip_creates_file() {
        let path = temp_cfg("roundtrip");
        let _ = fs::remove_file(&path);
        assert!(!path.exists());

        config_set_cmd(Some(&path), "provider", "cpu").unwrap();
        assert!(path.exists());

        let v = config_get(&path, "provider").unwrap();
        assert_eq!(v.as_deref(), Some("cpu"));

        config_get_cmd(Some(&path), "provider").unwrap();

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn get_omitted_key_prints_effective_default() {
        let path = temp_cfg("missing");
        fs::write(&path, "provider = \"cuda\"\n").unwrap();
        // n_threads omitted → effective default from Config::load, not an error.
        config_get_cmd(Some(&path), "n_threads").unwrap();
        let err = config_get_cmd(Some(&path), "not.a.key")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsupported"), "{err}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn set_unknown_key_lists_allowed() {
        let path = temp_cfg("unknown");
        let _ = fs::remove_file(&path);
        let err = config_set_cmd(Some(&path), "not.a.key", "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsupported"), "{err}");
        assert!(err.contains("provider"), "{err}");
        assert!(!path.exists(), "must not create a file for a bad key");
    }

    #[test]
    fn theme_set_rejects_unknown() {
        let path = temp_cfg("theme");
        let _ = fs::remove_file(&path);
        let err = theme_set(Some(&path), "neon").unwrap_err().to_string();
        assert!(err.contains("unknown theme"), "{err}");
        assert!(err.contains("pill"), "{err}");
    }

    #[test]
    fn theme_set_accepts_null_alias() {
        let path = temp_cfg("theme-null");
        let _ = fs::remove_file(&path);
        theme_set(Some(&path), "none").unwrap();
        assert_eq!(
            config_get(&path, "ui.theme").unwrap().as_deref(),
            Some("none")
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn resolve_model_arg_joins_bare_name() {
        let models = default_model_dir().unwrap();
        let _ = fs::create_dir_all(&models);
        let name = format!("cli-fake-model-{}", std::process::id());
        let dir = models.join(&name);
        fs::create_dir_all(&dir).unwrap();
        let got = resolve_model_arg(&name).unwrap();
        assert_eq!(got, dir);
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn config_set_cmd_validates_provider() {
        let path = temp_cfg("valid-provider");
        let _ = fs::remove_file(&path);
        let err = config_set_cmd(Some(&path), "provider", "invalid")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid provider"), "{err}");
        assert!(!path.exists());

        config_set_cmd(Some(&path), "provider", "cuda").unwrap();
        assert_eq!(config_get(&path, "provider").unwrap().as_deref(), Some("cuda"));
        config_set_cmd(Some(&path), "provider", "cpu").unwrap();
        assert_eq!(config_get(&path, "provider").unwrap().as_deref(), Some("cpu"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_set_cmd_validates_ui_theme() {
        let path = temp_cfg("valid-theme");
        let _ = fs::remove_file(&path);
        let err = config_set_cmd(Some(&path), "ui.theme", "invalid_theme")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown theme"), "{err}");
        assert!(!path.exists());

        config_set_cmd(Some(&path), "ui.theme", "dusk").unwrap();
        assert_eq!(config_get(&path, "ui.theme").unwrap().as_deref(), Some("dusk"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_set_cmd_validates_n_threads() {
        let path = temp_cfg("valid-threads");
        let _ = fs::remove_file(&path);

        let err0 = config_set_cmd(Some(&path), "n_threads", "0")
            .unwrap_err()
            .to_string();
        assert!(err0.contains("greater than 0"), "{err0}");

        let err_neg = config_set_cmd(Some(&path), "n_threads", "-5")
            .unwrap_err()
            .to_string();
        assert!(err_neg.contains("greater than 0"), "{err_neg}");

        let err_str = config_set_cmd(Some(&path), "n_threads", "abc")
            .unwrap_err()
            .to_string();
        assert!(err_str.contains("integer"), "{err_str}");

        assert!(!path.exists());

        config_set_cmd(Some(&path), "n_threads", "4").unwrap();
        assert_eq!(config_get(&path, "n_threads").unwrap().as_deref(), Some("4"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_set_cmd_validates_ui_colors() {
        let path = temp_cfg("valid-colors");
        let _ = fs::remove_file(&path);

        let err1 = config_set_cmd(Some(&path), "ui.colors.bg", "invalid")
            .unwrap_err()
            .to_string();
        assert!(err1.contains("color"), "{err1}");

        let err2 = config_set_cmd(Some(&path), "ui.colors.fg", "#123")
            .unwrap_err()
            .to_string();
        assert!(err2.contains("#RRGGBB or #RRGGBBAA"), "{err2}");

        assert!(!path.exists());

        config_set_cmd(Some(&path), "ui.colors.fg", "#112233").unwrap();
        assert_eq!(
            config_get(&path, "ui.colors.fg").unwrap().as_deref(),
            Some("#112233")
        );
        config_set_cmd(Some(&path), "ui.colors.fg", "#11223344").unwrap();
        assert_eq!(
            config_get(&path, "ui.colors.fg").unwrap().as_deref(),
            Some("#11223344")
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn theme_list_runs_successfully() {
        let path = temp_cfg("theme-list");
        config_set_cmd(Some(&path), "ui.theme", "dusk").unwrap();
        theme_list(Some(&path)).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn model_list_runs_successfully() {
        let path = temp_cfg("model-list");
        fs::write(&path, "").unwrap();
        model_list(Some(&path)).unwrap();
        let _ = fs::remove_file(&path);
    }
    #[test]
    fn config_set_cmd_validates_refine() {
        let path = temp_cfg("valid-refine");
        let _ = fs::remove_file(&path);

        let err1 = config_set_cmd(Some(&path), "refine.enabled", "maybe")
            .unwrap_err()
            .to_string();
        assert!(err1.contains("boolean"), "{err1}");

        let err2 = config_set_cmd(Some(&path), "refine.backend", "magic")
            .unwrap_err()
            .to_string();
        assert!(err2.contains("invalid refine backend"), "{err2}");

        assert!(!path.exists());

        config_set_cmd(Some(&path), "refine.enabled", "false").unwrap();
        assert_eq!(
            config_get(&path, "refine.enabled").unwrap().as_deref(),
            Some("false")
        );
        config_set_cmd(Some(&path), "refine.backend", "rules").unwrap();
        assert_eq!(
            config_get(&path, "refine.backend").unwrap().as_deref(),
            Some("rules")
        );
        config_set_cmd(Some(&path), "refine.backend", "llm").unwrap();
        assert_eq!(
            config_get(&path, "refine.backend").unwrap().as_deref(),
            Some("llm")
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_show_reflects_refine_dictionary() {
        let path = temp_cfg("show-refine");
        fs::write(&path, "[dict.overrides]\n\"vayon\" = \"veyyon\"\n").unwrap();
        config_show(Some(&path)).unwrap();
        let _ = fs::remove_file(&path);
    }
}
