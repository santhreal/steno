//! Animated status pill: a small borderless ARGB window at the bottom
//! center of the primary monitor. Three states match the product mock:
//!
//! - Recording  → "Transcribing" (waveform + elapsed timer)
//! - Transcribing → "Processing" (spinner)
//! - Done       → "Done" (check draw)
//!
//! Pure display: override-redirect, takes no input, never focuses.
//! Cosmetic and fail-open: no DISPLAY / no ARGB visual / any X error
//! simply disables the overlay — dictation itself is unaffected.

use std::f32::consts::PI;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use fontdue::{Font, FontSettings};
use serde::Deserialize;
use tiny_skia::{
    Color, FillRule, Paint, Path as SkPath, PathBuilder, Pixmap as SkPixmap, PixmapPaint, Rect, Stroke,
    StrokeDash, Transform,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Show the bottom-center status overlay (X11 only).
    pub overlay: bool,
    /// How long the "done"/"error" stage stays visible before hide.
    pub done_flash_ms: u64,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            overlay: true,
            // Matches the mock's quick Done celebration (~1.2s).
            done_flash_ms: 1200,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Window unmapped — idle between utterances.
    Hidden,
    /// Live capture (shown as "Transcribing" with waveform + timer).
    Recording,
    /// Decode in flight (shown as "Processing" with spinner).
    Transcribing,
    Done,
    Error,
}

fn label(stage: Stage) -> &'static str {
    match stage {
        Stage::Hidden => "",
        Stage::Recording => "Transcribing",
        Stage::Transcribing => "Processing",
        Stage::Done => "Done",
        Stage::Error => "Error",
    }
}

