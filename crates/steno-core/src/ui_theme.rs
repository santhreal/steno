//! Named overlay color themes and stage-label resolution.
//!
//! Embedders, the CLI, and platform overlays share [`resolve_ui`] /
//! [`stage_label`] so palette + copy stay in one place. Platform `create`
//! still owns NullOverlay selection for `theme = "null"|"none"|"off"`.

use anyhow::{Result, ensure};

use crate::config::{UiColors, UiConfig, UiStages};
use crate::overlay::Stage;

/// Premultiplied-ready straight RGBA byte tuple (`R, G, B, A`).
pub type Rgba = [u8; 4];

/// Resolved overlay palette after theme preset + optional hex overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePalette {
    /// Background fill color.
    pub bg: Rgba,
    /// Foreground text color.
    pub fg: Rgba,
    /// Border outline color.
    pub border: Rgba,
    /// Icon badge background color.
    pub icon_bg: Rgba,
    /// Icon glyph color.
    pub icon_fg: Rgba,
    /// Secondary metadata text color.
    pub meta: Rgba,
    /// Drop shadow color.
    pub shadow: Rgba,
    /// Accent highlight color.
    pub accent: Rgba,
    /// Error stage color.
    pub error: Rgba,
}

/// Fully resolved UI knobs for overlays / CLI / embedders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUi {
    /// Theme name that produced the palette (`"pill"` after unknown fallback).
    ///
    /// `null` / `none` / `off` still yield the pill palette here; platform
    /// `create` maps those names to [`crate::overlay::NullOverlay`].
    pub theme: String,
    /// Resolved color palette after theme preset and hex overrides.
    pub colors: ThemePalette,
    /// Stage labels and transition timing.
    pub stages: UiStages,
    /// Whether the status overlay is shown.
    pub overlay: bool,
    /// How long the done or error stage stays visible before hide, in milliseconds.
    pub done_flash_ms: u64,
}

/// Built-in palette names (excludes null/none/off aliases).
pub fn list_themes() -> &'static [&'static str] {
    &["pill", "mono", "dusk", "dawn", "contrast"]
}

/// Parse `#RRGGBB` (alpha `FF`) or `#RRGGBBAA` into straight RGBA.
pub fn parse_rgba(raw: &str) -> Result<Rgba> {
    let s = raw.trim();
    let hex = s
        .strip_prefix('#')
        .ok_or_else(|| anyhow::anyhow!("color {raw:?} must start with '#'"))?;
    ensure!(
        hex.len() == 6 || hex.len() == 8,
        "color {raw:?} must be #RRGGBB or #RRGGBBAA"
    );
    ensure!(
        hex.chars().all(|c| c.is_ascii_hexdigit()),
        "color {raw:?} contains a non-hex digit"
    );
    let n = u32::from_str_radix(hex, 16)
        .map_err(|e| anyhow::anyhow!("color {raw:?} is not valid hex: {e}"))?;
    if hex.len() == 6 {
        let r = ((n >> 16) & 0xff) as u8;
        let g = ((n >> 8) & 0xff) as u8;
        let b = (n & 0xff) as u8;
        Ok([r, g, b, 0xff])
    } else {
        let r = ((n >> 24) & 0xff) as u8;
        let g = ((n >> 16) & 0xff) as u8;
        let b = ((n >> 8) & 0xff) as u8;
        let a = (n & 0xff) as u8;
        Ok([r, g, b, a])
    }
}

fn pill() -> ThemePalette {
    // Current white mock chip (see linux_x11 overlay constants).
    ThemePalette {
        bg: [0xff, 0xff, 0xff, 0xf0],
        fg: [0x11, 0x11, 0x11, 0xff],
        border: [0x11, 0x11, 0x11, 0x29],
        icon_bg: [0x11, 0x11, 0x11, 0xff],
        icon_fg: [0xff, 0xff, 0xff, 0xff],
        meta: [0x77, 0x77, 0x77, 0xff],
        shadow: [0x00, 0x00, 0x00, 0x1c],
        accent: [0x11, 0x11, 0x11, 0xff],
        error: [0xb0, 0x00, 0x20, 0xff],
    }
}

