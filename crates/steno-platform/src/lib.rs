//! OS backends for steno-platform: hotkey, overlay, typing.
//!
//! Re-exports overlay trait types from `steno-core` so embedders can
//! depend on `steno-platform` alone for UI wiring.

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

pub use steno_core::InjectTyper;
pub use steno_core::overlay::{NullOverlay, OverlayBackend, Stage};
pub use null::{NullHotkey, NullTyper};
pub use traits::{HotkeyEvent, HotkeySource, OutputMode, Typer};

#[cfg(target_os = "linux")]
pub use linux::{Emitter, Hotkey, Overlay, create, create as create_overlay};
#[cfg(target_os = "linux")]
pub use linux_x11::restore_caps_lock_mapping;
#[cfg(target_os = "windows")]
pub use windows::{Emitter, Hotkey, Overlay, create, create as create_overlay};
#[cfg(target_os = "macos")]
pub use macos::{Emitter, Hotkey, Overlay, create, create as create_overlay};

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub mod fallback {
    use anyhow::Result;
    use steno_core::config::UiConfig;
    use steno_core::overlay::{NullOverlay, OverlayBackend};

    use crate::null::{NullHotkey, NullTyper};
    use crate::traits::{HotkeySource, OutputMode, Typer};

    /// Fallback hotkey source returning `NullHotkey`.
    pub type Hotkey = FallbackHotkey;
    /// Fallback keystroke injector returning `NullTyper`.
    pub type Emitter = FallbackEmitter;
    /// Fallback overlay backend returning `NullOverlay`.
    pub type Overlay = NullOverlay;

    /// Fallback hotkey wrapper for non-tier-1 targets.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub struct FallbackHotkey(NullHotkey);

    impl FallbackHotkey {
        pub fn grab_caps_lock() -> Result<Self> {
            Ok(Self(NullHotkey::new()))
        }
    }

    impl HotkeySource for FallbackHotkey {
        fn next_event(&mut self) -> Result<crate::traits::HotkeyEvent> {
            self.0.next_event()
        }

        fn drain_pending(&mut self) {
            self.0.drain_pending();
        }
    }

    /// Fallback typer wrapper for non-tier-1 targets.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub struct FallbackEmitter(NullTyper);

    impl FallbackEmitter {
        pub fn new(_mode: OutputMode) -> Self {
            Self(NullTyper::new())
        }
    }

    impl Typer for FallbackEmitter {
        fn type_text(&mut self, text: &str) -> Result<()> {
            self.0.type_text(text)
        }
    }

    impl steno_core::InjectTyper for FallbackEmitter {
        fn type_text(&mut self, text: &str) -> Result<()> {
            steno_core::InjectTyper::type_text(&mut self.0, text)
        }
    }

    pub fn create(_ui_cfg: &UiConfig) -> Box<dyn OverlayBackend> {
        Box::new(NullOverlay)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub use fallback::{Emitter, Hotkey, Overlay, create, create as create_overlay};

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
    /// Hotkey source bound to Caps Lock.
    pub hotkey: Box<dyn HotkeySource>,
    /// Keystroke injector for the selected output mode.
    pub typer: Box<dyn InjectTyper>,
    /// Status overlay backend.
    pub overlay: Box<dyn OverlayBackend>,
}

impl std::fmt::Debug for PlatformBackends {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformBackends")
            .field("hotkey", &"Box<dyn HotkeySource>")
            .field("typer", &"Box<dyn InjectTyper>")
            .field("overlay", &"Box<dyn OverlayBackend>")
            .finish()
    }
}

/// Construct all platform backends from output mode and UI configuration.
pub fn create_platform_backends(
    mode: OutputMode,
    ui_cfg: &steno_core::config::UiConfig,
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
    //! WHY: Platform backend creation functions (`create_typer`, `create_overlay`, `create_platform_backends`)
    //! must instantiate valid backends across supported target platforms.
    use super::*;
    use steno_core::config::UiConfig;

    #[test]
    fn test_create_typer() {
        // WHY: create_typer must instantiate a functional Box<dyn InjectTyper> for each OutputMode variant
        // without emitting live keystrokes into the host desktop during unit tests.
        for mode in [OutputMode::Stdout, OutputMode::Type] {
            let mut typer = create_typer(mode);
            if mode == OutputMode::Stdout {
                // Stdout mode fails fail-closed in library unit tests
                assert!(typer.type_text("test string").is_err());
            }
        }
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
    fn test_create_hotkey() {
        // WHY: create_hotkey must attempt to create a Caps Lock hotkey source cleanly.
        // It returns Ok(hk) on supported desktop hosts or Err(e) in headless CI without crashing.
        match create_hotkey() {
            Ok(mut hk) => {
                hk.drain_pending();
            }
            Err(e) => {
                assert!(!e.to_string().is_empty(), "error output must be non-empty");
            }
        }
    }

    #[test]
    fn test_create_platform_backends() {
        // WHY: create_platform_backends must aggregate hotkey, typer, and overlay into a PlatformBackends.
        let cfg = UiConfig {
            overlay: false,
            ..UiConfig::default()
        };
        for mode in [OutputMode::Stdout, OutputMode::Type] {
            match create_platform_backends(mode, &cfg) {
                Ok(backends) => {
                    backends.overlay.set(Stage::Recording);
                    assert!(format!("{backends:?}").contains("PlatformBackends"));
                }
                Err(e) => {
                    assert!(!e.to_string().is_empty());
                }
            }
        }
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
    /// WHY: Verify `PlatformBackends` `Debug` implementation formats cleanly without unwrapping or panicking.
    #[test]
    fn test_platform_backends_debug() {
        let hk: Box<dyn HotkeySource> = Box::new(null::NullHotkey::new());
        let typer: Box<dyn InjectTyper> = Box::new(null::NullTyper::new());
        let overlay: Box<dyn OverlayBackend> = Box::new(NullOverlay);
        let backends = PlatformBackends {
            hotkey: hk,
            typer,
            overlay,
        };
        let debug_str = format!("{backends:?}");
        assert!(debug_str.contains("PlatformBackends"));
    }
}
