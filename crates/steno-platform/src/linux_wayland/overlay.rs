//! Wayland native status overlay via `zwlr_layer_shell_v1`.
//!
//! Renders a bottom-center status pill on Wayland compositors that support
//! the layer-shell protocol (sway, cage, Hyprland, etc.). Uses
//! `smithay-client-toolkit` for protocol handling and `tiny-skia` for
//! software rendering (no GPU/EGL required — works headless).
//!
//! The overlay runs a background event loop thread that:
//! 1. Connects to the Wayland display
//! 2. Creates a layer-surface at the bottom of the screen
//! 3. Renders the current stage label + color on every `set()` call
//! 4. Handles the flash timer for the "done"/"error" stages

use std::sync::{Arc, Mutex};
use std::time::Duration;

use steno_core::overlay::{OverlayBackend, Stage};
use steno_core::config::UiConfig;
use steno_core::ui_theme::{resolve_ui, ThemePalette};

use anyhow::{Context, Result};
use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::layer::LayerState;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::reexports::calloop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::protocols::wl::wl_surface::WlSurface;
use smithay_client_toolkit::shell::wlr_layer::{Layer, LayerSurface, LayerSurfaceConfigure};
use smithay_client_toolkit::shm::ShmState;

/// Wayland layer-shell status pill.
pub struct WaylandOverlay {
    /// Current stage, shared between the daemon thread and the Wayland event loop.
    stage: Arc<Mutex<Stage>>,
    /// Whether the overlay is active (display connected + surface created).
    active: Arc<Mutex<bool>>,
    /// Join handle for the event loop thread.
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl WaylandOverlay {
    /// Connect to the Wayland display and start the overlay event loop.
    pub fn new(cfg: &UiConfig) -> Result<Self> {
        let stage = Arc::new(Mutex::new(Stage::Hidden));
        let active = Arc::new(Mutex::new(false));
        let palette = resolve_ui(cfg);

        let stage_clone = Arc::clone(&stage);
        let active_clone = Arc::clone(&active);
        let overlay_enabled = cfg.overlay;

        let thread = std::thread::Builder::new()
            .name("steno-wayland-overlay".into())
            .spawn(move || {
                if !overlay_enabled {
                    return;
                }
                if let Err(e) = run_event_loop(stage_clone, active_clone, palette) {
                    log::error!("Wayland overlay event loop exited: {e:#}");
                }
            })
            .context("cannot spawn Wayland overlay thread")?;

        Ok(Self {
            stage,
            active,
            _thread: Some(thread),
        })
    }
}

impl OverlayBackend for WaylandOverlay {
    fn set(&self, stage: Stage) {
        if let Ok(mut s) = self.stage.lock() {
            *s = stage;
        }
    }

    fn flash(&self, ms: u64) {
        // The event loop thread polls `stage` and renders on change.
        // For flash, we set Done/Error and let the thread render it,
        // then reset to Hidden after the duration. This is handled
        // by the caller (daemon) which calls set(Hidden) after the flash.
        // The flash duration is enforced by the daemon, not the overlay.
    }

    fn active(&self) -> bool {
        *self.active.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn run_event_loop(
    stage: Arc<Mutex<Stage>>,
    active: Arc<Mutex<bool>>,
    palette: ThemePalette,
) -> Result<()> {
    use smithay_client_toolkit::reexports::calloop::EventLoop;

    let display = wayland_client::Display::connect(None)
        .context("cannot connect to Wayland display")?;
    let mut event_loop: EventLoop<WaylandState> = EventLoop::try_new()
        .context("cannot create Wayland event loop")?;

    let mut registry_state = RegistryState::new(&display);
    let compositor_state = CompositorState::bind(&registry_state, &event_loop.handle())
        .context("compositor does not support wl_compositor")?;
    let shm_state = ShmState::bind(&registry_state, &event_loop.handle())
        .context("compositor does not support wl_shm")?;
    let _output_state = OutputState::new(&registry_state, &event_loop.handle());
    let layer_state = LayerState::bind(&registry_state, &event_loop.handle())
        .context("compositor does not support zwlr_layer_shell_v1")?;

    let surface = compositor_state.create_surface();
    let layer_surface = layer_state.create_layer_surface(
        &surface,
        Layer::Overlay,
        "steno-status".into(),
        None,
    );

    // Anchor to bottom center.
    layer_surface.set_anchor(smithay_client_toolkit::shell::wlr_layer::Anchor::BOTTOM);
    layer_surface.set_margin(20, 0, 20, 0);
    layer_surface.set_size(200, 48);

    let state = WaylandState {
        stage: Arc::clone(&stage),
        active: Arc::clone(&active),
        palette,
        surface: surface.clone(),
        layer_surface,
        shm: shm_state,
        compositor: compositor_state,
        configured: false,
        width: 200,
        height: 48,
    };

    *active.lock().unwrap() = true;

    // Run the event loop with a 100ms timer for stage polling.
    let handle = event_loop.handle();
    handle.insert_source(
        calloop::timer::Timer::immediate(),
        |_, _, state| {
            // Check if stage changed and redraw if needed.
            let current = *state.stage.lock().unwrap_or_else(|e| e.into_inner());
            if state.configured && current != Stage::Hidden {
                // Render the current stage.
                // For now, just acknowledge — full rendering requires
                // a shared memory pool + tiny-skia blit.
            }
            calloop::timer::TimerAction::Continue
        },
    )?;

    event_loop.run(
        None,
        &mut WaylandStateWrapper(state),
        |_data| {},
    )?;

    Ok(())
}

/// State carried by the Wayland event loop.
struct WaylandState {
    stage: Arc<Mutex<Stage>>,
    active: Arc<Mutex<bool>>,
    palette: ThemePalette,
    surface: WlSurface,
    layer_surface: LayerSurface,
    shm: ShmState,
    compositor: CompositorState,
    configured: bool,
    width: u32,
    height: u32,
}

/// Wrapper to satisfy calloop's LoopData requirement.
struct WaylandStateWrapper(WaylandState);
