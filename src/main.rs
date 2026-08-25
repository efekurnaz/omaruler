use std::cell::RefCell;
use std::collections::HashMap;
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

const APP_NAME: &str = "omaruler";
const APP_ID: &str = "sh.omarchy.omaruler";
const TOLERANCE_LEVELS: [(u8, &str); 4] = [(0, "Off"), (10, "Low"), (24, "Med"), (48, "High")];
const DEFAULT_TOLERANCE_LEVEL: usize = 3;
const MAX_EXTENT_SCAN: i64 = 2000;
const RATIO_MAX: u32 = 21;
const RATIO_HIDE_ERROR: f64 = 0.05;
const RATIO_EXACT_ERROR: f64 = 0.005;

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

/// A logical-space rectangle as (left, top, right, bottom).
type Rect = (f64, f64, f64, f64);

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
    snapped_rect: Option<Rect>,
    /// On-screen bounds of the size-readout box drawn for `snapped_rect`,
    /// so a click can tell whether it landed on "save this" vs "start a new
    /// drag". Recomputed every draw() call.
    hover_box: Option<Rect>,
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
    let tmp = std::env::temp_dir().join(format!("{}-{}.png", APP_NAME, std::process::id()));
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
        .args(["--app-name", APP_NAME, "-u", "low", "-t", "1200", "-r", "48291"])
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
    let close = |x: i64, y: i64| color_close(img, x, y, w, h, (rr, rg, rb), tol);

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

fn color_close(img: &RgbaImage, x: i64, y: i64, w: i64, h: i64, reference: (u8, u8, u8), tol: u8) -> bool {
    if x < 0 || y < 0 || x >= w || y >= h {
        return false;
    }
    let (r, g, b, _) = rgba_at(img, x as u32, y as u32);
    let tol = tol as i32;
    (r as i32 - reference.0 as i32).abs() <= tol
        && (g as i32 - reference.1 as i32).abs() <= tol
        && (b as i32 - reference.2 as i32).abs() <= tol
}

/// Same walk as `scan_extent`, in the drawing area's logical coordinate
/// space (physical / monitor scale), which is what CSS/devtools-style
/// measurements should be reported in.
fn scan_extent_logical(st: &State, cx: f64, cy: f64) -> Rect {
    let px = (cx * st.scale).round() as i64;
    let py = (cy * st.scale).round() as i64;
    let (l, r, t, b) = scan_extent(&st.img, px, py, TOLERANCE_LEVELS[st.tolerance_level].0);
    (l as f64 / st.scale, t as f64 / st.scale, r as f64 / st.scale, b as f64 / st.scale)
}

/// The most common exact color along a line segment — either a column
/// (`horizontal = false`, `fixed` = x) or a row (`horizontal = true`,
/// `fixed` = y) — used as a background-color estimate for one edge of a
/// selection. A single sampled point is too easy to land on noise or
/// anti-aliasing right at the moment you release the drag; the mode of the
/// whole line is much more representative of what the edge is actually
/// sitting on.
fn edge_mode_color(img: &RgbaImage, horizontal: bool, fixed: i64, from: i64, to: i64) -> (u8, u8, u8) {
    let w = img.width() as i64;
    let h = img.height() as i64;
    let (lo, hi) = (from.min(to), from.max(to));
    let mut counts: HashMap<(u8, u8, u8), u32> = HashMap::new();
    for v in lo..=hi {
        let (x, y) = if horizontal { (v, fixed) } else { (fixed, v) };
        if x < 0 || y < 0 || x >= w || y >= h {
            continue;
        }
        let (r, g, b, _) = rgba_at(img, x as u32, y as u32);
        *counts.entry((r, g, b)).or_insert(0) += 1;
    }
    counts.into_iter().max_by_key(|(_, c)| *c).map(|(rgb, _)| rgb).unwrap_or((255, 255, 255))
}