pub struct Overlay {
    tx: Option<Sender<Stage>>,
    /// Set when the overlay thread failed (no X, no font, X error).
    failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Overlay {
    /// Start the overlay thread, or a no-op handle when disabled/unavailable.
    pub fn start(cfg: &UiConfig) -> Self {
        let failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        if !cfg.overlay || std::env::var_os("DISPLAY").is_none() {
            return Self { tx: None, failed };
        }
        let (tx, rx) = channel::<Stage>();
        let failed2 = failed.clone();
        match thread::Builder::new()
            .name("dictate-overlay".into())
            .spawn(move || run(rx, failed2))
        {
            Ok(_) => Self {
                tx: Some(tx),
                failed,
            },
            Err(e) => {
                log::debug!("overlay disabled: cannot spawn thread: {e}");
                Self { tx: None, failed }
            }
        }
    }

    pub fn set(&self, stage: Stage) {
        if let Some(tx) = &self.tx {
            // A dead overlay thread must never block dictation.
            let _ = tx.send(stage);
        }
    }

    /// True unless the overlay is disabled or already known-dead.
    pub fn active(&self) -> bool {
        self.tx.is_some() && !self.failed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Keep the final stage visible briefly before the caller hides it.
    pub fn flash(&self, ms: u64) {
        if self.active() {
            thread::sleep(Duration::from_millis(ms));
        }
    }
}

// Dropping the Overlay closes the channel; the thread notices and
// destroys the window before exiting.

/// Geometry of the primary monitor's active CRTC, or None when RandR or
/// a primary output is unavailable.
fn primary_rect<C: x11rb::connection::Connection>(
    conn: &C,
    root: u32,
) -> Option<(i32, i32, i32, i32)> {
    use x11rb::protocol::randr;
    let output = randr::get_output_primary(conn, root)
        .ok()?
        .reply()
        .ok()?
        .output;
    if output == 0 {
        return None;
    }
    let info = randr::get_output_info(conn, output, 0).ok()?.reply().ok()?;
    if info.crtc == 0 {
        return None;
    }
    let c = randr::get_crtc_info(conn, info.crtc, 0)
        .ok()?
        .reply()
        .ok()?;
    Some((
        i32::from(c.x),
        i32::from(c.y),
        i32::from(c.width),
        i32::from(c.height),
    ))
}

fn run(rx: Receiver<Stage>, failed: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    if let Err(e) = run_inner(&rx) {
        log::debug!("overlay disabled: {e}");
        failed.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Logical design size (CSS px from the mock). Drawn at SCALE for sharpness.
const SCALE: u32 = 2;
const WIN_W: u32 = 260 * SCALE;
const WIN_H: u32 = 90 * SCALE;
const PILL_H: f32 = 46.0 * SCALE as f32;
const ICON: f32 = 26.0 * SCALE as f32;
const PAD_X: f32 = 16.0 * SCALE as f32;
const GAP: f32 = 12.0 * SCALE as f32;

fn run_inner(rx: &Receiver<Stage>) -> anyhow::Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    use x11rb::wrapper::ConnectionExt as _;

    let font = load_font()?;
    let (conn, screen_num) = x11rb::rust_connection::RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let (visual, depth) = find_argb_visual(&conn, screen)
        .ok_or_else(|| anyhow::anyhow!("no 32-bit ARGB visual — compositor required for pill"))?;

    let (ox, oy, ow, oh) = primary_rect(&conn, screen.root).unwrap_or((
        0,
        0,
        i32::from(screen.width_in_pixels),
        i32::from(screen.height_in_pixels),
    ));
    let x = (ox + (ow - WIN_W as i32) / 2).clamp(0, i32::from(i16::MAX)) as i16;
    let y = (oy + oh - WIN_H as i32 - 72).clamp(0, i32::from(i16::MAX)) as i16;

    let colormap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)?;

    let win = conn.generate_id()?;
    conn.create_window(
        depth,
        win,
        screen.root,
        x,
        y,
        WIN_W as u16,
        WIN_H as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .border_pixel(0)
            .background_pixel(0)
            .colormap(colormap)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE),
    )?;
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"dictate",
    )?;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0))?;

    let mut pixmap =
        SkPixmap::new(WIN_W, WIN_H).ok_or_else(|| anyhow::anyhow!("cannot allocate overlay pixmap"))?;
    let mut mapped = false;
    let mut stage = Stage::Hidden;
    let mut stage_since = Instant::now();
    let mut rec_since = Instant::now();
    let mut pill_w = pill_width(Stage::Recording);
    let frame = Duration::from_millis(33);
    let mut last_frame = Instant::now();
    let anim_origin = Instant::now();

    loop {
        // Drain stage updates (also detects main-thread exit).
        loop {
            match rx.try_recv() {
                Ok(next) => {
                    if next != stage {
                        if next == Stage::Recording {
                            rec_since = Instant::now();
                        }
                        stage = next;
                        stage_since = Instant::now();
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    conn.destroy_window(win)?;
                    conn.flush()?;
                    return Ok(());
                }
            }
        }

        let target_w = pill_width(stage);
        // Ease width toward the mock's per-state sizes.
        pill_w += (target_w - pill_w) * 0.28;
        if (pill_w - target_w).abs() < 0.5 {
            pill_w = target_w;
        }

        let now = Instant::now();
        let should_draw = (stage != Stage::Hidden && now.duration_since(last_frame) >= frame)
            || (stage == Stage::Hidden && mapped);
        if should_draw {
            if stage == Stage::Hidden {
                conn.unmap_window(win)?;
                conn.flush()?;
                mapped = false;
            } else {
                let anim_t = anim_origin.elapsed().as_secs_f32();
                draw_frame(
                    &mut pixmap,
                    &font,
                    stage,
                    anim_t,
                    pill_w,
                    stage_since,
                    rec_since,
                );
                if !mapped {
                    conn.map_window(win)?;
                    conn.configure_window(
                        win,
                        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                    )?;
                    mapped = true;
                }
                put_argb(&conn, win, gc, &pixmap)?;
            }
            last_frame = now;
        }

        match conn.poll_for_event() {
            Ok(Some(x11rb::protocol::Event::Expose(_))) if mapped && stage != Stage::Hidden => {
                put_argb(&conn, win, gc, &pixmap)?;
            }
            Ok(_) => thread::sleep(Duration::from_millis(if stage == Stage::Hidden {
                50
            } else {
                8
            })),
            Err(e) => return Err(e.into()),
        }
    }
}

