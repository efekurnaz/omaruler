# Packaging

Two AUR recipes, both installable with `omarchy-pkg-add` / `yay` / `paru`:

| Package         | Recipe                      | What the user gets                                        |
|-----------------|-----------------------------|----------------------------------------------------------|
| **`omaruler-bin`** | `omaruler-bin/PKGBUILD`   | The prebuilt binary from the matching GitHub release. No Rust toolchain, no compile — installs in seconds. **Recommended.** |
| `omaruler-git`  | `omaruler-git/PKGBUILD`      | Builds the latest `master` from source (`pkgver` = commit count + short hash). For people who want unreleased changes. |

`pacman` is Arch's package manager; the AUR is its community recipe
repository. Omarchy is built on Arch, so once these are published any Omarchy
user installs Omaruler with:

```sh
omarchy-pkg-add omaruler-bin       # or: yay -S omaruler-bin
```

and updates it with a normal system update.

## "Why does it have to build?"

It doesn't. `omaruler-git` compiles because it is a source (VCS) package by
definition — that is the point of a `-git` package. `omaruler-bin` skips all
of it: the release binary is compiled once by
[`.github/workflows/release.yml`](../.github/workflows/release.yml) in an Arch
container against the exact same `gtk4` / `gtk4-layer-shell` the package
depends on, attached to the GitHub release, and `omaruler-bin` just downloads
and installs it. Same result as building locally, minus ~400 MB of Rust
toolchain and a two-minute `cargo build`.

## Cutting a release

`omaruler-bin` points at a tagged GitHub release. To make one:

```sh
git tag v0.1.0        # match Cargo.toml's version, prefixed with v
git push origin v0.1.0
```

The `Release` workflow builds the binary, generates notes with `git-cliff`,
creates the GitHub release, and prints the binary's `sha256` to the workflow
run summary.

## Build and test locally

From the recipe's own directory (`omaruler-bin/` or `omaruler-git/`):

```sh
makepkg -si          # build, then install the resulting package
namcap PKGBUILD      # lint the recipe (pacman -S --asdeps namcap)
namcap ./*.pkg.tar.zst
```

`makepkg` fetches from GitHub (a fresh clone for `-git`, the release asset for
`-bin`), so commit, push, and — for `-bin` — tag the release first; a local
edit that isn't on GitHub won't be in the build.

`omaruler-git`'s `check()` step runs `cargo test` (pure unit tests — PPM
decoding, color math, ratio fitting; no display needed).

## Publish to the AUR

One-time: create an account at https://aur.archlinux.org and add your SSH
public key under *My Account*. Then, for each package:

```sh
git clone ssh://aur@aur.archlinux.org/omaruler-bin.git aur-omaruler-bin
cd aur-omaruler-bin
cp ../omaruler/packaging/omaruler-bin/{PKGBUILD,.SRCINFO} .
git add PKGBUILD .SRCINFO
git commit -m "Initial import"
git push
```

Later changes: edit `PKGBUILD`, run `makepkg --printsrcinfo > .SRCINFO`,
commit, push.

- **`omaruler-bin`**: bump on every release. Set `pkgver`, run `updpkgsums`
  (from `pacman-contrib`) to replace the `SKIP` checksums with the real ones
  — the workflow summary prints the binary hash — then regenerate `.SRCINFO`.
- **`omaruler-git`**: only push when the recipe itself changes (new
  dependency, build flag). It already rebuilds from the latest commit on
  every install.

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
