//! Wayland native status overlay via `zwlr_layer_shell_v1`.
//!
//! Renders a bottom-center status pill on Wayland compositors that support
//! the layer-shell protocol (sway, cage, Hyprland, etc.). Uses
//! `smithay-client-toolkit` for protocol handling and `tiny-skia` for
//! software rendering (no GPU/EGL required — works headless).
//!
//! The overlay runs a background event loop thread that:
//! 1. Connects to the Wayland display
//! 2. Creates a layer-surface anchored to the bottom of the screen
//! 3. Renders the current stage label + color, redrawing on stage change
//! 4. Fails open: if Wayland is unavailable the overlay silently deactivates

use std::sync::Arc;
use std::time::Duration;

use steno_core::config::UiConfig;
use steno_core::overlay::{OverlayBackend, Stage};
use steno_core::ui_theme::{resolve_ui, ResolvedUi, Rgba};

use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, FrameCallbackData};
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::{slot::SlotPool, Shm, ShmHandler};

use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, Path as SkPath, PathBuilder, Pixmap as SkPixmap,
    PixmapPaint, PremultipliedColorU8, Stroke, Transform,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};

// ── Metrics (device pixels, 1× scale) ───────────────────────────────

const SURFACE_W: u32 = 240;
const SURFACE_H: u32 = 56;
const PILL_W: f32 = 220.0;
const PILL_H: f32 = 48.0;
const PILL_X: f32 = 10.0;
const PILL_Y: f32 = 4.0;
const PILL_R: f32 = 24.0;
const ICON: f32 = 26.0;
const ICON_X: f32 = 20.0;
const GAP: f32 = 12.0;
const LABEL_PX: f32 = 13.0;

// ── Public API ──────────────────────────────────────────────────────

