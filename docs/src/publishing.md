# Publishing to an archive

The [pool](using-the-pool.md) a run leaves behind is a complete archive for one
suite and one architecture, and serving it directly is the shortest path from a
build to `apt install`. A published archive is usually a different shape: one
`Release` covering every architecture of a suite, managed by an archive tool
with snapshots and a signing key behind it.

`src2deb export` bridges the two. It copies what a work directory holds into a
directory laid out for an archive tool to ingest, so a publisher never reads
anything under the work directory.

```sh
src2deb export recipes/cosmic-epoch --work /mnt/build/work --to /srv/drop/rk1
```

## What it writes

```text
/srv/drop/rk1/trixie/
├── export.toml
├── manifests/
│   └── cosmic-epoch/
│       ├── amd64.toml
│       └── arm64.toml
├── cosmic-comp_1.0.0+deb13.20260802.abc1234_amd64.buildinfo
├── cosmic-comp_1.0.0+deb13.20260802.abc1234_amd64.changes
├── cosmic-comp_1.0.0+deb13.20260802.abc1234_amd64.deb
├── cosmic-comp_1.0.0+deb13.20260802.abc1234_arm64.deb
└── cosmic-icons_1.0.0+deb13.20260802.abc1234_all.deb
```

The suite is a directory of its own, so one destination holds several suites.
Inside it the packages are flat, so the whole suite is one argument:

```sh
aptly repo add cosmic-trixie /srv/drop/rk1/trixie
```

An archive tool that scans a directory takes the packages and passes over the
rest. Keep the directory to what src2deb wrote, though: aptly, for one, fails a
whole `repo add` over a single file named `*.deb` that it cannot parse, whoever
put it there.

Beside each package travel the `.changes` and `.buildinfo` its build wrote, and
a copy of each architecture's [provenance manifest](provenance.md). Together
they are the record of how the packages were built, in a place a publisher can
archive next to a release without reaching into a build host's work directory.

src2deb writes the export and stops there. It does not run an archive tool, sign
anything, or upload.

## What an export carries

Every component the work directory records as **built**, for every architecture
it holds a manifest for — not only what the last run produced. A
`--skip-published` run may build two components of twenty-six while the archive
still wants all twenty-six, and the manifest carries a built record forward for
exactly this reason.

To carry one architecture, name it:

```sh
src2deb export recipes/cosmic-epoch --to /srv/drop/rk1 --architecture arm64
```

A component the manifest calls built whose packages are no longer in the output
tree fails the export, naming the component. An archive quietly missing a
package it was told about is worse than an export that stops.

## `Architecture: all` packages

An arch-indep package's file name carries no architecture, and its stamped
version does not vary with one, so a recipe built for two architectures produces
one file name over two sets of bytes. A merged archive holds one of them, so an
export carries one of them:

```text
src2deb: cosmic-icons: Architecture: all package taken from arm64, not from amd64
src2deb: set arch-indep-owner in the recipe to build those once rather than once
per architecture
```

Which copy is carried follows the recipe. With an
[arch-indep owner](cross-architecture.md#who-builds-the-architecture-all-packages)
declared, the owner's copy is carried — and the other architecture never builds
it in the first place, so there is nothing to choose between. With none, the
later version is carried, and architecture name order breaks a tie, so an export
is a function of what the work directory holds rather than of the order it was
read in.

The `.changes` and `.buildinfo` of a build whose arch-indep output was dropped
still travel, and still name it: they record what that build produced, which is
what they are kept for. Declaring an owner removes the divergence at its source.

## Exporting again

An export replaces the one before it. `export.toml` names every file the export
wrote, so the next export into the same directory removes exactly those and
writes its own — a scheduled run stays idempotent, and a superseded version
never reaches the archive by being left behind.

Two rules make that safe to point at a shared drop directory:

- **An export removes only files an export of its own wrote.** Anything else in
  the directory is left alone.
- **The index is keyed by recipe.** Several recipes may export into one
  directory, and each replaces only its own files — which is the ordinary case
  for an archive publishing more than one recipe into a suite.

An export into a directory whose `export.toml` names a different suite is
refused, since that means the destination is not the one intended.

Keeping an export beyond the next one is a matter of choosing where it goes:
`--to /srv/drop/rk1-$(date +%F)` writes a fresh directory each time. It is worth
knowing what that buys before reaching for it — the pool upstream keeps as many
versions as [`--keep`](using-the-pool.md#pruning-the-pool) is told to, and an
archive tool downstream keeps snapshots, so the export itself is usually the one
copy not worth archiving.

## A scheduled build and publish

The whole cycle, as a build host runs it overnight:

```sh
#!/bin/sh
set -eu
src2deb build recipes/cosmic-epoch --work /mnt/build/work \
    --suite trixie --skip-published --keep 2
src2deb check recipes/cosmic-epoch --work /mnt/build/work --suite trixie
src2deb export recipes/cosmic-epoch --work /mnt/build/work \
    --suite trixie --to /srv/drop/rk1
```

`--skip-published` builds only what moved, `--keep 2` bounds the pool, the check
stops a publish whose packages would not install, and the export replaces the
drop directory's contents with the archive's current state. What the publisher
then does with `/srv/drop/rk1/trixie` — ingest, snapshot, sign, publish — is
outside src2deb.

The check earns its place under `set -eu`: a build validates that every
component *builds*, and a suite that drops a package overnight makes what built
yesterday uninstallable today without failing a single build. See
[Checking installability](installability.md).
