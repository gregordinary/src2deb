# Cross-architecture builds

src2deb builds for one architecture per run. A recipe that names no
`architecture` builds for whichever host runs it, and `--architecture` selects
any other target. Building foreign bootstraps the build root for the target and
runs the target's `dpkg`, maintainer scripts, and `dpkg-buildpackage` through a
`qemu-user` binfmt handler.

## Selecting the target

`--architecture` takes a Debian architecture name and applies to both `build`
and `plan`:

```sh
src2deb build recipes/cosmic-epoch --architecture arm64
```

The flag overrides whatever the recipe names, so a single recipe serves every
target. A recipe is most portable when it leaves `architecture` out entirely;
name one in the file only when the recipe is meaningful for a single
architecture. src2deb prints the effective target in its opening banner.

The produced `.deb`s carry the target architecture, the local pool indexes them
under `binary-<arch>`, and the provenance manifest records the architecture
built for. Each architecture gets a pool and an output tree of its own, keyed
alongside the suite, so runs for several targets share one work directory
without overwriting one another. That holds for `Architecture: all` packages
too, whose file names carry no architecture at all.

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

## Building for several architectures

Each architecture is its own run. With the target named on the command line
rather than in the recipe, a matrix is a loop:

```sh
for arch in amd64 arm64; do
  src2deb build recipes/cosmic-epoch --work "work-$arch" --architecture "$arch"
done
```

Prefer a native host for each architecture where hardware is available, and run
each target there. Emulation costs roughly an order of magnitude in compile
time, which a large Rust source tree turns into hours; the same build on
hardware native to the target runs at full speed. Reserve the foreign path for
architectures you have no hardware for, and for smoke-testing a single component
with `--only` before committing a machine to a full run.

A `--only` smoke test still resolves every component's source, because the build
order is read from all of them — so the first run against a fresh work directory
clones the whole recipe whatever it goes on to build. The saving is in the
building, which under emulation is where the hours are. Later runs fetch rather
than clone.

src2deb does not orchestrate hosts. Run it once per host and collect the
resulting pools — each is a complete archive for its own architecture.
