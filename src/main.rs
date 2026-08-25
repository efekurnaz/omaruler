use std::cell::RefCell;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::rc::Rc;

use gtk4::cairo::Context;
use gtk4::prelude::*;
use gtk4::{
    gdk, glib, Application, ApplicationWindow, ContentFit, DrawingArea, EventControllerKey,
    GestureClick, Overlay, Picture,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use image::RgbaImage;

const APP_ID: &str = "sh.omarchy.pixel-snap";
const TOLERANCE_LEVELS: [(u8, &str); 4] = [(0, "Off"), (10, "Low"), (24, "Med"), (48, "High")];
const DEFAULT_TOLERANCE_LEVEL: usize = 1;
const MAX_EXTENT_SCAN: i64 = 2000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Ruler,
    Color,
}

#[derive(Clone, Copy)]
struct Theme {
    accent: (f64, f64, f64),
    foreground: (f64, f64, f64),
    background: (f64, f64, f64),
}

struct MonitorInfo {
    name: String,
    scale: f64,
}

struct State {
    img: RgbaImage,
    grad: Vec<f32>,
    gw: u32,
    gh: u32,
    scale: f64,
    theme: Theme,
    cursor: (f64, f64),
    mode: Mode,
    dragging: bool,
    start: Option<(f64, f64)>,
    snap_enabled: bool,
    tolerance_level: usize,
    show_legend: bool,
    last_message: Option<String>,
}

fn rgba_at(img: &RgbaImage, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let p = img.get_pixel(x, y).0;
    (p[0], p[1], p[2], p[3])
}

fn cursor_pos() -> Option<(f64, f64)> {
    let out = Command::new("hyprctl").args(["cursorpos", "-j"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some((v.get("x")?.as_f64()?, v.get("y")?.as_f64()?))
}

fn active_monitor() -> Option<MonitorInfo> {
    let out = Command::new("hyprctl").args(["monitors", "-j"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let arr = v.as_array()?;

    if let Some((cx, cy)) = cursor_pos() {
        for m in arr {
            let x = m.get("x")?.as_i64().unwrap_or(0) as i32;
            let y = m.get("y")?.as_i64().unwrap_or(0) as i32;
            let width = m.get("width")?.as_i64().unwrap_or(0) as i32;
            let height = m.get("height")?.as_i64().unwrap_or(0) as i32;
            if (cx as i32) >= x && (cx as i32) < x + width && (cy as i32) >= y && (cy as i32) < y + height
            {
                let name = m.get("name")?.as_str()?.to_string();
                let scale = m.get("scale").and_then(|s| s.as_f64()).unwrap_or(1.0);
                return Some(MonitorInfo { name, scale });
            }
        }
    }

    let m = arr.iter().find(|m| m.get("focused").and_then(|f| f.as_bool()).unwrap_or(false))
        .or_else(|| arr.first())?;
    let name = m.get("name")?.as_str()?.to_string();
    let scale = m.get("scale").and_then(|s| s.as_f64()).unwrap_or(1.0);
    Some(MonitorInfo { name, scale })
}

fn capture_monitor(name: &str) -> Option<RgbaImage> {
    let tmp = std::env::temp_dir().join(format!("pixel-snap-{}.png", std::process::id()));
    let status = Command::new("grim").args(["-o", name]).arg(&tmp).status().ok()?;
    if !status.success() {
        return None;
    }
    let img = image::open(&tmp).ok()?.to_rgba8();
    let _ = std::fs::remove_file(&tmp);
    Some(img)
}

fn parse_hex_color(s: &str) -> Option<(f64, f64, f64)> {
    let s = s.trim().trim_start_matches('#');
    if s.len() < 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0))
}

/// Resolves one semantic color from the active Omarchy theme via
/// `omarchy-theme-color`, which handles the alias/fallback cascade that
/// every other theme consumer (templates, tmux, GNOME, ...) shares — so
/// this app follows the same palette as the rest of the desktop instead of
/// hand-parsing colors.toml itself.
fn theme_color_rgb(key: &str, fallback: (f64, f64, f64)) -> (f64, f64, f64) {
    Command::new("omarchy-theme-color")
        .arg(key)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| parse_hex_color(&s))
        .unwrap_or(fallback)
}

fn fetch_theme() -> Theme {
    Theme {
        accent: theme_color_rgb("accent", (1.0, 0.85, 0.2)),
        foreground: theme_color_rgb("foreground", (1.0, 1.0, 1.0)),
        background: theme_color_rgb("background", (0.0, 0.0, 0.0)),
    }
}

fn notify(headline: &str, description: &str) {
    let _ = Command::new("omarchy-notification-send")
        .args(["--app-name", "Pixel ruler", "-u", "low", "-t", "1200", "-r", "48291"])
        .arg(headline)
        .arg(description)
        .spawn();
}

fn compute_gradient(img: &RgbaImage) -> (Vec<f32>, u32, u32) {
    let (w, h) = img.dimensions();
    let gray: Vec<f32> = (0..h)
        .flat_map(|y| {
            (0..w).map(move |x| {
                let (r, g, b, _) = rgba_at(img, x, y);
                0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32
            })
        })
        .collect();

    let idx = |x: i32, y: i32| -> usize {
        let x = x.clamp(0, w as i32 - 1) as u32;
        let y = y.clamp(0, h as i32 - 1) as u32;
        (y * w + x) as usize
    };

    let mut grad = vec![0f32; (w * h) as usize];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let gx = -gray[idx(x - 1, y - 1)] + gray[idx(x + 1, y - 1)]
                - 2.0 * gray[idx(x - 1, y)]
                + 2.0 * gray[idx(x + 1, y)]
                - gray[idx(x - 1, y + 1)]
                + gray[idx(x + 1, y + 1)];
            let gy = -gray[idx(x - 1, y - 1)] - 2.0 * gray[idx(x, y - 1)] - gray[idx(x + 1, y - 1)]
                + gray[idx(x - 1, y + 1)]
                + 2.0 * gray[idx(x, y + 1)]
                + gray[idx(x + 1, y + 1)];
            grad[(y as u32 * w + x as u32) as usize] = (gx * gx + gy * gy).sqrt();
        }
    }
    (grad, w, h)
}