/// Whether every pixel along a line segment matches `reference` within
/// `tol`. Used to decide whether a whole row/column is still "background"
/// and can be trimmed away.
fn line_matches(img: &RgbaImage, horizontal: bool, fixed: i64, from: i64, to: i64, reference: (u8, u8, u8), tol: u8) -> bool {
    let w = img.width() as i64;
    let h = img.height() as i64;
    let (lo, hi) = (from.min(to), from.max(to));
    for v in lo..=hi {
        let (x, y) = if horizontal { (v, fixed) } else { (fixed, v) };
        if !color_close(img, x, y, w, h, reference, tol) {
            return false;
        }
    }
    true
}

/// The "snap to content" step for a dragged selection. This is the same
/// idea as ImageMagick's `-trim -fuzz` or GIMP's Autocrop: estimate each
/// edge's background color, then only trim a row/column once the *entire*
/// line is still within tolerance of it — not just one sampled point,
/// which is what made the original version unreliable. All 4 edges shrink
/// together, one step at a time, so a corner only stops where both its
/// row and column actually disagree with the background; each edge is
/// still capped at the rectangle's own center so opposing edges can't
/// cross.
fn shrink_rect(img: &RgbaImage, rect: (i64, i64, i64, i64), tol: u8) -> (i64, i64, i64, i64) {
    let w = img.width() as i64;
    let h = img.height() as i64;
    let (mut l, mut t, mut r, mut b) = rect;
    l = l.clamp(0, w - 1);
    r = r.clamp(0, w - 1);
    t = t.clamp(0, h - 1);
    b = b.clamp(0, h - 1);
    if r <= l || b <= t {
        return (l, t, r, b);
    }

    let mid_x = (l + r) / 2;
    let mid_y = (t + b) / 2;

    let left_ref = edge_mode_color(img, false, l, t, b);
    let right_ref = edge_mode_color(img, false, r, t, b);
    let top_ref = edge_mode_color(img, true, t, l, r);
    let bottom_ref = edge_mode_color(img, true, b, l, r);

    let (mut nl, mut nr, mut nt, mut nb) = (l, r, t, b);
    let mut iterations = 0;
    loop {
        let mut changed = false;
        if nl + 1 < mid_x && line_matches(img, false, nl + 1, nt, nb, left_ref, tol) {
            nl += 1;
            changed = true;
        }
        if nr - 1 > mid_x && line_matches(img, false, nr - 1, nt, nb, right_ref, tol) {
            nr -= 1;
            changed = true;
        }
        if nt + 1 < mid_y && line_matches(img, true, nt + 1, nl, nr, top_ref, tol) {
            nt += 1;
            changed = true;
        }
        if nb - 1 > mid_y && line_matches(img, true, nb - 1, nl, nr, bottom_ref, tol) {
            nb -= 1;
            changed = true;
        }
        iterations += 1;
        if !changed || iterations >= MAX_EXTENT_SCAN {
            break;
        }
    }
    (nl, nt, nr, nb)
}

/// Best small-integer aspect ratio approximation with both terms in
/// 1..=RATIO_MAX, e.g. 1366x768 -> ~16:9. Returns None when nothing in that
/// range comes within RATIO_HIDE_ERROR relative error, so we don't show
/// nonsense like 2345:123.
fn best_ratio(w: f64, h: f64) -> Option<(u32, u32, bool)> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let target = w / h;
    let mut best: Option<(u32, u32, f64)> = None;
    for a in 1..=RATIO_MAX {
        for b in 1..=RATIO_MAX {
            let err = (a as f64 / b as f64 - target).abs() / target;
            if best.map_or(true, |(_, _, be)| err < be) {
                best = Some((a, b, err));
            }
        }
    }
    let (a, b, err) = best?;
    if err > RATIO_HIDE_ERROR {
        return None;
    }
    Some((a, b, err <= RATIO_EXACT_ERROR))
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

