# pixel-snap-omarchy

A [PixelSnap](https://getpixelsnap.com/)-style measuring ruler and color
picker for Omarchy / Hyprland. Fullscreen overlay, edge-snapping ruler,
zoomed pixel loupe with hex/RGB, click to copy.

## How it works

- Reads the active output and cursor position via `hyprctl`.
- Grabs a screenshot of that output with `grim`.
- Runs a one-time Sobel edge-detection pass over the captured frame.
- Opens a `wlr-layer-shell` overlay (via `gtk4-layer-shell`) pinned to that
  output, with the screenshot as its background and the ruler/loupe drawn
  on top with Cairo.
- The cursor magnet-snaps to the strongest nearby edge in the pre-computed
  gradient map, so dragging naturally catches UI edges the way PixelSnap
  does on macOS.

No continuous screen capture, no background daemon — it launches, does its
job, and exits.

## Controls

| Input | Action |
|---|---|
| Click + drag | Measure distance (px, dx, dy) between two points, edge-snapped |
| Release | Copies the measurement text to the clipboard |
| `c` | Toggle color-picker mode (magnified pixel loupe) |
| Click (color mode) | Copy the hex color under the cursor |
| `s` | Toggle edge-snapping on/off |
| `r` | Reset the current measurement |
| `Escape` | Quit |

## Build

```sh
cargo build --release
cp target/release/pixel-snap ~/.local/bin/
```

Requires (all present on stock Omarchy): `gtk4`, `gtk4-layer-shell`,
`grim`, `wl-copy`, `hyprctl`.

## Keybind

Bound to `SUPER + SHIFT + PRINT` in `~/.config/hypr/bindings.lua`,
alongside the rest of the capture-key family (`PRINT` = screenshot,
`SUPER+PRINT` = color picker, `ALT+PRINT` = screenrecord,
`SUPER+CTRL+PRINT` = OCR).

## Known limitations (v1)

- Single monitor only — the output under the cursor at launch time.
  Dragging a measurement across a monitor boundary isn't supported.
- No persistent/pinned guides, no multi-element alignment detection
  (PixelSnap's "smart guides" across several elements at once).
- Edge-snap radius and threshold are fixed constants (`radius = 6px`,
  `threshold = 40.0`) rather than user-configurable.
