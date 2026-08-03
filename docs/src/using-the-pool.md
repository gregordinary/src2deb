# Using the pool

A finished run leaves a complete Debian archive at
`<work>/pool/<suite>/<architecture>/`. It is a `dists/`-structured pool with a
`Release` and a `Packages` index, which is exactly what apt reads — so serving it
is a matter of putting it somewhere a client can reach and pointing the client at
it.

This chapter covers getting from that directory to `apt install`. It takes no
view on where a pool should be hosted.

## What the pool is

```text
pool/trixie/amd64/
├── dists/
│   └── trixie/
│       ├── Release
│       └── main/
│           └── binary-amd64/
│               ├── Packages
│               ├── Packages.gz
│               └── by-hash/
└── pool/
    └── main/
        └── c/cosmic-comp/cosmic-comp_1.0.0~alpha.7-1+deb13.20260731.abc1234_amd64.deb
```

One pool serves one suite and one architecture. A run building for several
architectures fills several pools, each complete on its own; see
[The local pool](how-a-build-runs.md#the-local-pool) for why they are kept
apart.

The pool is **unsigned**. src2deb writes the `Release` but signs nothing, which
matters for every client below.

## What the `Release` declares

```text
Origin: texor.io
Label: COSMIC for Debian
Suite: trixie
Codename: trixie
Architectures: amd64
Components: main
Description: COSMIC desktop packages for Debian
Date: Fri, 31 Jul 2026 00:00:00 UTC
```

The **date** is the run's build date, not the moment of the publish. That is what
makes a run pinned with [`--build-date`](package-versions.md#pinning-the-date)
produce a byte-identical pool every time: a publish clock would leave the one
file the pin cannot reach differing between two runs of the same build.

`Origin`, `Label`, and `Description` come from the recipe and are written only
when it names them:

```toml
origin = "texor.io"
label = "COSMIC for Debian"
description = "COSMIC desktop packages for Debian"
```

They have no defaults. An origin names the organization behind an archive, and
src2deb has none to offer on your behalf. A pool that declares none is still a
valid archive; it is pinnable only by its URL.

Recipes built into one pool should declare one identity. A pool has a single
`Release`, so the last recipe to publish writes the one every client reads.

## Serving it

Over HTTP, from the pool directory:

```sh
cd work/pool/trixie/amd64
python3 -m http.server 8000
```

Any static file server will do — the archive is plain files, and apt asks for
them by path. A directory served over HTTP, an object store, or a CDN in front
of either all work the same way.

For a client on the same machine, or one that mounts the directory, no server is
needed at all: apt reads `file://` URLs directly.

## Pointing a client at it

Add a source naming the pool's URL, the suite, and the `main` component. Because
the pool is unsigned, the client has to be told to trust it:

```sh
# /etc/apt/sources.list.d/src2deb.sources
Types: deb
URIs: http://build-host.example:8000
Suites: trixie
Components: main
Trusted: yes
```

The one-line form, for a `sources.list` entry:

```text
deb [trusted=yes] http://build-host.example:8000 trixie main
```

And for a pool on the same machine:

```text
deb [trusted=yes] file:///srv/pool/trixie/amd64 trixie main
```

Then:

```sh
sudo apt update
sudo apt install cosmic-desktop
```

### Pinning against the pool

A recipe that named an origin and a label can be pinned on them, which is the
form apt's own documentation leads with:

```text
# /etc/apt/preferences.d/cosmic
Package: *
Pin: release o=texor.io,l=COSMIC for Debian
Pin-Priority: 1001
```

A priority above 1000 installs the pool's package even where that means
downgrading one the archive also ships — which is what a
[backport](package-versions.md#rebuilds-of-packages-the-archive-also-ships)
stamp otherwise arranges by version alone.

`apt policy` shows what a client resolved:

```text
500 http://build-host.example:8000 trixie/main amd64 Packages
    release o=texor.io,l=COSMIC for Debian,c=main,b=amd64
```

A pool that declared no identity renders that line with the fields blank, and
can be pinned only by its URL: `Pin: origin build-host.example`.

### What `Trusted: yes` gives up

`Trusted: yes` tells apt to install from the archive without verifying a
signature over its `Release`. Nothing then attests that the packages a client
receives are the packages the build produced: anything that can answer the
archive's URL, or modify the files behind it, can substitute a package, and apt
will install it.

That is acceptable for a pool on the machine that built it, or one served over a
network you control to hosts you control. It is not acceptable for a pool served
to anyone else. Sign it instead.

## Signing a pool

A signed archive carries an `InRelease` — the `Release` document with an inline
OpenPGP signature over it — beside the `Release` in `dists/<suite>/`. A client
given the corresponding public key verifies it and needs no `Trusted: yes`.

Two things about a src2deb pool decide when signing happens:

- **A publish replaces the `Release`.** A signature covers the exact document it
  was made over, so publishing invalidates it. Sign after the run that finished
  the pool, and re-sign after every run that publishes into it.
- **A run with nothing to build does not touch the pool.** A fully-skipped
  `--skip-published` re-run therefore leaves a signed pool signed.

See [Signing follows a run, never precedes
it](how-a-build-runs.md#signing-follows-a-run-never-precedes-it).

## Pruning the pool

A pool's index names **one version of each package**. Publishing merges each
run's `.debs` into the index by highest version, so a package superseded by a
later build stops being named the moment that build publishes — but the file
stays where it was written. Every build carries the build date in its
[version](package-versions.md), so a recipe built nightly writes a fresh set of
`.deb` files each night and leaves the previous set on disk, reachable by
nothing.

`src2deb prune` removes them:

```sh
src2deb prune recipes/cosmic-epoch --work /mnt/build/work
```

By default it keeps one version of each binary package — the version the index
names — so the pool on disk ends up exactly matching the archive it serves.
`--keep N` leaves the newest `N` instead, which is worth doing only to have a
superseded `.deb` to hand to someone or to roll back to by hand: apt is never
offered it, because the index names one.

`--dry-run` reports what would go without removing it:

```text
src2deb: arm64: would remove 78 file(s), 3.1 GiB, of 26 package(s)
src2deb: 1 pool(s) pruned: would remove 78 file(s), 3.1 GiB
```

Two guarantees hold whatever is pruned:

- **A file the index names is never removed.** The index is the pool's contract
  with every client resolving against it.
- **No index is rewritten.** Nothing indexed is removed, so the `Release`, the
  `Packages`, and any signature written over them still describe the pool
  exactly.

A build prunes its own pool when told to, once the run has finished:

```sh
src2deb build recipes/cosmic-epoch --skip-published --keep 2
```

After the run rather than as each component publishes, because a superseded file
may still be being fetched by a build root provisioning against the pool. For
the same reason, prune when no build is running: a client that read an earlier
`Release` may still be fetching a file the prune removes. A pool is pruned
across every recipe that publishes into it, since a pool for a suite and
architecture is one archive whoever built it.

## Before serving a pool to anything but the next build

Three things matter more once something other than src2deb is reading the pool:

- **Its packages may not install.** A component builds without anything
  guaranteeing that its runtime dependencies exist in the suite it was built
  for. `src2deb check` resolves them and reports what nothing satisfies; see
  [Checking installability](installability.md).
- [Publishing is incremental and
  forward-only](how-a-build-runs.md#publishing-is-incremental-and-forward-only):
  a lower version does not publish, and there is no unpublish.
- [Package versions](package-versions.md): every build carries the suite, the
  build date, and the source revision, which is what makes a rebuild reach a
  machine that already installed the last one.

## Handing the packages to an archive

Serving the pool directly is one destination for a run's output; ingesting it
into a managed archive is the other. See
[Publishing to an archive](publishing.md).
