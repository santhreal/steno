//! Animated status pill: a small borderless ARGB window at the bottom
//! center of the primary monitor. Pixel copy of the product mock.
//!
//! Pure display: override-redirect, takes no focus. Cosmetic and
//! fail-open: no DISPLAY / no ARGB visual / any X error simply disables
//! the overlay — dictation itself is unaffected.
//!
//! # Embedders
//!
//! Prefer [`create`] + [`OverlayBackend`] (`Box<dyn OverlayBackend>`) so a
//! host can swap the built-in pill for [`NullOverlay`] or its own loading
//! UI. [`Overlay::start`] remains for existing callers; migrate when ready.

use std::f32::consts::PI;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use dictate_core::config::UiConfig;
use dictate_core::overlay::{NullOverlay, OverlayBackend, Stage};
use fontdue::{Font, FontSettings};
use tiny_skia::{
    Color, FillRule, Paint, Path as SkPath, PathBuilder, Pixmap as SkPixmap, PixmapPaint,
    PremultipliedColorU8, Stroke, StrokeDash, Transform,
};

/// Build an overlay from [`UiConfig`].
///
/// - `overlay = false` → [`NullOverlay`]
/// - `theme` of `"null"` / `"none"` / `"off"` → [`NullOverlay`]
/// - `"pill"`, empty, or unknown → X11 [`Overlay`] (unknown logs a warning)
pub fn create(cfg: &UiConfig) -> Box<dyn OverlayBackend> {
    if !cfg.overlay {
        return Box::new(NullOverlay);
    }
    match cfg.theme.as_str() {
        "null" | "none" | "off" => Box::new(NullOverlay),
        "pill" | "" => Box::new(Overlay::start(cfg)),
        other => {
            log::warn!("unknown ui.theme {other:?}; falling back to pill");
            Box::new(Overlay::start(cfg))
        }
    }
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

impl OverlayBackend for Overlay {
    fn set(&self, stage: Stage) {
        Overlay::set(self, stage);
    }

    fn flash(&self, ms: u64) {
        Overlay::flash(self, ms);
    }

    fn active(&self) -> bool {
        Overlay::active(self)
    }
}

// Dropping the Overlay closes the channel; the thread notices and
// destroys the window before exiting.

/// Logical (CSS px) design metrics straight from the mock.
mod mock {
    pub const WIN_W: f32 = 268.0; // pill 188 + 40 shadow bleed each side
    pub const WIN_H: f32 = 126.0; // 24 top + 46 pill + 56 shadow bleed
    pub const TOP_PAD: f32 = 24.0;
    pub const PILL_H: f32 = 46.0;
    pub const ICON: f32 = 26.0;
    pub const PAD_X: f32 = 16.0;
    pub const GAP: f32 = 12.0;
    pub const LABEL_PX: f32 = 13.0;
    pub const META_PX: f32 = 11.0;
    pub const BOTTOM_MARGIN: f32 = 48.0; // gap above the screen edge
    pub const SHADOW_DY: f32 = 12.0;
    pub const SHADOW_BLUR: f32 = 12.0; // box-blur radius ≈ css 34px blur
    pub const SHADOW_ALPHA: u8 = 28; // rgba(0,0,0,.11)
}

/// Device-space geometry: logical metrics × the display scale factor.
#[derive(Debug, Clone, Copy)]
struct Geo {
    s: f32,
    win_w: u32,
    win_h: u32,
}

impl Geo {
    fn new(s: f32) -> Self {
        Self {
            s,
            win_w: (mock::WIN_W * s).round() as u32,
            win_h: (mock::WIN_H * s).round() as u32,
        }
    }
    fn l(&self, logical: f32) -> f32 {
        logical * self.s
    }
}

fn pill_width(stage: Stage, geo: &Geo) -> f32 {
    let logical = match stage {
        Stage::Recording => 188.0,
        Stage::Transcribing => 164.0,
        Stage::Done => 118.0,
        Stage::Error => 128.0,
        Stage::Hidden => 118.0,
    };
    geo.l(logical)
}

/// Display scale factor from the X resource database (Xft.dpi / 96).
/// Defaults to 1.0 — rendering twice as big on a 1x display was the
/// "massive" bug; never guess 2x.
fn detect_scale<C: x11rb::connection::Connection>(conn: &C, root: u32) -> f32 {
    use x11rb::protocol::xproto::*;
    let reply = conn
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            8192,
        )
        .ok()
        .and_then(|c| c.reply().ok());
    if let Some(reply) = reply {
        let text = String::from_utf8_lossy(&reply.value);
        for line in text.lines() {
            if let Some(v) = line.trim().strip_prefix("Xft.dpi:") {
                if let Ok(dpi) = v.trim().parse::<f32>() {
                    if dpi.is_finite() && dpi > 0.0 {
                        return (dpi / 96.0).clamp(1.0, 2.0);
                    }
                }
            }
        }
    }
    1.0
}

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
    let c = randr::get_crtc_info(conn, info.crtc, 0).ok()?.reply().ok()?;
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

