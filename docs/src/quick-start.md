# Quick start

## Prerequisites

- Linux with unprivileged user namespaces enabled — the requirement
  [ferroday-cage](https://github.com/gregordinary/ferroday-cage) imposes for its
  rootless sandbox.
- `git`, to resolve component sources on the host.
- `git-lfs`, for components whose repositories keep assets in Git LFS. A build
  that needs it and cannot find it stops during resolve, so a package is never
  built against pointer stubs.
- A Rust toolchain, to build src2deb itself.

That is the whole host requirement: src2deb provisions each build root itself,
so the Debian build tooling lives inside the sandbox.

## Install

src2deb is built from source with Cargo. `rust-toolchain.toml` pins the Rust
version it builds with, which rustup installs on demand.

From a checkout of the repository:

```sh
cargo install --path crates/src2deb-cli
```

That puts `src2deb` on `PATH` via `~/.cargo/bin`. To build without installing it,
`cargo build --release` leaves the binary at `target/release/src2deb`.

## Build a recipe

Point src2deb at a recipe directory — a directory containing a `recipe.toml`:

```sh
src2deb build recipes/cosmic-epoch --work ./work
```

`--work` sets the working directory for sources, build roots, the package cache,
the local pool, and build output; it defaults to `./work`.

src2deb resolves each component's source, computes the build order, and builds
the components in turn, streaming each build's output to the terminal. Finished
packages are collected under the working directory and published to the local
pool so later components resolve against them.

By default a build stops at the first component that fails. Pass `--keep-going`
to build the rest and report a final tally instead:

```sh
src2deb build recipes/cosmic-epoch --keep-going
```

A component fails whether its source will not resolve, its `debian/control` or
`debian/changelog` cannot be read, or its build exits unsuccessfully;
`--keep-going` covers all of them, so one unreachable repository costs one
component rather than the run.

A run that reaches the build phase ends with a summary — how many components
built, failed, and were skipped and why; what the run produced and where; and
the names of any that failed — and exits non-zero if any component failed. A run
stopped before that point, by a locked work directory or a dependency cycle or a
selection it cannot satisfy, prints the error alone: it has nothing to summarize
and writes no manifest.

## Choose a target suite and architecture

A recipe names the suite it was written against and, optionally, an architecture.
Both are defaults: what a recipe fixes is its components and how they build, not
which target a run aims at. `--suite` and `--architecture` retarget it:

```sh
src2deb build recipes/cosmic-epoch --suite forky
src2deb build recipes/cosmic-epoch --architecture arm64
src2deb build recipes/cosmic-epoch --suite forky --architecture arm64
```

A recipe that names no `architecture` builds for whichever host runs it. Each run
builds for one suite and one architecture, so covering several means one run per
target — and because the pool, output tree, and manifest are all keyed by that
pair, those runs may share a single `--work` directory.

The version tag follows the suite. `--suite` supersedes a recipe's `version-tag`
along with the `suite` it described, so a retargeted run stamps the tag of the
suite it is actually building for. A suite src2deb has no tag for is refused
until `--version-tag` names one:

```sh
src2deb build recipes/cosmic-epoch --suite sid --version-tag debsid
```

See [Package versions](package-versions.md).

An override is not a promise that the recipe suits the target: a suite whose
archive cannot satisfy the components' build-dependencies fails while the build
root is provisioned. A target the host cannot run natively builds under emulation
and needs a `qemu-user` binfmt handler installed; see
[Cross-architecture builds](cross-architecture.md).

## Stop a run

Ctrl-C stops a run at the next point where stopping leaves a coherent state
behind, rather than killing it mid-provision: components that finished stay
built and published, a partly-provisioned build root is removed rather than left
half-made, and the manifest still records where the run got to. The run exits
`130`. See [Cancelling a run](how-a-build-runs.md#cancelling-a-run).

## Build in parallel

`--jobs N` builds up to `N` components at once, respecting the dependency order:

```sh
src2deb build recipes/cosmic-epoch --jobs 4
```

A component starts as soon as the components that produce its build-dependencies
have finished and published. Because the COSMIC graph is nearly flat, most
components build independently, so parallelism is close to linear in `N`. Each
line of in-cage output is prefixed with its component, since several builds'
output interleaves. Building defaults to one component at a time.

## Preview the build order

The `plan` subcommand resolves sources and computes the build order without
building anything:

```sh
src2deb plan recipes/cosmic-epoch
src2deb plan recipes/cosmic-epoch --build-deps
```

It prints the order to standard output, one component per line with the source it
resolved to; `--build-deps` adds each component's build-dependencies. Planning still
clones each source, because the order is read from every `debian/control`.

`plan` takes the same exclusive lock on its work directory that `build` does, for
the same reason — it writes to `<work>/sources/`. To inspect a recipe's order
while a long build is running, give the plan a `--work` directory of its own.

## Build from a tree you are editing

A component's source is usually a git repository, which means a change has to be
committed and pushed before src2deb can build it. Point `source.path` at a tree
on disk instead and it builds what is there now:

```toml
[[components]]
name = "cosmic-comp"
source.path = "../../checkouts/cosmic-comp"
```

The path is relative to the recipe's own directory. src2deb copies the tree into
the work directory and builds from the copy, so nothing is written into the tree
you are editing — which matters, because the build runs the component's own
`debian/rules clean` in whatever tree it is given.

Packages built this way are marked. The version carries `local` where a git build
carries a commit, the manifest records the input as unpinned, and
`--skip-published` never skips the component. See [Building from a tree on
disk](sources-and-toolchain.md#building-from-a-tree-on-disk).

## Resume and selective builds

A re-run against the same work directory can skip work already done and narrow
what it builds:

```sh
# Rebuild only what changed since the last run.
src2deb build recipes/cosmic-epoch --skip-published

# Build one component.
src2deb build recipes/cosmic-epoch --only cosmic-osd

# Resume from a component onward in the build order.
src2deb build recipes/cosmic-epoch --from cosmic-osd
```

`--skip-published` skips a component whose source resolves to what a prior run
recorded as built in the [manifest](provenance.md), so an interrupted or repeated
run rebuilds only what changed. `--only` (repeatable) builds just the
named components, and `--from` builds a component and everything after it in the
order; the two are mutually exclusive.

### What a narrowed run needs from the pool

Both `--only` and `--from` leave components out, and whatever the selected
components build-depend on still has to come from somewhere: an archive package
from the archive, and a package another component of the recipe produces from
the [local pool](how-a-build-runs.md#the-local-pool), where an earlier run put
it. On a warm pool that is automatic — skipped components' packages are still
there, and a selected component resolves against them.

On a pool that has never held them it is not, and src2deb says so before it
provisions anything:

```text
src2deb: unsatisfiable build-dependency: this run builds "cosmic-osd", which
  build-depends on "libcosmic-randr-dev"; that package is produced by component
  "cosmic-randr", which --only leaves out, and the pool does not hold it. Select
  "cosmic-randr" as well, or build it first
```

A selection naming a component the recipe does not have is refused the same way,
before a single source is cloned.

Every run resolves every component's source whatever it was asked to build,
because the build order is read from all of them. A selective run over a cold
work directory therefore still clones the whole recipe once; later runs only
fetch. A component outside the selection whose source will not resolve is
reported and passed over rather than ending the run — it was never going to be
built.

Both subcommands take `-q`/`--quiet` and `-v`/`--verbose`:

- `--quiet` prints only failures, cancellation, and the closing summary.
- The default prints the progress narrative, the provisioning counters, the
  shared base's package count, any note that the run's guarantees changed, and
  each build's in-cage output.
- `--verbose` adds per-component resolve and vendor detail, each root's own
  package count, and per-package provisioning detail.

See [Provisioning progress](how-a-build-runs.md#provisioning-progress) for what
each level reports while a build root is being provisioned.

## Output

Each component's artifacts — the `.deb` files, and the `.changes` and
`.buildinfo` that describe them — are written under
`<work>/out/<suite>/<architecture>/<component>/`. The local pool under
`<work>/pool/<suite>/<architecture>/` holds the same packages in a
`dists`-structured archive that later builds resolve against.

The versions on those packages are not the ones the components' changelogs
declare. Every build is stamped with the suite it was built for, the build date,
and the source revision — `1.0.0~alpha.7-1+deb13.20260731.abc1234` — which is
what lets a rebuild reach a machine that already installed the previous one. See
[Package versions](package-versions.md).

Each run also writes a provenance manifest to
`<work>/manifests/<recipe>/<suite>/<architecture>.toml`, mapping every component
to what its source resolved to, the `.buildinfo` its build wrote, and the package
versions it produced.

Those three are what a run is *for*. The work directory holds a good deal more
besides — the sources, the package cache, and the build roots, which are where
its size actually goes. See [The work directory](work-directory.md). For getting
the pool onto a machine that installs from it, see
[Using the pool](using-the-pool.md).

All three are keyed by the same identity: the recipe, suite, and architecture the
run targeted. Recipes may therefore share one `--work` directory freely — that is
how separate recipes publish into one pool — and so may the same recipe retargeted
at another suite or architecture, without either run overwriting the other's
packages, artifacts, or provenance. See
[The local pool](how-a-build-runs.md#the-local-pool) for why the key is that
pair, and [The provenance manifest](provenance.md).

## One run at a time

A run holds an exclusive lock on its work directory for its whole duration, so a
second run against the same `--work` is cleanly rejected rather than corrupting
the shared pool and output. A run killed outright — or ended by a second Ctrl-C —
leaves the lockfile behind; the rejection message names it so it can be removed
by hand.
