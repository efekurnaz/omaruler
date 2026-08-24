use std::cell::RefCell;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::rc::Rc;

use gtk4::cairo::{Context, Format, ImageSurface};
use gtk4::prelude::*;
use gtk4::{
    gdk, glib, Application, ApplicationWindow, DrawingArea, EventControllerKey,
    EventControllerMotion, GestureClick,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use image::RgbaImage;

const APP_ID: &str = "sh.omarchy.pixel-snap";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Ruler,
    Color,
}

struct MonitorInfo {
    name: String,
    scale: f64,
}

struct State {
    img: RgbaImage,
    surface: ImageSurface,
    grad: Vec<f32>,
    gw: u32,
    gh: u32,
    scale: f64,
    cursor: (f64, f64),
    mode: Mode,
    dragging: bool,
    start: Option<(f64, f64)>,
    snap_enabled: bool,
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

fn to_cairo_argb(img: &RgbaImage) -> ImageSurface {
    let (w, h) = img.dimensions();
    let mut surface =
        ImageSurface::create(Format::ARgb32, w as i32, h as i32).expect("create cairo surface");
    let stride = surface.stride() as usize;
    {
        let mut data = surface.data().expect("lock cairo surface data");
        for y in 0..h {
            let row = y as usize * stride;
            for x in 0..w {
                let (r, g, b, a) = rgba_at(img, x, y);
                let (r, g, b, a) = (r as u32, g as u32, b as u32, a as u32);
                let pr = (r * a) / 255;
                let pg = (g * a) / 255;
                let pb = (b * a) / 255;
                let off = row + x as usize * 4;
                data[off] = pb as u8;
                data[off + 1] = pg as u8;
                data[off + 2] = pr as u8;
                data[off + 3] = a as u8;
            }
        }
    }
    surface.mark_dirty();
    surface
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

fn draw_label(cr: &Context, x: f64, y: f64, text: &str) {
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
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.78);
    cr.rectangle(x - pad, y - th - pad, tw + pad * 2.0, th + pad * 2.0);
    let _ = cr.fill();
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.move_to(x, y);
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

    cr.set_source_rgba(1.0, 0.85, 0.2, 0.9);
    cr.set_line_width(2.0);
    cr.rectangle(lx + half as f64 * zoom, ly + half as f64 * zoom, zoom, zoom);
    let _ = cr.stroke();

    cr.set_source_rgba(1.0, 1.0, 1.0, 0.8);
    cr.set_line_width(1.5);
    cr.rectangle(lx, ly, size, size);
    let _ = cr.stroke();
}

fn draw(cr: &Context, w: i32, h: i32, st: &State) {
    cr.set_source_rgba(0.0, 0.0, 0.0, 1.0);
    let _ = cr.paint();

    let _ = cr.save();
    let inv_scale = 1.0 / st.scale;
    cr.scale(inv_scale, inv_scale);
    let _ = cr.set_source_surface(&st.surface, 0.0, 0.0);
    let _ = cr.paint();
    let _ = cr.restore();

    cr.set_source_rgba(0.0, 0.0, 0.0, 0.12);
    let _ = cr.paint();

    let (cx, cy) = st.cursor;
    cr.set_line_width(1.0);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
    cr.move_to(cx, 0.0);
    cr.line_to(cx, h as f64);
    let _ = cr.stroke();
    cr.move_to(0.0, cy);
    cr.line_to(w as f64, cy);
    let _ = cr.stroke();

    match st.mode {
        Mode::Ruler => {
            if let Some(start) = st.start {
                cr.set_source_rgba(1.0, 0.85, 0.2, 0.95);
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
                draw_label(
                    cr,
                    cx + 14.0,
                    cy + 14.0,
                    &format!("{:.0}px   dx {:.0}   dy {:.0}", dist, dx, dy),
                );
            } else {
                draw_label(
                    cr,
                    cx + 14.0,
                    cy + 14.0,
                    "drag to measure  ·  c: color mode  ·  s: toggle snap  ·  esc: quit",
                );
            }
        }
        Mode::Color => {
            draw_loupe(cr, st, cx, cy, w, h);
            let px = (cx * st.scale).round() as u32;
            let py = (cy * st.scale).round() as u32;
            if let Some(hex) = pixel_hex(&st.img, px, py) {
                draw_label(cr, cx + 14.0, cy - 60.0, &format!("{}  ·  click to copy  ·  esc: quit", hex));
            }
        }
    }

    if let Some(msg) = &st.last_message {
        draw_label(cr, 16.0, h as f64 - 20.0, msg);
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
    let surface = to_cairo_argb(&img);

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
        surface,
        grad,
        gw,
        gh,
        scale: monitor.scale,
        cursor: (0.0, 0.0),
        mode: Mode::Ruler,
        dragging: false,
        start: None,
        snap_enabled: true,
        last_message: None,
    }));

    let area = DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    area.set_can_focus(true);

    {
        let state = state.clone();
        area.set_draw_func(move |_area, cr, w, h| {
            draw(cr, w, h, &state.borrow());
        });
    }

    let motion = EventControllerMotion::new();
    {
        let state = state.clone();
        let area = area.clone();
        motion.connect_motion(move |_c, x, y| {
            let mut st = state.borrow_mut();
            let cursor = if st.snap_enabled {
                let px = x * st.scale;
                let py = y * st.scale;
                let (snx, sny) = snap_point(&st.grad, st.gw, st.gh, px, py, 6, 40.0);
                (snx / st.scale, sny / st.scale)
            } else {
                (x, y)
            };
            st.cursor = cursor;
            drop(st);
            area.queue_draw();
        });
    }
    area.add_controller(motion);

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
                _ => glib::Propagation::Proceed,
            }
        });
    }
    window.add_controller(key);

    window.set_child(Some(&area));
    window.present();
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}
