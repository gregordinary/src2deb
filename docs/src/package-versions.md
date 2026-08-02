# Package versions

Every package src2deb builds carries a stamped version: the version the
component's own `debian/changelog` declares, extended with the suite it was
built for, the build date, and the source revision.

```text
1.0.0~alpha.7-1+deb13.20260731.abc1234
└──────┬──────┘ └─┬─┘ └──┬───┘ └──┬──┘
   upstream      suite   date   revision
```

## Why builds are stamped

A component's version comes from upstream's `debian/changelog`, which does not
move when only the toolchain or the packaging around it does. Two builds of the
same pinned revision would otherwise produce the same version, and apt would
never offer the second as an upgrade over the first — so a rebuild carrying a
fixed compiler or a patched vendored dependency would never reach anyone who
already installed the first.

The stamp makes each build distinct and ordered, which is what an archive needs
in order to serve upgrades at all.

## How the parts order

The format is chosen for how `dpkg` compares versions.

- **`+` opens the suffix.** An empty string sorts before any character except
  `~`, so a stamped build sorts *after* the plain upstream version it was built
  from.
- **The suite appears as `deb13`, not `trixie`.** Spelled out, `forky` sorts
  before `trixie`, so a user moving from trixie to forky would see the forky
  packages as a downgrade and apt would refuse the upgrade. Release numbers sort
  the way the releases do.
- **The date is `YYYYMMDD`.** Digit runs compare numerically, so each build
  sorts after the one before it.
- **The revision is the first seven characters of the commit**, which makes the
  source a package was built from legible from `apt policy` alone. A component
  built from more than one input carries each of them, joined with `.`.

Compare two versions with `dpkg --compare-versions` to confirm any particular
pair orders the way you expect.

### More than one input