/// Crops the captured screenshot to `rect` (logical coords) and saves it
/// under ~/Pictures/Screenshots, matching Omarchy's own screenshot naming
/// convention.
fn save_selection(st: &State, rect: Rect) {
    let (l, t, r, b) = rect;
    let iw = st.img.width();
    let ih = st.img.height();
    let pl = ((l * st.scale).round() as i64).clamp(0, iw as i64 - 1) as u32;
    let pt = ((t * st.scale).round() as i64).clamp(0, ih as i64 - 1) as u32;
    let pr = ((r * st.scale).round() as i64).clamp(0, iw as i64) as u32;
    let pb = ((b * st.scale).round() as i64).clamp(0, ih as i64) as u32;
    let w = pr.saturating_sub(pl).max(1);
    let h = pb.saturating_sub(pt).max(1);

    let cropped = image::imageops::crop_imm(&st.img, pl, pt, w, h).to_image();

    let Ok(home) = std::env::var("HOME") else {
        notify(APP_NAME, "Could not resolve $HOME to save image");
        return;
    };
    let dir = format!("{}/Pictures/Screenshots", home);
    if std::fs::create_dir_all(&dir).is_err() {
        notify(APP_NAME, "Failed to create Pictures/Screenshots");
        return;
    }

    let ts = Command::new("date")
        .arg("+%Y-%m-%d_%H-%M-%S")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "output".to_string());

    let path = format!("{}/{}-{}.png", dir, APP_NAME, ts);
    if cropped.save(&path).is_ok() {
        copy_to_clipboard(&path);
        notify(APP_NAME, &format!("Saved {}x{} to {}", w, h, path));
        // omasnap (github.com/tobi/omasnap, already used elsewhere on this
        // machine for screenshots) accepts an existing image path via
        // `--file` and opens it straight into its annotation editor instead
        // of capturing the screen — hand the crop off to it so it can be
        // marked up right away.
        let _ = Command::new("omasnap").arg("--file").arg(&path).spawn();
    } else {
        notify(APP_NAME, "Failed to save image");
    }
}

fn point_in_rect(p: (f64, f64), r: Rect) -> bool {
    p.0 >= r.0 && p.0 <= r.2 && p.1 >= r.1 && p.1 <= r.3
}

