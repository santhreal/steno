//! Runtime Linux session selection: X11 stays primary when `DISPLAY` is
//! usable; pure Wayland (`WAYLAND_DISPLAY` only) takes the wtype path.

/// How keystrokes are injected on this Linux session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypingBackend {
    /// `xdotool` (X11 / XWayland).
    Xdotool,
    /// `wtype` (native Wayland virtual-keyboard), with optional `ydotool`.
    Wtype,
}

/// How Caps Lock is grabbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyBackend {
    /// Reuse the X11 grab (including XWayland when `DISPLAY` is set).
    X11,
    /// No global Wayland grab yet: callers must fail loudly.
    Unavailable,
}

/// Overlay implementation choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayBackendChoice {
    /// Existing X11 pill (works under XWayland too).
    X11,
    /// Wayland native layer-shell pill (requires `wayland` cargo feature).
    Wayland,
    /// No layer-shell chip: `NullOverlay` + warning.
    NullWarn,
}

#[inline]
fn non_empty(var: Option<&str>) -> bool {
    var.map(|v| !v.is_empty()).unwrap_or(false)
}

/// True when `DISPLAY` is set to a non-empty value.
pub fn has_display_var(display: Option<&str>) -> bool {
    non_empty(display)
}

/// True when `WAYLAND_DISPLAY` is set to a non-empty value.
pub fn has_wayland_var(wayland: Option<&str>) -> bool {
    non_empty(wayland)
}

/// Select the typing backend from explicit env snapshots (unit-testable).
///
/// Policy: X11 remains primary whenever `DISPLAY` works (including hybrid
/// XWayland sessions). Pure Wayland (`WAYLAND_DISPLAY` without `DISPLAY`)
/// uses `wtype`.
pub fn typing_backend_from(display: Option<&str>, wayland: Option<&str>) -> TypingBackend {
    if has_wayland_var(wayland) && !has_display_var(display) {
        TypingBackend::Wtype
    } else {
        TypingBackend::Xdotool
    }
}

/// Select the hotkey backend from explicit env snapshots.
pub fn hotkey_backend_from(display: Option<&str>, wayland: Option<&str>) -> HotkeyBackend {
    if has_display_var(display) {
        HotkeyBackend::X11
    } else if has_wayland_var(wayland) {
        HotkeyBackend::Unavailable
    } else {
        // Neither set — let the X11 connect path report its own error.
        HotkeyBackend::X11
    }
}
pub fn overlay_backend_from(display: Option<&str>, wayland: Option<&str>) -> OverlayBackendChoice {
    if has_display_var(display) {
        OverlayBackendChoice::X11
    } else if has_wayland_var(wayland) {
        #[cfg(feature = "wayland")]
        { OverlayBackendChoice::Wayland }
        #[cfg(not(feature = "wayland"))]
        { OverlayBackendChoice::NullWarn }
    } else {
        OverlayBackendChoice::X11
    }
}

/// Read live process environment.
pub fn typing_backend() -> TypingBackend {
    typing_backend_from(
        std::env::var("DISPLAY").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
    )
}

/// Read live process environment.
pub fn hotkey_backend() -> HotkeyBackend {
    hotkey_backend_from(
        std::env::var("DISPLAY").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
    )
}

/// Read live process environment.
pub fn overlay_backend() -> OverlayBackendChoice {
    overlay_backend_from(
        std::env::var("DISPLAY").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
    )
}

/// Loud, actionable error when Caps Lock cannot be grabbed on pure Wayland.
pub fn pure_wayland_hotkey_error() -> String {
    "Caps Lock hotkey is unavailable on a pure Wayland session (WAYLAND_DISPLAY is set, DISPLAY is not). \
     Corrective actions: (1) enable XWayland / ensure DISPLAY is set so the existing X11 grab can run \
     (echo $DISPLAY; on GNOME Wayland XWayland is usually :0 or :1), or (2) use stdout mode \
     (`type_output = false` / `steno --stdout`) until a native Wayland hotkey lands. \
     See docs/PLATFORM_TRAITS.md (Linux Wayland)."
        .to_string()
}

/// Warning text when the status chip cannot be shown on pure Wayland.
pub fn pure_wayland_overlay_warn() -> &'static str {
    "Wayland status overlay is not implemented yet (no layer-shell chip in-tree). \
     Using NullOverlay; dictation continues. Follow-up: zwlr-layer-shell status pill. \
     Workaround: run under XWayland (DISPLAY set) for the existing X11 pill, or set \
     overlay = false / theme = \"null\" to silence this. See docs/PLATFORM_TRAITS.md."
}

#[cfg(test)]
mod tests {
    //! WHY: hybrid GNOME (DISPLAY+WAYLAND_DISPLAY) must keep X11 primary;
    //! pure Wayland must select wtype and refuse hotkey silently-falling-through.
    use super::*;

    #[test]
    fn prefers_x11_typing_when_display_set() {
        assert_eq!(
            typing_backend_from(Some(":0"), None),
            TypingBackend::Xdotool
        );
        assert_eq!(
            typing_backend_from(Some(":0"), Some("wayland-0")),
            TypingBackend::Xdotool
        );
    }

    #[test]
    fn pure_wayland_selects_wtype() {
        assert_eq!(
            typing_backend_from(None, Some("wayland-0")),
            TypingBackend::Wtype
        );
        assert_eq!(
            typing_backend_from(Some(""), Some("wayland-0")),
            TypingBackend::Wtype
        );
    }

    #[test]
    fn neither_env_defaults_to_xdotool() {
        assert_eq!(typing_backend_from(None, None), TypingBackend::Xdotool);
    }

    #[test]
    fn hotkey_unavailable_only_on_pure_wayland() {
        assert_eq!(
            hotkey_backend_from(Some(":0"), Some("wayland-0")),
            HotkeyBackend::X11
        );
        assert_eq!(
            hotkey_backend_from(None, Some("wayland-0")),
            HotkeyBackend::Unavailable
        );
        assert_eq!(hotkey_backend_from(None, None), HotkeyBackend::X11);
    }

    #[test]
    fn overlay_null_warn_on_pure_wayland() {
        assert_eq!(
            overlay_backend_from(Some(":0"), Some("wayland-0")),
            OverlayBackendChoice::X11
        );
        assert_eq!(
            overlay_backend_from(None, Some("wayland-0")),
            OverlayBackendChoice::NullWarn
        );
    }

    #[test]
    fn pure_wayland_hotkey_error_has_corrective_action() {
        let msg = pure_wayland_hotkey_error();
        assert!(msg.contains("WAYLAND_DISPLAY"), "{msg}");
        assert!(msg.contains("DISPLAY"), "{msg}");
        assert!(
            msg.contains("stdout") || msg.contains("XWayland"),
            "{msg}"
        );
    }
}
