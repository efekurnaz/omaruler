# Packaging

`PKGBUILD` builds an Arch Linux package for Omaruler, published on the AUR as
**`omaruler-git`** (a VCS package: it always builds the latest `master`, and
`pkgver` is derived from the commit count + short hash).

`pacman` is Arch's package manager; the AUR is its community recipe
repository. Omarchy is built on Arch, and `omarchy-pkg-add` /
`yay` / `paru` all install from the AUR. Once this is published, any Omarchy
user gets Omaruler with:

```sh
omarchy-pkg-add omaruler-git      # or: yay -S omaruler-git
```

and updates it with a normal system update.

## Prerequisites in the repo

The package installs `LICENSE` into `/usr/share/licenses/omaruler-git/`, so
the `LICENSE` file at the repo root must be committed and pushed. `PKGBUILD`
and this directory should be pushed too so the recipe lives with the code
(the AUR still needs its own repo — see below).

## Build and test locally

From this directory:

```sh
makepkg -si          # build, then install the resulting package
namcap PKGBUILD       # lint the recipe (pacman -S --asdeps namcap)
namcap ../*.pkg.tar.zst
```

`makepkg` clones the GitHub repo fresh, so commit and push any changes
first — a local edit that isn't pushed won't be in the build.

The `check()` step runs `cargo test`. The tests are pure unit tests (PPM
decoding, color math, ratio fitting) and need no display.

## Publish to the AUR

One-time: create an account at https://aur.archlinux.org and add your SSH
public key under *My Account*.

```sh
git clone ssh://aur@aur.archlinux.org/omaruler-git.git aur-omaruler
cd aur-omaruler
cp ../omaruler/packaging/PKGBUILD .
makepkg --printsrcinfo > .SRCINFO      # regenerate whenever PKGBUILD changes
git add PKGBUILD .SRCINFO
git commit -m "Initial import"
git push
```

For every later change: edit `PKGBUILD`, regenerate `.SRCINFO`, commit, push.
A `-git` package rebuilds from the latest commit on each install, so you only
push here when the recipe itself changes (new dependency, build flag, etc.) —
not for ordinary code changes to Omaruler.

## Notes on dependencies

- `depends`: `gtk4`, `gtk4-layer-shell`, `grim`, `wl-clipboard`, `hyprland`
  (Omaruler shells out to `hyprctl`, so it is Hyprland-specific).
- `omarchy-theme-color` and `omarchy-osd` are called at runtime but ship with
  Omarchy itself, which is not a package — there is nothing to depend on. On
  stock Omarchy they are always present.
- `omarchy-legend` is optional; Omaruler draws the hint card itself when it is
  missing, so it is not listed.
- `libpulse` (`paplay`) is an `optdepends` — only the hidden easter egg uses
  it.
