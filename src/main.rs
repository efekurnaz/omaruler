use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Cursor, Write as _};
use std::os::unix::fs::PermissionsExt;
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
const DISPLAY_NAME: &str = "Omaruler";
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
    /// Placing a horizontal guide: a live line follows the cursor's Y, with
    /// the space above/below it read out on the left edge of the screen,
    /// until a click commits it (at the exact cursor position — no
    /// color-edge snapping) and mode returns to Ruler.
    GuideH,
    /// The vertical-guide counterpart: line follows cursor X, readouts on
    /// the top edge.
    GuideV,
}

#[derive(Clone, Copy, Debug)]
enum UndoItem {
    Rect,
    GuideH,
    GuideV,
    MeasureH,
    MeasureV,
}

#[derive(Clone, Copy)]
struct Theme {
    accent: (f64, f64, f64),
    foreground: (f64, f64, f64),
    background: (f64, f64, f64),
    /// Distinct color for pinned guides, so they read as a different kind of
    /// thing from the accent-colored measuring lines/selections — matches
    /// the design-tool convention (Figma/Sketch) of guides in pink/magenta.
    guide: (f64, f64, f64),
    /// Distinct color for pinned measure lines (h/v), so they read as a
    /// different kind of thing from both the accent-colored live ruler
    /// crosshair and the guide color.
    measure: (f64, f64, f64),
}

struct MonitorInfo {
    name: String,
    scale: f64,
}

/// A logical-space rectangle as (left, top, right, bottom).
type Rect = (f64, f64, f64, f64);
/// A pinned horizontal measure line: (y, left, right), all logical.
type HLine = (f64, f64, f64);
/// A pinned vertical measure line: (x, top, bottom), all logical.
type VLine = (f64, f64, f64);

/// A tiny xorshift64 PRNG so the duck easter egg's flight path doesn't need
/// to pull in the `rand` crate for the one place this app wants randomness.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        lo + unit * (hi - lo)
    }
}

/// Small procedurally-synthesized sound effects for the duck easter egg
/// (see `assets/sounds/`), embedded directly in the binary so this stays a
/// single self-contained executable — no asset files to ship or fetch
/// alongside it. Written out to temp files once at startup and played via
/// `paplay`, the standard PulseAudio/PipeWire client already present on
/// stock Omarchy, rather than pulling in an audio-playback crate for four
/// short clips.
struct Sounds {
    shot: std::path::PathBuf,
    quack: std::path::PathBuf,
    fail: std::path::PathBuf,
    chime: std::path::PathBuf,
}

fn write_sound_asset(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("{}-sound-{}.wav", APP_NAME, name));
    let _ = std::fs::write(&path, bytes);
    path
}

fn load_sounds() -> Sounds {
    Sounds {
        shot: write_sound_asset("shot", include_bytes!("../assets/sounds/shot.wav")),
        quack: write_sound_asset("quack", include_bytes!("../assets/sounds/quack.wav")),
        fail: write_sound_asset("fail", include_bytes!("../assets/sounds/fail.wav")),
        chime: write_sound_asset("chime", include_bytes!("../assets/sounds/chime.wav")),
    }
}

fn play_sound(path: &std::path::Path) {
    let _ = Command::new("paplay").arg(path).spawn();
}

/// Easter egg: press `d` to spawn one, click it to kill it. Flies with an
/// erratic Duck Hunt-style path (constant speed, occasional random turns)
/// and is lost for good — no bounce — if it reaches an edge or outruns the
/// clock before you do.
struct Duck {
    pos: (f64, f64),
    vel: (f64, f64),
    rng: Rng,
    spawned: std::time::Instant,
    last_update: std::time::Instant,
    next_turn: std::time::Instant,
}

const DUCK_HIT_RADIUS: f64 = 22.0;
/// Forgiving "kill zone" around the cursor, like a brush-tool radius — the
/// duck doesn't have to be exactly under the pointer, just overlapping this
/// circle, closer to aiming a rifle than requiring a pixel-perfect click.
const CURSOR_AIM_RADIUS: f64 = 26.0;
const DUCK_LIFETIME: std::time::Duration = std::time::Duration::from_secs(15);
/// Pause between one duck being gone (killed or escaped) and the next one
/// spawning, so kills read as discrete rounds rather than an instant swap.
const DUCK_RESPAWN_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// Plain-text high score, one integer, in the XDG data dir — small enough
/// state that hand-rolling this beats pulling in a config/serialization
/// crate for it.
fn high_score_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join(".local/share/omaruler/highscore"))
}

fn load_high_score() -> u32 {
    high_score_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn save_high_score(score: u32) {
    let Some(path) = high_score_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, score.to_string());
}

fn spawn_duck(screen_w: f64, screen_h: f64) -> Duck {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x2545_F491_4F6C_DD1D);
    let mut rng = Rng::new(seed);
    let speed = rng.range(140.0, 220.0);
    let angle = rng.range(0.0, std::f64::consts::TAU);
    let now = std::time::Instant::now();
    Duck {
        pos: (rng.range(screen_w * 0.2, screen_w * 0.8), rng.range(screen_h * 0.3, screen_h * 0.8)),
        vel: (speed * angle.cos(), speed * angle.sin()),
        rng,
        spawned: now,
        last_update: now,
        next_turn: now + std::time::Duration::from_millis(600),
    }
}

/// Advances flight physics by real elapsed time (clamped so a stalled frame
/// clock can't fling the duck across the screen in one jump), bounces it
/// off the screen edges, and occasionally injects a random turn — the
/// erratic part of the flight path.
/// No edge bounce — a duck that reaches the screen boundary just keeps
/// going and is lost for good (see `duck_offscreen`), same as the real
/// game. That's what makes it a threat instead of a target you can take
/// your time lining up: it flies away permanently if you don't catch it
/// first.
fn update_duck(duck: &mut Duck) {
    let now = std::time::Instant::now();
    let dt = now.duration_since(duck.last_update).as_secs_f64().min(0.05);
    duck.last_update = now;

    duck.pos.0 += duck.vel.0 * dt;
    duck.pos.1 += duck.vel.1 * dt;

    if now >= duck.next_turn {
        let speed = (duck.vel.0 * duck.vel.0 + duck.vel.1 * duck.vel.1).sqrt();
        let angle = duck.vel.1.atan2(duck.vel.0) + duck.rng.range(-1.2, 1.2);
        duck.vel = (speed * angle.cos(), speed * angle.sin());
        duck.next_turn = now + std::time::Duration::from_millis(duck.rng.range(500.0, 1400.0) as u64);
    }
}

/// True once the duck has fully left the visible screen (not just touched
/// the edge) — it's allowed to fly a bit past the boundary first so it
/// visibly exits the frame rather than vanishing right at the edge.
fn duck_offscreen(duck: &Duck, screen_w: f64, screen_h: f64) -> bool {
    const OFFSCREEN_MARGIN: f64 = 60.0;
    duck.pos.0 < -OFFSCREEN_MARGIN
        || duck.pos.0 > screen_w + OFFSCREEN_MARGIN
        || duck.pos.1 < -OFFSCREEN_MARGIN
        || duck.pos.1 > screen_h + OFFSCREEN_MARGIN
}

fn duck_hit(duck: &Duck, point: (f64, f64)) -> bool {
    let dx = point.0 - duck.pos.0;
    let dy = point.1 - duck.pos.1;
    (dx * dx + dy * dy).sqrt() <= DUCK_HIT_RADIUS + CURSOR_AIM_RADIUS
}