fn pill_width(stage: Stage) -> f32 {
    let logical = match stage {
        Stage::Recording => 188.0,
        Stage::Transcribing => 164.0,
        Stage::Done => 118.0,
        Stage::Error => 128.0,
        Stage::Hidden => 118.0,
    };
    logical * SCALE as f32
}

fn load_font() -> anyhow::Result<Font> {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/opentype/inter/Inter-SemiBold.otf",
        "/usr/share/fonts/opentype/inter/Inter-Medium.otf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
                log::debug!("overlay font: {}", Path::new(path).display());
                return Ok(font);
            }
        }
    }
    anyhow::bail!("no usable overlay font (tried Inter + DejaVu)")
}

fn find_argb_visual(
    _conn: &x11rb::rust_connection::RustConnection,
    screen: &x11rb::protocol::xproto::Screen,
) -> Option<(x11rb::protocol::xproto::Visualid, u8)> {
    use x11rb::protocol::xproto::VisualClass;
    for depth in &screen.allowed_depths {
        if depth.depth != 32 {
            continue;
        }
        for vis in &depth.visuals {
            if vis.class == VisualClass::TRUE_COLOR
                && vis.bits_per_rgb_value >= 8
                && vis.red_mask != 0
                && vis.green_mask != 0
                && vis.blue_mask != 0
            {
                return Some((vis.visual_id, depth.depth));
            }
        }
    }
    None
}

fn put_argb<C: x11rb::connection::Connection>(
    conn: &C,
    win: u32,
    gc: u32,
    pixmap: &SkPixmap,
) -> anyhow::Result<()> {
    use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
    // X expects native-endian 32-bit BGRA for ARGB visuals on little-endian.
    let rgba = pixmap.data();
    let mut bgra = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        bgra.push(px[2]);
        bgra.push(px[1]);
        bgra.push(px[0]);
        bgra.push(px[3]);
    }
    conn.put_image(
        ImageFormat::Z_PIXMAP,
        win,
        gc,
        pixmap.width() as u16,
        pixmap.height() as u16,
        0,
        0,
        0,
        32,
        &bgra,
    )?;
    conn.flush()?;
    Ok(())
}

fn draw_frame(
    pixmap: &mut SkPixmap,
    font: &Font,
    stage: Stage,
    anim_t: f32,
    pill_w: f32,
    stage_since: Instant,
    rec_since: Instant,
) {
    pixmap.fill(Color::from_rgba8(0, 0, 0, 0));

    let pill_h = PILL_H;
    let x = (WIN_W as f32 - pill_w) * 0.5;
    let y = (WIN_H as f32 - pill_h) * 0.5;

    // Soft drop shadow under the pill.
    draw_round_rect(
        pixmap,
        x,
        y + 6.0 * SCALE as f32,
        pill_w,
        pill_h,
        pill_h * 0.5,
        Color::from_rgba8(0, 0, 0, 28),
    );
    draw_round_rect(
        pixmap,
        x,
        y + 2.0 * SCALE as f32,
        pill_w,
        pill_h,
        pill_h * 0.5,
        Color::from_rgba8(0, 0, 0, 18),
    );

    // White pill body + hairline border.
    draw_round_rect(
        pixmap,
        x,
        y,
        pill_w,
        pill_h,
        pill_h * 0.5,
        Color::from_rgba8(255, 255, 255, 240),
    );
    stroke_round_rect(
        pixmap,
        x + 0.5,
        y + 0.5,
        pill_w - 1.0,
        pill_h - 1.0,
        pill_h * 0.5,
        Color::from_rgba8(17, 17, 17, 40),
        1.0 * SCALE as f32,
    );

    // Icon circle.
    let ix = x + PAD_X;
    let iy = y + (pill_h - ICON) * 0.5;
    fill_circle(
        pixmap,
        ix + ICON * 0.5,
        iy + ICON * 0.5,
        ICON * 0.5,
        Color::from_rgba8(17, 17, 17, 255),
    );

    match stage {
        Stage::Recording => draw_wave(pixmap, ix, iy, anim_t),
        Stage::Transcribing => draw_spinner(pixmap, ix, iy, anim_t),
        Stage::Done => draw_check(pixmap, ix, iy, stage_since.elapsed().as_secs_f32()),
        Stage::Error => draw_x(pixmap, ix, iy),
        Stage::Hidden => {}
    }

    // Label.
    let label = label(stage);
    let text_x = ix + ICON + GAP;
    let text_size = 13.0 * SCALE as f32;
    draw_text(
        pixmap,
        font,
        label,
        text_x,
        y + pill_h * 0.5,
        text_size,
        Color::from_rgba8(17, 17, 17, 255),
    );

    // Elapsed timer on the live-capture state.
    if stage == Stage::Recording {
        let secs = rec_since.elapsed().as_secs();
        let meta = format!("{}:{:02}", secs / 60, secs % 60);
        let meta_size = 11.0 * SCALE as f32;
        let tw = text_width(font, &meta, meta_size);
        draw_text(
            pixmap,
            font,
            &meta,
            x + pill_w - PAD_X - tw,
            y + pill_h * 0.5,
            meta_size,
            Color::from_rgba8(119, 119, 119, 255),
        );
    }
}

