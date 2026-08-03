# The work directory

Everything a run reads and writes lives under one directory, named by `--work`
and defaulting to `./work`. It holds the run's outputs, and also the much larger
working state those outputs were made from.

```text
work/
├── out/                       artifacts, per suite and architecture
├── pool/                      the servable .deb archive, per suite and architecture
├── manifests/                 provenance, per recipe, suite, and architecture
├── sources/                   each component's resolved source tree
├── packaging/                 packaging overlays cloned from a repository
├── tarballs/                  fetched archives and .dsc files, named by digest
├── cache/                     downloaded .debs, shared by every build root
├── base/                      the shared base build root, per suite and architecture
├── uppers/                    per-component overlay layers, during a build
├── roots/                     per-component build roots, on hosts with no overlay
└── .lock                      held for the duration of a run
```

## What each entry is

| Entry | Holds | Safe to delete | Cost of deleting it |
| --- | --- | --- | --- |
| `out/` | Each component's `.deb`, `.changes`, and `.buildinfo` | Yes | The artifacts themselves. The same packages remain in `pool/` |
| `pool/` | The `dists/`-structured archive later builds resolve against | Yes | Every package built so far. A selective re-run can no longer resolve against them, and a signed pool loses its signature. To reclaim only what it no longer serves, [prune](using-the-pool.md#pruning-the-pool) it instead |
| `manifests/` | One TOML record per recipe, suite, and architecture | Yes | The run's provenance, and the state `--skip-published` reads. The next run rebuilds everything |
| `sources/` | One tree per component: a git checkout with submodules and LFS content, a copy of a `source.path` tree, an unpacked `source.tarball` archive, or a `source.dsc` source package assembled from the files its `.dsc` names | Yes | A full re-clone of every git component on the next run. A path, archive, or source-package component is re-copied, re-unpacked, or reassembled either way |
| `packaging/` | One tree per component that takes its `debian/` from a `packaging.git` repository, a `packaging.tarball` archive, or a `packaging.dsc` source package. A `packaging.path` overlay is read where it lies and appears here not at all | Yes | A re-clone or re-unpack of each on the next run |
| `tarballs/` | Fetched archives, each named by the SHA-256 it was verified against and shared by every component that declares it: release tarballs, and the `.dsc` files and component tarballs a `source.dsc` names | Yes | A re-fetch of every archive the next run resolves, which needs `curl` and the network |
| `cache/` | Downloaded `.deb` files, keyed by content and shared across roots | Yes | A re-download of every package the next provision installs |
| `base/` | One shared base build root per suite and architecture, at `base/<suite>/<arch>/`, each with a `.plan` recording the package set it was provisioned from and a `.lock` guarding its preparation | Yes, root and sidecars together | One base bootstrap — several hundred packages — per target on the next run |
| `uppers/` | A component's overlay layer, and its overlay work directory, while it builds, under `uppers/<suite>/<arch>/` | Between runs | Nothing. A layer is staged fresh for every build, and the next run clears whatever a killed one left |
| `roots/` | A fully-provisioned root per component, under `roots/<suite>/<arch>/` and each with its own `.plan` and `.lock`, on hosts with no unprivileged overlay | Yes | One full provision per component on the next run |
| `.lock` | The exclusive lock a run holds | Only when no run is active | Nothing, if no run is active. See [One run at a time](quick-start.md#one-run-at-a-time) |

Everything a run writes lands under the work directory, and src2deb creates
whatever of it does not exist yet.

## Where the size goes

The outputs are the small part. On a recipe of two trivial packages:

```text
672M  work/
533M  work/base
138M  work/cache
500K  work/sources
136K  work/pool
 52K  work/out
```

`base/` and `cache/` are the shared base system and the packages it was
installed from, and they are near enough constant per target: they are sized by
the suite, not by the recipe. What grows with the recipe is `sources/` — one tree
per component, including vendored Rust crates left in the tree between runs — and
`pool/`, which [grows until it is
pruned](how-a-build-runs.md#the-pool-directory-grows-until-it-is-pruned) as
every run adds a fresh set of `.deb` files and removes none.

Budget for the base and the cache once per suite and architecture the work
directory builds for, and watch `sources/` over time. For `pool/`, run
[`src2deb prune`](using-the-pool.md#pruning-the-pool), or pass `--keep N` to the
build that fills it.

## What a re-run reuses

A second run against the same work directory reuses, in order of what it saves:

- **The shared base for each target it builds**, when that base's plan key still
  matches — that is, when the exact set of packages a bootstrap would install has
  not moved in the archive. See
  [The build-root cache](build-roots.md#the-build-root-cache).
- **The package cache**, for every `.deb` it already holds, whatever root wants
  it.
- **The source checkouts**, and the packaging-overlay checkouts beside them,
  which are fetched and re-checked-out rather than re-cloned. A `source.path`
  component is the exception: its tree is copied afresh every run.
- **The pool**, which carries earlier components' packages forward so a
  selective run can resolve against them.
- **The manifest**, which is what `--skip-published` consults to decide a
  component's source has not moved since it was built.

Deleting any one of them costs only the work in the table above; none of them is
state a run cannot rebuild.

## Sharing one work directory

Several recipes may share a work directory, and so may one recipe retargeted at
another suite or architecture. `out/`, `pool/`, `manifests/`, `base/`, `roots/`,
and `uppers/` are keyed by the suite and architecture of the run that wrote them,
so no two targets overwrite each other, while `sources/` and `cache/` are shared
outright.

Keying the build roots is what makes a work directory hold a warm base for each
target rather than one that whichever run went last had rebuilt. A root's plan
key names the suite and the architecture, so a base bootstrapped for `trixie`/
`amd64` could never be reused for `forky`/`arm64` in any case — sharing the path
would only have meant each target discarding the other's bootstrap. The cost is
disk: one base per target rather than one in total.

`cache/` is shared safely because it is content-addressed: two architectures'
`.deb` files are different content and sit beside each other, so every target
pays for a package once.

One thing is not keyed: `sources/` holds one tree per component name, and
`packaging/` does the same. Two recipes that name the same component for
different sources would fight over one directory. Give them separate work
directories, or separate component names.
