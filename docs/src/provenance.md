# The provenance manifest

Every run writes a manifest to the work directory, tying the run's inputs to its
outputs: what each component's source resolved to, the sandbox its builds ran in,
and the package versions each build produced. It is the basis of a
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

The manifest records the recipe's identity, the date the run's versions were
stamped with, the sandbox the run's builds ran in, and one entry per component,
in build order. A component that built carries its produced packages and their
versions; a component that failed carries the failure reason. Both carry what
their source resolved to, so a run that stops partway still records the exact
inputs it reached.

```toml
recipe = "cosmic-epoch"
suite = "trixie"
architecture = "amd64"
build-date = "2026-07-31"

# The [sandbox] section goes here; see below.

[[component]]
name = "cosmic-randr"
status = "built"

  [component.buildinfo]
  path = "out/trixie/amd64/cosmic-randr/cosmic-randr_1.0.0-1+deb13.20260731.1f3a9c2_amd64.buildinfo"
  sha256 = "4c1a0f..."

  [[component.source]]
  role = "source"
  kind = "git"
  value = "1f3a9c2e5b7d..."
  pinned = true

  [[component.package]]
  name = "cosmic-randr"
  version = "1.0.0-1+deb13.20260731.1f3a9c2"

  [[component.package]]
  name = "libcosmic-randr-dev"
  version = "1.0.0-1+deb13.20260731.1f3a9c2"

[[component]]
name = "cosmic-osd"
status = "failed"
error = "building cosmic-osd: dpkg-buildpackage exited with status 2"

  [[component.source]]
  role = "source"
  kind = "git"
  value = "9b2e4d6a8c1f..."
  pinned = true
```

## The source record

Each `[[component.source]]` entry is one input the component was built from. It
names four things:

