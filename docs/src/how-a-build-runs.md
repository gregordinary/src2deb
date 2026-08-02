# How a build runs

A build is driven by the engine over a recipe. For each recipe, in order:

## 1. Resolve

Every component's source is put under the work directory. A git source is cloned
or updated, its ref checked out, and its submodules initialized — so a submodule
superproject such as cosmic-epoch resolves its members. A `source.path` tree is
copied under the work directory as it stands, afresh each run, so nothing writes
into the tree the recipe named. Either way the result is the source tree that
holds the component's `debian/` directory, and it is one src2deb owns.

Every component is resolved, whatever the run was asked to build, because the
build order derives from all of them: which component produces a package is read
from that component's own `debian/control`, so the graph is complete only once
every control file has been read. A `--only` run over a cold work directory
therefore clones the whole recipe once; later runs fetch.

A component whose source will not resolve is a failure of that component, not of
the run — the same as one whose build fails. Without `--keep-going` the run still
stops at the first; with it, the run goes on and the component is recorded as
failed. A component outside the run's
[selection](quick-start.md#resume-and-selective-builds) is weaker still: the run
was never going to build it, so its source failing is reported and passed over
whatever `--keep-going` says.

Any Git LFS content in a resolved tree is then fetched. A repository may hold
large assets outside itself, leaving a short text pointer in their place, and a
checkout made without LFS support writes those pointers as ordinary files. They
are valid files, so a build embeds or installs one and succeeds; the
substitution only surfaces when the installed program reads the asset and finds
a stub. Resolve therefore fetches the real content and then verifies no pointer
survives in the tree it is about to build. A surviving pointer fails the
component, on the same terms as any other resolve failure.

The check is scoped twice over. It covers the subdirectory actually built, so an
unrelated component's assets elsewhere in a superproject do not concern it; and
within that, the files git tracks, which is the only place a pointer can come
from. That second bound also keeps it off a previous build's vendored crates,
which stay in the tree between runs and are not the component's assets.

A `source.path` tree is scanned on the same terms but never fetched for: a
pointer there fails the component with the command that fixes it, and src2deb
leaves the tree alone. A path that is not part of a git working tree carries no
pointers, and is passed over.

Last, a component's declared patch series is applied over the resolved tree, in
the order the recipe lists it. This happens inside resolve, before any
`debian/control` is read, so a patch may change what a component build-depends
on or what it produces and the build order follows the patched file. A patch
that does not apply fails the component; the series is a pinned input of the
component's fingerprint, so changing it rebuilds. See
[Patches](recipes.md#patches).

## 2. Plan

Each component's `debian/control` is read for two things: what it build-depends
on, and the binary packages it produces (its `Package:` stanzas). An edge runs
from a producer to each consumer, and a topological sort of that graph yields the
build order. Build-dependencies the recipe's own components do not produce are
left to archive and pool resolution at provision time, so the components may be
listed in any order.

Only the first alternative of an `a | b` build-dependency group is considered,
so an in-set build edge must be a direct dependency rather than a later
alternative. The COSMIC recipe's single edge is direct, so this bounds how a
future recipe may express an edge rather than affecting the current build.

## 3. Build each component

For each component, in order:

- A build root is provisioned — the base system, the toolchain, and that
  component's build-dependencies, resolved against the archive sources and the
  local pool. See [Build roots](build-roots.md), and
  [Provisioning progress](#provisioning-progress) below for what it reports
  while it works.
- The component is built in two cage passes, for packages that vendor Rust
  crates, as COSMIC's do. The vendor pass runs `debian/rules clean` in a cage
  with the host network, which triggers the component's own vendoring and leaves
  a `vendor.tar` in the tree. The build pass runs `dpkg-buildpackage -nc` in a
  cage with an isolated network, building offline from that `vendor.tar`; `-nc`
  keeps it from re-triggering vendoring. Before it builds, the build pass
  prepends a `debian/changelog` entry stamping the build's version — see
  [Package versions](package-versions.md) — to its own copy of the tree, leaving
  the resolved checkout as upstream wrote it. Both passes carry
  `SOURCE_GIT_HASH`, the resolved commit, which packaging reads to stamp a
  revision into the built binary. Output is assembled into lines and streamed to
  the terminal, and the artifacts are read from the `.changes` file the build
  writes.
- The finished packages are published to the local pool, so the next component
  resolves against them.

A component's failure is recorded rather than propagated: by default the run
stops at the first failure, and with `--keep-going` it continues to the next
component. Either way the run finishes with a report of what built and what
failed.

What ends a run outright is what leaves it nothing coherent to do: a selection
naming a component the recipe does not have, a dependency cycle, a target suite
with no version tag, a selection that leaves out a producer of a selected
component's build-dependencies, and a shared base that will not bootstrap. Each
of those is settled before the build phase, so the run prints the error alone and
writes no manifest — there is nothing yet to record.

### Building in parallel

With `--jobs N`, up to `N` components build concurrently. A scheduler over the
dependency graph releases each component the moment the components producing its
build-dependencies have published, so the build fans out across the graph's
independent components while still ordering each consumer after its producers.
Every phase runs in parallel, including the two that touch shared state. The
package cache stages each download under a name unique to its writer, so two
components fetching the same package both succeed; and the pool serializes
publishes internally while making each one visible to a reader all at once, so a
component may resolve against the pool while another publishes into it. A single
job (the default) reproduces the sequential order exactly.

## 4. Record

The run writes a provenance manifest to
`<work>/manifests/<recipe>/<suite>/<architecture>.toml`, mapping every component
to what its source resolved to, the `.buildinfo` its build wrote, and the package
versions it produced. See [The provenance manifest](provenance.md).

## Provisioning progress

Provisioning is the longest stretch of a run: a cold shared base resolves,
fetches, and unpacks several hundred packages before the first build starts. It
reports as it goes, labeled with the root each event belongs to — `base` for the
shared base, and the component's own name for a per-component root — so a
concurrent run's interleaved output stays attributable.

At the default verbosity a run announces each root as it starts on it, says how
many packages the shared base will install, then counts the packages it
downloads and unpacks:

```text
src2deb: provisioning the shared base
src2deb: base: 163 package(s) to install
src2deb: base: downloading 66/163
src2deb: base: unpacking 66/163
src2deb: base: installing the rustup 1.97.0 toolchain
src2deb: building cosmic-comp (1/5)
src2deb: provisioning the build root for cosmic-comp
```

The base's package count reports by default because it is the largest single
commitment a run makes, and a cold work directory has no other way of saying so
before the counters start. Each component's own count stays behind `-v`, where
it is one line among many rather than the run's headline cost.

The toolchain line appears only when the recipe pins one and the root was
actually provisioned; a root reused from a prior run already carries it. Its
own output is captured rather than printed, so a failure reports what the
installer wrote and a successful install says nothing further.

On a terminal, and with a single job, the counter is one line rewritten in
place. With `--jobs N`, or with stderr redirected to a file, it becomes a line
each time the count passes a tenth of the total — several workers rewriting one
row is unreadable, and a redirected stream would collect carriage returns rather
than lines.

A package the shared cache already holds is not downloaded, so a warm cache
counts only what it has to fetch, and the unpack counter carries the run.

`-v` replaces the counters with a line per package, and adds each URL fetched
and the size of each resolved package set. `-q` prints no provisioning progress
at all.

### Notes that report whatever the verbosity

Two things report at every verbosity above `-q`, because they change what the
run *guarantees* rather than what it is doing:

- **A foreign-architecture target**, which runs every compiler invocation under
  emulation. See [Cross-architecture builds](cross-architecture.md).
- **No unprivileged overlay**, which drops the run to [full
  reprovisioning](build-roots.md#full-reprovisioning), the weaker of the two
  isolation guarantees.

The layered default stays behind `-v`, as the stronger strategy and the ordinary
case. Both notes are shown in
[Troubleshooting](troubleshooting.md#notes-that-are-not-failures).

### In-cage build output

Each line a build writes is passed through indented by two spaces, from both its
standard output and its standard error, unchanged otherwise. The two streams
render alike because Debian's build tooling uses the choice of stream to
separate its output rather than to signal severity — `dpkg-buildpackage` writes
its ordinary progress to stderr. With `--jobs N`, each line also carries its
component, since several builds' output interleaves.

## Cancelling a run

Ctrl-C — or `SIGTERM` — stops a run at the next point where stopping leaves a
coherent state behind, rather than killing the process mid-provision. The run
prints `cancelled; stopping`, winds down, writes its manifest, and exits `130`.

What "the next point" means depends on what the run is doing:

| While it is | A cancel takes effect |
| --- | --- |
| Cloning sources | Between components |
| Resolving or downloading a root's packages | At the next package |
| Unpacking a root | At the next package |
| Configuring a root (`dpkg --configure`) | When configuration finishes |
| Installing a pinned Rust toolchain | When the install finishes |
| Staging a layered increment | When the increment finishes |
| Building a component | Within a fraction of a second |

A build is stopped with `SIGTERM` first, so `dpkg-buildpackage` can finish the
file it is writing, and killed if it has not exited within five seconds. Every
build sandbox is also tied to this process's lifetime, so even a src2deb that is
killed outright leaves no in-cage build running.

A second Ctrl-C exits immediately, for the cases in the table above where the
first has to wait.

### What a cancelled run leaves behind

Everything the next run can pick up from:

- **Components that finished** are built, published to the pool, and recorded in
  the manifest as built. A later `--skip-published` run skips them.
- **The component the cancel interrupted, and any it never reached,** are
  recorded as skipped, with what their source resolved to. Nothing claims
  they were built.
- **A partly-provisioned build root** is removed rather than left half-made, so
  it can never be mistaken for a usable one. The next run provisions it from
  clean.
- **The work directory lock** is released, unless the run was ended by a second
  Ctrl-C or killed outright. Then `<work>/.lock` survives, and the next run
  reports it as locked and names the file to remove.

The run's exit status is `130` whenever it was cancelled, including when a
component had already failed: the run did not finish, so nothing can be
concluded about the components it never reached.

## The local pool

The pool is a `dists/`-structured `.deb` archive, trusted without a signature,
that carries build-dependencies from one component to the next. It is written up
front as a valid empty pool, so the first component can declare it as a
repository; each build then adds its packages and regenerates the index. A
component that build-depends on an earlier one resolves against the packages
src2deb just built.

A pool lives at `<work>/pool/<suite>/<architecture>/` and belongs to that pair.
Any number of recipes may publish into one pool — that is what sharing a work
directory is for — but a run targeting another suite or architecture publishes
into a pool of its own.

Scoping by architecture is a requirement. An `Architecture: all` package's file
name carries no architecture, and its
[stamped version](package-versions.md) is identical however the package was
built, so the same component built for amd64 and for arm64 yields one file name
for two different files. Sharing a pool would mean the second publish overwriting
the first and leaving the earlier architecture's `Packages` naming a checksum that
no longer matches — a hash mismatch at the next `apt update`.

Scoping by suite is a choice. A rebuild for another suite differs in the
`deb13`/`deb14` field of its version, so its file names differ and it could
share the pool. It gets one of its own so that a pool is a single servable
archive — one that can be signed, mirrored, or discarded without reference to
another suite's builds, and whose index accounts for every file beside it. That
is a departure from Debian's layout, where one `pool/` serves every distribution
in the archive.

## Publishing is incremental and forward-only

A publish merges into whatever the pool already indexes rather than replacing it,
and keeps the highest version of each package name. Two consequences are worth
knowing before serving a pool to anything but the next build:

- **A lower version does not publish.** Its `.deb` is copied into the pool, but
  the index keeps the higher version it already recorded. Correcting a package's
  version downward therefore needs a pool that never saw the higher one.
- **There is no unpublish.** Nothing removes a package from the index, and
  deleting a `.deb` by hand leaves the index naming a file that is no longer
  there, which a client reports as a failed fetch. To stop shipping a package,
  stop anything from depending on it rather than trying to take it out of the
  pool.

Both follow from the index being merged under the pool's lock, which is what lets
one component resolve against the pool while another publishes into it.

### The pool directory grows without bound

The index keeps only the highest version of each package name, but nothing
removes the file a higher version superseded. Because every build carries the
build date in its [version](package-versions.md), each run writes a fresh set of
`.deb` files and leaves the previous set on disk, indexed by nothing. A recipe
rebuilt daily accretes its full artifact set per day, indefinitely.

That is the pool's design — a publish is additive and takes no view on what came
before — so pruning is an operational task rather than something a run does. A
superseded file is safe to delete once no index names it, which for a pool that
has published since means every `.deb` of a package other than its highest
version. Plan for it wherever the pool is served from, and watch the directory's
size rather than the index's.

The pool is not the largest thing in a work directory, though: the shared base
and the package cache usually are. See
[The work directory](work-directory.md#where-the-size-goes).

### Signing follows a run, never precedes it

A run publishes an empty set into its pool before building, so the first
component has a valid `Release` to declare as a repository. A publish replaces
that `Release`, and a signature only covers the `Release` it was made over, so
publishing discards any signature the pool carried.

Sign a pool after the run that finished it, and re-sign after every run that
publishes into it. A run with nothing to build does not touch the pool at all, so
a signed pool stays signed across a fully-skipped re-run.
