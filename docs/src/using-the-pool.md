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

One pool serves one suite and one architecture. Building for several targets
means several pools, each complete on its own; see
[The local pool](how-a-build-runs.md#the-local-pool) for why they are kept
apart.

The pool is **unsigned**. src2deb writes the `Release` but signs nothing, which
matters for every client below.

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

## Before serving a pool to anything but the next build

Three properties of the pool matter more once something other than src2deb is
reading it, and each is covered in [How a build runs](how-a-build-runs.md):

- [Publishing is incremental and
  forward-only](how-a-build-runs.md#publishing-is-incremental-and-forward-only):
  a lower version does not publish, and there is no unpublish.
- [The pool directory grows without
  bound](how-a-build-runs.md#the-pool-directory-grows-without-bound): pruning
  superseded `.deb` files is an operational task, not something a run does.
- [Package versions](package-versions.md): every build carries the suite, the
  build date, and the source revision, which is what makes a rebuild reach a
  machine that already installed the last one.