- `role` — what part the input played in assembling the tree. The component's own
  source is `source`; a [packaging overlay](recipes.md#packaging-overlays) is
  `packaging`; a [patch series](recipes.md#patches) is `patches`.
- `kind` — what sort of input it is. A git checkout is `git`; a fetched release
  archive is `sha256`; a source tree on disk is `path`; a patch series is
  `patches`; a packaging directory on disk is `tree`.
- `value` — what identifies it. For `git`, the exact `HEAD` the tree was checked
  out at, so a branch or default-branch ref is recorded as the concrete revision
  it resolved to, not the moving ref that named it. For `sha256`, the digest the
  archive was verified against before it was unpacked. For `path`, the canonical
  directory the tree was read from, so the record names one path however the
  recipe reached it. For `patches`, a SHA-256 over the series' members in the
  order they were applied. For `tree`, a SHA-256 over the contents of the
  `debian/` directory the overlay supplied.
- `pinned` — whether the value names the exact content that went into the build.
  A hash does. A value that only says where a tree was read from does not, since
  the tree may be anything by the time the record is read.

`tree` and `path` both name a directory on disk and differ in what is recorded
about it. A [packaging overlay](recipes.md#packaging-overlays) contributes one
small directory that nothing writes to, so src2deb measures it and records what
it held; a component's own source is an arbitrarily large tree the build writes
into, so the record names where it was read from and says plainly that it pins
nothing. The recipe stays the authority for *where* either was; the manifest is
the authority for *what* the first held.

`sha256` and `tree` are both digests and differ in where the digest came from.
A `sha256` is one the recipe declared and src2deb verified against an archive it
fetched, so the record names something that can be fetched again and checked; a
`tree` is one src2deb measured off a directory the recipe pointed at. Both pin
content, and only the first pins something a third party can obtain.

A build from a tree on disk therefore records:

```toml
  [[component.source]]
  role = "source"
  kind = "path"
  value = "/home/someone/checkouts/cosmic-comp"
  pinned = false
```

which is the manifest saying plainly that this build cannot be reproduced from
what it records. Where the tree was is worth keeping — it is the only trace of
what was built — but it is not a revision, and the record does not let it pass
for one.

A component's tree may be assembled from more than one input, and its record
then carries one entry per input, in the order they were applied: the source,
then any [packaging overlay](recipes.md#packaging-overlays), then any
[patch series](recipes.md#patches). A component whose packaging comes from a
second repository and which carries a local fix records all three:

```toml
  [[component.source]]
  role = "source"
  kind = "git"
  value = "1f3a9c2e5b7d..."
  pinned = true

  [[component.source]]
  role = "packaging"
  kind = "git"
  value = "8d4b0e1c7a92..."
  pinned = true

  [[component.source]]
  role = "patches"
  kind = "patches"
  value = "5f2e1a9c3b8d..."
  pinned = true
```

The role is what tells the two `git` entries apart. Nothing else does: a
packaging repository and a source repository are the same sort of thing, and only
the part each played says which was which. `SOURCE_GIT_HASH` carries the one
whose role is `source`, so packaging that stamps a revision into what it builds
reports the source's rather than its own.

Role and kind answer different questions, and neither implies the other: an
overlay may come from a repository or from a tree on disk, and so may a source.
The one pairing that always holds is a patch series, whose kind and role are both
`patches` — it is identified by a digest over the patches, and applying patches is
the only thing it does.

A patch series' digest covers the series' members in the order they were
applied, so it changes when a patch is edited, added, removed, or reordered —
and not when one is merely renamed. The recipe remains the authority for *which*
patches were applied; this records *what* they were.

`pinned` follows from the kind, and is written out so that a reproducible build
can be told from one that only looks like one without knowing which kinds are
which. It is what `--skip-published` rests on: see [Resume
state](#resume-state).

A component that failed before it resolved anything — its source would not
clone, or its `debian/control` would not read — carries no entry at all, which is
the manifest saying it never got that far rather than naming an input it never
reached.

The recorded versions are the stamped ones the packages actually carry, so the
suite, the build date, and the abbreviated source are legible from the manifest
as well as from `apt policy`. See [Package versions](package-versions.md).

## The declared version

A component whose packaging carries no `debian/changelog` takes its version from
the recipe, and the record names it:

```toml
[[component]]
name = "foo"
status = "built"
version = "1.2.3"
```

`version` is the upstream version the recipe declared or derived — the base the
stamp extends — and it appears only for a component that declares one. A
component that takes its version from a changelog it already has records no such
field.

It sits beside the source record rather than in it, because it is not a tree the
build consumed: it is a name the recipe gave. It is recorded all the same,
because it is the one thing a recipe can change that produces different packages
while every input the fingerprint names stays exactly where it was — so
`--skip-published` compares it alongside the fingerprint. See [Components with no
changelog](recipes.md#components-with-no-changelog).

## The build date

`build-date` is the date the run stamped into every version it produced, as
`YYYY-MM-DD`. Passing it back as `--build-date` reproduces that run's versions
and hands the build the same `SOURCE_DATE_EPOCH`; `--build-date manifest` reads
it from here rather than making you transcribe it. See [Pinning the
date](package-versions.md#pinning-the-date).

Like the sandbox record, it is carried forward: a run that builds nothing keeps
the date of the run that produced the packages this manifest still calls built.
Overwriting it with the date of a run that produced nothing would make a later
reproduction build against the wrong clock.

## The `.buildinfo` reference

`dpkg-buildpackage` writes a `.buildinfo` for every build, and src2deb keeps it
alongside the packages in the output tree. A component that built names it:

- `path` — where the file is, relative to the work directory, so a work directory
  that is moved or copied keeps a manifest whose references still resolve.
- `sha256` — the file's checksum, measured from the bytes on disk, so the
  recorded file can be told from one that has since changed.

`.buildinfo` is Debian's own record of what a package was built against: the
exact set of packages installed in the build root, the build environment, and the
checksums of the binaries produced. The `[sandbox]` section below records the
environment and the filesystem a build saw, but not the installed package set —
`.buildinfo` is what carries that, in the format the rest of Debian's tooling
already reads.

The manifest names the file rather than restating what it holds, so there is one
authority for it instead of two that can disagree. src2deb writes it and carries
it; it does not interpret it.

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
is skipped when its source resolves to what the manifest already records as
`built`, at the same [declared version](#the-declared-version). Every input has to
match, so a component gains a rebuild the moment any one of them moves. Each run
folds the prior manifest forward — a component this run did not build keeps its
earlier record — so the manifest always describes the whole recipe, and a chain
of selective runs stays consistent.

A source that is not pinned is never skipped, however exactly it matches the
record. An unpinned value says where a tree was read from and not what it held,
so a run agreeing with the record establishes nothing about whether the source
moved; skipping on that basis would publish an earlier build as though it were
this one. In practice that means a `source.path` component is rebuilt on every
run, which is the right answer for a tree someone is editing.

A run reads only the manifest of its own recipe, suite, and architecture, so
retargeting a recipe starts from a clean slate rather than skipping components on
the strength of packages built for somewhere else.
