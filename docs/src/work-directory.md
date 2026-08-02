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
├── cache/                     downloaded .debs, shared by every build root
├── base/, base.plan, base.lock   the shared base build root
├── uppers/                    per-component overlay layers, during a build
├── roots/                     per-component build roots, on hosts with no overlay
└── .lock                      held for the duration of a run
```

## What each entry is

| Entry | Holds | Safe to delete | Cost of deleting it |
| --- | --- | --- | --- |
| `out/` | Each component's `.deb`, `.changes`, and `.buildinfo` | Yes | The artifacts themselves. The same packages remain in `pool/` |
| `pool/` | The `dists/`-structured archive later builds resolve against | Yes | Every package built so far. A selective re-run can no longer resolve against them, and a signed pool loses its signature |
| `manifests/` | One TOML record per recipe, suite, and architecture | Yes | The run's provenance, and the state `--skip-published` reads. The next run rebuilds everything |
| `sources/` | One tree per component: a git checkout with submodules and LFS content, or a copy of a `source.path` tree | Yes | A full re-clone of every git component on the next run. A path component is re-copied either way |
| `cache/` | Downloaded `.deb` files, keyed by content and shared across roots | Yes | A re-download of every package the next provision installs |
| `base/` | The shared base build root, with `base.plan` recording the package set it was provisioned from and `base.lock` guarding its preparation | Yes, all three together | One base bootstrap — several hundred packages — on the next run |
| `uppers/` | A component's overlay layer, and its overlay work directory, while it builds | Between runs | Nothing. A layer is staged fresh for every build, and the next run clears whatever a killed one left |
| `roots/` | A fully-provisioned root per component, each with its own `.plan` and `.lock`, on hosts with no unprivileged overlay | Yes | One full provision per component on the next run |
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
installed from, and they are near enough constant: they are sized by the suite,
not by the recipe. What grows with the recipe is `sources/` — one tree per
component, including vendored Rust crates left in the tree between runs — and
`pool/`, which [grows without
bound](how-a-build-runs.md#the-pool-directory-grows-without-bound) as every run
adds a fresh set of `.deb` files and removes none.

Budget for the base and the cache once per work directory, and watch `sources/`
and `pool/` over time.

## What a re-run reuses

A second run against the same work directory reuses, in order of what it saves:

- **The shared base**, when its plan key still matches — that is, when the exact
  set of packages a bootstrap would install has not moved in the archive. See
  [The build-root cache](build-roots.md#the-build-root-cache).
- **The package cache**, for every `.deb` it already holds, whatever root wants
  it.
- **The source checkouts**, which are fetched and re-checked-out rather than
  re-cloned. A `source.path` component is the exception: its tree is copied
  afresh every run.
- **The pool**, which carries earlier components' packages forward so a
  selective run can resolve against them.
- **The manifest**, which is what `--skip-published` consults to decide a
  component's source has not moved since it was built.

Deleting any one of them costs only the work in the table above; none of them is
state a run cannot rebuild.

## Sharing one work directory

Several recipes may share a work directory, and so may one recipe retargeted at
another suite or architecture. `out/`, `pool/`, and `manifests/` are keyed by
the identity of the run that wrote them, so no two runs overwrite each other,
while `sources/`, `cache/`, and `base/` are shared — which is the point, since
the base and the cache are the expensive parts.

One thing is not keyed: `sources/` holds one tree per component name. Two
recipes that name the same component for different sources would fight over one
directory. Give them separate work directories, or separate component names.
