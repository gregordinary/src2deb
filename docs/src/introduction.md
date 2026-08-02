# Introduction

src2deb builds Debian `.deb` packages from source. It reads a recipe that
lists a set of components, resolves each component's source, works out the order
they must build in, and builds each one inside an unprivileged
[ferroday-cage][cage] sandbox. Every component is built in a Debian root src2deb
provisions itself, and the finished packages are collected onto the host.

Its first target is the COSMIC desktop (cosmic-epoch): 27 components, built from
source for Debian Trixie and Forky using the `debian/` packaging trees upstream
ships. Their build graph is nearly flat — a single inter-component build edge —
so the order src2deb builds them in is derived from their declared dependencies.

[cage]: https://github.com/gregordinary/ferroday-cage

## The model

A build is driven by a recipe — a `recipe.toml` that names a Debian suite and
lists the components to build, each with a source — a git repository, or a tree
already on disk. src2deb:

1. resolves each component's source into an unpacked tree with a `debian/`
   directory — taking that `debian/` from a second source, and applying a patch
   series over the result, where the recipe says so,
2. reads every `debian/control` to learn what each component build-depends on
   and what binary packages it produces, and orders the components so each one
   builds after the components that produce its build-dependencies,
3. provisions a build root for each component — the base system, the Rust
   toolchain, and that component's build-dependencies — and builds it,
4. publishes each component's packages to a local pool, so a later component
   that build-depends on an earlier one resolves against the packages src2deb
   just built.

What a run leaves behind is a servable Debian archive, a tree of artifacts, and
a provenance manifest tying the two to the revisions they were built from.

## Hermetic builds

Each component builds in its own sandbox with a controlled package set and, for
the build itself, an isolated network. A package that vendors its dependencies
(as COSMIC's Rust components do) is handled in two passes: a vendor pass with
network access that captures the dependencies into the source tree, then an
offline build pass that consumes them. What the recipe declares is what the
build sees.

### The vendor pass is the trust boundary

The build pass runs with an isolated network; the vendor pass runs with the
host's. It runs the component's own `debian/rules clean` — arbitrary upstream
code — in a sandbox whose filesystem is isolated but whose network is the
host's, so the vendoring step can fetch its crates. Upstream code therefore
executes with host network access during that pass. The filesystem sandbox still
confines it to the source tree, and only the offline build pass produces the
packages, but the vendor pass is where src2deb trusts upstream: the build is
hermetic; acquiring the dependencies to build it is not.

Sharing the host's network means sharing the host's resolver: `/etc/resolv.conf`
is bound into that sandbox read-only, so upstream code can resolve the names it
fetches from. It is the one host file the pipeline exposes to a build — the
environment, the tooling, and the keyring a build sees are all the build root's
own — and the build pass, which is the one that produces the packages, runs with
an isolated network throughout.

## Status

src2deb is at 0.1, and its interfaces will change. This guide grows as the
project does.

src2deb is released under MIT OR Apache-2.0.

## Where to go next

- [Quick start](quick-start.md) installs src2deb and runs a build.
- [Using the pool](using-the-pool.md) gets from a finished run to `apt install`.
- [Publishing to an archive](publishing.md) hands the packages to an archive
  tool.
- [Recipe reference](recipes.md) describes every field a recipe may set.
- [How a build runs](how-a-build-runs.md) is the detailed pass-by-pass account.