fn snap_point(grad: &[f32], w: u32, h: u32, px: f64, py: f64, radius: i32, threshold: f32) -> (f64, f64) {
    let cx = px.round() as i32;
    let cy = py.round() as i32;
    let mut best: Option<(i32, i32, f32)> = None;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let x = cx + dx;
            let y = cy + dy;
            if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                continue;
            }
            let g = grad[(y as u32 * w + x as u32) as usize];
            if g < threshold {
                continue;
            }
            let dist2 = (dx * dx + dy * dy) as f32;
            let score = g - dist2 * 0.5;
            if best.map_or(true, |(_, _, bs)| score > bs) {
                best = Some((x, y, score));
            }
        }
    }
    match best {
        Some((x, y, _)) => (x as f64, y as f64),
        None => (px, py),
    }
}

/// Walks outward from (px, py) in all 4 directions while neighboring pixels
/// stay within `tol` of the reference color at (px, py), per channel. This is
/// the "how wide is this gap" auto-measurement: hovering over a padding
/// between two differently-colored regions reports the padding's extent
/// without needing to click two points. Each direction is capped at
/// MAX_EXTENT_SCAN steps so a uniform-color background can't turn one
/// motion frame into an unbounded scan.
fn scan_extent(img: &RgbaImage, px: i64, py: i64, tol: u8) -> (i64, i64, i64, i64) {
    let w = img.width() as i64;
    let h = img.height() as i64;
    if px < 0 || py < 0 || px >= w || py >= h {
        return (px, px, py, py);
    }
    let (rr, rg, rb, _) = rgba_at(img, px as u32, py as u32);
    let tol = tol as i32;
    let close = |x: i64, y: i64| -> bool {
        if x < 0 || y < 0 || x >= w || y >= h {
            return false;
        }
        let (r, g, b, _) = rgba_at(img, x as u32, y as u32);
        (r as i32 - rr as i32).abs() <= tol
            && (g as i32 - rg as i32).abs() <= tol
            && (b as i32 - rb as i32).abs() <= tol
    };

    let mut left = px;
    let mut steps = 0;
    while steps < MAX_EXTENT_SCAN && close(left - 1, py) {
        left -= 1;
        steps += 1;
    }
    let mut right = px;
    steps = 0;
    while steps < MAX_EXTENT_SCAN && close(right + 1, py) {
        right += 1;
        steps += 1;
    }
    let mut top = py;
    steps = 0;
    while steps < MAX_EXTENT_SCAN && close(px, top - 1) {
        top -= 1;
        steps += 1;
    }
    let mut bottom = py;
    steps = 0;
    while steps < MAX_EXTENT_SCAN && close(px, bottom + 1) {
        bottom += 1;
        steps += 1;
    }
    (left, right, top, bottom)
}