fn draw_wave(pixmap: &mut SkPixmap, ix: f32, iy: f32, t: f32) {
    let cx = ix + ICON * 0.5;
    let cy = iy + ICON * 0.5;
    let heights = [5.0, 10.0, 14.0, 8.0];
    let delays = [-0.4, -0.2, 0.0, -0.3];
    let bar_w = 2.0 * SCALE as f32;
    let gap = 2.0 * SCALE as f32;
    let total = 4.0 * bar_w + 3.0 * gap;
    let left = cx - total * 0.5;
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(255, 255, 255, 255));
    paint.anti_alias = true;
    for (i, (&h, &d)) in heights.iter().zip(delays.iter()).enumerate() {
        let phase = (t + d) * 2.0 * PI;
        let scale = 0.55 + 0.45 * (0.5 + 0.5 * phase.sin());
        let bh = h * SCALE as f32 * scale;
        let x = left + i as f32 * (bar_w + gap);
        let y = cy - bh * 0.5;
        if let Some(rect) = Rect::from_xywh(x, y, bar_w, bh) {
            let mut pb = PathBuilder::new();
            pb.push_rect(rect);
            // rounded bars via thick stroke circle ends — simple rect is fine at 2x
            if let Some(path) = pb.finish() {
                pixmap.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }
    }
}