/// Wayland layer-shell status pill.
pub struct WaylandOverlay {
    stage: Arc<std::sync::Mutex<Stage>>,
    active: Arc<std::sync::Mutex<bool>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WaylandOverlay {
    /// Connect to the Wayland display and start the overlay event loop.
    ///
    /// Returns a non-active overlay when Wayland is unavailable; the
    /// daemon continues without a status pill.
    pub fn new(cfg: &UiConfig) -> Result<Self> {
        let stage = Arc::new(std::sync::Mutex::new(Stage::Hidden));
        let active = Arc::new(std::sync::Mutex::new(false));
        let ui = resolve_ui(cfg);
        let overlay_enabled = cfg.overlay;

        let stage_c = Arc::clone(&stage);
        let active_c = Arc::clone(&active);

        let thread = std::thread::Builder::new()
            .name("steno-wayland-overlay".into())
            .spawn(move || {
                if !overlay_enabled {
                    return;
                }
                if let Err(e) = run_event_loop(stage_c, active_c, ui) {
                    log::warn!("Wayland overlay disabled: {e:#}");
                }
            })
            .context("cannot spawn Wayland overlay thread")?;

        Ok(Self {
            stage,
            active,
            thread: Some(thread),
        })
    }
}

impl Drop for WaylandOverlay {
    fn drop(&mut self) {
        // Signal the event loop to stop.
        if let Ok(mut a) = self.active.lock() {
            *a = false;
        }
        // Wait for the thread to finish (it should exit within ~100ms).
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl OverlayBackend for WaylandOverlay {
    fn set(&self, stage: Stage) {
        if let Ok(mut s) = self.stage.lock() {
            *s = stage;
        }
    }

    fn flash(&self, _ms: u64) {
        // Flash timing is enforced by the daemon (set → wait → set(Hidden)).
    }

    fn active(&self) -> bool {
        *self.active.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ── Event loop ──────────────────────────────────────────────────────

fn run_event_loop(
    stage: Arc<std::sync::Mutex<Stage>>,
    active: Arc<std::sync::Mutex<bool>>,
    ui: ResolvedUi,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("cannot connect to Wayland display")?;
    let (globals, event_queue) =
        registry_queue_init::<WaylandState>(&conn).context("cannot initialize Wayland registry")?;
    let qh = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let layer_shell =
        LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1 not available")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("steno-status"),
        None,
    );
    layer.set_anchor(Anchor::BOTTOM);
    layer.set_size(SURFACE_W, SURFACE_H);
    layer.commit();

    // Allocate enough for 4x scale (max common HiDPI) to avoid pool exhaustion.
    let pool_size = (SURFACE_W * SURFACE_H * 4 * 4) as usize;
    let pool = SlotPool::new(pool_size, &shm)
        .context("cannot create SHM pool")?;

    let font = load_font();

    let mut state = WaylandState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        qh: qh.clone(),
        width: SURFACE_W,
        height: SURFACE_H,
        scale: 1,
        first_configure: true,
        exit: false,
        stage,
        active: Arc::clone(&active),
        ui,
        last_drawn: Stage::Hidden,
        font,
    };

    let mut event_loop: EventLoop<WaylandState> =
        EventLoop::try_new().context("cannot create event loop")?;

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("cannot insert Wayland source: {e}"))?;

    event_loop
        .handle()
        .insert_source(
            Timer::from_duration(Duration::from_millis(100)),
            |_, _, state| {
                // Check external shutdown signal (active set to false by Drop).
                let still_active = state
                    .active
                    .lock()
                    .map(|a| *a)
                    .unwrap_or(false);
                if state.exit || !still_active {
                    state.exit = true;
                    TimeoutAction::Drop
                } else {
                    state.poll_redraw();
                    TimeoutAction::ToDuration(Duration::from_millis(100))
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("cannot insert timer: {e}"))?;

    // Mark active — the overlay is live.
    if let Ok(mut a) = active.lock() {
        *a = true;
    }

    let signal = event_loop.get_signal();
    event_loop
        .run(None, &mut state, |state| {
            if state.exit {
                signal.stop();
            }
        })
        .context("event loop error")?;
    // Mark inactive — the overlay is shutting down.
    if let Ok(mut a) = active.lock() {
        *a = false;
    }

    Ok(())
}

// ── Wayland state ───────────────────────────────────────────────────

/// State carried by the Wayland event loop.
struct WaylandState {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    qh: QueueHandle<WaylandState>,
    width: u32,
    height: u32,
    scale: i32,
    first_configure: bool,
    exit: bool,
    stage: Arc<std::sync::Mutex<Stage>>,
    active: Arc<std::sync::Mutex<bool>>,
    ui: ResolvedUi,
    last_drawn: Stage,
    font: Option<Font>,
}

impl WaylandState {
    fn current_stage(&self) -> Stage {
        *self.stage.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn poll_redraw(&mut self) {
        let current = self.current_stage();
        if current != self.last_drawn {
            self.draw();
            self.last_drawn = current;
        }
    }

    fn draw(&mut self) {
        // Render at physical resolution: logical size × scale factor.
        // The compositor scales the buffer back down to logical size for
        // display, giving us crisp text on HiDPI outputs.
        let phys_w = self.width * self.scale as u32;
        let phys_h = self.height * self.scale as u32;
        let stride = phys_w as i32 * 4;

        // Render to an off-screen pixmap first, before borrowing the SHM
        // pool mutably. This avoids splitting borrows of `self`.
        let stage = self.current_stage();
        let Some(mut pixmap) = SkPixmap::new(phys_w, phys_h) else { return };
        // Scale the rendering to physical pixels.
        let scale_factor = self.scale as f32;
        render_pill_scaled(&mut pixmap, stage, &self.ui, self.font.as_ref(), scale_factor);

        let (buffer, canvas) = match self
            .pool
            .create_buffer(phys_w as i32, phys_h as i32, stride, wl_shm::Format::Argb8888)
        {
            Ok(v) => v,
            Err(e) => {
                log::warn!("Wayland overlay: cannot create buffer: {e}");
                return;
            }
        };

        // tiny-skia stores premultiplied RGBA; wl_shm Argb8888 is BGRA on LE.
        let src = pixmap.data();
        for (s, d) in src.chunks_exact(4).zip(canvas.chunks_exact_mut(4)) {
            d[0] = s[2]; // B
            d[1] = s[1]; // G
            d[2] = s[0]; // R
            d[3] = s[3]; // A
        }

        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, phys_w as i32, phys_h as i32);
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        if let Err(e) = buffer.attach_to(surface) {
            log::warn!("Wayland overlay: cannot attach buffer: {e}");
            return;
        }
        self.layer.commit();
    }
}

impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        self.scale = new_factor.max(1);
        surface.set_buffer_scale(self.scale);
        // Force a redraw at the new scale on the next poll.
        self.last_drawn = Stage::Hidden;
    }


    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for WaylandState {
    fn closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
    ) {
        self.exit = true;
        if let Ok(mut a) = self.active.lock() {
            *a = false;
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        self.width = if configure.new_size.0 > 0 {
            configure.new_size.0
        } else {
            SURFACE_W
        };
        self.height = if configure.new_size.1 > 0 {
            configure.new_size.1
        } else {
            SURFACE_H
        };
        if self.first_configure {
            self.first_configure = false;
            self.draw();
            self.last_drawn = self.current_stage();
        }
    }
}

impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(WaylandState);

impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(WaylandState);

// ── Rendering ───────────────────────────────────────────────────────

fn rgba(c: Rgba) -> Color {
    Color::from_rgba8(c[0], c[1], c[2], c[3])
}

fn stage_text(ui: &ResolvedUi, stage: Stage) -> &str {
    match stage {
        Stage::Hidden => "",
        Stage::Recording => ui.stages.recording.as_str(),
        Stage::Transcribing => ui.stages.transcribing.as_str(),
        Stage::Done => ui.stages.done.as_str(),
        Stage::Error => ui.stages.error.as_str(),
    }
}

fn load_font() -> Option<Font> {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/opentype/inter/Inter-SemiBold.otf",
        "/usr/share/fonts/opentype/inter/Inter-Medium.otf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
                return Some(font);
            }
        }
    }
    None
}