/// A stylized duck silhouette (no image asset needed) — body, head, beak,
/// eye, and a wing that flaps on a simple sine cycle. Mirrored horizontally
/// to face whichever way it's currently flying.
fn draw_duck(cr: &Context, duck: &Duck) {
    let (x, y) = duck.pos;
    let facing_right = duck.vel.0 >= 0.0;
    let flap = (duck.spawned.elapsed().as_secs_f64() * 9.0).sin();

    let _ = cr.save();
    cr.translate(x, y);
    if !facing_right {
        cr.scale(-1.0, 1.0);
    }

    cr.set_source_rgb(0.95, 0.78, 0.2);
    let _ = cr.save();
    cr.scale(1.0, 0.7);
    cr.arc(0.0, 0.0, 16.0, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
    let _ = cr.restore();

    cr.arc(12.0, -10.0, 8.0, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();

    cr.set_source_rgb(0.8, 0.63, 0.1);
    let wing_y = flap * 6.0;
    cr.move_to(-6.0, -2.0);
    cr.line_to(-16.0, -10.0 + wing_y);
    cr.line_to(-2.0, 3.0 + wing_y * 0.3);
    cr.close_path();
    let _ = cr.fill();

    cr.set_source_rgb(0.9, 0.5, 0.1);
    cr.move_to(19.0, -10.0);
    cr.line_to(29.0, -8.0);
    cr.line_to(19.0, -6.0);
    cr.close_path();
    let _ = cr.fill();

    cr.set_source_rgb(0.05, 0.05, 0.05);
    cr.arc(14.0, -12.0, 1.4, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();

    let _ = cr.restore();
}

/// A faint reticle around the cursor while a duck is up, sized to
/// `CURSOR_AIM_RADIUS` so the forgiving "nearby counts" kill zone is
/// visible rather than a hidden rule.
fn draw_aim_circle(cr: &Context, theme: &Theme, cx: f64, cy: f64) {
    let ac = theme.guide;
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.5);
    cr.set_line_width(1.0);
    cr.arc(cx, cy, CURSOR_AIM_RADIUS, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();
}

/// A prominent centered prompt — used for the duck easter egg's "click to
/// continue" gate after a miss, so the round being paused is unmistakable
/// rather than an easy-to-miss corner message.
fn draw_center_prompt(cr: &Context, theme: &Theme, screen_w: i32, screen_h: i32, text: &str) {
    let size = 16.0;
    let (tw, th) = measure_text_sized(cr, text, size);
    let pad = 14.0;
    let box_w = tw + pad * 2.0;
    let box_h = th + pad * 2.0;
    let x = (screen_w as f64 - box_w) / 2.0;
    let y = (screen_h as f64 - box_h) / 2.0;

    let (bg, fg, ac) = (theme.background, theme.foreground, theme.accent);
    cr.set_source_rgba(bg.0, bg.1, bg.2, 0.92);
    cr.rectangle(x, y, box_w, box_h);
    let _ = cr.fill();
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.8);
    cr.set_line_width(1.5);
    cr.rectangle(x, y, box_w, box_h);
    let _ = cr.stroke();

    select_font_sized(cr, size);
    cr.set_source_rgba(fg.0, fg.1, fg.2, 1.0);
    cr.move_to(x + pad, y + pad + th);
    let _ = cr.show_text(text);
}

struct State {
    img: RgbaImage,
    grad: Vec<f32>,
    gw: u32,
    gh: u32,
    scale: f64,
    theme: Theme,
    cursor: (f64, f64),
    /// Last raw (un-snapped) pointer position seen from the compositor, used
    /// to tell real mouse motion apart from a keyboard nudge to `cursor` —
    /// see the tick callback in `build_ui`.
    last_raw_cursor: (f64, f64),
    mode: Mode,
    dragging: bool,
    start: Option<(f64, f64)>,
    snapped_rects: Vec<Rect>,
    /// On-screen bounds of the size-readout box drawn for the *last*
    /// selection (only the most recent one is hover/click-interactive), so
    /// a click can tell whether it landed on "open in omasnap" vs "start a
    /// new drag". Recomputed every draw() call.
    hover_box: Option<Rect>,
    snap_enabled: bool,
    tolerance_level: usize,
    show_legend: bool,
    /// Whether `omarchy-legend` is on PATH, checked once at startup. When
    /// it isn't (e.g. this app lands before the shell-side legend service
    /// does), the same hint entries are drawn locally in Cairo instead of
    /// shelling out — see `legend_entries`/`draw_builtin_legend`.
    legend_available: bool,
    shift_held: bool,
    measure_lines_h: Vec<HLine>,
    measure_lines_v: Vec<VLine>,
    guides_h: Vec<f64>,
    guides_v: Vec<f64>,
    /// What was added most recently, across all of the above — so Ctrl+Z
    /// can undo "the last thing", whatever kind it was, rather than only
    /// one specific collection.
    undo_stack: Vec<UndoItem>,
    last_message: Option<String>,
    /// Easter egg (`d`) — see `Duck`/`spawn_duck`/`update_duck`/`draw_duck`.
    duck: Option<Duck>,
    /// `Some(deadline)` right after a kill, before the next duck spawns —
    /// distinct from `duck.is_none()` at rest (game off) so the tick
    /// callback and the `d`/Escape/`r` handlers can tell "mid-round,
    /// waiting to respawn" apart from "not playing".
    duck_next_spawn: Option<std::time::Instant>,
    /// Set when a duck escapes (flies off or outruns the clock) instead of
    /// an auto-respawn timer — the round pauses on a centered "click to
    /// continue" prompt until the next click, rather than silently handing
    /// you a fresh duck.
    duck_waiting_continue: bool,
    duck_score: u32,
    duck_high_score: u32,
    sounds: Sounds,
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

/// Parses a binary PPM (P6) buffer straight into an RGBA image, expanding
/// RGB to RGBA (a screenshot is always fully opaque). This is what actually
/// fixes slow startup: capturing as PNG and decoding it back through the
/// generic `image` crate meant paying for a full compress-then-decompress
/// round trip on a whole-screen image for no reason — PPM is uncompressed,
/// so both sides of that trip are just a memory copy.
fn decode_ppm(bytes: &[u8]) -> Option<RgbaImage> {
    if !bytes.starts_with(b"P6") {
        return None;
    }
    let mut pos = 2;
    let mut fields = [0u32; 3];
    for field in fields.iter_mut() {
        loop {
            while bytes.get(pos).is_some_and(|b| b.is_ascii_whitespace()) {
                pos += 1;
            }
            if bytes.get(pos) == Some(&b'#') {
                while bytes.get(pos).is_some_and(|&b| b != b'\n') {
                    pos += 1;
                }
                continue;
            }
            break;
        }
        let start = pos;
        while bytes.get(pos).is_some_and(|b| !b.is_ascii_whitespace()) {
            pos += 1;
        }
        *field = std::str::from_utf8(bytes.get(start..pos)?).ok()?.parse().ok()?;
    }
    pos += 1; // exactly one whitespace byte separates the header from pixel data
    let (w, h, maxval) = (fields[0], fields[1], fields[2]);
    if maxval != 255 || w == 0 || h == 0 {
        return None;
    }
    let pixel_count = w as usize * h as usize;
    let rgb = bytes.get(pos..pos + pixel_count * 3)?;
    let mut rgba = vec![0u8; pixel_count * 4];
    for i in 0..pixel_count {
        rgba[i * 4] = rgb[i * 3];
        rgba[i * 4 + 1] = rgb[i * 3 + 1];
        rgba[i * 4 + 2] = rgb[i * 3 + 2];
        rgba[i * 4 + 3] = 255;
    }
    RgbaImage::from_raw(w, h, rgba)
}

/// Captures straight to stdout (grim's `-` output-file) rather than a temp
/// file, and as uncompressed PPM rather than PNG — no disk round trip and
/// no compression, since this is a full-screen capture done fresh on every
/// launch and startup latency is directly user-visible.
fn capture_monitor(name: &str) -> Option<RgbaImage> {
    let output = Command::new("grim").args(["-t", "ppm", "-o", name, "-"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    decode_ppm(&output.stdout)
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

/// Whether `name` resolves to an executable file somewhere on `$PATH` —
/// used to detect `omarchy-legend` specifically, since (unlike the rest of
/// this app's Omarchy dependencies) it's a shell-service companion built
/// alongside this app rather than something guaranteed present on stock
/// Omarchy yet. Checked once at startup rather than per-call so behavior
/// stays consistent for the life of the process.
fn command_exists(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        std::fs::metadata(dir.join(name))
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// Starts resolving one semantic color from the active Omarchy theme via
/// `omarchy-theme-color`, which handles the alias/fallback cascade that
/// every other theme consumer (templates, tmux, GNOME, ...) shares — so
/// this app follows the same palette as the rest of the desktop instead of
/// hand-parsing colors.toml itself. Returns the still-running child;
/// `fetch_theme` spawns all of these before waiting on any of them; each
/// `omarchy-theme-color` invocation is its own process (fork/exec plus
/// theme file I/O), so waiting on them one at a time serialized costs that
/// pay five times over for no reason.
fn theme_color_spawn(key: &str) -> Option<std::process::Child> {
    Command::new("omarchy-theme-color").arg(key).stdout(Stdio::piped()).spawn().ok()
}

fn theme_color_collect(child: Option<std::process::Child>, fallback: (f64, f64, f64)) -> (f64, f64, f64) {
    child
        .and_then(|c| c.wait_with_output().ok())
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| parse_hex_color(&s))
        .unwrap_or(fallback)
}

fn fetch_theme() -> Theme {
    let accent = theme_color_spawn("accent");
    let foreground = theme_color_spawn("foreground");
    let background = theme_color_spawn("background");
    let guide = theme_color_spawn("magenta");
    let measure = theme_color_spawn("cyan");
    Theme {
        accent: theme_color_collect(accent, (1.0, 0.85, 0.2)),
        foreground: theme_color_collect(foreground, (1.0, 1.0, 1.0)),
        background: theme_color_collect(background, (0.0, 0.0, 0.0)),
        guide: theme_color_collect(guide, (1.0, 0.2, 0.6)),
        measure: theme_color_collect(measure, (0.2, 0.85, 0.9)),
    }
}

/// Transient status flash via Omarchy's own volume/brightness on-screen-
/// display service — genuinely general-purpose (arbitrary message, not
/// hardcoded to a fixed set of OSD kinds), so this needed no changes on the
/// Omarchy side beyond fixing a pre-existing icon-fallback bug. Matches the
/// "window/workspace switch" feel: a brief themed flash, not a pinned
/// overlay we'd have to hand-draw and keep in sync with the theme
/// ourselves.
fn flash_status(message: &str) {
    let _ = Command::new("omarchy-osd").args(["-m", message, "-d", "1200"]).spawn();
}

fn flash_tolerance(level_name: &str) {
    flash_status(&format!("Tolerance: {}", level_name));
}

const LEGEND_IDLE: &[(&str, &str)] = &[
    ("drag", "select & snap"),
    ("h/v", "measure line"),
    ("shift+h/v", "guide"),
    ("ctrl+z", "undo"),
    ("t", "tolerance"),
    ("n", "toggle snap"),
    ("c", "color"),
    ("l", "hide legend"),
];

/// Legend shown once at least one selection is pinned — `c`/`s` mean
/// something different there (copy/save the selection rather than
/// toggling color mode / edge-snap), so the hint card needs to say so.
const LEGEND_SELECTION: &[(&str, &str)] = &[
    ("drag", "new selection"),
    ("c", "copy"),
    ("s", "save"),
    ("ctrl+z", "undo"),
    ("t", "tolerance"),
    ("n", "toggle snap"),
    ("l", "hide legend"),
];

/// Legend shown while placing a guide (`Mode::GuideH`/`GuideV`) — `c`/`s`
/// and the rest of the idle/selection hints don't apply here, only
/// committing or canceling the guide do.
const LEGEND_GUIDE: &[(&str, &str)] = &[("click", "place guide"), ("esc", "cancel")];

/// The entries the legend should show for the current state, or `None` if
/// it should be hidden — the single source of truth both the shell-service
/// dispatch and the built-in Cairo fallback draw from, so the two backends
/// can never drift out of sync with each other.
fn legend_entries(st: &State) -> Option<&'static [(&'static str, &'static str)]> {
    if !st.show_legend {
        return None;
    }
    match st.mode {
        Mode::Ruler => Some(if st.snapped_rects.is_empty() { LEGEND_IDLE } else { LEGEND_SELECTION }),
        Mode::GuideH | Mode::GuideV => Some(LEGEND_GUIDE),
        Mode::Color => None,
    }
}

/// Shows the shortcut-hint card via Omarchy's `legend` shell service
/// (companion to `omarchy-osd`, built alongside this feature) instead of
/// hand-drawing it in Cairo — themed and positioned by the shell itself,
/// so it looks Omarchy-native for free and stays in sync with the theme
/// automatically. Only called when `State::legend_available` — see
/// `draw_builtin_legend` for the fallback when the service isn't installed.
fn show_legend_entries(entries: &[(&str, &str)]) {
    let mut args: Vec<String> = Vec::new();
    for (key, action) in entries {
        args.push("-e".to_string());
        args.push(format!("{}:{}", key, action));
    }
    args.push("-c".to_string());
    args.push("top-right".to_string());
    let _ = Command::new("omarchy-legend").args(&args).spawn();
}

/// Blocks until the hide IPC call actually completes, not just spawns.
/// Hiding is what should happen right before this app's own frozen-
/// screenshot overlay disappears (on quit, or any mode switch that drops
/// the legend) — a fire-and-forget spawn races the window closing, so for
/// a brief moment the legend would still be showing, now floating over the
/// real live desktop instead of the overlay, which reads as a flash/blink
/// right as the app quits. The call itself is a fast local IPC round trip,
/// so blocking on it is imperceptible.
fn hide_legend_card() {
    let _ = Command::new("omarchy-legend").arg("--hide").status();
}

/// Tells the shell-service legend which entries to show (or hides it) for
/// the current state. Called at every transition that could change which
/// entries apply: a selection being made or cleared, the `l` toggle,
/// entering/leaving Color or guide-placement mode. A no-op when
/// `omarchy-legend` isn't installed — `draw()` derives the built-in
/// fallback straight from state every frame instead, so there's nothing to
/// imperatively push in that case.
fn refresh_legend(st: &State) {
    if !st.legend_available {
        return;
    }
    match legend_entries(st) {
        Some(entries) => show_legend_entries(entries),
        None => hide_legend_card(),
    }
}

#[inline]
fn clamped_idx(x: i32, y: i32, w: i32, h: i32) -> usize {
    let x = x.clamp(0, w - 1) as u32;
    let y = y.clamp(0, h - 1) as u32;
    (y * w as u32 + x) as usize
}

/// Sobel gradient magnitude over the whole screenshot, used to magnet-snap
/// the cursor to nearby edges — the one substantial per-pixel computation
/// done at startup. Split across all available CPU cores (each thread owns
/// a disjoint row range of `grad`, reading the shared read-only `gray`
/// buffer) rather than a single-threaded pass, since this runs fresh on
/// every launch and startup latency is directly user-visible.
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

    let (wi, hi) = (w as i32, h as i32);
    let mut grad = vec![0f32; (w * h) as usize];
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(h.max(1) as usize).max(1);
    let rows_per_chunk = (h as usize).div_ceil(threads);

    std::thread::scope(|scope| {
        for (i, chunk) in grad.chunks_mut(rows_per_chunk * w as usize).enumerate() {
            let y0 = (i * rows_per_chunk) as i32;
            let gray = &gray;
            scope.spawn(move || {
                for (row, out_row) in chunk.chunks_mut(w as usize).enumerate() {
                    let y = y0 + row as i32;
                    for x in 0..wi {
                        let gx = -gray[clamped_idx(x - 1, y - 1, wi, hi)] + gray[clamped_idx(x + 1, y - 1, wi, hi)]
                            - 2.0 * gray[clamped_idx(x - 1, y, wi, hi)]
                            + 2.0 * gray[clamped_idx(x + 1, y, wi, hi)]
                            - gray[clamped_idx(x - 1, y + 1, wi, hi)]
                            + gray[clamped_idx(x + 1, y + 1, wi, hi)];
                        let gy = -gray[clamped_idx(x - 1, y - 1, wi, hi)] - 2.0 * gray[clamped_idx(x, y - 1, wi, hi)] - gray[clamped_idx(x + 1, y - 1, wi, hi)]
                            + gray[clamped_idx(x - 1, y + 1, wi, hi)]
                            + 2.0 * gray[clamped_idx(x, y + 1, wi, hi)]
                            + gray[clamped_idx(x + 1, y + 1, wi, hi)];
                        out_row[x as usize] = (gx * gx + gy * gy).sqrt();
                    }
                }
            });
        }
    });
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
/// motion frame into an unbounded scan. A scan also stops the instant it
/// would cross a pinned guide (`guides_x`/`guides_y`, physical coords),
/// even if the color hasn't actually changed there — a guide is a
/// deliberate boundary you placed, so it should behave like one regardless
/// of what's actually under it.
fn scan_extent(img: &RgbaImage, px: i64, py: i64, tol: u8, guides_x: &[i64], guides_y: &[i64]) -> (i64, i64, i64, i64) {
    let w = img.width() as i64;
    let h = img.height() as i64;
    if px < 0 || py < 0 || px >= w || py >= h {
        return (px, px, py, py);
    }
    let (rr, rg, rb, _) = rgba_at(img, px as u32, py as u32);
    let close = |x: i64, y: i64| color_close(img, x, y, w, h, (rr, rg, rb), tol);

    let left_bound = guides_x.iter().copied().filter(|&g| g <= px).max().unwrap_or(i64::MIN / 2);
    let right_bound = guides_x.iter().copied().filter(|&g| g >= px).min().unwrap_or(i64::MAX / 2);
    let top_bound = guides_y.iter().copied().filter(|&g| g <= py).max().unwrap_or(i64::MIN / 2);
    let bottom_bound = guides_y.iter().copied().filter(|&g| g >= py).min().unwrap_or(i64::MAX / 2);

    // `>=`/`<=` (not `>`/`<`): a guide is a real boundary, so the scan is
    // allowed to reach all the way to it — stopping one pixel short would
    // mean a guide never actually blocks the color scan when the color
    // happens not to change there, which is the entire point of placing
    // one.
    let mut left = px;
    let mut steps = 0;
    while steps < MAX_EXTENT_SCAN && left - 1 >= left_bound && close(left - 1, py) {
        left -= 1;
        steps += 1;
    }
    let mut right = px;
    steps = 0;
    while steps < MAX_EXTENT_SCAN && right + 1 <= right_bound && close(right + 1, py) {
        right += 1;
        steps += 1;
    }
    let mut top = py;
    steps = 0;
    while steps < MAX_EXTENT_SCAN && top - 1 >= top_bound && close(px, top - 1) {
        top -= 1;
        steps += 1;
    }
    let mut bottom = py;
    steps = 0;
    while steps < MAX_EXTENT_SCAN && bottom + 1 <= bottom_bound && close(px, bottom + 1) {
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
    let gx: Vec<i64> = st.guides_v.iter().map(|&x| (x * st.scale).round() as i64).collect();
    let gy: Vec<i64> = st.guides_h.iter().map(|&y| (y * st.scale).round() as i64).collect();
    let (l, r, t, b) = scan_extent(&st.img, px, py, TOLERANCE_LEVELS[st.tolerance_level].0, &gx, &gy);
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

fn copy_image_to_clipboard(png_bytes: &[u8]) {
    if let Ok(mut child) = Command::new("wl-copy").args(["--type", "image/png"]).stdin(Stdio::piped()).spawn() {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(png_bytes);
        }
        let _ = child.wait();
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn crop_rect(st: &State, rect: Rect) -> RgbaImage {
    let (l, t, r, b) = rect;
    let iw = st.img.width();
    let ih = st.img.height();
    let pl = ((l * st.scale).round() as i64).clamp(0, iw as i64 - 1) as u32;
    let pt = ((t * st.scale).round() as i64).clamp(0, ih as i64 - 1) as u32;
    let pr = ((r * st.scale).round() as i64).clamp(0, iw as i64) as u32;
    let pb = ((b * st.scale).round() as i64).clamp(0, ih as i64) as u32;
    let w = pr.saturating_sub(pl).max(1);
    let h = pb.saturating_sub(pt).max(1);
    image::imageops::crop_imm(&st.img, pl, pt, w, h).to_image()
}

fn crop_to_png_bytes(st: &State, rect: Rect) -> Option<Vec<u8>> {
    let cropped = crop_rect(st, rect);
    let mut buf = Cursor::new(Vec::new());
    cropped.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

fn timestamp() -> String {
    Command::new("date")
        .arg("+%Y-%m-%d_%H-%M-%S")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "output".to_string())
}

/// Crops the captured screenshot to `rect` (logical coords) to a temp file
/// and opens it in omasnap's annotation editor (`--file`, which loads an
/// existing image instead of capturing the screen). The crop is scratch
/// space, not a saved screenshot: omasnap's own Save/Copy actions are what
/// actually keep it (Save moves it into ~/Pictures/Screenshots itself and
/// sends its own confirmation notification), so closing the editor without
/// saving shouldn't leave a stray file behind. The temp file is removed the
/// moment the omasnap process exits, whichever way that happens — spawned
/// as one shell chain so cleanup runs independently of omaruler's own
/// lifetime (you may well hit Escape and quit before you're done in
/// omasnap). This is the mouse hover-and-click path; `s`/`c` (below) are a
/// separate, more direct pair of keyboard shortcuts that skip the editor.
fn open_in_omasnap(st: &State, rect: Rect) {
    let cropped = crop_rect(st, rect);
    let path = std::env::temp_dir().join(format!("{}-{}.png", APP_NAME, timestamp()));
    let Some(path) = path.to_str() else {
        flash_status("Failed to open image");
        return;
    };
    if cropped.save(path).is_err() {
        flash_status("Failed to open image");
        return;
    }

    let quoted = shell_quote(path);
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!("omasnap --file {quoted}; rm -f {quoted}"))
        .spawn();
}

/// `s` on an active selection: crop straight to ~/Pictures/Screenshots, no
/// editor detour. Matches Omarchy's own screenshot-tool naming convention.
fn save_selection_direct(st: &State, rect: Rect) {
    let Ok(home) = std::env::var("HOME") else {
        flash_status("Could not resolve $HOME");
        return;
    };
    let dir = format!("{}/Pictures/Screenshots", home);
    if std::fs::create_dir_all(&dir).is_err() {
        flash_status("Failed to create Pictures/Screenshots");
        return;
    }
    let path = format!("{}/{}-{}.png", dir, APP_NAME, timestamp());
    let cropped = crop_rect(st, rect);
    if cropped.save(&path).is_ok() {
        copy_to_clipboard(&path);
        flash_status("Screenshot saved");
    } else {
        flash_status("Failed to save image");
    }
}

/// `c` on an active selection: crop straight to the clipboard as image
/// data (`wl-copy --type image/png`), no file written at all.
fn copy_selection_direct(st: &State, rect: Rect) {
    match crop_to_png_bytes(st, rect) {
        Some(bytes) => {
            copy_image_to_clipboard(&bytes);
            flash_status("Screenshot copied");
        }
        None => flash_status("Failed to copy image"),
    }
}

fn point_in_rect(p: (f64, f64), r: Rect) -> bool {
    p.0 >= r.0 && p.0 <= r.2 && p.1 >= r.1 && p.1 <= r.3
}

/// The system cursor is rendered by the compositor via a low-latency
/// hardware-cursor path, completely separate from our own client-side
/// redraw — leaving it visible during precision work (hovering or
/// dragging) means there are always two pointers on screen, and ours will
/// always look laggy by comparison. Hide it while measuring; once at least
/// one rectangle has snapped there's no more precision tracking to do, and
/// a visible cursor makes it much easier to land a click on the small
/// hover-to-open box.
fn sync_cursor_visibility(window: &ApplicationWindow, st: &State) {
    // GuideH/GuideV use the standard row-resize/col-resize cursors (a
    // double-headed arrow with a bar through it) so placing a guide looks
    // and feels like dragging one out of a ruler in any other design tool.
    let name = match st.mode {
        Mode::Ruler if !st.snapped_rects.is_empty() => "default",
        Mode::GuideH => "row-resize",
        Mode::GuideV => "col-resize",
        _ => "none",
    };
    window.set_cursor_from_name(Some(name));
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

/// Omarchy's own shell (shell/Commons/Style.qml) keeps UI chrome at regular
/// weight — none of its Ui/ components set font.bold — with hierarchy
/// carried by size and color instead, at a 12px base with named steps
/// (bodySmall = 11px). Every label this app draws (the measurement
/// readout, the color hex, the legend, the tolerance badge) follows that:
/// regular weight, bodySmall size, no exceptions.
fn select_font(cr: &Context) {
    let _ = cr.select_font_face(
        "monospace",
        gtk4::cairo::FontSlant::Normal,
        gtk4::cairo::FontWeight::Normal,
    );
    cr.set_font_size(11.0);
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

const SMALL_FONT_SIZE: f64 = 8.0;

fn select_font_sized(cr: &Context, size: f64) {
    let _ = cr.select_font_face(
        "monospace",
        gtk4::cairo::FontSlant::Normal,
        gtk4::cairo::FontWeight::Normal,
    );
    cr.set_font_size(size);
}

fn measure_text_sized(cr: &Context, text: &str, size: f64) -> (f64, f64) {
    select_font_sized(cr, size);
    cr.text_extents(text)
        .map(|e| (e.x_advance(), e.height()))
        .unwrap_or((text.len() as f64 * size * 0.7, size))
}

/// Same idea as `draw_label_box` but at `SMALL_FONT_SIZE` — used for the
/// guide edge-distance readouts, which are meant to read as a quieter,
/// secondary measurement rather than the main W x H-style readout.
fn draw_small_label_box(cr: &Context, x: f64, y: f64, tw: f64, th: f64, text: &str, theme: &Theme) {
    select_font_sized(cr, SMALL_FONT_SIZE);
    let pad = 4.0;
    let (bg, fg, ac) = (theme.background, theme.foreground, theme.accent);

    cr.set_source_rgba(bg.0, bg.1, bg.2, 0.85);
    cr.rectangle(x, y, tw + pad * 2.0, th + pad * 2.0);
    let _ = cr.fill();

    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.35);
    cr.set_line_width(1.0);
    cr.rectangle(x, y, tw + pad * 2.0, th + pad * 2.0);
    let _ = cr.stroke();

    cr.set_source_rgba(fg.0, fg.1, fg.2, 0.9);
    cr.move_to(x + pad, y + pad + th);
    let _ = cr.show_text(text);
}

fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.0 < b.2 && a.2 > b.0 && a.1 < b.3 && a.3 > b.1
}

/// Draws a themed label, nudging it downward in fixed steps until it
/// doesn't overlap anything already placed this frame (tracked in
/// `occupied`, one accumulator per draw() call). Without this, a pinned
/// measure line's size label and the live hover crosshair's readout can
/// land right on top of each other whenever they happen to share the same
/// screen position.
fn place_label(cr: &Context, occupied: &mut Vec<Rect>, x: f64, y: f64, text: &str, theme: &Theme) -> Rect {
    let (tw, th) = measure_text(cr, text);
    let pad = 6.0;
    let box_w = tw + pad * 2.0;
    let box_h = th + pad * 2.0;
    let mut by = y;
    for _ in 0..20 {
        let candidate = (x, by, x + box_w, by + box_h);
        if !occupied.iter().any(|&o| rects_overlap(candidate, o)) {
            draw_label_box(cr, x, by, tw, th, text, theme);
            occupied.push(candidate);
            return candidate;
        }
        by += box_h + 4.0;
    }
    let candidate = (x, by, x + box_w, by + box_h);
    draw_label_box(cr, x, by, tw, th, text, theme);
    occupied.push(candidate);
    candidate
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

/// A pinned horizontal measure line — bounded to its own extent (unlike a
/// guide, which spans the full screen), with the size centered above it.
fn draw_pinned_h(cr: &Context, theme: &Theme, occupied: &mut Vec<Rect>, line: HLine) {
    let (y, l, r) = line;
    let ac = theme.measure;
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.95);
    cr.set_line_width(1.5);
    cr.move_to(l, y);
    cr.line_to(r, y);
    let _ = cr.stroke();
    for tx in [l, r] {
        cr.move_to(tx, y - 5.0);
        cr.line_to(tx, y + 5.0);
        let _ = cr.stroke();
    }
    let text = format!("{:.0}px", r - l);
    let (tw, th) = measure_text(cr, &text);
    let pad = 6.0;
    let bx = (l + r) / 2.0 - (tw + pad * 2.0) / 2.0;
    let by = y - th - pad * 2.0 - 8.0;
    place_label(cr, occupied, bx, by, &text, theme);
}

/// A pinned vertical measure line, label to the right, vertically centered.
fn draw_pinned_v(cr: &Context, theme: &Theme, occupied: &mut Vec<Rect>, line: VLine) {
    let (x, t, b) = line;
    let ac = theme.measure;
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.95);
    cr.set_line_width(1.5);
    cr.move_to(x, t);
    cr.line_to(x, b);
    let _ = cr.stroke();
    for ty in [t, b] {
        cr.move_to(x - 5.0, ty);
        cr.line_to(x + 5.0, ty);
        let _ = cr.stroke();
    }
    let text = format!("{:.0}px", b - t);
    place_label(cr, occupied, x + 10.0, (t + b) / 2.0, &text, theme);
}

fn draw_guide_h(cr: &Context, theme: &Theme, y: f64, screen_w: i32) {
    let gc = theme.guide;
    cr.set_source_rgba(gc.0, gc.1, gc.2, 0.7);
    cr.set_line_width(1.0);
    cr.move_to(0.0, y);
    cr.line_to(screen_w as f64, y);
    let _ = cr.stroke();
}

fn draw_guide_v(cr: &Context, theme: &Theme, x: f64, screen_h: i32) {
    let gc = theme.guide;
    cr.set_source_rgba(gc.0, gc.1, gc.2, 0.7);
    cr.set_line_width(1.0);
    cr.move_to(x, 0.0);
    cr.line_to(x, screen_h as f64);
    let _ = cr.stroke();
}

/// The not-yet-committed guide line while in `Mode::GuideH`/`GuideV`,
/// brighter and slightly thicker than a pinned guide so it reads as "still
/// following the cursor" rather than placed.
fn draw_guide_h_live(cr: &Context, theme: &Theme, y: f64, screen_w: i32) {
    let gc = theme.guide;
    cr.set_source_rgba(gc.0, gc.1, gc.2, 1.0);
    cr.set_line_width(1.5);
    cr.move_to(0.0, y);
    cr.line_to(screen_w as f64, y);
    let _ = cr.stroke();
}

fn draw_guide_v_live(cr: &Context, theme: &Theme, x: f64, screen_h: i32) {
    let gc = theme.guide;
    cr.set_source_rgba(gc.0, gc.1, gc.2, 1.0);
    cr.set_line_width(1.5);
    cr.move_to(x, 0.0);
    cr.line_to(x, screen_h as f64);
    let _ = cr.stroke();
}

/// For every gap between consecutive horizontal guides — including the top
/// and bottom screen edges as implicit boundaries — draws a small "gap
/// size" readout on the left edge of the screen, vertically centered in
/// that gap. Called both for the committed `guides_h` set (in Ruler mode)
/// and, with the live cursor position appended, as the placement preview
/// while in `Mode::GuideH`.
fn draw_guide_gaps_h(cr: &Context, theme: &Theme, guides: &[f64], screen_h: i32) {
    if guides.is_empty() {
        return;
    }
    let mut ys: Vec<f64> = guides.to_vec();
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut bounds = vec![0.0];
    bounds.extend(ys);
    bounds.push(screen_h as f64);
    for w in bounds.windows(2) {
        let (top, bottom) = (w[0], w[1]);
        let gap = bottom - top;
        if gap < 1.0 {
            continue;
        }
        let text = format!("{:.0}px", gap);
        let (tw, th) = measure_text_sized(cr, &text, SMALL_FONT_SIZE);
        let pad = 4.0;
        let bx = 8.0;
        let by = (top + bottom) / 2.0 - (th + pad * 2.0) / 2.0;
        draw_small_label_box(cr, bx, by, tw, th, &text, theme);
    }
}

/// The vertical-guide counterpart: gaps read out along the top edge,
/// horizontally centered in each gap.
fn draw_guide_gaps_v(cr: &Context, theme: &Theme, guides: &[f64], screen_w: i32) {
    if guides.is_empty() {
        return;
    }
    let mut xs: Vec<f64> = guides.to_vec();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut bounds = vec![0.0];
    bounds.extend(xs);
    bounds.push(screen_w as f64);
    for w in bounds.windows(2) {
        let (left, right) = (w[0], w[1]);
        let gap = right - left;
        if gap < 1.0 {
            continue;
        }
        let text = format!("{:.0}px", gap);
        let (tw, th) = measure_text_sized(cr, &text, SMALL_FONT_SIZE);
        let pad = 4.0;
        let by = 8.0;
        let bx = (left + right) / 2.0 - (tw + pad * 2.0) / 2.0;
        draw_small_label_box(cr, bx, by, tw, th, &text, theme);
    }
}

/// Shift held, no h/v: full-screen crosshair with no color computation and
/// no size readout — pure visual eye alignment, not a measurement.
fn draw_alignment_lines(cr: &Context, theme: &Theme, cx: f64, cy: f64, screen_w: i32, screen_h: i32) {
    let fg = theme.foreground;
    cr.set_source_rgba(fg.0, fg.1, fg.2, 0.6);
    cr.set_line_width(1.0);
    cr.move_to(cx, 0.0);
    cr.line_to(cx, screen_h as f64);
    let _ = cr.stroke();
    cr.move_to(0.0, cy);
    cr.line_to(screen_w as f64, cy);
    let _ = cr.stroke();
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

/// Draws one pinned selection rectangle: outline, size label (centered
/// inside it or below, whichever fits), and aspect ratio. Only the
/// `interactive` (most recent) selection swaps its size box for a camera
/// icon on hover and reports its bounds back for click-hit-testing — with
/// several selections on screen, only the newest one has an
/// affordance to "open in omasnap"; `s`/`c` (keyboard) are the more direct
/// way to act on it regardless of hover.
fn draw_selection_rect(cr: &Context, st: &State, rect: Rect, interactive: bool, cx: f64, cy: f64) -> Option<Rect> {
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

    if interactive && point_in_rect((cx, cy), bounds) {
        draw_camera_box(cr, bx, by, tw, th, &st.theme);
    } else {
        draw_label_box(cr, bx, by, tw, th, &size_text, &st.theme);
    }

    if let Some((num, den, exact)) = best_ratio(rect_w, rect_h) {
        let prefix = if exact { "" } else { "~" };
        let rtext = format!("{}{}:{}", prefix, num, den);
        let (rtw, rth) = measure_text(cr, &rtext);
        let stack_bottom = bounds.3.max(b);
        let rx = l + (rect_w - (rtw + pad * 2.0)) / 2.0;
        draw_label_box(cr, rx, stack_bottom + 8.0, rtw, rth, &rtext, &st.theme);
    }

    if interactive { Some(bounds) } else { None }
}

/// Fallback for when `omarchy-legend` isn't installed (e.g. this app
/// lands somewhere before the shell-side legend service does): the same
/// entries `legend_entries` would otherwise hand to the shell, drawn
/// locally as a plain two-column card in the top-right corner. No
/// hover-flip-to-the-other-corner like the shell version does — just
/// enough to not leave the hints missing entirely.
fn draw_builtin_legend(cr: &Context, theme: &Theme, screen_w: i32, entries: &[(&str, &str)]) {
    select_font(cr);
    let pad = 14.0;
    let row_gap = 10.0;
    let col_gap = 24.0;
    let margin = 14.0;

    let mut key_w = 0.0f64;
    let mut action_w = 0.0f64;
    let mut row_h = 0.0f64;
    for (key, action) in entries {
        let (kw, kh) = measure_text(cr, key);
        let (aw, ah) = measure_text(cr, action);
        key_w = key_w.max(kw);
        action_w = action_w.max(aw);
        row_h = row_h.max(kh).max(ah);
    }
    let content_w = key_w + col_gap + action_w;
    let content_h = entries.len() as f64 * row_h + entries.len().saturating_sub(1) as f64 * row_gap;
    let box_w = content_w + pad * 2.0;
    let box_h = content_h + pad * 2.0;
    let x = screen_w as f64 - box_w - margin;
    let y = margin;

    let (bg, fg, ac) = (theme.background, theme.foreground, theme.accent);
    cr.set_source_rgba(bg.0, bg.1, bg.2, 0.97);
    cr.rectangle(x, y, box_w, box_h);
    let _ = cr.fill();
    cr.set_source_rgba(ac.0, ac.1, ac.2, 0.5);
    cr.set_line_width(1.0);
    cr.rectangle(x, y, box_w, box_h);
    let _ = cr.stroke();

    let mut ty = y + pad;
    for (key, action) in entries {
        cr.set_source_rgba(ac.0, ac.1, ac.2, 1.0);
        cr.move_to(x + pad, ty + row_h);
        let _ = cr.show_text(key);
        cr.set_source_rgba(fg.0, fg.1, fg.2, 1.0);
        cr.move_to(x + pad + key_w + col_gap, ty + row_h);
        let _ = cr.show_text(action);
        ty += row_h + row_gap;
    }
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
            // Tracks every label box placed this frame so pinned lines'
            // readouts and the live crosshair box can steer clear of each
            // other instead of stacking on top of one another.
            let mut occupied: Vec<Rect> = Vec::new();

            for &gy in &st.guides_h {
                draw_guide_h(cr, &st.theme, gy, w);
            }
            for &gx in &st.guides_v {
                draw_guide_v(cr, &st.theme, gx, h);
            }
            draw_guide_gaps_h(cr, &st.theme, &st.guides_h, h);
            draw_guide_gaps_v(cr, &st.theme, &st.guides_v, w);
            for &line in &st.measure_lines_h {
                draw_pinned_h(cr, &st.theme, &mut occupied, line);
            }
            for &line in &st.measure_lines_v {
                draw_pinned_v(cr, &st.theme, &mut occupied, line);
            }

            let n = st.snapped_rects.len();
            for (i, &rect) in st.snapped_rects.iter().enumerate() {
                let interactive = i + 1 == n;
                let hb = draw_selection_rect(cr, st, rect, interactive, cx, cy);
                if hb.is_some() {
                    hover_box = hb;
                }
            }

            if st.dragging {
                if let Some(start) = st.start {
                    let rect = (start.0.min(cx), start.1.min(cy), start.0.max(cx), start.1.max(cy));
                    draw_rect_shape(cr, &st.theme, rect);
                    let text = format!("{:.0} x {:.0}", rect.2 - rect.0, rect.3 - rect.1);
                    place_size_box(cr, &st.theme, rect, &text, true);
                }
            } else if st.shift_held {
                draw_alignment_lines(cr, &st.theme, cx, cy, w, h);
            } else {
                let (l, t, r, b) = scan_extent_logical(st, cx, cy);
                draw_extent_bars(cr, &st.theme, cx, cy, l, r, t, b);
                let text = format!("{:.0} x {:.0}", r - l, b - t);
                place_label(cr, &mut occupied, cx + 18.0, cy + 18.0, &text, &st.theme);
            }
        }
        Mode::GuideH => {
            for &gy in &st.guides_h {
                draw_guide_h(cr, &st.theme, gy, w);
            }
            draw_guide_h_live(cr, &st.theme, cy, w);
            let mut all = st.guides_h.clone();
            all.push(cy);
            draw_guide_gaps_h(cr, &st.theme, &all, h);
        }
        Mode::GuideV => {
            for &gx in &st.guides_v {
                draw_guide_v(cr, &st.theme, gx, h);
            }
            draw_guide_v_live(cr, &st.theme, cx, h);
            let mut all = st.guides_v.clone();
            all.push(cx);
            draw_guide_gaps_v(cr, &st.theme, &all, w);
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

    if let Some(duck) = &st.duck {
        draw_duck(cr, duck);
        draw_aim_circle(cr, &st.theme, cx, cy);
    }

    if st.duck_waiting_continue {
        draw_center_prompt(cr, &st.theme, w, h, "Click to continue");
    }

    if let Some(msg) = &st.last_message {
        draw_label(cr, 16.0, h as f64 - 64.0, msg, &st.theme);
    }

    if !st.legend_available {
        if let Some(entries) = legend_entries(st) {
            draw_builtin_legend(cr, &st.theme, w, entries);
        }
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
        .title(DISPLAY_NAME)
        .build();

    // GTK4's default theme applies an implicit opacity transition to
    // top-level windows on map, which reads as a brief fade-in — on top of
    // the Hyprland layer_rule (bindings.lua) that already disables the
    // compositor's own open/close animation for this namespace. This is a
    // one-shot overlay meant to appear the instant the keybind is pressed,
    // so both layers of animation are killed.
    let css = gtk4::CssProvider::new();
    css.load_from_data("window { transition: none; }");
    gtk4::style_context_add_provider_for_display(
        &gtk4::prelude::WidgetExt::display(&window),
        &css,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

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
        last_raw_cursor: (0.0, 0.0),
        mode: Mode::Ruler,
        dragging: false,
        start: None,
        snapped_rects: Vec::new(),
        hover_box: None,
        snap_enabled: true,
        tolerance_level: DEFAULT_TOLERANCE_LEVEL,
        show_legend: true,
        legend_available: command_exists("omarchy-legend"),
        shift_held: false,
        measure_lines_h: Vec::new(),
        measure_lines_v: Vec::new(),
        guides_h: Vec::new(),
        guides_v: Vec::new(),
        undo_stack: Vec::new(),
        last_message: None,
        duck: None,
        duck_next_spawn: None,
        duck_waiting_continue: false,
        duck_score: 0,
        duck_high_score: load_high_score(),
        sounds: load_sounds(),
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
    // truly is *right now*. It also only syncs `cursor` from the mouse when
    // the raw polled position actually changed since last frame — arrow-key
    // nudges set `cursor` directly and are otherwise indistinguishable from
    // "the mouse didn't move", so this is what lets a keyboard nudge stick
    // instead of being clobbered by the next frame's poll of an unmoved
    // mouse.
    {
        let state = state.clone();
        area.add_tick_callback(move |area, _clock| {
            if let Some(surface) = area.native().and_then(|n| n.surface()) {
                let display = gtk4::prelude::WidgetExt::display(area);
                if let Some(device) = display.default_seat().and_then(|s| s.pointer()) {
                    if let Some((x, y, _mask)) = surface.device_position(&device) {
                        let mut st = state.borrow_mut();
                        let raw_changed = (x, y) != st.last_raw_cursor;
                        st.last_raw_cursor = (x, y);
                        if raw_changed {
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
            }

            // Easter egg: the duck flies continuously (not just in
            // response to cursor motion), so it needs its own unconditional
            // per-frame update independent of the pointer-driven redraw
            // logic above. Also handles the post-kill/post-miss respawn
            // timer and the "flew away untouched" timeout.
            {
                let mut st = state.borrow_mut();
                let now = std::time::Instant::now();

                if st.duck.is_none() && st.duck_next_spawn.is_some_and(|t| now >= t) {
                    let screen_w = st.img.width() as f64 / st.scale;
                    let screen_h = st.img.height() as f64 / st.scale;
                    st.duck = Some(spawn_duck(screen_w, screen_h));
                    st.duck_next_spawn = None;
                }

                let mut miss_message = None;
                let mut fail_path = None;
                let screen_w = st.img.width() as f64 / st.scale;
                let screen_h = st.img.height() as f64 / st.scale;
                // A duck is lost for good either by outrunning the 15s
                // clock or by reaching an edge and flying off — no bounce,
                // so a miss costs the whole run, not just that one duck.
                let escaped = st.duck.as_ref().is_some_and(|d| {
                    d.spawned.elapsed() >= DUCK_LIFETIME || duck_offscreen(d, screen_w, screen_h)
                });
                if escaped {
                    st.duck = None;
                    st.duck_waiting_continue = true;
                    // omarchy-osd elides non-media messages past ~22
                    // characters (Osd.qml's maxMessageWidth) — confirmed
                    // "The duck got away! Final score: N" was truncating to
                    // "The duck got away! Fi…" in practice, so this stays
                    // short rather than descriptive.
                    miss_message = Some(format!("Missed! Score: {}", st.duck_score));
                    fail_path = Some(st.sounds.fail.clone());
                    st.duck_score = 0;
                } else if let Some(duck) = st.duck.as_mut() {
                    update_duck(duck);
                }

                let should_redraw = st.duck.is_some() || st.duck_next_spawn.is_some() || miss_message.is_some();
                drop(st);
                if let Some(path) = fail_path {
                    play_sound(&path);
                }
                if let Some(msg) = miss_message {
                    flash_status(&msg);
                }
                if should_redraw {
                    area.queue_draw();
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
            if st.duck_waiting_continue {
                st.duck_waiting_continue = false;
                let screen_w = st.img.width() as f64 / st.scale;
                let screen_h = st.img.height() as f64 / st.scale;
                st.duck = Some(spawn_duck(screen_w, screen_h));
                let chime_path = st.sounds.chime.clone();
                drop(st);
                play_sound(&chime_path);
                area.queue_draw();
                return;
            }
            if st.duck.is_some() {
                // The trigger fires whether or not the shot connects.
                let shot_path = st.sounds.shot.clone();
                play_sound(&shot_path);
            }
            if st.duck.as_ref().is_some_and(|d| duck_hit(d, st.cursor)) {
                st.duck = None;
                st.duck_next_spawn = Some(std::time::Instant::now() + DUCK_RESPAWN_DELAY);
                st.duck_score += 1;
                let msg = if st.duck_score > st.duck_high_score {
                    st.duck_high_score = st.duck_score;
                    save_high_score(st.duck_high_score);
                    // Same OSD width limit as the miss message above.
                    format!("🦆 High score: {}!", st.duck_score)
                } else {
                    format!("🦆 Quack! Score: {}", st.duck_score)
                };
                let quack_path = st.sounds.quack.clone();
                drop(st);
                play_sound(&quack_path);
                flash_status(&msg);
                area.queue_draw();
                return;
            }
            match st.mode {
                Mode::Ruler => {
                    let hovering_open = !st.snapped_rects.is_empty()
                        && st.hover_box.map_or(false, |hb| point_in_rect(st.cursor, hb));
                    if hovering_open {
                        if let Some(&rect) = st.snapped_rects.last() {
                            open_in_omasnap(&st, rect);
                        }
                    } else {
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
                Mode::GuideH => {
                    // No color-edge snapping here (for now) — it was landing
                    // guides in surprising places on noisy/gradient
                    // backgrounds. Placed exactly where the cursor is.
                    let y = st.cursor.1;
                    st.guides_h.push(y);
                    st.undo_stack.push(UndoItem::GuideH);
                    st.mode = Mode::Ruler;
                    refresh_legend(&st);
                }
                Mode::GuideV => {
                    let x = st.cursor.0;
                    st.guides_v.push(x);
                    st.undo_stack.push(UndoItem::GuideV);
                    st.mode = Mode::Ruler;
                    refresh_legend(&st);
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
                        let (nl, nt, nr, nb) = if st.snap_enabled {
                            let tol = TOLERANCE_LEVELS[st.tolerance_level].0;
                            shrink_rect(&st.img, prect, tol)
                        } else {
                            prect
                        };
                        st.snapped_rects.push((nl as f64 / scale, nt as f64 / scale, nr as f64 / scale, nb as f64 / scale));
                        st.undo_stack.push(UndoItem::Rect);
                        refresh_legend(&st);
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
        key.connect_key_pressed(move |_c, keyval, _keycode, modifier| {
            let shift = modifier.contains(gdk::ModifierType::SHIFT_MASK);
            let ctrl = modifier.contains(gdk::ModifierType::CONTROL_MASK);
            match keyval {
                gdk::Key::Escape => {
                    let mut st = state.borrow_mut();
                    if matches!(st.mode, Mode::GuideH | Mode::GuideV) {
                        st.mode = Mode::Ruler;
                        sync_cursor_visibility(&window, &st);
                        refresh_legend(&st);
                        drop(st);
                        area.queue_draw();
                        return glib::Propagation::Stop;
                    }
                    let has_anything = st.duck.is_some()
                        || st.duck_next_spawn.is_some()
                        || st.duck_waiting_continue
                        || (st.mode == Mode::Ruler
                            && (st.dragging
                                || !st.snapped_rects.is_empty()
                                || !st.guides_h.is_empty()
                                || !st.guides_v.is_empty()
                                || !st.measure_lines_h.is_empty()
                                || !st.measure_lines_v.is_empty()));
                    if has_anything {
                        st.dragging = false;
                        st.start = None;
                        st.snapped_rects.clear();
                        st.guides_h.clear();
                        st.guides_v.clear();
                        st.measure_lines_h.clear();
                        st.measure_lines_v.clear();
                        st.undo_stack.clear();
                        st.duck = None;
                        st.duck_next_spawn = None;
                        st.duck_waiting_continue = false;
                        st.duck_score = 0;
                        sync_cursor_visibility(&window, &st);
                        refresh_legend(&st);
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
                    if st.mode == Mode::Ruler && !st.snapped_rects.is_empty() {
                        if let Some(&rect) = st.snapped_rects.last() {
                            copy_selection_direct(&st, rect);
                        }
                        drop(st);
                        return glib::Propagation::Stop;
                    }
                    st.mode = if st.mode == Mode::Ruler { Mode::Color } else { Mode::Ruler };
                    st.dragging = false;
                    st.start = None;
                    st.snapped_rects.clear();
                    st.last_message = None;
                    sync_cursor_visibility(&window, &st);
                    refresh_legend(&st);
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::s | gdk::Key::S => {
                    let st = state.borrow();
                    if st.mode == Mode::Ruler && !st.snapped_rects.is_empty() {
                        if let Some(&rect) = st.snapped_rects.last() {
                            save_selection_direct(&st, rect);
                        }
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::n | gdk::Key::N => {
                    let mut st = state.borrow_mut();
                    st.snap_enabled = !st.snap_enabled;
                    let msg = if st.snap_enabled { "Snap: On" } else { "Snap: Off" };
                    drop(st);
                    flash_status(msg);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::r | gdk::Key::R => {
                    let mut st = state.borrow_mut();
                    st.mode = Mode::Ruler;
                    st.start = None;
                    st.dragging = false;
                    st.snapped_rects.clear();
                    st.guides_h.clear();
                    st.guides_v.clear();
                    st.measure_lines_h.clear();
                    st.measure_lines_v.clear();
                    st.undo_stack.clear();
                    st.duck = None;
                    st.duck_next_spawn = None;
                    st.duck_waiting_continue = false;
                    st.duck_score = 0;
                    sync_cursor_visibility(&window, &st);
                    refresh_legend(&st);
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::z | gdk::Key::Z => {
                    if !ctrl {
                        return glib::Propagation::Proceed;
                    }
                    let mut st = state.borrow_mut();
                    if let Some(item) = st.undo_stack.pop() {
                        match item {
                            UndoItem::Rect => { st.snapped_rects.pop(); }
                            UndoItem::GuideH => { st.guides_h.pop(); }
                            UndoItem::GuideV => { st.guides_v.pop(); }
                            UndoItem::MeasureH => { st.measure_lines_h.pop(); }
                            UndoItem::MeasureV => { st.measure_lines_v.pop(); }
                        }
                    }
                    sync_cursor_visibility(&window, &st);
                    refresh_legend(&st);
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::t | gdk::Key::T => {
                    let mut st = state.borrow_mut();
                    st.tolerance_level = (st.tolerance_level + 1) % TOLERANCE_LEVELS.len();
                    let name = TOLERANCE_LEVELS[st.tolerance_level].1;
                    drop(st);
                    flash_tolerance(name);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::minus | gdk::Key::underscore => {
                    let mut st = state.borrow_mut();
                    st.tolerance_level = st.tolerance_level.saturating_sub(1);
                    let name = TOLERANCE_LEVELS[st.tolerance_level].1;
                    drop(st);
                    flash_tolerance(name);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::equal | gdk::Key::plus => {
                    let mut st = state.borrow_mut();
                    st.tolerance_level = (st.tolerance_level + 1).min(TOLERANCE_LEVELS.len() - 1);
                    let name = TOLERANCE_LEVELS[st.tolerance_level].1;
                    drop(st);
                    flash_tolerance(name);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::l | gdk::Key::L => {
                    let mut st = state.borrow_mut();
                    st.show_legend = !st.show_legend;
                    refresh_legend(&st);
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                // Easter egg — deliberately not in the legend.
                gdk::Key::d | gdk::Key::D => {
                    let mut st = state.borrow_mut();
                    let playing = st.duck.is_some() || st.duck_next_spawn.is_some() || st.duck_waiting_continue;
                    let mut msg = None;
                    let mut chime = false;
                    if playing {
                        st.duck = None;
                        st.duck_next_spawn = None;
                        st.duck_waiting_continue = false;
                        if st.duck_score > 0 {
                            msg = Some(format!("Final score: {}", st.duck_score));
                        }
                        st.duck_score = 0;
                    } else {
                        st.duck_score = 0;
                        let screen_w = st.img.width() as f64 / st.scale;
                        let screen_h = st.img.height() as f64 / st.scale;
                        st.duck = Some(spawn_duck(screen_w, screen_h));
                        chime = true;
                    }
                    let chime_path = st.sounds.chime.clone();
                    drop(st);
                    if chime {
                        play_sound(&chime_path);
                    }
                    if let Some(msg) = msg {
                        flash_status(&msg);
                    }
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::h | gdk::Key::H => {
                    let mut st = state.borrow_mut();
                    if shift {
                        // Allowed from Ruler or from the other guide axis —
                        // so switching straight from placing a vertical
                        // guide to a horizontal one doesn't require
                        // escaping/committing first.
                        if !matches!(st.mode, Mode::Ruler | Mode::GuideH | Mode::GuideV) {
                            return glib::Propagation::Proceed;
                        }
                        st.mode = Mode::GuideH;
                        sync_cursor_visibility(&window, &st);
                        refresh_legend(&st);
                    } else {
                        if st.mode != Mode::Ruler {
                            return glib::Propagation::Proceed;
                        }
                        let (l, _t, r, _b) = scan_extent_logical(&st, st.cursor.0, st.cursor.1);
                        let y = st.cursor.1;
                        st.measure_lines_h.push((y, l, r));
                        st.undo_stack.push(UndoItem::MeasureH);
                    }
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::v | gdk::Key::V => {
                    let mut st = state.borrow_mut();
                    if shift {
                        if !matches!(st.mode, Mode::Ruler | Mode::GuideH | Mode::GuideV) {
                            return glib::Propagation::Proceed;
                        }
                        st.mode = Mode::GuideV;
                        sync_cursor_visibility(&window, &st);
                        refresh_legend(&st);
                    } else {
                        if st.mode != Mode::Ruler {
                            return glib::Propagation::Proceed;
                        }
                        let (_l, t, _r, b) = scan_extent_logical(&st, st.cursor.0, st.cursor.1);
                        let x = st.cursor.0;
                        st.measure_lines_v.push((x, t, b));
                        st.undo_stack.push(UndoItem::MeasureV);
                    }
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::Shift_L | gdk::Key::Shift_R => {
                    state.borrow_mut().shift_held = true;
                    area.queue_draw();
                    glib::Propagation::Proceed
                }
                gdk::Key::Up => {
                    let mut st = state.borrow_mut();
                    let step = if shift { 10.0 } else { 1.0 };
                    st.cursor.1 -= step;
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::Down => {
                    let mut st = state.borrow_mut();
                    let step = if shift { 10.0 } else { 1.0 };
                    st.cursor.1 += step;
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::Left => {
                    let mut st = state.borrow_mut();
                    let step = if shift { 10.0 } else { 1.0 };
                    st.cursor.0 -= step;
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                gdk::Key::Right => {
                    let mut st = state.borrow_mut();
                    let step = if shift { 10.0 } else { 1.0 };
                    st.cursor.0 += step;
                    drop(st);
                    area.queue_draw();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
    }
    {
        let state = state.clone();
        let area = area.clone();
        key.connect_key_released(move |_c, keyval, _keycode, _modifier| {
            if matches!(keyval, gdk::Key::Shift_L | gdk::Key::Shift_R) {
                state.borrow_mut().shift_held = false;
                area.queue_draw();
            }
        });
    }
    window.add_controller(key);

    // Whichever way the window closes, make sure the shell-rendered legend
    // doesn't linger after this process is gone — omarchy-legend has no
    // auto-hide timer (unlike omarchy-osd) since it's meant to persist
    // alongside the calling app, so it's this app's job to clean it up.
    // Nothing to do here when the built-in fallback is in play: that's
    // drawn straight from state and disappears the instant this window
    // does, no separate hide call needed.
    {
        let state = state.clone();
        window.connect_close_request(move |_| {
            if state.borrow().legend_available {
                hide_legend_card();
            }
            glib::Propagation::Proceed
        });
    }

    window.set_child(Some(&overlay));
    window.present();
    let st = state.borrow();
    if st.legend_available {
        if let Some(entries) = legend_entries(&st) {
            show_legend_entries(entries);
        }
    }
}

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}