/// Same walk as `scan_extent`, in the drawing area's logical coordinate
/// space (physical / monitor scale), which is what CSS/devtools-style
/// measurements should be reported in.
fn scan_extent_logical(st: &State, cx: f64, cy: f64) -> (f64, f64, f64, f64) {
    let px = (cx * st.scale).round() as i64;
    let py = (cy * st.scale).round() as i64;
    let (l, r, t, b) = scan_extent(&st.img, px, py, TOLERANCE_LEVELS[st.tolerance_level].0);
    (l as f64 / st.scale, r as f64 / st.scale, t as f64 / st.scale, b as f64 / st.scale)
}

fn pixel_hex(img: &RgbaImage, x: u32, y: u32) -> Option<String> {
    if x >= img.width() || y >= img.height() {
        return None;
    }
    let (r, g, b, _) = rgba_at(img, x, y);
    Some(format!("#{:02X}{:02X}{:02X}", r, g, b))
}

fn copy_to_clipboard(text: &str) {
    if let Ok(mut child) = Command::new("wl-copy").stdin(Stdio::piped()).spawn() {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn find_gdk_monitor(window: &ApplicationWindow, name: &str) -> Option<gdk::Monitor> {
    let display = gtk4::prelude::WidgetExt::display(window);
    let monitors = display.monitors();
    for i in 0..monitors.n_items() {
        let obj = monitors.item(i)?;
        let m = obj.downcast::<gdk::Monitor>().ok()?;
        if m.connector().as_deref() == Some(name) {
            return Some(m);
        }
    }
    None
}

/// Draws a themed label box. `x, y` is the box's top-left corner (not a
/// text baseline), so callers can anchor it directly to a screen position
/// — e.g. offset down-right of the cursor — without reasoning about font
/// metrics.
fn draw_label(cr: &Context, x: f64, y: f64, text: &str, theme: &Theme) {
    let _ = cr.select_font_face(
        "monospace",
        gtk4::cairo::FontSlant::Normal,
        gtk4::cairo::FontWeight::Bold,
    );
    cr.set_font_size(13.0);
    let (tw, th) = cr
        .text_extents(text)
        .map(|e| (e.width(), e.height()))
        .unwrap_or((text.len() as f64 * 8.0, 12.0));
    let pad = 6.0;
    let (bg, fg, ac) = (theme.background, theme.foreground, theme.accent);

    cr.set_source_rgba(bg.0, bg.1, bg.2, 0.85);
    cr.rectangle(x, y, tw + pad * 2.0, th + pad * 2.0);
    let _ = cr.fill();

    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.5);
    cr.set_line_width(1.0);
    cr.rectangle(x, y, tw + pad * 2.0, th + pad * 2.0);
    let _ = cr.stroke();

    cr.set_source_rgba(fg.0, fg.1, fg.2, 1.0);
    cr.move_to(x + pad, y + pad + th);
    let _ = cr.show_text(text);
}

fn draw_loupe(cr: &Context, st: &State, cx: f64, cy: f64, w: i32, _h: i32) {
    let block: i64 = 11;
    let zoom = 9.0;
    let size = block as f64 * zoom;

    let mut lx = cx + 24.0;
    let mut ly = cy - size - 24.0;
    if lx + size > w as f64 {
        lx = cx - size - 24.0;
    }
    if ly < 0.0 {
        ly = cy + 24.0;
    }

    let px = (cx * st.scale).round() as i64;
    let py = (cy * st.scale).round() as i64;
    let half = block / 2;

    let _ = cr.save();
    cr.rectangle(lx, ly, size, size);
    cr.clip();
    cr.set_source_rgba(0.0, 0.0, 0.0, 1.0);
    let _ = cr.paint();
    for j in 0..block {
        for i in 0..block {
            let sx = px - half + i;
            let sy = py - half + j;
            let (r, g, b) = if sx >= 0 && sy >= 0 && (sx as u32) < st.img.width() && (sy as u32) < st.img.height()
            {
                let (r, g, b, _) = rgba_at(&st.img, sx as u32, sy as u32);
                (r, g, b)
            } else {
                (0, 0, 0)
            };
            cr.set_source_rgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
            cr.rectangle(lx + i as f64 * zoom, ly + j as f64 * zoom, zoom, zoom);
            let _ = cr.fill();
        }
    }
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.15);
    cr.set_line_width(1.0);
    for i in 0..=block {
        cr.move_to(lx + i as f64 * zoom, ly);
        cr.line_to(lx + i as f64 * zoom, ly + size);
        let _ = cr.stroke();
        cr.move_to(lx, ly + i as f64 * zoom);
        cr.line_to(lx + size, ly + i as f64 * zoom);
        let _ = cr.stroke();
    }
    let _ = cr.restore();

    let ac = st.theme.accent;
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.9);
    cr.set_line_width(2.0);
    cr.rectangle(lx + half as f64 * zoom, ly + half as f64 * zoom, zoom, zoom);
    let _ = cr.stroke();

    cr.set_source_rgba(1.0, 1.0, 1.0, 0.8);
    cr.set_line_width(1.5);
    cr.rectangle(lx, ly, size, size);
    let _ = cr.stroke();
}