/// The system cursor is rendered by the compositor via a low-latency
/// hardware-cursor path, completely separate from our own client-side
/// redraw — leaving it visible during precision work (hovering or
/// dragging) means there are always two pointers on screen, and ours will
/// always look laggy by comparison. Hide it while measuring; once a
/// rectangle has snapped there's no more precision tracking to do, and a
/// visible cursor makes it much easier to land a click on the small
/// hover-to-save box.
fn sync_cursor_visibility(window: &ApplicationWindow, st: &State) {
    let visible = st.mode == Mode::Ruler && st.snapped_rect.is_some();
    window.set_cursor_from_name(Some(if visible { "default" } else { "none" }));
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

fn select_font(cr: &Context) {
    let _ = cr.select_font_face(
        "monospace",
        gtk4::cairo::FontSlant::Normal,
        gtk4::cairo::FontWeight::Bold,
    );
    cr.set_font_size(13.0);
}

/// Returns (advance width, height) for `text` in the current font. Uses
/// Cairo's x_advance rather than the ink-bounds `width()` — the latter
/// only measures rendered glyph pixels, so a string that's pure spaces
/// (e.g. the gaps between legend words) measures as ~0 wide even though it
/// takes up real horizontal room, which was collapsing spacing and
/// under-sizing background boxes.
fn measure_text(cr: &Context, text: &str) -> (f64, f64) {
    select_font(cr);
    cr.text_extents(text)
        .map(|e| (e.x_advance(), e.height()))
        .unwrap_or((text.len() as f64 * 8.0, 12.0))
}

/// Draws a themed label box of exactly `(tw, th)` text size at top-left
/// `(x, y)` — the fixed-size counterpart to `draw_label`, used where the
/// box footprint must stay stable across frames (e.g. so hover-testing the
/// save button doesn't jitter as its text content changes).
fn draw_label_box(cr: &Context, x: f64, y: f64, tw: f64, th: f64, text: &str, theme: &Theme) {
    select_font(cr);
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

/// Draws a themed label box. `x, y` is the box's top-left corner (not a
/// text baseline), so callers can anchor it directly to a screen position
/// — e.g. offset down-right of the cursor — without reasoning about font
/// metrics.
fn draw_label(cr: &Context, x: f64, y: f64, text: &str, theme: &Theme) {
    let (tw, th) = measure_text(cr, text);
    draw_label_box(cr, x, y, tw, th, text, theme);
}

/// A status badge pinned top-center, styled after omasnap's mode badge
/// (`overlay-chrome.cpp::drawModeBadge`) and Hyprland's own workspace/window
/// switcher OSDs: a small floating pill reporting current state rather than
/// a line buried in a hint bar. Always visible in Ruler mode regardless of
/// the `l` legend toggle, the same way a switcher HUD isn't gated behind a
/// separate help overlay.
fn draw_tolerance_badge(cr: &Context, theme: &Theme, screen_w: f64, tolerance_level: usize) {
    select_font(cr);
    let (bg, fg, ac) = (theme.background, theme.foreground, theme.accent);
    let prefix = "tolerance (t): ";
    let value = TOLERANCE_LEVELS[tolerance_level].1.to_lowercase();
    let (pw, ph) = measure_text(cr, prefix);
    let (vw, _) = measure_text(cr, &value);
    let pad_x = 14.0;
    let pad_y = 8.0;
    let box_w = pw + vw + pad_x * 2.0;
    let box_h = ph + pad_y * 2.0;
    let x = (screen_w - box_w) / 2.0;
    let y = 14.0;

    cr.set_source_rgba(bg.0, bg.1, bg.2, 0.9);
    cr.rectangle(x, y, box_w, box_h);
    let _ = cr.fill();
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.5);
    cr.set_line_width(1.0);
    cr.rectangle(x, y, box_w, box_h);
    let _ = cr.stroke();

    let text_y = y + pad_y + ph;
    cr.set_source_rgba(fg.0, fg.1, fg.2, 1.0);
    cr.move_to(x + pad_x, text_y);
    let _ = cr.show_text(prefix);
    cr.set_source_rgba(ac.0, ac.1, ac.2, 1.0);
    cr.move_to(x + pad_x + pw, text_y);
    let _ = cr.show_text(&value);
}

/// The hint card, top-right — a compact key/action list rather than one
/// long line, adapted from omasnap's `drawHotkeyLegend` (same idea: a small
/// floating card, key column dimmer than the action it does) but pulling
/// colors from the active Omarchy theme instead of a hardcoded palette.
fn draw_legend(cr: &Context, theme: &Theme, screen_w: f64, cursor: (f64, f64)) {
    const ENTRIES: [(&str, &str); 4] =
        [("drag", "select & snap"), ("r", "reset"), ("c", "color"), ("l", "hide legend")];
    select_font(cr);
    let (bg, fg, ac) = (theme.background, theme.foreground, theme.accent);
    let pad = 10.0;
    let key_gap = 14.0;
    let (_, line_h) = measure_text(cr, "Ag");
    let row_h = line_h + 6.0;

    let key_w = ENTRIES.iter().map(|(k, _)| measure_text(cr, k).0).fold(0.0_f64, f64::max);
    let val_w = ENTRIES.iter().map(|(_, v)| measure_text(cr, v).0).fold(0.0_f64, f64::max);
    let card_w = pad * 2.0 + key_w + key_gap + val_w;
    let card_h = pad * 2.0 + ENTRIES.len() as f64 * row_h;
    let y = 14.0;

    // Sits top-right by default, but hovering it (with a little slack, so it
    // doesn't flip right at the pixel edge) snaps it to top-left instead —
    // same idea as omasnap's card flip, just triggered by direct hover
    // rather than predicting where a drag might go.
    let right_x = screen_w - card_w - 14.0;
    let margin = 24.0;
    let over_right = cursor.0 >= right_x - margin
        && cursor.0 <= right_x + card_w + margin
        && cursor.1 >= y - margin
        && cursor.1 <= y + card_h + margin;
    let x = if over_right { 14.0 } else { right_x };

    cr.set_source_rgba(bg.0, bg.1, bg.2, 0.9);
    cr.rectangle(x, y, card_w, card_h);
    let _ = cr.fill();
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.5);
    cr.set_line_width(1.0);
    cr.rectangle(x, y, card_w, card_h);
    let _ = cr.stroke();

    for (i, (key, val)) in ENTRIES.iter().enumerate() {
        let row_y = y + pad + i as f64 * row_h + line_h;
        cr.set_source_rgba(ac.0, ac.1, ac.2, 0.85);
        cr.move_to(x + pad, row_y);
        let _ = cr.show_text(key);
        cr.set_source_rgba(fg.0, fg.1, fg.2, 1.0);
        cr.move_to(x + pad + key_w + key_gap, row_y);
        let _ = cr.show_text(val);
    }
}

