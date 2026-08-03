# cosmic-epoch recipe

Builds the COSMIC desktop from source for Debian. Generated from
[pop-os/cosmic-epoch](https://github.com/pop-os/cosmic-epoch).

## Topology

cosmic-epoch is a git submodule superproject. **Each component is its own Debian
source package** with its own `debian/` tree (Pop's convention — see the
superproject README's Packaging section). `recipe.toml` lists the 27 components
that ship `debian/` on `master`, each pinned to the commit cosmic-epoch
references, so a build is reproducible. Set a component's `git-ref` to `master`
to track latest.

- **cosmic-sound-theme** is a submodule but has no `debian/` on `master`; it is
  commented out in `recipe.toml` pending its packaging source.
- **Build order is derived, not listed.** src2deb reads each `debian/control` for
  `Build-Depends` and the binary packages it produces, and orders accordingly.
  The COSMIC build graph is nearly flat: only `cosmic-osd` build-depends on
  another component (`cosmic-randr`). Everything else builds independently. (The
  familiar cross-component links — `cosmic-session` pulling in the whole
  desktop — are runtime `Depends`, which do not affect the build.)

## Installing what this builds

This recipe builds COSMIC. Two companion recipes complete a pool, and both belong
in the same one:

- `recipes/pop-desktop-data/` — the theme, icon, font and metadata packages
  COSMIC depends on and Debian does not ship.
- `recipes/cosmic-debian/` — the `cosmic-desktop` metapackage and the
  compatibility packages for dependency names Debian has retired.

With all three in a pool, the whole desktop installs by naming one package:

```sh
apt install cosmic-desktop
```

That reaches 35 packages and deliberately reaches neither
`cosmic-initial-setup-casper` nor the GNOME-era themes. See
`recipes/cosmic-debian/README.md`.

Getting from the pool the run leaves behind to a client that can install from it
— serving it, the `sources.list` entry, and signing — is covered by the guide's
[Using the pool](../../docs/src/using-the-pool.md).

## Runtime dependencies outside Debian

COSMIC's runtime `Depends` name several packages Debian does not ship.
`recipes/pop-desktop-data/` builds the five that have upstream Debian packaging
(`adw-gtk3`, `appstream-data-pop`, `pop-fonts`, `pop-gtk-theme`,
`pop-icon-theme`); build it into the same pool or those components do not
install. Two more need explaining:

- **`casper`** is Ubuntu's live-boot integration, and only the
  `cosmic-initial-setup-casper` binary depends on it. That binary exists to
  *disable* first-run setup on live media — it ships a single casper-bottom
  initramfs hook that deletes the initial-setup autostart entry from the target
  filesystem — so an installed system wants it left out. The first-run experience
  is in the main `cosmic-initial-setup` binary, which does not depend on `casper`
  and installs on both suites.
- **`network-manager-gnome`** exists in trixie as a transitional package and is
  gone from forky, where `nm-connection-editor` replaces it. `cosmic-settings`
  depends on the old name, so on forky it needs the transitional package from
  `recipes/cosmic-debian/` in the same pool.

## Rust vendoring (important)

COSMIC components build with `cargo`/`just`, pulling crates (including libcosmic)
from the network. Pop's `debian/rules` vendors *outside* a chroot
(`cargo vendor` / `just vendor` / `make vendor` → `vendor.tar`) and then builds
*offline* inside the chroot. src2deb mirrors this with two cage passes per
component: a first pass runs `debian/rules clean` with the network to produce
`vendor.tar`, then a second pass builds offline with `dpkg-buildpackage -nc`
(no pre-clean, so vendoring is not re-triggered). The build stays hermetic;
only the vendor pass touches the network.

## Rust toolchain

The recipe pins a rustup toolchain (`[toolchain.rust]`). COSMIC uses Rust
features that trixie's archive `rustc` does not yet support, so the build root
carries the pinned toolchain and prefers it on `PATH`.

The install happens while a build root is being provisioned, not while a build
pass is running. Under the layered strategy that means once per run for the whole
recipe: a pass writes into a per-component overlay that is discarded when the
component finishes, so a toolchain installed from a pass would have to be
fetched again for every component.

The pin stays in place for every suite. A suite whose archive Rust is new enough —
forky, for instance — could use `provider = "debian"` instead, but keeping the
pinned toolchain means one recipe serves both suites and both build against a
known compiler rather than whichever version the archive has moved to.

## Running

```sh
src2deb build recipes/cosmic-epoch --work ./work
```

The recipe names no architecture, so it builds for whichever host runs it, and its
`suite` is a default. Name another target with `--architecture`, another suite
with `--suite`, or both:

```sh
src2deb build recipes/cosmic-epoch --work ./work --architecture arm64
src2deb build recipes/cosmic-epoch --work ./work --suite forky --architecture arm64
```

`--architecture` is repeatable, and a run builds each named architecture in turn
from one set of resolved sources:

```sh
src2deb build recipes/cosmic-epoch --work ./work --architecture amd64 --architecture arm64
```

Each suite and architecture gets its own pool, output tree, and manifest, so those
runs may share one work directory.

COSMIC is a large Rust build, and a foreign target runs the whole toolchain
under emulation. Prefer a native host for each architecture where one is
available; see the guide's cross-architecture chapter.

## Updating the pins

Re-run against a newer cosmic-epoch checkout to refresh the pinned commits (the
recipe was generated from its submodule gitlinks).