fn mono() -> ThemePalette {
    // Near-black inverted chip.
    ThemePalette {
        bg: [0x11, 0x11, 0x11, 0xf0],
        fg: [0xff, 0xff, 0xff, 0xff],
        border: [0xff, 0xff, 0xff, 0x29],
        icon_bg: [0xff, 0xff, 0xff, 0xff],
        icon_fg: [0x11, 0x11, 0x11, 0xff],
        meta: [0xaa, 0xaa, 0xaa, 0xff],
        shadow: [0x00, 0x00, 0x00, 0x28],
        accent: [0xff, 0xff, 0xff, 0xff],
        error: [0xff, 0x6b, 0x6b, 0xff],
    }
}

fn dusk() -> ThemePalette {
    // Dark slate (~#1E1E24).
    ThemePalette {
        bg: [0x1e, 0x1e, 0x24, 0xf0],
        fg: [0xec, 0xec, 0xf0, 0xff],
        border: [0xff, 0xff, 0xff, 0x1c],
        icon_bg: [0xec, 0xec, 0xf0, 0xff],
        icon_fg: [0x1e, 0x1e, 0x24, 0xff],
        meta: [0xa0, 0xa0, 0xaa, 0xff],
        shadow: [0x00, 0x00, 0x00, 0x30],
        accent: [0xb4, 0xbe, 0xff, 0xff],
        error: [0xff, 0x64, 0x78, 0xff],
    }
}

fn dawn() -> ThemePalette {
    // Warm off-white + soft brown/ink.
    ThemePalette {
        bg: [0xff, 0xf8, 0xf0, 0xf5],
        fg: [0x3c, 0x28, 0x1e, 0xff],
        border: [0x3c, 0x28, 0x1e, 0x28],
        icon_bg: [0x3c, 0x28, 0x1e, 0xff],
        icon_fg: [0xff, 0xf8, 0xf0, 0xff],
        meta: [0x8c, 0x6e, 0x5a, 0xff],
        shadow: [0x3c, 0x28, 0x1e, 0x1c],
        accent: [0xa0, 0x5a, 0x32, 0xff],
        error: [0xb0, 0x00, 0x20, 0xff],
    }
}

fn contrast() -> ThemePalette {
    // Pure black / white max contrast.
    ThemePalette {
        bg: [0x00, 0x00, 0x00, 0xff],
        fg: [0xff, 0xff, 0xff, 0xff],
        border: [0xff, 0xff, 0xff, 0xff],
        icon_bg: [0xff, 0xff, 0xff, 0xff],
        icon_fg: [0x00, 0x00, 0x00, 0xff],
        meta: [0xff, 0xff, 0xff, 0xff],
        shadow: [0x00, 0x00, 0x00, 0x00],
        accent: [0xff, 0xff, 0xff, 0xff],
        error: [0xff, 0xff, 0xff, 0xff],
    }
}

fn preset_for(name: &str) -> (String, ThemePalette) {
    match name {
        "pill" | "null" | "none" | "off" => ("pill".to_string(), pill()),
        "mono" => ("mono".to_string(), mono()),
        "dusk" => ("dusk".to_string(), dusk()),
        "dawn" => ("dawn".to_string(), dawn()),
        "contrast" => ("contrast".to_string(), contrast()),
        other => {
            log::warn!("unknown ui.theme {other:?}; falling back to pill palette");
            ("pill".to_string(), pill())
        }
    }
}

fn apply_override(slot: &mut Rgba, raw: &Option<String>, field: &str) {
    let Some(hex) = raw.as_deref() else {
        return;
    };
    match parse_rgba(hex) {
        Ok(rgba) => *slot = rgba,
        Err(err) => {
            // Config::validate rejects bad hex at load; this path covers
            // callers that build UiConfig by hand.
            log::warn!("ignoring invalid ui.colors.{field} = {hex:?}: {err}");
        }
    }
}