/// Draws the local ticked bars for the auto-detected color-continuity
/// extent (no full-screen guide lines — just the bounded measurement
/// itself) plus a small dot marking the exact reference pixel.
fn draw_extent_bars(cr: &Context, theme: &Theme, cx: f64, cy: f64, l: f64, r: f64, t: f64, b: f64) {
    let ac = theme.accent;
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.95);
    cr.set_line_width(1.5);

    cr.move_to(l, cy);
    cr.line_to(r, cy);
    let _ = cr.stroke();
    for tx in [l, r] {
        cr.move_to(tx, cy - 5.0);
        cr.line_to(tx, cy + 5.0);
        let _ = cr.stroke();
    }

    cr.move_to(cx, t);
    cr.line_to(cx, b);
    let _ = cr.stroke();
    for ty in [t, b] {
        cr.move_to(cx - 5.0, ty);
        cr.line_to(cx + 5.0, ty);
        let _ = cr.stroke();
    }

    cr.set_source_rgba(ac.0, ac.1, ac.2, 1.0);
    cr.arc(cx, cy, 2.5, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
}

fn draw(cr: &Context, _w: i32, h: i32, st: &State) {
    // The screenshot itself is painted by a GdkTexture-backed Picture widget
    // underneath this transparent DrawingArea (composited by GSK, effectively
    // free per frame). This draw_func never touches the full viewport — every
    // shape below is sized to its local content — which is what keeps cost
    // independent of screen resolution and the redraw fast enough to track
    // the cursor without lag.
    let (cx, cy) = st.cursor;

    match st.mode {
        Mode::Ruler => {
            let size_text;
            if let Some(start) = st.start {
                let ac = st.theme.accent;
                cr.set_source_rgba(ac.0, ac.1, ac.2, 0.95);
                cr.set_line_width(1.5);
                cr.move_to(start.0, start.1);
                cr.line_to(cx, cy);
                let _ = cr.stroke();
                for (px, py) in [start, (cx, cy)] {
                    cr.arc(px, py, 3.0, 0.0, std::f64::consts::TAU);
                    let _ = cr.fill();
                }
                let dx = cx - start.0;
                let dy = cy - start.1;
                let dist = (dx * dx + dy * dy).sqrt();
                size_text = format!("{:.0}px  ({:.0} x {:.0})", dist, dx.abs(), dy.abs());
            } else {
                let (l, r, t, b) = scan_extent_logical(st, cx, cy);
                draw_extent_bars(cr, &st.theme, cx, cy, l, r, t, b);
                size_text = format!("{:.0} x {:.0}", r - l, b - t);
            }
            // Size readout in a box offset down-right of the cursor, out of
            // the way of the measuring lines themselves.
            draw_label(cr, cx + 18.0, cy + 18.0, &size_text, &st.theme);

            if st.show_legend {
                let (_, tol_name) = TOLERANCE_LEVELS[st.tolerance_level];
                let legend = format!(
                    "tolerance: {}  ·  t: cycle  ·  drag: manual measure  ·  r: reset  ·  c: color  ·  s: snap {}  ·  l: hide legend  ·  esc: quit",
                    tol_name,
                    if st.snap_enabled { "on" } else { "off" }
                );
                draw_label(cr, 16.0, h as f64 - 38.0, &legend, &st.theme);
            }
        }
        Mode::Color => {
            draw_loupe(cr, st, cx, cy, _w, h);
            let px = (cx * st.scale).round() as u32;
            let py = (cy * st.scale).round() as u32;
            if let Some(hex) = pixel_hex(&st.img, px, py) {
                draw_label(cr, cx + 18.0, cy + 18.0, &format!("{}  ·  click to copy  ·  esc: quit", hex), &st.theme);
            }
        }
    }

    if let Some(msg) = &st.last_message {
        draw_label(cr, 16.0, h as f64 - 64.0, msg, &st.theme);
    }
}

