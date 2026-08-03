# The provenance manifest

Every run writes a manifest to the work directory, tying the run's inputs to its
outputs: what each component's source resolved to, the sandbox its builds ran in,
and the package versions each build produced. It is the basis of a
reproducibility story — the manifest names the revisions to check out and the
conditions they were built under.

## Location

A manifest belongs to one recipe built for one suite and architecture, and is
written to that identity's own path as each architecture finishes, once every
component's outcome for it is known — so a run building for two architectures
writes two manifests:

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
stamped with, the sandbox the run's builds ran in, the archives its build roots
resolved against, and one entry per component, in build order. A component that
built carries its produced packages and their versions; a component that failed
carries the failure reason. Both carry what their source resolved to, so a run
that stops partway still records the exact inputs it reached.

```toml
recipe = "cosmic-epoch"
suite = "trixie"
architecture = "amd64"
build-date = "2026-07-31"

# The [sandbox] and [[archive]] sections go here; see below.

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
  archive is `sha256`; a [Debian source
  package](recipes.md#rebuilding-a-debian-source-package) is `dsc`; a source tree
  on disk is `path`; a patch series is `patches`; a packaging directory on disk
  is `tree`.
- `value` — what identifies it. For `git`, the exact `HEAD` the tree was checked
  out at, so a branch or default-branch ref is recorded as the concrete revision
  it resolved to, not the moving ref that named it. For `sha256`, the digest the
  archive was verified against before it was unpacked. For `dsc`, the digest of
  the `.dsc` itself, which declares the digest of every file the package is
  assembled from and so pins all of them. For `path`, the canonical
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

`dsc` is a declared-and-verified digest like `sha256`, and is a kind of its own
because it identifies something else: not one archive but a whole Debian source
package, and one built without the vendor pass — so the record says both what the
build consumed and that it was [hermetic
throughout](how-a-build-runs.md#the-vendor-pass-and-the-one-source-that-skips-it).

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

What a build produces depends on what it was rooted on, the identity it held, the
network and limits it ran under, the hardening applied to it, the environment it
carried, and the filesystem it saw. None of that follows from the source
revisions. The `[sandbox]` section records all of it, as the build cage actually
resolved it:

```toml
[sandbox]
component = "cosmic-randr"
network = "isolated"

  [sandbox.root]
  kind = "overlay"
  lower = ["/mnt/build/work/base/trixie/arm64"]
  upper = "/mnt/build/work/uppers/trixie/arm64/cosmic-randr"
  work = "/mnt/build/work/uppers/trixie/arm64/.cosmic-randr.work"

  [sandbox.identity]
  kind = "single"

  [sandbox.hardening]
  kind = "unavailable"

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

- **`root`** is what the command's filesystem was, and is its own field rather
  than a mount because it is what the mounts were laid over. `overlay` is the
  layered strategy: the shared base as the read-only `lower`, the component's
  build-dependency increment as the writable `upper`. `plain` is full
  reprovisioning's per-component root. Without this a plain root and an overlay
  over the same base produced byte-identical records while describing two
  different builds.
- **`identity`** is `single` for every build src2deb runs: the calling user is
  root inside the sandbox and no other id is mapped. It decides whether a build
  sees uid 0, which is what `Rules-Requires-Root` handling turns on.
- **`network`** is `isolated` for a build pass. It is the first thing a
  reproducibility claim is challenged on.
- **`rlimit`** entries are the resource limits in force, and are absent because
  src2deb sets none. A build that adapts its parallelism to `RLIMIT_NOFILE`
  produces different output.
- **`hardening`** is `unavailable` when the sandbox library was built without the
  hardening layer, which is how src2deb builds it. That is recorded rather than
  omitted, and it is a different fact from `applied` with no controls set — a
  build that could have hardened and did not.
- **`env`** is the build command's complete environment. `SOURCE_GIT_HASH` is the
  resolved commit, passed to both passes; packaging that stamps a revision into
  the built binary reads it from there, so the package reports the commit the
  manifest names.
- **`mount`** is every mount the sandbox established, in the order it established
  them — the managed profile first, then src2deb's own read-only source bind and
  read-write output bind. Each carries a `kind`: `tmpfs`, `procfs`, `devpts`,
  `bind`, `raw`, or `symlink`.

Recording all of this rather than assuming it matters because the sandbox's base
environment and managed mount profile are not fixed by the sandbox library's
version — they may change between releases. The manifest states what a build ran
under instead of leaving it to be inferred.

The record is run-level, because every component's build applies the same
environment, the same mount sequence, and the same posture, differing only in
host paths: the source and output binds name the component's own directories,
and under the layered strategy so does the overlay upper. What an overlay root
says about a build is its shape — a merge of this read-only lower stack with a
writable layer — and the lower stack is the shared base every component builds
over. `component` names the one it was taken from: the earliest in build order
the run built, so a `--jobs N` run records what a sequential run would. A run
that builds nothing keeps the record already in the manifest, since the packages
it still calls built were built under it.