fn apply_color_overrides(base: ThemePalette, colors: &UiColors) -> ThemePalette {
    let mut out = base;
    apply_override(&mut out.bg, &colors.bg, "bg");
    apply_override(&mut out.fg, &colors.fg, "fg");
    apply_override(&mut out.border, &colors.border, "border");
    apply_override(&mut out.icon_bg, &colors.icon_bg, "icon_bg");
    apply_override(&mut out.icon_fg, &colors.icon_fg, "icon_fg");
    apply_override(&mut out.meta, &colors.meta, "meta");
    apply_override(&mut out.shadow, &colors.shadow, "shadow");
    apply_override(&mut out.accent, &colors.accent, "accent");
    apply_override(&mut out.error, &colors.error, "error");
    out
}

/// Resolve theme preset then apply optional `[ui.colors]` hex overrides.
///
/// Unknown theme names warn and fall back to the pill palette (UI is
/// fail-open). `null` / `none` / `off` also resolve to pill colors; the
/// platform layer maps those names to a no-op overlay.
pub fn resolve_ui(ui: &UiConfig) -> ResolvedUi {
    let (theme, base) = preset_for(ui.theme.as_str());
    ResolvedUi {
        theme,
        colors: apply_color_overrides(base, &ui.colors),
        stages: ui.stages.clone(),
        overlay: ui.overlay,
        done_flash_ms: ui.done_flash_ms,
    }
}

/// Configurable label for a visible overlay stage.
///
/// Defaults match the historical hard-coded copy: Recording→"Transcribing",
/// Transcribing→"Processing", Done→"Done", Error→"Error". [`Stage::Hidden`]
/// returns an empty string.
pub fn stage_label(ui: &UiConfig, stage: Stage) -> String {
    match stage {
        Stage::Hidden => String::new(),
        Stage::Recording => ui.stages.recording.clone(),
        Stage::Transcribing => ui.stages.transcribing.clone(),
        Stage::Done => ui.stages.done.clone(),
        Stage::Error => ui.stages.error.clone(),
    }
}