fn render_pill(pixmap: &mut SkPixmap, stage: Stage, ui: &ResolvedUi, font: Option<&Font>) {
    pixmap.fill(Color::from_rgba8(0, 0, 0, 0));
    if stage == Stage::Hidden {
        return;
    }

    let c = &ui.colors;

    // Pill body + hairline border.
    draw_round_rect(pixmap, PILL_X, PILL_Y, PILL_W, PILL_H, PILL_R, rgba(c.bg));
    stroke_round_rect(
        pixmap,
        PILL_X + 0.5,
        PILL_Y + 0.5,
        PILL_W - 1.0,
        PILL_H - 1.0,
        PILL_R,
        rgba(c.border),
        1.0,
    );

    // Icon disc.
    let icon_y = PILL_Y + (PILL_H - ICON) * 0.5;
    let disc = if stage == Stage::Error {
        rgba(c.error)
    } else {
        rgba(c.icon_bg)
    };
    fill_circle(
        pixmap,
        ICON_X + ICON * 0.5,
        icon_y + ICON * 0.5,
        ICON * 0.5,
        disc,
    );

    // Icon glyph.
    let glyph = rgba(c.icon_fg);
    match stage {
        Stage::Recording => draw_wave(pixmap, ICON_X, icon_y, glyph),
        Stage::Transcribing => draw_spinner(pixmap, ICON_X, icon_y, glyph),
        Stage::Done => draw_check(pixmap, ICON_X, icon_y, glyph),
        Stage::Error => draw_x(pixmap, ICON_X, icon_y, glyph),
        Stage::Hidden => {}
    }

    // Label.
    if let Some(f) = font {
        let text = stage_text(ui, stage);
        let text_x = ICON_X + ICON + GAP;
        draw_text(
            pixmap,
            f,
            text,
            text_x,
            PILL_Y + PILL_H * 0.5,
            LABEL_PX,
            rgba(c.fg),
        );
    }
}