fn draw_spinner(pixmap: &mut SkPixmap, ix: f32, iy: f32, t: f32) {
    let cx = ix + ICON * 0.5;
    let cy = iy + ICON * 0.5;
    let r = 6.5 * SCALE as f32;
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(255, 255, 255, 90));
    paint.anti_alias = true;
    let stroke = Stroke {
        width: 1.5 * SCALE as f32,
        ..Stroke::default()
    };
    // Track ring.
    if let Some(path) = circle_path(cx, cy, r) {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
    // Sweep arc.
    paint.set_color(Color::from_rgba8(255, 255, 255, 255));
    let ang = t * 2.0 * PI * 1.25;
    if let Some(path) = arc_path(cx, cy, r, ang, ang + 1.7) {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

fn draw_check(pixmap: &mut SkPixmap, ix: f32, iy: f32, age: f32) {
    let cx = ix + ICON * 0.5;
    let cy = iy + ICON * 0.5;
    let s = SCALE as f32;
    // Mock path roughly: M3.2 8.3 L6.6 11.5 L12.9 4.8 in 16x16 viewBox.
    let p0 = (cx - 4.8 * s, cy + 0.3 * s);
    let p1 = (cx - 1.4 * s, cy + 3.5 * s);
    let p2 = (cx + 4.9 * s, cy - 3.2 * s);
    let mut pb = PathBuilder::new();
    pb.move_to(p0.0, p0.1);
    pb.line_to(p1.0, p1.1);
    pb.line_to(p2.0, p2.1);
    let Some(path) = pb.finish() else { return };
    let len = ((p1.0 - p0.0).hypot(p1.1 - p0.1)) + ((p2.0 - p1.0).hypot(p2.1 - p1.1));
    let progress = ((age - 0.08) / 0.45).clamp(0.0, 1.0);
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(255, 255, 255, 255));
    paint.anti_alias = true;
    let stroke = Stroke {
        width: 2.2 * s,
        line_cap: tiny_skia::LineCap::Round,
        line_join: tiny_skia::LineJoin::Round,
        dash: StrokeDash::new(vec![len, len], len * (1.0 - progress)),
        ..Stroke::default()
    };
    if let Some(dash) = stroke.dash.clone() {
        let stroke = Stroke {
            width: 2.2 * s,
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            dash: Some(dash),
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    } else {
        // Fallback: full check if dash construction fails.
        let stroke = Stroke {
            width: 2.2 * s,
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

fn draw_x(pixmap: &mut SkPixmap, ix: f32, iy: f32) {
    let cx = ix + ICON * 0.5;
    let cy = iy + ICON * 0.5;
    let s = 4.2 * SCALE as f32;
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(255, 255, 255, 255));
    paint.anti_alias = true;
    let stroke = Stroke {
        width: 2.0 * SCALE as f32,
        line_cap: tiny_skia::LineCap::Round,
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

fn draw_round_rect(
    pixmap: &mut SkPixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    color: Color,
) {
    let Some(path) = round_rect_path(x, y, w, h, radius) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn stroke_round_rect(
    pixmap: &mut SkPixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    color: Color,
    width: f32,
) {
    let Some(path) = round_rect_path(x, y, w, h, radius) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let stroke = Stroke {
        width,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn round_rect_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<SkPath> {
    let r = radius.min(w * 0.5).min(h * 0.5);
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
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
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

fn text_width(font: &Font, text: &str, size: f32) -> f32 {
    text.chars()
        .map(|ch| font.metrics(ch, size).advance_width)
        .sum()
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
    // Vertically center on center_y using the font ascent/descent of 'H'.
    let metrics = font.horizontal_line_metrics(size);
    let baseline = if let Some(m) = metrics {
        center_y - (m.ascent + m.descent) * 0.5 + m.ascent
    } else {
        center_y + size * 0.35
    };
    let mut pen_x = x;
    let mut paint = PixmapPaint::default();
    paint.opacity = color.alpha();
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size);
        if !bitmap.is_empty() {
            if let Some(mut glyph) = SkPixmap::new(metrics.width.max(1) as u32, metrics.height.max(1) as u32)
            {
                for (i, a) in bitmap.iter().enumerate() {
                    let px = i % metrics.width;
                    let py = i / metrics.width;
                    let alpha = ((*a as f32) / 255.0 * color.alpha() * 255.0).round() as u8;
                    glyph.pixels_mut()[py * metrics.width + px] = tiny_skia::PremultipliedColorU8::from_rgba(
                        ((color.red() * 255.0) as u8).saturating_mul(alpha) / 255,
                        ((color.green() * 255.0) as u8).saturating_mul(alpha) / 255,
                        ((color.blue() * 255.0) as u8).saturating_mul(alpha) / 255,
                        alpha,
                    )
                    .unwrap_or_else(|| tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, 0).unwrap());
                }
                pixmap.draw_pixmap(
                    (pen_x + metrics.xmin as f32).round() as i32,
                    (baseline + metrics.ymin as f32).round() as i32,
                    glyph.as_ref(),
                    &paint,
                    Transform::identity(),
                    None,
                );
            }
        }
        pen_x += metrics.advance_width;
    }
}