/// Reject malformed `[ui.colors]` hex strings at config load.
pub(crate) fn validate_color_overrides(colors: &UiColors, path: &std::path::Path) -> Result<()> {
    for (name, raw) in [
        ("bg", &colors.bg),
        ("fg", &colors.fg),
        ("border", &colors.border),
        ("icon_bg", &colors.icon_bg),
        ("icon_fg", &colors.icon_fg),
        ("meta", &colors.meta),
        ("shadow", &colors.shadow),
        ("accent", &colors.accent),
        ("error", &colors.error),
    ] {
        if let Some(hex) = raw {
            parse_rgba(hex).map_err(|e| {
                anyhow::anyhow!(
                    "invalid ui.colors.{name} = {hex:?} in {} — {e}",
                    path.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! WHY: Theme presets, hex color parsing, partial color overrides, and color range
    //! validation must remain stable and reject malformed color strings.
    use super::*;

    #[test]
    fn parse_rgb_and_rgba_hex() {
        // WHY: #RRGGBB must default alpha to FF; #RRGGBBAA keeps the given alpha.
        assert_eq!(parse_rgba("#112233").unwrap(), [0x11, 0x22, 0x33, 0xff]);
        assert_eq!(parse_rgba("#11223344").unwrap(), [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(parse_rgba("  #AaBbCc  ").unwrap(), [0xaa, 0xbb, 0xcc, 0xff]);
        assert!(parse_rgba("112233").unwrap_err().to_string().contains('#'));
        assert!(parse_rgba("#123").is_err());
        assert!(parse_rgba("#gg0000").is_err());
    }

    #[test]
    fn list_themes_names_presets() {
        assert_eq!(list_themes(), &["pill", "mono", "dusk", "dawn", "contrast"]);
    }

    #[test]
    fn resolve_presets_match_spec() {
        // WHY: named themes must stay stable for embedders / overlays.
        let pill_ui = UiConfig {
            theme: "pill".into(),
            ..UiConfig::default()
        };
        let r = resolve_ui(&pill_ui);
        assert_eq!(r.theme, "pill");
        assert_eq!(r.colors.bg, [0xff, 0xff, 0xff, 0xf0]);
        assert_eq!(r.colors.fg, [0x11, 0x11, 0x11, 0xff]);
        assert_eq!(r.colors.border, [0x11, 0x11, 0x11, 0x29]);
        assert_eq!(r.colors.shadow, [0x00, 0x00, 0x00, 0x1c]);
        assert_eq!(r.colors.error, [0xb0, 0x00, 0x20, 0xff]);

        let dusk = resolve_ui(&UiConfig {
            theme: "dusk".into(),
            ..UiConfig::default()
        });
        assert_eq!(dusk.theme, "dusk");
        assert_eq!(dusk.colors.bg, [0x1e, 0x1e, 0x24, 0xf0]);
        assert_eq!(dusk.colors.fg, [0xec, 0xec, 0xf0, 0xff]);

        let mono = resolve_ui(&UiConfig {
            theme: "mono".into(),
            ..UiConfig::default()
        });
        assert_eq!(mono.colors.bg, [0x11, 0x11, 0x11, 0xf0]);
        assert_eq!(mono.colors.fg, [0xff, 0xff, 0xff, 0xff]);

        let dawn = resolve_ui(&UiConfig {
            theme: "dawn".into(),
            ..UiConfig::default()
        });
        assert_eq!(dawn.colors.bg, [0xff, 0xf8, 0xf0, 0xf5]);

        let contrast = resolve_ui(&UiConfig {
            theme: "contrast".into(),
            ..UiConfig::default()
        });
        assert_eq!(contrast.colors.bg, [0x00, 0x00, 0x00, 0xff]);
        assert_eq!(contrast.colors.fg, [0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn color_override_wins_only_named_field() {
        // WHY: partial [ui.colors] must patch one slot and leave the rest.
        let ui = UiConfig {
            theme: "dusk".into(),
            colors: UiColors {
                fg: Some("#FF0000FF".into()),
                ..UiColors::default()
            },
            ..UiConfig::default()
        };
        let r = resolve_ui(&ui);
        assert_eq!(r.colors.fg, [0xff, 0x00, 0x00, 0xff]);
        assert_eq!(r.colors.bg, dusk().bg);
        assert_eq!(r.colors.meta, dusk().meta);
    }

    #[test]
    fn unknown_theme_falls_back_to_pill() {
        // WHY: UI is fail-open — unknown theme names still produce a palette.
        let ui = UiConfig {
            theme: "neon-void".into(),
            ..UiConfig::default()
        };
        let r = resolve_ui(&ui);
        assert_eq!(r.theme, "pill");
        assert_eq!(r.colors, pill());
    }

    #[test]
    fn null_aliases_still_resolve_pill_palette() {
        for theme in ["null", "none", "off"] {
            let r = resolve_ui(&UiConfig {
                theme: theme.into(),
                ..UiConfig::default()
            });
            assert_eq!(r.theme, "pill", "theme={theme}");
            assert_eq!(r.colors, pill());
        }
    }

    #[test]
    fn stage_labels_default_and_override() {
        // WHY: defaults match historical hard-coded overlay copy.
        let ui = UiConfig::default();
        assert_eq!(stage_label(&ui, Stage::Recording), "Transcribing");
        assert_eq!(stage_label(&ui, Stage::Transcribing), "Processing");
        assert_eq!(stage_label(&ui, Stage::Done), "Done");
        assert_eq!(stage_label(&ui, Stage::Error), "Error");
        assert_eq!(stage_label(&ui, Stage::Hidden), "");

        let custom = UiConfig {
            stages: UiStages {
                recording: "Listening".into(),
                transcribing: "Thinking".into(),
                done: "Ready".into(),
                error: "Failed".into(),
                ..UiStages::default()
            },
            ..UiConfig::default()
        };
        assert_eq!(stage_label(&custom, Stage::Recording), "Listening");
        assert_eq!(stage_label(&custom, Stage::Transcribing), "Thinking");
        assert_eq!(stage_label(&custom, Stage::Done), "Ready");
        assert_eq!(stage_label(&custom, Stage::Error), "Failed");
    }

    #[test]
    fn validate_rejects_bad_hex() {
        let err = validate_color_overrides(
            &UiColors {
                bg: Some("not-a-color".into()),
                ..UiColors::default()
            },
            std::path::Path::new("cfg.toml"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ui.colors.bg"), "{err}");
        assert!(err.contains("not-a-color"), "{err}");
    }
}