fn build_ui(app: &Application) {
    let Some(monitor) = active_monitor() else {
        eprintln!("pixel-snap: could not read monitor list from hyprctl");
        std::process::exit(1);
    };
    let Some(img) = capture_monitor(&monitor.name) else {
        eprintln!("pixel-snap: grim capture failed for output {}", monitor.name);
        std::process::exit(1);
    };
    let (grad, gw, gh) = compute_gradient(&img);
    let theme = fetch_theme();

    let texture = gdk::MemoryTexture::new(
        img.width() as i32,
        img.height() as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from(img.as_raw().as_slice()),
        img.width() as usize * 4,
    );

    let window = ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .title("pixel-snap")
        .build();

    window.init_layer_shell();
    window.set_namespace(Some(APP_ID));
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, true);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Bottom, true);
    window.set_exclusive_zone(-1);

    if let Some(gdk_monitor) = find_gdk_monitor(&window, &monitor.name) {
        window.set_monitor(Some(&gdk_monitor));
    }

    let state = Rc::new(RefCell::new(State {
        img,
        grad,
        gw,
        gh,
        scale: monitor.scale,
        theme,
        cursor: (0.0, 0.0),
        mode: Mode::Ruler,
        dragging: false,
        start: None,
        snap_enabled: true,
        tolerance_level: DEFAULT_TOLERANCE_LEVEL,
        show_legend: true,
        last_message: None,
    }));

    let picture = Picture::for_paintable(&texture);
    picture.set_content_fit(ContentFit::Fill);
    picture.set_can_shrink(true);
    picture.set_can_target(false);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_halign(gtk4::Align::Fill);
    picture.set_valign(gtk4::Align::Fill);

    let area = DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    area.set_halign(gtk4::Align::Fill);
    area.set_valign(gtk4::Align::Fill);
    area.set_can_focus(true);

    {
        let state = state.clone();
        area.set_draw_func(move |_area, cr, w, h| {
            draw(cr, w, h, &state.borrow());
        });
    }

    let overlay = Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&area);

    // Rather than reacting to Wayland motion events (which can be batched or
    // delivered a frame late), poll the seat's live pointer position once
    // per compositor frame via the frame clock. This decouples our redraw
    // from event delivery entirely: every frame draws wherever the pointer
    // truly is *right now*, which is what actually removes the perceived
    // lag rather than just making the redraw itself cheaper.
    {
        let state = state.clone();
        area.add_tick_callback(move |area, _clock| {
            if let Some(surface) = area.native().and_then(|n| n.surface()) {
                let display = gtk4::prelude::WidgetExt::display(area);
                if let Some(device) = display.default_seat().and_then(|s| s.pointer()) {
                    if let Some((x, y, _mask)) = surface.device_position(&device) {
                        let mut st = state.borrow_mut();
                        let cursor = if st.snap_enabled {
                            let px = x * st.scale;
                            let py = y * st.scale;
                            let (snx, sny) = snap_point(&st.grad, st.gw, st.gh, px, py, 6, 40.0);
                            (snx / st.scale, sny / st.scale)
                        } else {
                            (x, y)
                        };
                        if st.cursor != cursor {
                            st.cursor = cursor;
                            drop(st);
                            area.queue_draw();
                        }
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    let click = GestureClick::new();
    click.set_button(1);
    {
        let state = state.clone();
        let area = area.clone();
        click.connect_pressed(move |_g, _n, _x, _y| {
            let mut st = state.borrow_mut();
            match st.mode {
                Mode::Ruler => {
                    st.start = Some(st.cursor);
                    st.dragging = true;
                }
                Mode::Color => {
                    let px = (st.cursor.0 * st.scale).round() as u32;
                    let py = (st.cursor.1 * st.scale).round() as u32;
                    if let Some(hex) = pixel_hex(&st.img, px, py) {
                        copy_to_clipboard(&hex);
                        st.last_message = Some(format!("copied {}", hex));
                    }
                }
            }
            drop(st);
            area.queue_draw();
        });
    }
    {
        let state = state.clone();
        let area = area.clone();
        click.connect_released(move |_g, _n, _x, _y| {
            let mut st = state.borrow_mut();
            if st.mode == Mode::Ruler && st.dragging {
                st.dragging = false;
                if let Some(start) = st.start {
                    let dx = st.cursor.0 - start.0;
                    let dy = st.cursor.1 - start.1;
                    let dist = (dx * dx + dy * dy).sqrt().round();
                    let text = format!("{}px  (dx {:.0}, dy {:.0})", dist, dx, dy);
                    copy_to_clipboard(&text);
                    st.last_message = Some(format!("copied: {}", text));
                }
            }
            drop(st);
            area.queue_draw();
        });
    }
    area.add_controller(click);

    let key = EventControllerKey::new();
    {
        let state = state.clone();
        let window = window.clone();
        let area = area.clone();
        key.connect_key_pressed(move |_c, keyval, _keycode, _modifier| {
            match keyval {
                gdk::Key::Escape => {
                    window.close();
                    glib::Propagation::Stop
                }
                gdk::Key::c | gdk::Key::C => {
                    let mut st = state.borrow_mut();
                    st.mode = if st.mode == Mode::Ruler { Mode::Color } else { Mode::Ruler };
                    st.dragging = false;
                    st.start = None;
                    st.last_message = None;
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::s | gdk::Key::S => {
                    let mut st = state.borrow_mut();
                    st.snap_enabled = !st.snap_enabled;
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::r | gdk::Key::R => {
                    let mut st = state.borrow_mut();
                    st.start = None;
                    st.dragging = false;
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::t | gdk::Key::T => {
                    let mut st = state.borrow_mut();
                    st.tolerance_level = (st.tolerance_level + 1) % TOLERANCE_LEVELS.len();
                    let name = TOLERANCE_LEVELS[st.tolerance_level].1;
                    drop(st);
                    notify("Tolerance", name);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::l | gdk::Key::L => {
                    let mut st = state.borrow_mut();
                    st.show_legend = !st.show_legend;
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
    }
    window.add_controller(key);

    window.set_child(Some(&overlay));
    window.present();
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}
