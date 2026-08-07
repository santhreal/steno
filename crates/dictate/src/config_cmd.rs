//! CLI for config / model / theme: `dictate config|model|theme …`.
//!
//! Uses [`dictate_core`] helpers (`config_get` / `config_set` /
//! `list_settable_keys` / `list_themes` / `resolve_model`). Does not
//! start or stop the daemon. Typing stays fail-closed: arm it only via
//! `dictate config set type_output true` (never a one-shot CLI flag).

use anyhow::{Context, Result, bail, ensure};
use dictate_core::config::{
    self, Config, config_get, config_set, default_config_path, default_model_dir, list_settable_keys,
    resolve_model,
};
use dictate_core::list_themes;
use std::fs;
use std::path::{Path, PathBuf};

/// Resolve `--config` or the default path (tilde-expanded). Does not require
/// the file to exist — `config set` creates it.
pub fn resolve_config_path(cli: Option<&Path>) -> Result<PathBuf> {
    match cli {
        Some(p) => config::expand_tilde(p),
        None => default_config_path(),
    }
}

/// `dictate config show` — effective path, settable keys, resolved model,
/// and `dict.overrides` count (not the map itself).
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
    println!("dict.overrides = {} entries", cfg.dict.overrides.len());
    Ok(())
}

/// `dictate config get <key>` — file value when present, else the effective
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

/// `dictate config set <key> <value>` — surgical write; creates the file when
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

/// `dictate model list` — directories under the default models dir; mark current.
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

/// `dictate model use <name-or-path>` — write `model_path` (and optional provider).
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

/// `dictate theme list`
pub fn theme_list() -> Result<()> {
    println!("themes: {}", list_themes().join(" "));
    println!("null aliases (no-op overlay): null | none | off");
    Ok(())
}

/// `dictate theme set <name>` — validates then writes `ui.theme`.
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
            "dictate-cli-cfg-{}-{}-{name}.toml",
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
}
