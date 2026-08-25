# omeasure

A [PixelSnap](https://getpixelsnap.com/)-style measuring ruler and color
picker for Omarchy / Hyprland: hover to auto-measure the padding under your
cursor, drag a rectangle and have it snap tightly to whatever's inside it,
save that selection as an image, or pick a color.

## How it works

- Reads the active output and cursor position via `hyprctl`.
- Grabs a screenshot of that output with `grim`, decoded once into an RGBA
  buffer.
- Runs a one-time Sobel edge-detection pass over it, used to magnet-snap the
  cursor to nearby edges.
- Opens a `wlr-layer-shell` overlay (via `gtk4-layer-shell`) pinned to that
  output. The screenshot is handed to GTK as a `GdkTexture` and drawn by a
  `Picture` widget — composited by the scene graph for free every frame — with
  a transparent `DrawingArea` on top for the thin lines/text, which is what
  keeps the redraw cheap regardless of screen resolution.
- The pointer position is polled once per compositor frame (via
  `gdk::Surface::device_position`, from a frame-clock tick callback) rather
  than reacted to as motion events, and the system cursor is hidden while the
  overlay is active — otherwise there are always two pointers on screen and
  the hand-drawn one will always look like it's lagging behind the real one.
- Colors (measuring lines, labels) are pulled from the active Omarchy theme
  via `omarchy-theme-color`, so it matches whatever theme is set system-wide.

No continuous screen capture, no background daemon — it launches, does its
job, and exits.

## Modes

**Idle (default)**: hovering shows the horizontal/vertical extent of the
color-continuous region under the cursor — the width of a padding or gap,
read live as you move, no clicking needed.

**Drag a rectangle**: click-drag draws a box with a live `W x H` readout
(centered inside it, or below it if it's too small to fit) and a best-fit
small-integer aspect ratio underneath (`16:9`, or `~16:9` if it's only an
approximation; hidden if nothing close fits in the 1–21 range). On release,
each of the box's 4 edges independently shrinks inward — using the majority
background color it's sitting on as its own reference — until the *entire*
row or column stops matching, the same idea as ImageMagick's `-trim -fuzz` or
GIMP's Autocrop. A loose box around an object on a plain background ends up
snapped tight to the object.

**Hover the snapped box**: once a rectangle has snapped, hovering its size
readout swaps it to a camera icon — click to crop the screenshot to that
rectangle and save it to `~/Pictures/Screenshots/omeasure-<timestamp>.png`
(path copied to clipboard, confirmed by notification).

**Color mode** (`c`): magnified pixel loupe with hex readout; click to copy.

## Controls

| Input | Action |
|---|---|
| Hover | Live padding/gap measurement (auto color-continuity extent) |
| Click + drag | Draw a selection rectangle, snaps to content on release |
| Hover snapped box | Shows a camera button — click to save that area as a PNG |
| `t` | Cycle color tolerance: off / low / med / high (shown in the legend) |
| `c` | Toggle color-picker mode (magnified pixel loupe) |
| Click (color mode) | Copy the hex color under the cursor |
| `s` | Toggle cursor edge-snapping |
| `r` | Reset current selection |
| `l` | Show/hide the hint legend |
| `Escape` | Reset an active selection, or quit if idle |

## Build

```sh
cargo build --release
cp target/release/omeasure ~/.local/bin/
```

Requires (all present on stock Omarchy): `gtk4` (with the GTK C library at
4.8+), `gtk4-layer-shell`, `grim`, `wl-copy`, `hyprctl`,
`omarchy-theme-color`, `omarchy-notification-send`.

## Keybind

Bound to `SUPER + SHIFT + U` in `~/.config/hypr/bindings.lua` (with
`SUPER + SHIFT + PRINT` as a fallback for a real PC keyboard) — not a bare
`PRINT`-key bind, since Apple keyboards (laptop or external Magic Keyboard)
have never had a Print Screen key.

## Known limitations

- Single monitor only — the output under the cursor at launch time. A
  selection can't be dragged across a monitor boundary.
- The shrink-to-fit step assumes a reasonably uniform background near each
  edge; it won't do well against a busy/textured background.
- No multi-element alignment detection (PixelSnap's "smart guides" across
  several elements at once).
