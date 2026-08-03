# Cross-architecture builds

A recipe names the architectures it builds for, and a run builds each of them in
turn. A recipe that names none builds for whichever host runs it, and
`--architecture` selects any other target. Building foreign bootstraps the build
root for the target and runs the target's `dpkg`, maintainer scripts, and
`dpkg-buildpackage` through a `qemu-user` binfmt handler.

## Selecting the targets

`architectures` takes a list of Debian architecture names:

```toml
name = "cosmic-epoch"
suite = "trixie"
architectures = ["amd64", "arm64"]
```

`--architecture` names them on the command line instead, and applies to both
`build` and `plan`. It is repeatable, and replaces whatever the recipe names
rather than adding to it:

```sh
src2deb build recipes/cosmic-epoch --architecture arm64
src2deb build recipes/cosmic-epoch --architecture amd64 --architecture arm64
```

A recipe is most portable when it leaves `architectures` out entirely; name them
in the file only when the recipe is meaningful for a fixed set. src2deb prints
the effective targets in its opening banner.

The produced `.deb`s carry the target architecture, the local pool indexes them
under `binary-<arch>`, and the provenance manifest records the architecture built
for. Each architecture gets a pool, an output tree, a manifest, and a build root
of its own, keyed alongside the suite, so runs for several targets share one work
directory without overwriting one another. That holds for `Architecture: all`
packages too, whose file names carry no architecture at all.

## What a multi-architecture run shares

The architectures are built one after another, in the order they are named, and
everything before the first build is done once for the run:

- **Sources resolve once.** Every architecture is built from the same checkouts,
  at the same commits, with the same patches applied. Two separate runs cannot
  promise that — a `git-ref` naming a branch may move between them — so a run
  that targets both is the way to get one set of packages built from one set of
  sources.
- **The build order is computed once**, from those sources' `debian/control`
  files, and every architecture follows it.
- **The build date is one date**, so every package the run produces carries the
  same stamped version whichever architecture built it. See
  [Package versions](package-versions.md).

What each architecture settles for itself is its build roots, its pool, its
output tree, its manifest, and — through that manifest — what `--skip-published`
skips.

`--jobs N` parallelizes the components within one architecture, not the
architectures themselves. Two emulated builds running alongside each other
contend for the same cores and the same package cache without finishing sooner.

A run stops where it is when something goes wrong: a component that fails ends
the run unless `--keep-going` is passed, and a cancel ends it outright. Either
way the architectures after it are never started, and the summary says so:

```text
src2deb: the run stopped before building for arm64
```