A manifest an older src2deb wrote may not carry a field this one requires, and is
refused rather than read with the missing fields defaulted — a default would have
the record state a posture no build observed. Delete it and rebuild.

Only the build pass is recorded. The vendor pass runs with the host network to
fetch sources into the tree; it does not produce packages, so what it ran under
says nothing about what the packages were built from. See
[How a build runs](how-a-build-runs.md).

## The interpreter record

A foreign build runs every target binary through a `qemu-user` interpreter:
`rustc`, `cc`, `ld`, and every configure probe execute under an emulator, and a
changed emulator silently changes compiled output. The manifest already named the
architecture and whether the build was foreign; `[interpreter]` names what
executed it.

```toml
[interpreter]
name = "aarch64"
path = "/usr/libexec/qemu-binfmt/aarch64-binfmt-P"
resolved = "/usr/bin/qemu-aarch64-static"
sha256 = "bfcd46c842441912baed36158569ac29a7fb656684ca73c1b3b2f0f3971e9bec"
enabled = true
flags = "POF"
```

The values come from the kernel's own `binfmt_misc` registration, which is the
path binaries actually execute through — not whatever a `PATH` lookup would find,
and the build environment has no `PATH` to look in anyway. `path` is the
registration as written, usually a wrapper; `resolved` is that path
canonicalized, and the two are separate facts because repointing the symlink
changes the interpreter without changing the registration.

**The digest carries a caveat.** It is of `path`, which `open` follows the
symlink along, so it is the real binary's bytes. But the `F` flag means the
kernel opened and holds the interpreter at *registration* time, so a digest taken
during a build may be of a file that replaced the one actually running. Two runs
whose digests differ definitely ran different interpreters; two that agree agree
only about the file on disk.

A native build records no `[interpreter]` at all. Nothing interpreted anything,
which is a different statement from having failed to look.

## The archive record

A component's packages were built against a set of build-dependencies, and those
came from somewhere. Each `[[archive]]` entry says where, as the resolver found
it:

```toml
[[archive]]
mirror = "http://deb.debian.org/debian"
suite = "trixie"
components = ["main"]
release-sha256 = "74122bafc4253d3d42ba3657a21f7219aed1423dcbeb1b3b2c2d52fb66ed7070"
date = "Sat, 11 Jul 2026 09:02:23 UTC"
valid-until = "Sat, 18 Jul 2026 09:02:23 UTC"
signed-by = ["4CB50190207B4758A3F73A796ED0E7B82643E131"]

[[archive]]
mirror = "file:///mnt/build/work/pool/trixie/amd64"
suite = "trixie"
components = ["main"]
release-sha256 = "c50692c33fa2726827a5a6173eedd3d8f56a8f69f52a2ccbe1feabae7186f610"
date = "Mon, 03 Aug 2026 06:41:35 UTC"
signed-by = []
```

- **`mirror`** is the URL that answered, not the list that was configured. A
  repository with a fallback resolves against whichever mirror served.
- **`release-sha256`** is the digest of the release body that was verified. For a
  signed archive that is the cleartext the signature covers, so it names the
  exact archive state the signature vouched for.
- **`signed-by`** is the key that verified it. Written empty rather than omitted
  for an archive trusted unsigned — the run's own pool is one — because an
  archive that verified nothing is a fact, and an absent key could not be told
  from a record written before the field existed.
- **`valid-until`** appears only when the release carried one. Debian's do; a
  locally written pool's does not.

This is what the plan key each root is cached on cannot say. That key digests the
selection — names, versions, and package digests — and says nothing about what
the selection was made from, and the same suite resolves to different versions a
week apart.

Every root a run provisions resolves against the same archives, so a run observes
each of them many times over. The states are compared rather than assumed
identical, and only the distinct ones are recorded. One entry per configured
repository is the ordinary result. **Two entries for one mirror and suite is a
run that saw the archive publish while it was building against it**, and the
`date` each carries is what orders them: some of the run's roots hold packages
selected from one state and some from the other.

The record covers the run rather than any one component, and a run that
provisions nothing keeps what is already there, both for the same reasons the
sandbox record does.

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

Each architecture reads only the manifest of its own recipe, suite, and
architecture, so retargeting a recipe starts from a clean slate rather than
skipping components on the strength of packages built for somewhere else. A run
building for two architectures therefore skips per architecture: one may be up to
date while the other has everything to build.

## Carrying the record with the packages

[`src2deb export`](publishing.md) copies each architecture's manifest into the
export beside the packages it describes, under
`manifests/<recipe>/<architecture>.toml`. A publisher archiving a release
therefore keeps the record of how it was built without reading anything under a
build host's work directory.
