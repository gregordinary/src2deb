# The provenance manifest

Every run writes a manifest to the work directory, tying the run's inputs to its
outputs: the commit each component's source resolved to, the sandbox its builds
ran in, and the package versions each build produced. It is the basis of a
reproducibility story — the manifest names the revisions to check out and the
conditions they were built under.

## Location

A manifest belongs to one recipe built for one suite and architecture, and is
written to that identity's own path at the end of every run, once every
component's outcome is known:

```
<work>/manifests/<recipe>/<suite>/<architecture>.toml
```

Work directories are shared deliberately: pointing two recipes at one `--work`
is how their packages reach a single pool. Giving each identity its own manifest
keeps that composable. A run neither overwrites the provenance of the recipe
built before it, nor reads resume state from a run that targeted another suite or
architecture — where the pool it would resolve against holds none of the same
packages, since the pool and the output tree are keyed by that same suite and
architecture.

## Contents

The manifest records the recipe's identity, the sandbox the run's builds ran in,
and one entry per component, in build order. A component that built carries its
produced packages and their versions; a component that failed carries the failure
reason. Both carry the commit their source resolved to, so a run that stops
partway still records the exact inputs it reached.

```toml
recipe = "cosmic-epoch"
suite = "trixie"
architecture = "amd64"

# The [sandbox] section goes here; see below.

[[component]]
name = "cosmic-randr"
commit = "1f3a9c2e5b7d..."
status = "built"

  [[component.package]]
  name = "cosmic-randr"
  version = "1.0.0-1+deb13.20260731.1f3a9c2"

  [[component.package]]
  name = "libcosmic-randr-dev"
  version = "1.0.0-1+deb13.20260731.1f3a9c2"

[[component]]
name = "cosmic-osd"
commit = "9b2e4d6a8c1f..."
status = "failed"
error = "building cosmic-osd: dpkg-buildpackage exited with status 2"
```

A component's resolved commit is the exact `HEAD` its source was checked out at,
so a branch or default-branch ref is recorded as the concrete revision it
resolved to, not the moving ref that named it. A component that failed before it
had one — its source would not clone, or its `debian/control` would not read —
records an empty `commit`, which is the manifest saying it never got that far
rather than naming a revision it did not reach.

The recorded versions are the stamped ones the packages actually carry, so the
suite, the build date, and the abbreviated revision are legible from the manifest
as well as from `apt policy`. See [Package versions](package-versions.md).

## The sandbox record

What a build produces depends on the environment it runs in and the filesystem it
sees, and neither follows from the source revisions. The `[sandbox]` section
records both, as the build cage actually resolved them:

```toml
[sandbox]
component = "cosmic-randr"

  [sandbox.env]
  HOME = "/root"
  PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
  SOURCE_GIT_HASH = "6e8e795970fa06d434af22775e415b517f7552d3"

  [[sandbox.mount]]
  kind = "procfs"
  target = "/proc"

  [[sandbox.mount]]
  kind = "tmpfs"
  target = "/dev"
  flags = 2
  data = "mode=0755"

  [[sandbox.mount]]
  kind = "symlink"
  path = "/dev/stdin"
  target = "/proc/self/fd/0"

  [[sandbox.mount]]
  kind = "bind"
  source = "/work/sources/cosmic-randr"
  target = "/src"
  read-only = true
```

`SOURCE_GIT_HASH` is the resolved commit, passed to both passes. Packaging that
stamps a revision into the built binary reads it from there, so the package
reports the commit the manifest names. `env` is the build command's complete
environment. `mount` is every mount the
sandbox established, in the order it established them — the managed profile
first, then src2deb's own read-only source bind and read-write output bind. Each
entry carries a `kind`: `tmpfs`, `procfs`, `devpts`, `bind`, `raw`, or `symlink`.

Recording this rather than assuming it matters because the sandbox's base
environment and managed mount profile are not fixed by the sandbox library's
version — they may change between releases. The manifest states what a build ran
under instead of leaving it to be inferred.

The record is run-level, because every component's build applies the same
environment and the same mount sequence, differing only in the host paths of the
source and output binds. `component` names the one it was taken from: the
earliest in build order the run built, so a `--jobs N` run records what a
sequential run would. A run that builds nothing keeps the record already in the
manifest, since the packages it still calls built were built under it.

Only the build pass is recorded. The vendor pass runs with the host network to
fetch sources into the tree; it does not produce packages, so what it ran under
says nothing about what the packages were built from. See
[How a build runs](how-a-build-runs.md).

## Resume state

The manifest is also the build-state record `--skip-published` reads: a component
is skipped when its source resolves to the commit the manifest already records as
`built`. Each run folds the prior manifest forward — a component this run did not
build keeps its earlier record — so the manifest always describes the whole
recipe, and a chain of selective runs stays consistent.

A run reads only the manifest of its own recipe, suite, and architecture, so
retargeting a recipe starts from a clean slate rather than skipping components on
the strength of packages built for somewhere else.