fn run_inner(rx: &Receiver<Stage>) -> anyhow::Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    use x11rb::wrapper::ConnectionExt as _;

    let font = load_font()?;
    let (conn, screen_num) = x11rb::rust_connection::RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let (visual, depth) = find_argb_visual(&conn, screen)
        .ok_or_else(|| anyhow::anyhow!("no 32-bit ARGB visual — compositor required for pill"))?;
    // Without a compositor the ARGB window renders as an opaque black
    // box. Check the compositor selection owner and refuse instead.
    let cm_atom = conn
        .intern_atom(false, format!("_NET_WM_CM_S{screen_num}").as_bytes())?
        .reply()?
        .atom;
    if conn
        .get_selection_owner(cm_atom)?
        .reply()?
        .owner
        == x11rb::NONE
    {
        anyhow::bail!(
            "no compositor owns _NET_WM_CM_S{screen_num} — the pill needs one (e.g. picom) or it renders as an opaque box"
        );
    }

    let geo = Geo::new(detect_scale(&conn, screen.root));
    let (ox, oy, ow, oh) = primary_rect(&conn, screen.root).unwrap_or((
        0,
        0,
        i32::from(screen.width_in_pixels),
        i32::from(screen.height_in_pixels),
    ));
    let x = (ox + (ow - geo.win_w as i32) / 2).clamp(0, i32::from(i16::MAX)) as i16;
    let y = (oy + oh - geo.win_h as i32 - geo.l(mock::BOTTOM_MARGIN) as i32)
        .clamp(0, i32::from(i16::MAX)) as i16;

    let colormap = conn.generate_id()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)?;

    let win = conn.generate_id()?;
    conn.create_window(
        depth,
        win,
        screen.root,
        x,
        y,
        geo.win_w as u16,
        geo.win_h as u16,
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
    // Click-through: empty INPUT shape. Without this the (mostly
    // transparent) window still swallows every click in its rectangle.
    x11rb::protocol::shape::rectangles(
        &conn,
        x11rb::protocol::shape::SO::SET,
        x11rb::protocol::shape::SK::INPUT,
        ClipOrdering::UNSORTED,
        win,
        0,
        0,
        &[],
    )?
    .check()?;

    let gc = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0))?;

    let mut pixmap = SkPixmap::new(geo.win_w, geo.win_h)
        .ok_or_else(|| anyhow::anyhow!("cannot allocate overlay pixmap"))?;
    let mut shadow_mask = SkPixmap::new(geo.win_w, geo.win_h)
        .ok_or_else(|| anyhow::anyhow!("cannot allocate shadow mask"))?;
    // Reused across frames (ReviewOverlayX #4): no per-frame allocation.
    let mut bgra: Vec<u8> = Vec::with_capacity((geo.win_w * geo.win_h * 4) as usize);
    let mut mapped = false;
    let mut stage = Stage::Hidden;
    let mut stage_since = Instant::now();
    let mut rec_since = Instant::now();
    let mut pill_w = pill_width(Stage::Recording, &geo);
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

        let target_w = pill_width(stage, &geo);
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
                draw_frame(
                    &mut pixmap,
                    &mut shadow_mask,
                    &font,
                    stage,
                    anim_origin.elapsed().as_secs_f32(),
                    pill_w,
                    stage_since.elapsed().as_secs_f32(),
                    rec_since.elapsed().as_secs(),
                    &geo,
                );
                if !mapped {
                    conn.map_window(win)?;
                    conn.configure_window(
                        win,
                        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                    )?;
                    mapped = true;
                }
                put_argb(&conn, win, gc, &pixmap, &mut bgra)?;
            }
            last_frame = now;
        }

        match conn.poll_for_event() {
            Ok(Some(x11rb::protocol::Event::Expose(_))) if mapped && stage != Stage::Hidden => {
                put_argb(&conn, win, gc, &pixmap, &mut bgra)?;
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
    scratch: &mut Vec<u8>,
) -> anyhow::Result<()> {
    use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
    // X expects native-endian 32-bit BGRA for ARGB visuals on little-endian.
    let rgba = pixmap.data();
    let width = pixmap.width() as u16;
    let height = pixmap.height() as u16;
    // Core protocol caps one request at maximum_request_length 4-byte
    // units (often 65535 → 262140 bytes). A full 2x frame exceeds that,
    // so upload in horizontal bands (ReviewOverlayX #5).
    let max_payload = (u64::from(conn.setup().maximum_request_length) * 4)
        .saturating_sub(64)
        .min(262_140) as usize;
    let row_bytes = width as usize * 4;
    let band_rows = (max_payload / row_bytes).max(1).min(height as usize);
    let mut y = 0usize;
    while y < height as usize {
        let rows = band_rows.min(height as usize - y);
        scratch.clear();
        scratch.reserve(rows * row_bytes);
        for px in rgba[y * row_bytes..(y + rows) * row_bytes].chunks_exact(4) {
            scratch.push(px[2]);
            scratch.push(px[1]);
            scratch.push(px[0]);
            scratch.push(px[3]);
        }
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            win,
            gc,
            width,
            rows as u16,
            0,
            y as i16,
            0,
            32,
            scratch,
        )?;
        y += rows;
    }
    conn.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn draw_frame(
    pixmap: &mut SkPixmap,
    shadow_mask: &mut SkPixmap,
    font: &Font,
    stage: Stage,
    anim_t: f32,
    pill_w: f32,
    stage_age: f32,
    rec_secs: u64,
    geo: &Geo,
) {
    pixmap.fill(Color::from_rgba8(0, 0, 0, 0));

    let pill_h = geo.l(mock::PILL_H);
    let x = (geo.win_w as f32 - pill_w) * 0.5;
    let y = geo.l(mock::TOP_PAD);

    // Brief scale(.97→1) pulse on every state change (mock transition cue).
    let pulse = if stage_age < 0.18 {
        let t = stage_age / 0.18;
        let e = 1.0 - (1.0 - t).powi(3); // ease-out cubic
        0.97 + 0.03 * e
    } else {
        1.0
    };
    let cx = x + pill_w * 0.5;
    let cy = y + pill_h * 0.5;
    let pw = pill_w * pulse;
    let ph = pill_h * pulse;
    let px = cx - pw * 0.5;
    let py = cy - ph * 0.5;

    draw_shadow(pixmap, shadow_mask, px, py, pw, ph, geo);

    // White pill body rgba(255,255,255,.94) + hairline border.
    draw_round_rect(
        pixmap,
        px,
        py,
        pw,
        ph,
        ph * 0.5,
        Color::from_rgba8(255, 255, 255, 240),
    );
    stroke_round_rect(
        pixmap,
        px + 0.5 * geo.s,
        py + 0.5 * geo.s,
        pw - 1.0 * geo.s,
        ph - 1.0 * geo.s,
        ph * 0.5,
        Color::from_rgba8(17, 17, 17, 41), // rgba(17,17,17,.16)
        1.0 * geo.s,
    );

    // Icon disc.
    let icon = geo.l(mock::ICON) * pulse;
    let pad_x = geo.l(mock::PAD_X) * pulse;
    let gap = geo.l(mock::GAP) * pulse;
    let ix = px + pad_x;
    let iy = py + (ph - icon) * 0.5;
    fill_circle(
        pixmap,
        ix + icon * 0.5,
        iy + icon * 0.5,
        icon * 0.5,
        Color::from_rgba8(17, 17, 17, 255),
    );

    match stage {
        Stage::Recording => draw_wave(pixmap, ix, iy, icon, anim_t),
        Stage::Transcribing => draw_spinner(pixmap, ix, iy, icon, anim_t),
        Stage::Done => draw_check(pixmap, ix, iy, icon, stage_age),
        Stage::Error => draw_x(pixmap, ix, iy, icon),
        Stage::Hidden => {}
    }

    // Label.
    let text_x = ix + icon + gap;
    draw_text(
        pixmap,
        font,
        label(stage),
        text_x,
        py + ph * 0.5,
        geo.l(mock::LABEL_PX) * pulse,
        Color::from_rgba8(17, 17, 17, 255),
    );

    // Elapsed timer on the live-capture state.
    if stage == Stage::Recording {
        let meta = format!("{}:{:02}", rec_secs / 60, rec_secs % 60);
        let meta_size = geo.l(mock::META_PX) * pulse;
        let tw = text_width(font, &meta, meta_size);
        draw_text(
            pixmap,
            font,
            &meta,
            px + pw - pad_x - tw,
            py + ph * 0.5,
            meta_size,
            Color::from_rgba8(119, 119, 119, 255),
        );
    }
}

/// The mock's `0 12px 34px rgba(0,0,0,.11)` drop shadow: a rounded-rect
/// alpha mask, box-blurred (3 passes ≈ gaussian), tinted black.
fn draw_shadow(
    pixmap: &mut SkPixmap,
    mask: &mut SkPixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    geo: &Geo,
) {
    mask.fill(Color::from_rgba8(0, 0, 0, 0));
    draw_round_rect(
        mask,
        x,
        y + geo.l(mock::SHADOW_DY),
        w,
        h,
        h * 0.5,
        Color::from_rgba8(0, 0, 0, mock::SHADOW_ALPHA),
    );
    box_blur_alpha(mask, geo.l(mock::SHADOW_BLUR).max(1.0) as u32);
    pixmap.draw_pixmap(
        0,
        0,
        mask.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
}

/// Separable box blur over the alpha channel (premultiplied black, so all
/// channels scale with alpha), 3 passes ≈ gaussian. Prefix-sum windows
/// with constant divisor: edges fade to transparent, no lopsidedness.
fn box_blur_alpha(pm: &mut SkPixmap, radius: u32) {
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    if radius == 0 || w < 2 || h < 2 {
        return;
    }
    let mut buf: Vec<u32> = pm.pixels().iter().map(|p| p.alpha() as u32).collect();
    let mut tmp = vec![0u32; buf.len()];
    let r = radius as usize;
    let n = (2 * r + 1) as u32;
    let mut ps = vec![0u32; w.max(h) + 1];
    for _ in 0..3 {
        // Horizontal.
        for row in 0..h {
            let base = row * w;
            ps[0] = 0;
            for i in 0..w {
                ps[i + 1] = ps[i] + buf[base + i];
            }
            for col in 0..w {
                let lo = col.saturating_sub(r);
                let hi = (col + r).min(w - 1);
                tmp[base + col] = (ps[hi + 1] - ps[lo]) / n;
            }
        }
        // Vertical.
        for col in 0..w {
            ps[0] = 0;
            for i in 0..h {
                ps[i + 1] = ps[i] + tmp[i * w + col];
            }
            for row in 0..h {
                let lo = row.saturating_sub(r);
                let hi = (row + r).min(h - 1);
                buf[row * w + col] = (ps[hi + 1] - ps[lo]) / n;
            }
        }
    }
    for (px, a) in pm.pixels_mut().iter_mut().zip(buf.iter()) {
        *px = PremultipliedColorU8::from_rgba(0, 0, 0, (*a).min(255) as u8)
            .unwrap_or_else(|| PremultipliedColorU8::from_rgba(0, 0, 0, 0).unwrap());
    }
}

fn draw_wave(pixmap: &mut SkPixmap, ix: f32, iy: f32, icon: f32, t: f32) {
    let cx = ix + icon * 0.5;
    let cy = iy + icon * 0.5;
    let u = icon / mock::ICON; // device px per logical px
    let heights = [5.0, 10.0, 14.0, 8.0];
    let delays = [-0.4, -0.2, 0.0, -0.3];
    let bar_w = 2.0 * u;
    let gap = 2.0 * u;
    let total = 4.0 * bar_w + 3.0 * gap;
    let left = cx - total * 0.5;
    for (i, (&h, &d)) in heights.iter().zip(delays.iter()).enumerate() {
        let phase = ((t + d) * 2.0 * PI).sin() * 0.5 + 0.5; // 0..1
        let scale = 0.55 + 0.60 * phase; // mock: scaleY .55↔1.15
        let alpha = (0.65 + 0.35 * phase) * 255.0; // mock: opacity .65↔1
        let bh = (h * u * scale).max(bar_w);
        let x = left + i as f32 * (bar_w + gap);
        let y = cy - bh * 0.5;
        // Mock bars are fully rounded (radius 99px).
        draw_round_rect(
            pixmap,
            x,
            y,
            bar_w,
            bh,
            bar_w * 0.5,
            Color::from_rgba8(255, 255, 255, alpha.round() as u8),
        );
    }
}

fn draw_spinner(pixmap: &mut SkPixmap, ix: f32, iy: f32, icon: f32, t: f32) {
    let cx = ix + icon * 0.5;
    let cy = iy + icon * 0.5;
    let u = icon / mock::ICON;
    // 13px box, 1.5px border → centerline radius 5.75.
    let r = 5.75 * u;
    let stroke = Stroke {
        width: 1.5 * u,
        ..Stroke::default()
    };
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    // Track ring rgba(255,255,255,.35).
    paint.set_color(Color::from_rgba8(255, 255, 255, 89));
    if let Some(path) = circle_path(cx, cy, r) {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
    // White quarter arc, .8s per revolution.
    paint.set_color(Color::from_rgba8(255, 255, 255, 255));
    let ang = t * 2.0 * PI * 1.25 - PI * 0.5; // start at the top
    if let Some(path) = arc_path(cx, cy, r, ang, ang + PI * 0.5) {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

fn draw_check(pixmap: &mut SkPixmap, ix: f32, iy: f32, icon: f32, age: f32) {
    let cx = ix + icon * 0.5;
    let cy = iy + icon * 0.5;
    // Mock: 16-unit viewBox (M3.2 8.3 L6.6 11.5 L12.9 4.8, stroke 2.2)
    // mapped onto a 13px glyph box.
    let k = 13.0 * (icon / mock::ICON) / 16.0;
    let p0 = (cx - 4.8 * k, cy + 0.3 * k);
    let p1 = (cx - 1.4 * k, cy + 3.5 * k);
    let p2 = (cx + 4.9 * k, cy - 3.2 * k);
    let mut pb = PathBuilder::new();
    pb.move_to(p0.0, p0.1);
    pb.line_to(p1.0, p1.1);
    pb.line_to(p2.0, p2.1);
    let Some(path) = pb.finish() else { return };
    let len = (p1.0 - p0.0).hypot(p1.1 - p0.1) + (p2.0 - p1.0).hypot(p2.1 - p1.1);
    let progress = ((age - 0.08) / 0.45).clamp(0.0, 1.0);
    // At progress 0 the dash gap swallows the whole path; tiny-skia
    // fails to dash an empty result and logs "path dashing failed".
    if progress <= 0.0 {
        return;
    }
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(Color::from_rgba8(255, 255, 255, 255));
    let dash = if progress >= 1.0 {
        None // fully drawn: plain stroke
    } else {
        StrokeDash::new(vec![len.max(0.01), len.max(0.01)], len * (1.0 - progress))
    };
    let stroke = Stroke {
        width: 2.2 * k,
        line_cap: tiny_skia::LineCap::Round,
        line_join: tiny_skia::LineJoin::Round,
        dash,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn draw_x(pixmap: &mut SkPixmap, ix: f32, iy: f32, icon: f32) {
    let cx = ix + icon * 0.5;
    let cy = iy + icon * 0.5;
    let u = icon / mock::ICON;
    let s = 4.2 * u;
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(Color::from_rgba8(255, 255, 255, 255));
    let stroke = Stroke {
        width: 2.0 * u,
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
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(color);
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

#[allow(clippy::too_many_arguments)]
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
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(color);
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
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(color);
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
                    // coverage × color alpha → final alpha; premultiply rgb
                    // with u16 math (u8 saturating_mul truncated dark text).
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
                    // fontdue: ymin is the bitmap's BOTTOM edge in y-up
                    // outline space; convert to a y-down top edge.
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

#[cfg(test)]
mod backend_tests {
    use super::*;

    #[test]
    fn null_overlay_methods_do_not_panic() {
        let n = NullOverlay;
        n.set(Stage::Recording);
        n.set(Stage::Transcribing);
        n.set(Stage::Done);
        n.set(Stage::Error);
        n.set(Stage::Hidden);
        n.flash(0);
        n.flash(1);
        assert!(!n.active());
    }

    #[test]
    fn ui_config_default_theme_is_pill() {
        let cfg = UiConfig::default();
        assert_eq!(cfg.theme, "pill");
        assert!(cfg.overlay);
        assert_eq!(cfg.done_flash_ms, 1200);
    }

    #[test]
    fn create_with_overlay_false_is_null() {
        let cfg = UiConfig {
            overlay: false,
            ..UiConfig::default()
        };
        let ov = create(&cfg);
        ov.set(Stage::Recording);
        ov.flash(0);
        assert!(!ov.active());
    }

    #[test]
    fn create_theme_null_aliases_are_null() {
        for theme in ["null", "none", "off"] {
            let cfg = UiConfig {
                theme: theme.to_string(),
                ..UiConfig::default()
            };
            let ov = create(&cfg);
            ov.set(Stage::Done);
            assert!(!ov.active(), "theme {theme}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render every mock state to PNG for off-screen visual review.
    /// Writes only when DICTATE_DUMP_DIR is set: pure CPU, no X, no mic.
    #[test]
    fn dump_mock_frames() {
        let Ok(dir) = std::env::var("DICTATE_DUMP_DIR") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();
        let font = load_font().unwrap();
        eprintln!("dump font: {:?}", font.name());
        for s in [1.0f32, 2.0] {
            let geo = Geo::new(s);
            for (name, stage, age) in [
                ("recording", Stage::Recording, 0.5f32),
                ("processing", Stage::Transcribing, 0.5),
                ("done", Stage::Done, 1.0),
                ("done-draw", Stage::Done, 0.2),
                ("error", Stage::Error, 0.5),
            ] {
                let mut pm = SkPixmap::new(geo.win_w, geo.win_h).unwrap();
                let mut mask = SkPixmap::new(geo.win_w, geo.win_h).unwrap();
                draw_frame(
                    &mut pm,
                    &mut mask,
                    &font,
                    stage,
                    0.35,
                    pill_width(stage, &geo),
                    age,
                    8,
                    &geo,
                );
                write_png(&format!("{dir}/{name}-{s}x.png"), &pm);
            }
        }
    }

    fn write_png(path: &str, pm: &SkPixmap) {
        // Unpremultiply for PNG's straight alpha.
        let mut rgba = Vec::with_capacity(pm.data().len());
        for px in pm.data().chunks_exact(4) {
            let a = px[3] as u32;
            let un = |c: u8| ((c as u32 * 255 + a / 2).checked_div(a).unwrap_or(0)).min(255) as u8;
            if a == 0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                rgba.push(un(px[0]));
                rgba.push(un(px[1]));
                rgba.push(un(px[2]));
                rgba.push(px[3]);
            }
        }
        let (w, h) = (pm.width(), pm.height());
        let mut raw = Vec::with_capacity((w as usize + 1) * h as usize);
        for row in 0..h as usize {
            raw.push(0); // filter: none
            raw.extend_from_slice(&rgba[row * w as usize * 4..(row + 1) * w as usize * 4]);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
        png_chunk(&mut out, b"IHDR", &ihdr);
        // zlib stream with stored (uncompressed) deflate blocks.
        let mut z = vec![0x78, 0x01];
        let chunks: Vec<&[u8]> = raw.chunks(65535).collect();
        for (i, block) in chunks.iter().enumerate() {
            z.push(if i + 1 == chunks.len() { 1 } else { 0 });
            z.extend_from_slice(&(block.len() as u16).to_le_bytes());
            z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
            z.extend_from_slice(block);
        }
        z.extend_from_slice(&adler32(&raw).to_be_bytes());
        png_chunk(&mut out, b"IDAT", &z);
        png_chunk(&mut out, b"IEND", &[]);
        std::fs::write(path, &out).unwrap();
    }

    fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_data = Vec::with_capacity(4 + data.len());
        crc_data.extend_from_slice(kind);
        crc_data.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_data).to_be_bytes());
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &x in data {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }
}

#[cfg(test)]
mod probe {
    use super::*;

    /// Regression: glyphs must land centered on center_y. fontdue's ymin
    /// is the bitmap's bottom edge in y-up outline space — using it as a
    /// y-down top edge shifted every label ~its cap height downward
    /// (caught by review; verified by this probe).
    #[test]
    fn glyph_placement_probe() {
        let font = load_font().unwrap();
        let size = 26.0_f32;
        let mut pm = SkPixmap::new(200, 200).unwrap();
        draw_text(
            &mut pm,
            &font,
            "Tg",
            20.0,
            100.0,
            size,
            Color::from_rgba8(0, 0, 0, 255),
        );
        let mut top = 200u32;
        let mut bottom = 0u32;
        for (i, px) in pm.pixels().iter().enumerate() {
            if px.alpha() > 0 {
                let y = (i / 200) as u32;
                top = top.min(y);
                bottom = bottom.max(y);
            }
        }
        assert!(top > 0 && bottom > top, "no ink rendered");
        let center = (top + bottom) as f32 / 2.0;
        assert!(
            (center - 100.0).abs() <= 4.0,
            "text center {center} is more than 4px from center_y=100 (rows {top}..={bottom})"
        );
    }
}