/// A small hand-drawn camera glyph (no emoji font fallback needed), centered
/// inside a fixed `(tw, th)` footprint so the box doesn't resize relative to
/// the plain size-readout box it replaces on hover.
fn draw_camera_box(cr: &Context, x: f64, y: f64, tw: f64, th: f64, theme: &Theme) {
    select_font(cr);
    let pad = 6.0;
    let (bg, fg, ac) = (theme.background, theme.foreground, theme.accent);
    let box_w = tw + pad * 2.0;
    let box_h = th + pad * 2.0;

    cr.set_source_rgba(bg.0, bg.1, bg.2, 0.9);
    cr.rectangle(x, y, box_w, box_h);
    let _ = cr.fill();
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.9);
    cr.set_line_width(1.5);
    cr.rectangle(x, y, box_w, box_h);
    let _ = cr.stroke();

    let icon_h = (box_h - 10.0).max(8.0);
    let icon_w = icon_h * 1.4;
    let icon_x = x + (box_w - icon_w) / 2.0;
    let icon_cy = y + box_h / 2.0;

    cr.set_source_rgba(fg.0, fg.1, fg.2, 1.0);
    cr.rectangle(icon_x, icon_cy - icon_h / 2.0, icon_w, icon_h);
    let _ = cr.fill();
    cr.rectangle(icon_x + icon_w * 0.15, icon_cy - icon_h / 2.0 - icon_h * 0.22, icon_w * 0.35, icon_h * 0.22);
    let _ = cr.fill();

    cr.set_source_rgba(bg.0, bg.1, bg.2, 1.0);
    cr.arc(icon_x + icon_w / 2.0, icon_cy, icon_h * 0.28, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
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

fn draw_rect_shape(cr: &Context, theme: &Theme, rect: Rect) {
    let (l, t, r, b) = rect;
    let ac = theme.accent;
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.10);
    cr.rectangle(l, t, r - l, b - t);
    let _ = cr.fill();
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.9);
    cr.set_line_width(1.5);
    cr.rectangle(l, t, r - l, b - t);
    let _ = cr.stroke();
}

