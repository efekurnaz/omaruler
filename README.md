# Omaruler

A [PixelSnap](https://getpixelsnap.com/)-style measuring ruler and color
picker for Omarchy / Hyprland: hover to auto-measure the padding under your
cursor, drag a rectangle and have it snap tightly to whatever's inside it,
save that selection as an image, or pick a color.

## How it works

- Reads the active output and cursor position via `hyprctl`.
- Grabs a screenshot of that output with `grim`, captured as uncompressed
  PPM straight to stdout (`grim -t ppm -o <output> -`, parsed by a small
  built-in decoder) rather than PNG to a temp file — PNG's compress/decode
  round trip on a full-screen image was most of this app's launch latency
  for no benefit, since the screenshot is immediately decoded back into raw
  pixels anyway.
- Runs a one-time Sobel edge-detection pass over it, used to magnet-snap the
  cursor to nearby edges — split across all available CPU cores, since it's
  the one substantial per-pixel computation this app does and it happens
  fresh on every launch.
- Fetches the theme colors (`omarchy-theme-color`) with all 5 lookups
  spawned before waiting on any of them, rather than one at a time.
- Opens a `wlr-layer-shell` overlay (via `gtk4-layer-shell`) pinned to that
  output. The screenshot is handed to GTK as a `GdkTexture` and drawn by a
  `Picture` widget — composited by the scene graph for free every frame — with
  a transparent `DrawingArea` on top for the thin lines/text, which is what
  keeps the redraw cheap regardless of screen resolution. A small CSS
  override (`window { transition: none; }`) kills GTK's default window-open
  fade, on top of the `no_anim` Hyprland layer_rule in `bindings.lua` — this
  is a one-shot overlay meant to appear the instant the keybind is pressed,
  not ease in.
- The pointer position is polled once per compositor frame (via
  `gdk::Surface::device_position`, from a frame-clock tick callback) rather
  than reacted to as motion events, and the system cursor is hidden while the
  overlay is active — otherwise there are always two pointers on screen and
  the hand-drawn one will always look like it's lagging behind the real one.
- Colors (measuring lines, labels) are pulled from the active Omarchy theme
  via `omarchy-theme-color`, so it matches whatever theme is set system-wide.
- The tolerance toggle and the shortcut-hint card are rendered by the
  Omarchy shell itself, not by Omaruler — `omarchy-osd` for the transient
  tolerance flash, and `omarchy-legend` (a small shell service added
  alongside Omaruler, since nothing like it existed) for the hint card.
  Themed and positioned by the shell, so they stay in sync with the active
  theme automatically instead of being hand-drawn in Cairo. See
  `shell/plugins/legend/` and `shell/plugins/osd/` in the Omarchy repo.
  `omarchy-legend` is checked for on `$PATH` once at startup; if it isn't
  there yet (this app may well land before that shell service does), the
  exact same hint entries are drawn locally in Cairo instead — same
  content, just without the shell's theming/hover-flip. `omarchy-osd` has
  no such fallback since it already ships on stock Omarchy today.

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
snapped tight to the object. The live crosshair stays active while a
rectangle is snapped, so you can keep dragging out more rectangles to compare
several elements at once — each one keeps its own snap-to-content result.
This shrink-to-fit trim can be turned off (`n`) if you want the rectangle
exactly as dragged.

**Hover the last snapped box**: hovering its size readout swaps it to a
camera icon — click to open the crop in `omasnap`'s editor. With a selection
active, `c` copies it straight to the clipboard and `s` saves it to
`~/Pictures/Screenshots/omaruler-<timestamp>.png` (path copied to clipboard),
each confirmed by a flashed `omarchy-osd` message instead of a desktop
notification.

**Pinned measure lines** (`h` / `v`): drop a horizontal or vertical line at
the cursor, auto-extended by color continuity the same way the idle
crosshair is, with its length labeled above/beside it. Lines stay on screen
so you can pin several at once and read them all together; labels nudge
out of each other's way if two would otherwise overlap.

**Guides** (Shift+`h` / Shift+`v`): enters guide-placement mode — the cursor
switches to a row-resize/col-resize icon, and a line follows it across the
full width (or height) of the screen. While placing, the space on either
side of the line is read out in small type on the left edge of the screen
(top edge for a vertical guide), vertically/horizontally centered in each
segment — same idea as dragging a guide off the ruler in Figma or Sketch.
Click to commit it at the exact cursor position (no color-edge snapping —
it made for surprising placement on noisy/gradient backgrounds) and mode
returns to Ruler. Shift+`h`/Shift+`v` also switch directly between the two
axes without needing to commit or cancel first. `Escape` cancels placement
without adding one. Guides render in a distinct color from measure lines,
and the same edge-distance readout stays pinned on screen for every
committed guide, not just the one being placed. Guides also bound every
other measurement: the idle crosshair and pinned measure lines will stop at
the nearest guide instead of reading past it, even where there's no real
color change there.

**Alignment lines** (hold Shift): a full-screen crosshair through the
cursor for eyeballing visual alignment — no color sampling, no readout, just
two lines. Disappears the moment Shift is released.

**Nudge the cursor**: arrow keys move it 1px at a time; hold Shift for
10px steps. Useful for lining a measurement or guide up exactly once you're
close.

**Color mode** (`c`): magnified pixel loupe with hex readout; click to copy.

## Controls

| Input | Action |
|---|---|
| Hover | Live padding/gap measurement (auto color-continuity extent) |
| Click + drag | Draw a selection rectangle, snaps to content on release — repeatable for multi-select |
| Hover snapped box | Shows a camera button — click to open in `omasnap` |
| `h` / `v` | Pin a horizontal / vertical measure line at the cursor |
| Shift+`h` / Shift+`v` | Enter guide-placement mode (switches axes directly into each other); click to commit at the cursor, `Escape` to cancel |
| Hold Shift | Full-screen alignment crosshair (no measurement) |
| Arrow keys | Nudge cursor 1px (hold Shift for 10px) |
| `c` | Copy the active selection, or toggle color-picker mode if none |
| `s` | Save the active selection to disk |
| `n` | Toggle snap: cursor magnet-snap to edges, and shrink-to-fit on dragged rectangles |
| Click (color mode) | Copy the hex color under the cursor |
| `t` | Cycle color tolerance: off / low / med / high |
| `-` / `=` | Step color tolerance down / up |
| `Ctrl+Z` | Undo the last pinned line, guide, or rectangle |
| `r` | Reset all selections, lines, and guides |
| `l` | Show/hide the hint legend (`omarchy-legend`) |
| `Escape` | Clear all selections, lines, and guides if any exist, else quit |

Tolerance changes and saved/copied screenshots are all flashed via
`omarchy-osd` rather than a desktop notification, matching how the rest of
the Omarchy shell surfaces transient state.

## Build

```sh
cargo build --release
cp target/release/omaruler ~/.local/bin/
```

Requires: `gtk4` (with the GTK C library at 4.8+), `gtk4-layer-shell`,
`grim`, `wl-copy`, `hyprctl`, `omarchy-theme-color`, and `omarchy-osd` (all
present on stock Omarchy). `omarchy-legend` (new — from the
`omarchy-legend-service` branch/PR until it lands upstream) is optional:
Omaruler falls back to drawing the hint card itself when it's missing.

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
- Measure lines snap to color edges the same way the cursor does — there's
  no layout/DOM awareness, so a busy background can produce one in the
  wrong place, same caveat as the shrink-to-fit step. Guides don't snap at
  all right now (placed exactly at the cursor) — color-edge snapping for
  guides was tried and pulled back for landing in surprising places; may
  come back in a different form later.
