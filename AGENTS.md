# Omaruler — Agent Guide

Omaruler is a PixelSnap-style measuring ruler and color picker overlay for
Omarchy/Hyprland: hover to auto-measure the padding under your cursor, drag a
rectangle and have it snap tightly to whatever's inside it, pin guides and
measure lines, save/copy a selection, or pick a color. It launches, does its
job, and exits — no background daemon, no continuous screen capture.

## Project principles

- **Speed first.** Startup is a user-visible delay on every launch, not a
  one-time cost — profile it for real (see `git log` for the PPM/stdout
  capture switch that cut ~1.2s to ~200ms) rather than assuming. No
  background daemon, no continuous capture.
- **Wayland/Hyprland only.** Monitor and cursor discovery via `hyprctl`,
  rendering via `gtk4-layer-shell`. No X11, no generic-compositor
  fallback.
- **Minimal dependencies.** Shell out to existing system tools (`grim`,
  `wl-copy`, `paplay`, `omarchy-theme-color`, `omarchy-osd`,
  `omarchy-legend`) and hand-roll small pieces (the PPM decoder, the
  xorshift PRNG behind the duck easter egg) rather than add a crate for a
  small need.
- **Single file.** Everything lives in `src/main.rs`. Keep it that way —
  don't split into a module maze for its own sake.
- **Omarchy-native, gracefully degrading.** Theme colors come from
  `omarchy-theme-color`; the shortcut legend and transient status flashes
  come from the `omarchy-legend`/`omarchy-osd` shell services. Since
  `omarchy-legend` is a companion service this project helped originate
  and may not be installed everywhere yet, its absence is detected at
  startup and the exact same hint content is drawn locally in Cairo
  instead (`legend_entries`/`draw_builtin_legend`) — never a hard
  dependency.
- **No backwards-compatibility shims.** Break keybindings or behavior
  whenever it makes the tool simpler or more correct. Don't keep a
  deprecated key/flag around "just in case."

## Repository layout

| Path | Purpose |
|---|---|
| `src/main.rs` | Everything: screen capture, the GTK4/layer-shell overlay, Ruler/Color/Guide modes, rectangle snap-to-content, the duck-hunt easter egg, and the `#[cfg(test)] mod smoke` test suite |
| `assets/sounds/*.wav` | Procedurally-synthesized duck-hunt sound effects (shot/quack/fail/chime), embedded into the binary via `include_bytes!` |
| `assets/sounds/synth_sounds.py` | Regenerates the WAV files above — pure-Python sine/sawtooth/noise synthesis, no sample library |
| `README.md` | User-facing features, controls, build/install instructions |
| `Cargo.toml` | Dependencies and the release build profile |
| `committed.toml` | Conventional Commits rules enforced by `.githooks/commit-msg` |
| `cliff.toml` | `git-cliff` changelog config, driven by the same Conventional Commits history |
| `.github/workflows/ci.yml` | Build + `cargo test` on every push/PR, inside an Arch container |
| `.github/workflows/release.yml` | Builds and publishes a GitHub Release (binary + changelog) on every `vX.Y.Z` tag push |

## Commit messages

Every commit must follow [Conventional Commits](https://www.conventionalcommits.org/):
`type: Subject` (or `type(scope): Subject`), imperative mood, capitalized
subject, ≤100 characters, type one of `feat fix chore docs style refactor
perf test ci build revert` (see `committed.toml`). This isn't just a style
preference — `git-cliff` groups the changelog by these types, so a
commit that doesn't follow the convention doesn't show up in a release's
notes at all.

Activate the enforcing hook once per clone:

```bash
git config core.hooksPath .githooks
```

It runs [`committed`](https://github.com/crate-ci/committed) (`pacman -S
committed` / `cargo install committed`) against every commit message and
rejects ones that don't conform; it degrades to a warning (doesn't block)
if `committed` isn't installed. Check a message by hand with:

```bash
committed --commit-file - <<< "feat: Add a thing"
```

## Build and verify

```bash
cargo test             # pure-logic smoke suite (mod smoke in src/main.rs)
cargo build --release  # optimized binary at target/release/omaruler
```

No Makefile: this is a single Rust crate, and Cargo already is the
build/test/install tool (`cargo build`, `cargo test`, `cargo install
--path .`) — a Makefile wrapping those would just be indirection.

`cargo test` covers the deterministic, pure-logic core: color/geometry math
and the hand-rolled parsers (PPM decoding, hex color parsing, the
color-continuity scan, snap-to-content trimming, aspect-ratio matching,
duck hit-testing). It does **not** drive the actual GTK4/layer-shell
window — that needs a real Wayland compositor implementing wlr-layer-shell,
which a headless test run doesn't have. Verify anything touching rendering,
input handling, or the live overlay manually against a running Hyprland
session: `cargo build --release`, install to `~/.local/bin/omaruler`,
launch it, and check behavior with real input and `grim` screenshots. This
project's own commit history is the model for what "verified" looks like
here — specific, manually-observed before/after behavior, not a claim of
automated coverage that doesn't exist.

Always run `cargo test` after changing anything in the pure-logic
functions (color matching, geometry, parsing) before considering a change
done.

## Release process

Not yet packaged or distributed via any package manager — built from
source (`cargo build --release`) and run directly.

1. Bump `version` in `Cargo.toml` (`Cargo.lock` updates with the next
   `cargo build`). Follow [SemVer](https://semver.org/): breaking change →
   major, new feature → minor, fix/internal-only → patch.
2. `cargo test`.
3. Commit (`chore: Bump version to X.Y.Z`), tag `vX.Y.Z`, push both
   (`git push && git push --tags`).
4. The `Release` GitHub Actions workflow takes it from there: builds the
   release binary and publishes a GitHub Release with `git-cliff`-generated
   notes (grouped by commit type — Features, Bug Fixes, etc.) and the
   binary attached. Nothing to do by hand beyond pushing the tag.

Since the changelog is generated from commit history, only commits that
actually follow Conventional Commits (see above) show up in a release's
notes — this is the concrete payoff of enforcing the format, not just
tidiness.

See `README.md` for user-facing features, keybindings, and build
instructions — keep it in sync when behavior changes.
