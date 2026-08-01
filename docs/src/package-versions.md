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
  source a package was built from legible from `apt policy` alone.

Compare two versions with `dpkg --compare-versions` to confirm any particular
pair orders the way you expect.

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

  * Automated build from source revision abc1234def5678.

 -- Pop Packaging <pop@example.invalid>  Fri, 31 Jul 2026 00:00:00 +0000
```

The entry lands on the build's own copy of the source tree, inside the cage —
not on the resolved checkout in the work directory. The checkout keeps
upstream's changelog, so each rebuild starts from the same base version rather
than compounding suffixes onto the last build's.
