# cosmic-debian recipe

Builds the packaging that belongs to a COSMIC pool rather than to COSMIC itself:
the metapackage that installs the desktop, and the compatibility packages needed
because Pop's packaging names its dependencies as Ubuntu's archive names them.
Source: the `cosmic-debian` repository, one packaging-only directory per source
package. `recipe.toml` names its home, which is not published yet; point
`source.git` at a local clone to build the recipe before then.

## Packages

| Package                 | Purpose                                                 |
| ----------------------- | ------------------------------------------------------- |
| `cosmic-desktop`        | The front door: `apt install cosmic-desktop`             |
| `network-manager-gnome` | Stands in for `nm-connection-editor` on forky and later  |

### cosmic-desktop

```
Depends: cosmic-session, cosmic-initial-setup
Recommends: cosmic-edit, cosmic-files, cosmic-player, cosmic-store,
            cosmic-term, cosmic-wallpapers
```

Short by design. COSMIC's own graph does nearly all the work: `cosmic-session`
depends on the compositor, panel, applets, settings, greeter, portal and the rest,
and those reach the themes, fonts and icons in turn — `pop-fonts` through
`cosmic-session`, `pop-icon-theme` through `cosmic-icons`, `pop-sound-theme` and
`adw-gtk3` through `cosmic-settings-daemon`, `network-manager-gnome` through
`cosmic-settings`. Restating them here would be redundant and would need
revisiting whenever upstream moved one.

`cosmic-initial-setup` is the exception and the reason the package earns its place:
nothing in COSMIC depends on it, so the first-run experience is absent from any
install that goes through `cosmic-session` alone. It also brings
`appstream-data-pop`.

Its version is a serial counting revisions of the dependency declaration, not a
version of COSMIC. Rebuilding the recipe against newer COSMIC packages needs no
bump: the dependencies are unversioned, so they resolve to whatever the pool
holds and the declaration stays true. Bump it when the declaration itself
changes. Reaching installed systems does not depend on that bump — src2deb
stamps every build with its own date, so a republished metapackage outranks the
installed one whether or not the serial moved (see
[Package versions](../../docs/src/package-versions.md)). What the bump buys is
legibility: the serial is how a reader tells one declaration from another, and a
changed declaration sharing a serial with the one before it reads as the same
package. The reasoning is recorded with the packaging.

The closure reaches 35 packages from the pool and leaves four unreached, which is
the intent rather than an accident:

- `cosmic-initial-setup-casper` suppresses first-run setup on live installation
  media, so an installed system wants it left out.
- `pop-gtk-theme` and `pop-gnome-shell-theme` are GNOME-era themes COSMIC does not
  use, and need a GTK 2 theme engine Debian removed after trixie.
- `gnome-shell-extension-pop-battery-icon-fix` is a GNOME transitional package.

Those four are byproducts of source packages the pool wants for other binaries —
`cosmic-initial-setup`, `pop-sound-theme`, `pop-icon-theme` — so they are built
whether or not anything installs them. Excluding them from the graph, rather than
from the build, is what keeps upstream packaging unpatched.

### network-manager-gnome

`cosmic-settings` depends on `network-manager-gnome`, which Debian shipped as a
transitional package of `network-manager-applet` and removed after trixie. It uses
NetworkManager over D-Bus for state and spawns one external binary,
`nm-connection-editor`, present in every current suite.

This package reinstates the name and splits those two needs — `Depends:
network-manager`, `Recommends: nm-connection-editor` — because the daemon is
required while the editor is only run to add a connection. It deliberately does not
pull `network-manager-applet` as Debian's transitional package did; that is a GNOME
tray applet COSMIC does not use.

Install it with the desktop rather than on its own. `nm-connection-editor` depends
on `policykit-1-gnome | polkit-1-auth-agent`, and `policykit-1-gnome` was also
removed after trixie, so the virtual alternative decides between about a dozen
providers that each belong to another desktop. `cosmic-osd` declares
`Provides: polkit-1-auth-agent` and comes in through `cosmic-session`, so one
`apt install cosmic-desktop` resolves it against COSMIC's own agent. Installing this
package first, alone, lets apt choose a foreign one instead — several hundred
packages of difference.

Its version, `1.36.0-3~src2deb1`, sorts above the `1.36.0-3~` threshold in
`nm-connection-editor`'s `Breaks: network-manager-gnome (<< 1.36.0-3~)` so the two
coexist, and below Debian's own `1.36.0-3` so the archive's package wins wherever
it still exists.

The build stamp src2deb appends preserves both bounds, which matters because the
version that ships is the stamped one. `1.36.0-3~src2deb1+deb14.20260731.abc1234`
still sorts above `1.36.0-3~` — after the `~`, a non-empty string beats an empty
one — and still below `1.36.0-3`, because the `~` sorts before the end of the
string whatever follows it.

## Architecture

Both packages are `Architecture: all` and empty: their whole content is their
dependency relationships. One build serves every architecture.

## Suite

The recipe's suite is forky, where `network-manager-gnome` has gone missing. On
trixie the shim is unnecessary — the archive carries the real package at a version
that supersedes it — but building for trixie with `--suite trixie` is harmless and
gives that pool its metapackage.

## Running

```sh
src2deb build recipes/cosmic-debian --work ./work
```

Point `--work` at the working directory of the COSMIC build whose pool these
belong in, and name that build's suite. The metapackage is only useful next to the
packages it names.

## Adding a package

Add a directory to the source repository holding the new source package's `debian/`
tree, then add a component here naming it with `source.subdir`. For a dependency
Debian has renamed, prefer a transitional package that depends on whatever Debian
calls it now over patching a component's own `debian/control`, which would make the
built package diverge from the packaging it came from.
