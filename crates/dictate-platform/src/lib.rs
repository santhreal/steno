//! OS backends for light-dictate: hotkey, overlay, typing.
//!
//! Re-exports overlay trait types from `dictate-core` so embedders can
//! depend on `dictate-platform` alone for UI wiring.

pub mod null;
pub mod traits;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub mod linux_wayland;
#[cfg(target_os = "linux")]
pub mod linux_x11;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "macos")]
pub mod macos;

pub use dictate_core::InjectTyper;
pub use dictate_core::overlay::{NullOverlay, OverlayBackend, Stage};
pub use null::{NullHotkey, NullTyper};
pub use traits::{HotkeySource, Typer};

#[cfg(target_os = "linux")]
pub use linux::{Emitter, Hotkey, HotkeyEvent, OutputMode, Overlay, create, create as create_overlay};
#[cfg(target_os = "linux")]
pub use linux_x11::restore_caps_lock_mapping;
#[cfg(target_os = "windows")]
pub use windows::{Emitter, Hotkey, HotkeyEvent, OutputMode, Overlay, create, create as create_overlay};
#[cfg(target_os = "macos")]
pub use macos::{Emitter, Hotkey, HotkeyEvent, OutputMode, Overlay, create, create as create_overlay};

/// Create a platform hotkey source bound to Caps Lock.
pub fn create_hotkey() -> anyhow::Result<Box<dyn HotkeySource>> {
    let hk = Hotkey::grab_caps_lock()?;
    Ok(Box::new(hk))
}

/// Create a platform keystroke injector for the specified [`OutputMode`].
pub fn create_typer(mode: OutputMode) -> Box<dyn InjectTyper> {
    Box::new(Emitter::new(mode))
}

/// Aggregated platform backends for hotkey, typing, and status overlay.
pub struct PlatformBackends {
    pub hotkey: Box<dyn HotkeySource>,
    pub typer: Box<dyn InjectTyper>,
    pub overlay: Box<dyn OverlayBackend>,
}

/// Construct all platform backends from output mode and UI configuration.
pub fn create_platform_backends(
    mode: OutputMode,
    ui_cfg: &dictate_core::config::UiConfig,
) -> anyhow::Result<PlatformBackends> {
    let hotkey = create_hotkey()?;
    let typer = create_typer(mode);
    let overlay = create_overlay(ui_cfg);
    Ok(PlatformBackends {
        hotkey,
        typer,
        overlay,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dictate_core::config::UiConfig;

    #[test]
    fn test_create_typer() {
        let mut typer = create_typer(OutputMode::Stdout);
        // Stdout mode InjectTyper::type_text fails fail-closed.
        assert!(typer.type_text("test").is_err());
    }

    #[test]
    fn test_create_overlay() {
        let cfg = UiConfig {
            overlay: false,
            ..UiConfig::default()
        };
        let overlay = create_overlay(&cfg);
        overlay.set(Stage::Recording);
        overlay.flash(10);
    }

    #[test]
    fn test_platform_backends_struct() {
        let hk: Box<dyn HotkeySource> = Box::new(null::NullHotkey::new());
        let typer: Box<dyn InjectTyper> = Box::new(null::NullTyper::new());
        let overlay: Box<dyn OverlayBackend> = Box::new(NullOverlay);
        let mut backends = PlatformBackends {
            hotkey: hk,
            typer,
            overlay,
        };
        backends.hotkey.drain_pending();
        assert!(backends.typer.type_text("hello").is_ok());
        backends.overlay.flash(10);
    }

    #[test]
    fn test_create_hotkey_call() {
        let _res = create_hotkey();
    }
}