/// Places the size-readout box for a rectangle: centered inside it if it
/// fits, otherwise centered below it. Optionally draws a ratio label
/// beneath whichever box ends up lowest. Returns the size box's bounds.
fn place_size_box(cr: &Context, theme: &Theme, rect: Rect, text: &str, show_ratio: bool) -> Rect {
    let (l, t, r, b) = rect;
    let (rect_w, rect_h) = (r - l, b - t);
    let (tw, th) = measure_text(cr, text);
    let pad = 6.0;
    let (box_w, box_h) = (tw + pad * 2.0, th + pad * 2.0);

    let (bx, by) = if box_w + 8.0 <= rect_w && box_h + 8.0 <= rect_h {
        (l + (rect_w - box_w) / 2.0, t + (rect_h - box_h) / 2.0)
    } else {
        (l + (rect_w - box_w) / 2.0, b + 10.0)
    };
    draw_label_box(cr, bx, by, tw, th, text, theme);
    let bounds = (bx, by, bx + box_w, by + box_h);

    if show_ratio {
        if let Some((num, den, exact)) = best_ratio(rect_w, rect_h) {
            let prefix = if exact { "" } else { "~" };
            let rtext = format!("{}{}:{}", prefix, num, den);
            let (rtw, rth) = measure_text(cr, &rtext);
            let stack_bottom = bounds.3.max(b);
            let rx = l + (rect_w - (rtw + pad * 2.0)) / 2.0;
            draw_label_box(cr, rx, stack_bottom + 8.0, rtw, rth, &rtext, theme);
        }
    }

    bounds
}