A component built from more than one input carries an abbreviation of each, in
the order they were applied. A component with [patches](recipes.md#patches) is
the ordinary case: the upstream revision, then the series applied over it.

```text
1.0.0~alpha.7-1+deb13.20260731.abc1234.5f2e1a9
                                └──┬──┘ └──┬──┘
                              revision   patches
```

A patched package and an unpatched one built from the same revision on the same
day are therefore distinct versions, and ordered — so a fix reaches a machine
that already installed the build without it.

A [packaging overlay](recipes.md#packaging-overlays) sits between the two, so a
component taking its `debian/` from a second repository and carrying a local fix
besides reads:

```text
1.0.0~alpha.7-1+deb13.20260731.abc1234.def5678.5f2e1a9
                                └──┬──┘ └──┬──┘ └──┬──┘
                              revision  packaging patches
```

The source's revision is always first, which is what makes the leading
abbreviation mean the same thing on every package src2deb builds.

A component built from a [release archive](recipes.md#building-from-a-release-archive)
carries the first seven characters of the digest it was verified against, in the
place a commit would sit. It abbreviates the same way for the same reason: it
pins exactly what the build consumed.

### A local build says so

A component built from a `source.path` tree has no revision to abbreviate — a
path says where a tree was read from, not what it held — so it carries `local`
where a git build carries a commit:

```text
1.0.0~alpha.7-1+deb13.20260731.local
```

That is deliberate. A package built from someone's working tree is not a package
anyone can reproduce, and the version says so in the one place everybody looks.
`apt policy` shows it without the manifest being consulted, and a local build and
a published one are never mistaken for each other. See [Building from a tree on
disk](sources-and-toolchain.md#building-from-a-tree-on-disk).

One consequence is worth knowing before it surprises you: successive local builds
are distinguished only by the date, so two builds of a changed tree on the same
day stamp the same version. `apt upgrade` sees nothing to do. Install the new
`.deb` directly with `dpkg -i`, which reinstalls a matching version, or pass a
distinct `--build-date` to separate the two.

### Packaging that ships no changelog

Not every component has an upstream version to extend. Packaging assembled from a
[packaging overlay](recipes.md#packaging-overlays) is often a `control` and a
`rules` and nothing else — no release history, and so no version for the stamp to
build on.

Such a component declares its version in the recipe, and src2deb writes the
`debian/changelog` the packaging lacks:

```toml
[[components]]
name = "foo"
source.git = "https://github.com/example/foo"
packaging.path = "packaging/foo"
version = "1.2.3"
```

```text
foo (1.2.3) UNRELEASED; urgency=medium

  * Version declared by the build recipe; this source carries no changelog of its own.

 -- Your Name <you@example.org>  Fri, 31 Jul 2026 00:00:00 +0000
```

That entry is a base and not a build record. The stamping path above extends it
exactly as it extends an upstream changelog, so the package is versioned by one
code path however its version was arrived at:

```text
1.2.3+deb13.20260731.abc1234.def5678
```

`version-from = "git-describe"` derives the version from the source's own tags
rather than stating it. See [Components with no
changelog](recipes.md#components-with-no-changelog) for both, and for where the
maintainer identity comes from.

The declared version is compared by `--skip-published` alongside the source
fingerprint, so editing it rebuilds the component — which it has to, since every
tree the component resolves is unchanged and only the version moved.

## The date is the build date

The date is when the build ran, not when the source was committed. A rebuild of
unchanged pinned sources therefore still supersedes its predecessor, which is
what lets a rebuild ship a fixed toolchain to users who already installed the
previous one.

The cost is that a rebuild which changes nothing still looks like an upgrade. How
often the stamp moves is decided by how often a build runs.

Every component in a run shares one date, taken once when the run starts and in
UTC, so a run that spans midnight or builds components in parallel still produces
one coherent set.

## Pinning the date

`--build-date` fixes the date instead of taking today's:

```sh
src2deb build recipes/cosmic-epoch --build-date 2026-07-31
```

The date settles more than how the packages are versioned. src2deb writes it into
the changelog entry it prepends, and `dpkg-buildpackage` derives
`SOURCE_DATE_EPOCH` from that entry — so the build itself sees the same clock,
which is what timestamps embedded in the packages are made from. Two runs from
the same pinned sources with the same `--build-date` therefore produce not just
the same version but the same build conditions.

That is what makes a build reproducible enough to check. Without it, every run
carries a different date, so no two runs can ever be compared.

`--build-date manifest` takes the date the prior run recorded, which reproduces
that build without transcribing anything:

```sh
src2deb build recipes/cosmic-epoch --build-date manifest
```

The run says which date it settled on before it starts:

```text
src2deb: stamping every version with build date 2026-07-31
```

A work directory that holds no build of this recipe for this suite and
architecture records no date, and the run is refused rather than quietly falling
back to today — which would produce a build that looks like a reproduction and is
not.

The default stays today's date. A moving date is what lets a rebuild reach a
machine that already installed the build before it, and that is what an ordinary
build wants; pinning is for verifying a build already made.

### Verifying a build

The manifest records everything a rebuild needs: the source each component
resolved to, the date the run was stamped with, and the `.buildinfo` each build
produced. See [The provenance manifest](provenance.md).

1. Build normally. The manifest records the run.
2. Rebuild from the same recipe with `--build-date manifest`, into a work
   directory of its own so nothing of the first run is reused.
3. Compare. The stamped versions match exactly when the sources have not moved;
   the two `.buildinfo` files name what each build ran against, and differ where
   the archive moved beneath them.

Pin the recipe's `git-ref` values to commits for step 3 to mean anything — a
branch ref resolves to whatever upstream has since pushed, and the manifest's
recorded source will say so.

src2deb provides the inputs for this comparison and does not perform it. Compare
the artefacts with whatever tool you prefer.

## The version tag

The `deb13` part is the *version tag*. src2deb knows the tag for each numbered
Debian release, and takes it from the recipe's suite:

| Suite | Tag |
| --- | --- |
| `bookworm` | `deb12` |
| `trixie` | `deb13` |
| `forky` | `deb14` |
| `duke` | `deb15` |

A qualified suite takes the tag of the release it qualifies, so
`trixie-backports` tags as `deb13`.

A recipe targeting a suite outside that set names its own tag:

```toml
suite = "sid"
version-tag = "debsid"
```

src2deb does not guess one. A rolling suite carries no release number, and a tag
that does not order the way the releases do is the trap the tag exists to avoid,
so a recipe naming an unknown suite is refused until it declares a tag.

### The tag follows the suite

A recipe's `version-tag` names the tag for the suite that recipe declares, and
only that one. When `--suite` retargets a run, the tag goes with it: the recipe's
own is set aside, and the new suite's tag is derived. A recipe declaring
`suite = "sid"` and `version-tag = "debsid"`, built with `--suite trixie`, stamps
`+deb13`.

Anything else would defeat the ordering the tag exists to give. A package built
against trixie but stamped `debsid` claims a suite it was not built for, and two
suites' builds tagged alike do not order against each other at all — which is
the failure the tag is there to prevent.

`--version-tag` names one directly, overriding both the recipe's and the derived
tag. It is what makes a suite src2deb has no tag for buildable without editing
the recipe:

```sh
src2deb build recipes/cosmic-epoch --suite sid --version-tag debsid
```

A `--suite` src2deb does not know, with no `--version-tag` alongside it, is a
usage error rather than a silent fallback.

A tag may contain only the characters a Debian revision allows — alphanumerics,
`+`, `.`, and `~`. The sharp one this excludes is `-`: a version's Debian
revision begins at its *last* hyphen, so a tag carrying one moves that boundary
and splits the version somewhere other than where it reads as splitting.
`1.0.0-1` tagged `deb-13` yields upstream `1.0.0-1+deb` with revision
`13.20260731.abc1234`, which still compares, just not as anything intended.

## Where the stamp is applied

src2deb prepends a `debian/changelog` entry declaring the stamped version, so
`dpkg-buildpackage` builds it the way it builds any other version. The entry
reuses the maintainer identity from the changelog it sits above, and records the
source revision in its text:

```text
cosmic-comp (1.0.0~alpha.7-1+deb13.20260731.abc1234) trixie; urgency=medium

  * Automated build from source abc1234def5678.

 -- Pop Packaging <pop@example.invalid>  Fri, 31 Jul 2026 00:00:00 +0000
```

Each input names the part it played, so a component assembled from more than one
reads without any of them having to be guessed at:

```text
  * Automated build from source abc1234def5678, packaging def5678abc1234,
    patches 5f2e1a9c3b8d.
```

A `source.path` tree appears as `local` rather than as the path it was read
from: this text ships inside the `.deb`, and a build host's directory layout is
not something a package should carry. The manifest, which stays in the work
directory, records the path. A packaging overlay taken from a path carries a
digest of what it held rather than a path in the first place, so it reads as
`packaging tree:483b0e8...` and names its kind so a digest is not taken for a
commit.

The entry lands on the build's own copy of the source tree, inside the cage —
not on the resolved checkout in the work directory. The checkout keeps
upstream's changelog, so each rebuild starts from the same base version rather
than compounding suffixes onto the last build's.

A component that [declares its version](#packaging-that-ships-no-changelog) is
the one exception to that last sentence, in form rather than in substance: the
base entry it extends is one src2deb wrote into the resolved tree, and each run
rewrites it from the recipe. The base version still comes from one place and
still does not compound.