/// Render the pill at physical-pixel resolution by scaling all metrics.
/// This produces crisp text and shapes on HiDPI Wayland outputs.
fn render_pill_scaled(
    pixmap: &mut SkPixmap,
    stage: Stage,
    ui: &ResolvedUi,
    font: Option<&Font>,
    scale: f32,
) {
    if scale == 1.0 {
        render_pill(pixmap, stage, ui, font);
        return;
    }
    pixmap.fill(Color::from_rgba8(0, 0, 0, 0));
    if stage == Stage::Hidden {
        return;
    }

    let c = &ui.colors;
    let s = |v: f32| v * scale;

    // Pill body + hairline border.
    draw_round_rect(pixmap, s(PILL_X), s(PILL_Y), s(PILL_W), s(PILL_H), s(PILL_R), rgba(c.bg));
    stroke_round_rect(
        pixmap,
        s(PILL_X + 0.5),
        s(PILL_Y + 0.5),
        s(PILL_W - 1.0),
        s(PILL_H - 1.0),
        s(PILL_R),
        rgba(c.border),
        s(1.0),
    );

    // Icon disc.
    let icon_y = s(PILL_Y + (PILL_H - ICON) * 0.5);
    let disc = if stage == Stage::Error {
        rgba(c.error)
    } else {
        rgba(c.icon_bg)
    };
    fill_circle(
        pixmap,
        s(ICON_X + ICON * 0.5),
        icon_y + s(ICON * 0.5),
        s(ICON * 0.5),
        disc,
    );

    // Icon glyph.
    let glyph = rgba(c.icon_fg);
    match stage {
        Stage::Recording => draw_wave(pixmap, s(ICON_X), icon_y, glyph),
        Stage::Transcribing => draw_spinner(pixmap, s(ICON_X), icon_y, glyph),
        Stage::Done => draw_check(pixmap, s(ICON_X), icon_y, glyph),
        Stage::Error => draw_x(pixmap, s(ICON_X), icon_y, glyph),
        Stage::Hidden => {}
    }

    // Label.
    if let Some(f) = font {
        let text = stage_text(ui, stage);
        let text_x = s(ICON_X + ICON + GAP);
        draw_text(
            pixmap,
            f,
            text,
            text_x,
            s(PILL_Y + PILL_H * 0.5),
            s(LABEL_PX),
            rgba(c.fg),
        );
    }
}

fn draw_round_rect(pixmap: &mut SkPixmap, x: f32, y: f32, w: f32, h: f32, r: f32, color: Color) {
    let Some(path) = round_rect_path(x, y, w, h, r) else {
        return;
    };
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(color);
    pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
}

#[allow(clippy::too_many_arguments)]
fn stroke_round_rect(
    pixmap: &mut SkPixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    color: Color,
    width: f32,
) {
    let Some(path) = round_rect_path(x, y, w, h, r) else {
        return;
    };
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(color);
    pixmap.stroke_path(
        &path,
        &paint,
        &Stroke {
            width,
            ..Stroke::default()
        },
        Transform::identity(),
        None,
    );
}