fn draw(cr: &Context, w: i32, h: i32, st: &State) -> Option<Rect> {
    // The screenshot itself is painted by a GdkTexture-backed Picture widget
    // underneath this transparent DrawingArea (composited by GSK, effectively
    // free per frame). This draw_func never touches the full viewport — every
    // shape below is sized to its local content — which is what keeps cost
    // independent of screen resolution and the redraw fast enough to track
    // the cursor without lag.
    let (cx, cy) = st.cursor;
    let mut hover_box = None;

    match st.mode {
        Mode::Ruler => {
            if let Some(rect) = st.snapped_rect {
                draw_rect_shape(cr, &st.theme, rect);
                let (l, t, r, b) = rect;
                let size_text = format!("{:.0} x {:.0}", r - l, b - t);
                let (tw, th) = measure_text(cr, &size_text);
                let pad = 6.0;
                let (rect_w, rect_h) = (r - l, b - t);
                let (box_w, box_h) = (tw + pad * 2.0, th + pad * 2.0);
                let (bx, by) = if box_w + 8.0 <= rect_w && box_h + 8.0 <= rect_h {
                    (l + (rect_w - box_w) / 2.0, t + (rect_h - box_h) / 2.0)
                } else {
                    (l + (rect_w - box_w) / 2.0, b + 10.0)
                };
                let bounds = (bx, by, bx + box_w, by + box_h);

                if point_in_rect((cx, cy), bounds) {
                    draw_camera_box(cr, bx, by, tw, th, &st.theme);
                } else {
                    draw_label_box(cr, bx, by, tw, th, &size_text, &st.theme);
                }
                hover_box = Some(bounds);

                if let Some((num, den, exact)) = best_ratio(rect_w, rect_h) {
                    let prefix = if exact { "" } else { "~" };
                    let rtext = format!("{}{}:{}", prefix, num, den);
                    let (rtw, rth) = measure_text(cr, &rtext);
                    let stack_bottom = bounds.3.max(b);
                    let rx = l + (rect_w - (rtw + pad * 2.0)) / 2.0;
                    draw_label_box(cr, rx, stack_bottom + 8.0, rtw, rth, &rtext, &st.theme);
                }
            } else if st.dragging {
                if let Some(start) = st.start {
                    let rect = (start.0.min(cx), start.1.min(cy), start.0.max(cx), start.1.max(cy));
                    draw_rect_shape(cr, &st.theme, rect);
                    let text = format!("{:.0} x {:.0}", rect.2 - rect.0, rect.3 - rect.1);
                    place_size_box(cr, &st.theme, rect, &text, true);
                }
            } else {
                let (l, t, r, b) = scan_extent_logical(st, cx, cy);
                draw_extent_bars(cr, &st.theme, cx, cy, l, r, t, b);
                draw_label(cr, cx + 18.0, cy + 18.0, &format!("{:.0} x {:.0}", r - l, b - t), &st.theme);
            }

            draw_tolerance_badge(cr, &st.theme, w as f64, st.tolerance_level);
            if st.show_legend {
                draw_legend(cr, &st.theme, w as f64, (cx, cy));
            }
        }
        Mode::Color => {
            draw_loupe(cr, st, cx, cy, w, h);
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

    hover_box
}

fn build_ui(app: &Application) {
    let Some(monitor) = active_monitor() else {
        eprintln!("{}: could not read monitor list from hyprctl", APP_NAME);
        std::process::exit(1);
    };
    let Some(img) = capture_monitor(&monitor.name) else {
        eprintln!("{}: grim capture failed for output {}", APP_NAME, monitor.name);
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
        .title(APP_NAME)
        .build();

    window.set_cursor_from_name(Some("none"));

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
        snapped_rect: None,
        hover_box: None,
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
            let hover = draw(cr, w, h, &state.borrow());
            state.borrow_mut().hover_box = hover;
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
        let window = window.clone();
        click.connect_pressed(move |_g, _n, _x, _y| {
            let mut st = state.borrow_mut();
            match st.mode {
                Mode::Ruler => {
                    let hovering_save = st.snapped_rect.is_some()
                        && st.hover_box.map_or(false, |hb| point_in_rect(st.cursor, hb));
                    if hovering_save {
                        if let Some(rect) = st.snapped_rect {
                            save_selection(&st, rect);
                        }
                    } else {
                        st.snapped_rect = None;
                        st.start = Some(st.cursor);
                        st.dragging = true;
                    }
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
            sync_cursor_visibility(&window, &st);
            drop(st);
            area.queue_draw();
        });
    }
    {
        let state = state.clone();
        let area = area.clone();
        let window = window.clone();
        click.connect_released(move |_g, _n, _x, _y| {
            let mut st = state.borrow_mut();
            if st.mode == Mode::Ruler && st.dragging {
                st.dragging = false;
                if let Some(start) = st.start {
                    let l = start.0.min(st.cursor.0);
                    let r = start.0.max(st.cursor.0);
                    let t = start.1.min(st.cursor.1);
                    let b = start.1.max(st.cursor.1);
                    if r - l > 2.0 && b - t > 2.0 {
                        let scale = st.scale;
                        let prect = (
                            (l * scale).round() as i64,
                            (t * scale).round() as i64,
                            (r * scale).round() as i64,
                            (b * scale).round() as i64,
                        );
                        let tol = TOLERANCE_LEVELS[st.tolerance_level].0;
                        let (nl, nt, nr, nb) = shrink_rect(&st.img, prect, tol);
                        st.snapped_rect =
                            Some((nl as f64 / scale, nt as f64 / scale, nr as f64 / scale, nb as f64 / scale));
                    }
                }
                st.start = None;
            }
            sync_cursor_visibility(&window, &st);
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
                    let mut st = state.borrow_mut();
                    if st.mode == Mode::Ruler && (st.dragging || st.snapped_rect.is_some()) {
                        st.dragging = false;
                        st.start = None;
                        st.snapped_rect = None;
                        sync_cursor_visibility(&window, &st);
                        drop(st);
                        area.queue_draw();
                    } else {
                        drop(st);
                        window.close();
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::c | gdk::Key::C => {
                    let mut st = state.borrow_mut();
                    st.mode = if st.mode == Mode::Ruler { Mode::Color } else { Mode::Ruler };
                    st.dragging = false;
                    st.start = None;
                    st.snapped_rect = None;
                    st.last_message = None;
                    sync_cursor_visibility(&window, &st);
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
                    st.snapped_rect = None;
                    sync_cursor_visibility(&window, &st);
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::t | gdk::Key::T => {
                    let mut st = state.borrow_mut();
                    st.tolerance_level = (st.tolerance_level + 1) % TOLERANCE_LEVELS.len();
                    drop(st);
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