The same holds for a failure that is not a component's — a build root that will
not provision, most often a foreign target with no
[binfmt handler](#requirements) registered. src2deb normally reports such a
failure alone, since a run that never built anything has nothing to summarize;
but once an architecture has published its packages and written its manifest,
that work stands, so the summary is printed and the error appears beneath it:

```text
src2deb: summary (amd64): 26 built, 0 failed, 0 skipped of 26 component(s)
src2deb: 120 artifact(s) produced, in work/out/trixie/amd64
src2deb: summary: 1 architecture(s), 120 artifact(s) in total
src2deb: provisioning a build root: no binfmt handler for arm64
src2deb: the run stopped before building for arm64
```

Nothing was recorded for the architecture that failed: an architecture writes its
manifest only once its components are done, so a later run starts it from clean.

## Who builds the `Architecture: all` packages

An `Architecture: all` package is architecture-independent: one build serves
every architecture. Its file name carries no architecture, and the version
src2deb stamps does not vary with one either — so building a recipe for two
architectures produces `cosmic-icons_1.0+deb13.20260731.abc1234_all.deb` twice,
under one name and one version, over two different sets of bytes.

Locally that is harmless, because each architecture has a pool of its own. It
stops being harmless the moment those architectures merge into a single published
archive, where one name and version must mean one file.

By default every architecture produces its own arch-indep packages, so each pool
holds every package its recipe declares and can be served exactly as it stands. A
run building for several says so before it starts spending the time:

```text
src2deb: no arch-indep owner named, so each of amd64, arm64 builds its own copy
of every Architecture: all package; set arch-indep-owner to build them once
```

`arch-indep-owner` hands them to one architecture instead:

```toml
name = "cosmic-epoch"
suite = "trixie"
architectures = ["amd64", "arm64"]
arch-indep-owner = "amd64"
```

or, per run, `--arch-indep-owner amd64`. Every other architecture then builds
only its architecture-dependent packages (`dpkg-buildpackage -B` rather than
`-b`), and a component whose every binary package is `Architecture: all` is
skipped outright for those architectures — there is nothing left of it to build.
An architecture in that position says so before it starts:

```text
src2deb: Architecture: all packages belong to amd64; this architecture builds
only its own, so its pool holds fewer packages than the recipe declares
```

Which to choose follows from what you do with the pool:

| | Leave `arch-indep-owner` unset | Name an owner |
| --- | --- | --- |
| Each architecture's pool | Complete; servable as it stands | Missing the arch-indep packages |
| `Architecture: all` packages | Built once per architecture | Built once in total |
| Emulated build time | Spent rebuilding packages that contain no compiled code | Not spent |
| Bytes | One set per architecture | One set |

Name an owner when several architectures feed one published archive. Leave it
unset when you serve a per-architecture pool directly — as a test machine does —
since that pool has to carry every package the recipe declares.

Either way, [`src2deb export`](publishing.md) carries one copy of each
`Architecture: all` package: with no owner declared it has two to choose
between, and it says which it took. Naming an owner is what stops the second
from being built at all.

One case an owner does not cover: a component that produces *only*
`Architecture: all` packages, and whose packages another component
build-depends on. A non-owner architecture skips that component, and the owner's
copy is published to the owner's pool, which the non-owner's build never
resolves against. src2deb refuses such a run before provisioning anything:

```text
src2deb: unsatisfiable build-dependency: this run builds "consumer", which
build-depends on "shared-data"; that package is produced by component "data",
which produces only Architecture: all packages, left to "amd64" by this recipe,
and the pool does not hold it. Stop naming an arch-indep owner, so this
architecture builds "data" itself; the owner's copy is published to the owner's
pool, which this build does not resolve against.
```

## Native and foreign

A build is native when the host CPU runs the target's binaries directly.
Identical architectures are native, and an amd64 host runs i386 binaries through
its IA-32 compatibility mode, so both need no emulator. Every other pair is
foreign. The relation is directional — an i386 host cannot run amd64. src2deb
announces a foreign build before provisioning.

A foreign build is emulated rather than cross-compiled: the target's own
toolchain runs under `qemu-user`, instead of a host toolchain emitting target
code. A foreign build is therefore identical in shape to a native one — the same
packages, the same `debian/rules`, the same build-dependency resolution — at the
cost of running every compiler invocation under emulation.

Emulation costs roughly an order of magnitude in compile time, which a large Rust
source tree turns into hours. Where hardware native to a target is available,
prefer running that target there; name the architectures in one run when it is
not.

## Requirements

A foreign build needs a `qemu-user` binfmt handler for the target, registered
with the fix-binary (`F`) flag so the interpreter is preloaded and keeps working
after a cage pivots into the target root. On Debian and derivatives:

```sh
sudo apt install qemu-user-static binfmt-support
```

The `qemu-user-static` package registers the handlers with the `F` flag. Without
a registered handler, provisioning a foreign root fails with a message naming the
missing handler.

## Building across hosts

src2deb does not orchestrate hosts. Where several architectures have hardware of
their own, run src2deb once per host and collect the resulting pools — each is a
complete archive for its own architecture, unless an [arch-indep
owner](#who-builds-the-architecture-all-packages) is named, in which case only
the owner's carries the `Architecture: all` packages.

Where several architectures share one work directory, one
[`src2deb export`](publishing.md) collects all of them into a single directory
for an archive; where they do not, each host exports its own.

A `--only` smoke test still resolves every component's source, because the build
order is read from all of them — so the first run against a fresh work directory
clones the whole recipe whatever it goes on to build. The saving is in the
building, which under emulation is where the hours are. Later runs fetch rather
than clone.