fn round_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<SkPath> {
    let r = r.min(w * 0.5).min(h * 0.5);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

fn fill_circle(pixmap: &mut SkPixmap, cx: f32, cy: f32, r: f32, color: Color) {
    let Some(path) = circle_path(cx, cy, r) else {
        return;
    };
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(color);
    pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
}

fn circle_path(cx: f32, cy: f32, r: f32) -> Option<SkPath> {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    pb.finish()
}

fn arc_path(cx: f32, cy: f32, r: f32, a0: f32, a1: f32) -> Option<SkPath> {
    let mut pb = PathBuilder::new();
    let steps = 24;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let a = a0 + (a1 - a0) * t;
        let x = cx + r * a.cos();
        let y = cy + r * a.sin();
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.finish()
}

fn draw_wave(pixmap: &mut SkPixmap, ix: f32, iy: f32, color: Color) {
    let cx = ix + ICON * 0.5;
    let cy = iy + ICON * 0.5;
    let heights = [5.0_f32, 10.0, 14.0, 8.0];
    let bar_w = 2.0_f32;
    let gap = 2.0_f32;
    let total = 4.0 * bar_w + 3.0 * gap;
    let left = cx - total * 0.5;
    for (i, &h) in heights.iter().enumerate() {
        let bh = h.max(bar_w);
        let x = left + i as f32 * (bar_w + gap);
        let y = cy - bh * 0.5;
        draw_round_rect(pixmap, x, y, bar_w, bh, bar_w * 0.5, color);
    }
}

fn draw_spinner(pixmap: &mut SkPixmap, ix: f32, iy: f32, color: Color) {
    let cx = ix + ICON * 0.5;
    let cy = iy + ICON * 0.5;
    let r = 5.75_f32;
    let stroke = Stroke {
        width: 1.5,
        ..Stroke::default()
    };
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    let cr = (color.red() * 255.0) as u8;
    let cg = (color.green() * 255.0) as u8;
    let cb = (color.blue() * 255.0) as u8;
    let ca = (color.alpha() * 255.0) as u8;
    // Track ring at ~35% of glyph alpha.
    paint.set_color(Color::from_rgba8(cr, cg, cb, ((ca as u16 * 89) / 255) as u8));
    if let Some(path) = circle_path(cx, cy, r) {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
    // Full glyph quarter arc.
    paint.set_color(color);
    let ang = -std::f32::consts::FRAC_PI_2;
    if let Some(path) = arc_path(cx, cy, r, ang, ang + std::f32::consts::FRAC_PI_2) {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

fn draw_check(pixmap: &mut SkPixmap, ix: f32, iy: f32, color: Color) {
    let cx = ix + ICON * 0.5;
    let cy = iy + ICON * 0.5;
    // 16-unit viewBox mapped onto a 13px glyph box.
    let k = 13.0 / 16.0;
    let p0 = (cx - 4.8 * k, cy + 0.3 * k);
    let p1 = (cx - 1.4 * k, cy + 3.5 * k);
    let p2 = (cx + 4.9 * k, cy - 3.2 * k);
    let mut pb = PathBuilder::new();
    pb.move_to(p0.0, p0.1);
    pb.line_to(p1.0, p1.1);
    pb.line_to(p2.0, p2.1);
    let Some(path) = pb.finish() else { return };
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(color);
    let stroke = Stroke {
        width: 2.2 * k,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn draw_x(pixmap: &mut SkPixmap, ix: f32, iy: f32, color: Color) {
    let cx = ix + ICON * 0.5;
    let cy = iy + ICON * 0.5;
    let s = 4.2_f32;
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(color);
    let stroke = Stroke {
        width: 2.0,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    let mut pb = PathBuilder::new();
    pb.move_to(cx - s, cy - s);
    pb.line_to(cx + s, cy + s);
    pb.move_to(cx + s, cy - s);
    pb.line_to(cx - s, cy + s);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}


fn draw_text(
    pixmap: &mut SkPixmap,
    font: &Font,
    text: &str,
    x: f32,
    center_y: f32,
    size: f32,
    color: Color,
) {
    // Vertically center the ascent/descent band on center_y.
    let baseline = if let Some(m) = font.horizontal_line_metrics(size) {
        center_y + (m.ascent + m.descent) * 0.5
    } else {
        center_y + size * 0.35
    };
    let cr = (color.red() * 255.0) as u16;
    let cg = (color.green() * 255.0) as u16;
    let cb = (color.blue() * 255.0) as u16;
    let ca = (color.alpha() * 255.0) as u16;
    let mut pen_x = x;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size);
        if !bitmap.is_empty() && metrics.width > 0 && metrics.height > 0 {
            if let Some(mut glyph) =
                SkPixmap::new(metrics.width as u32, metrics.height as u32)
            {
                for (i, coverage) in bitmap.iter().enumerate() {
                    let a = (*coverage as u16 * ca + 127) / 255;
                    let r = (cr * a + 127) / 255;
                    let g = (cg * a + 127) / 255;
                    let b = (cb * a + 127) / 255;
                    glyph.pixels_mut()[i] =
                        PremultipliedColorU8::from_rgba(r as u8, g as u8, b as u8, a as u8)
                            .unwrap_or_else(|| {
                                PremultipliedColorU8::from_rgba(0, 0, 0, 0).unwrap()
                            });
                }
                pixmap.draw_pixmap(
                    (pen_x + metrics.xmin as f32).round() as i32,
                    (baseline - metrics.ymin as f32 - metrics.height as f32).round() as i32,
                    glyph.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
            }
        }
        pen_x += metrics.advance_width;
    }
}
