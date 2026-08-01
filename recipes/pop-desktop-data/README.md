# pop-desktop-data recipe

Builds the theme, icon, font, and metadata packages COSMIC depends on at runtime
and Debian does not ship.

## Why it is here

Pop's COSMIC packaging names these in runtime `Depends` and `Recommends`. Debian
has no equivalent, so a pool that serves the COSMIC desktop carries them or its
packages do not install. Nothing in COSMIC build-depends on any of them, so they
play no part in the cosmic-epoch build graph and are a recipe of their own.

| Source package       | Upstream                    | Wanted by                              |
| -------------------- | --------------------------- | -------------------------------------- |
| `adw-gtk3`           | `pop-os/adw-gtk3`           | `cosmic-settings-daemon`, `cosmic-settings` (Recommends) |
| `appstream-data-pop` | `pop-os/appstream-data`     | `cosmic-store`, `cosmic-initial-setup` |
| `pop-fonts`          | `pop-os/fonts`              | `cosmic-session`                       |
| `pop-gtk-theme`      | `pop-os/gtk-theme`          | `cosmic-settings-daemon`, via the `pop-sound-theme` binary |
| `pop-icon-theme`     | `pop-os/icon-theme`         | `cosmic-icons`                         |

Three of the five source packages build more binaries than COSMIC asks for.
`pop-gtk-theme` builds `pop-sound-theme` (the one COSMIC depends on) alongside
`pop-gtk-theme` and `pop-gnome-shell-theme`; `pop-icon-theme` builds a
transitional `gnome-shell-extension-pop-battery-icon-fix`; `appstream-data-pop`
builds `appstream-data-pop-icons` and `-icons-hidpi`. All of them land in the
pool, and installing any is optional.

Two of those extras do not install on forky. `pop-gtk-theme` depends on
`gtk2-engines-murrine`, which was removed from the archive in January 2026 at the
maintainer's request as an unused GTK 2 theme engine, and `pop-gnome-shell-theme`
depends on `pop-gtk-theme` in turn. Nothing in COSMIC depends on either — its GTK
theming dependency is `adw-gtk3` — so leave both uninstalled. `pop-sound-theme`,
built from the same source, depends on nothing beyond `${misc:Depends}` and
installs on both suites.

Install these packages by name rather than handing a whole output directory to
`dpkg -i`, which would install the byproducts too and then report the missing
`gtk2-engines-murrine` as a broken dependency.

## Architecture

Every binary package is `Architecture: all`. These are data packages -- Sass
compiled to CSS, icons, fonts, and AppStream XML -- so a build produces the same
`<name>_<version>_all.deb` whichever architecture runs it, and that `.deb`
installs on arm64, amd64, and anything else. Build it natively and publish it to
the pools that need it, rather than repeating the build under emulation per
architecture.

The pool index is still architecture-scoped, so publishing into an existing pool
means building with that pool's `--work` directory and suite.

## Build system

All five build with plain `dh`: no Rust, no `cargo vendor`, no network beyond the
source clone, and no `[toolchain]` block. Build-dependencies are `debhelper`,
`meson`, `ninja-build`, `sassc`, `libglib2.0-dev`, and `git`, all present across
current suites.

Two of the sources declare compat 9 through `debian/compat` and the rest declare
10. debhelper supports compat 7 and up, so both build under trixie's debhelper 13
and forky's 14, with a deprecation warning.

## Suite

The recipe's own suite is trixie. Every component builds against forky too, with
only the compat-level deprecation warning noted above, so `--suite forky` covers
that target from this one recipe.

## Running

```sh
src2deb build recipes/pop-desktop-data --work ./work
src2deb build recipes/pop-desktop-data --suite forky --work ./work
```

The pool, output tree, and manifest are each keyed by suite and architecture, so
both runs share one work directory without overwriting the other's packages.

To publish alongside a COSMIC build, point `--work` at that build's working
directory and name the same suite, so the packages land in the same pool.

## Upstream state

Pop's adw-gtk3 fork carries a stale build tree committed under `debian/` (a
populated `debian/adw-gtk3/` staging directory, `debian/files`,
`debian/*.substvars`, and `debian/debhelper-build-stamp`). src2deb's first pass
runs `debian/rules clean`, which removes all of it before the offline build, so
the packaged contents come from the build and not from the checkout. The other
four sources carry clean `debian/` trees.

`pop-os/icon-theme` is a large checkout (roughly 200 MiB of icon assets), which
dominates the recipe's clone time on a cold work directory.
