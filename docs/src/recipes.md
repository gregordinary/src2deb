# Recipe reference

A recipe is a `recipe.toml` file in a recipe directory. It names a Debian suite,
selects a toolchain, lists any additional archive repositories, and lists the
components to build.

[Sources and the toolchain](sources-and-toolchain.md) explains the model these
fields describe; this chapter lists the fields themselves.

## Example

```toml
name = "cosmic-epoch"
suite = "trixie"

[toolchain.rust]
provider = "rustup"
version = "1.95.0"

[[components]]
name = "cosmic-comp"
source.git = "https://github.com/pop-os/cosmic-comp"
source.git-ref = "master"

[[components]]
name = "cosmic-settings"
source.git = "https://github.com/pop-os/cosmic-settings"
```

## Top-level fields

- `name` — the recipe name. Required.
- `suite` — the Debian suite to build for, such as `trixie` or `forky`.
  Required. It is the recipe's default rather than a binding: `--suite` builds the
  same recipe against another suite without editing the file, and each suite gets
  its own pool, output tree, and manifest. Name the suite the recipe was written
  and tested against.
- `architecture` — the target architecture, a Debian name such as `amd64` or
  `arm64`. Optional: a recipe that omits it builds for whichever host runs it,
  and `--architecture` selects any other target without editing the file, which
  is what keeps one recipe serving every target. Name one when the recipe is
  meaningful for a single architecture. See
  [Cross-architecture builds](cross-architecture.md).
- `arch-indep-owner` — the architecture that produces this recipe's
  `Architecture: all` packages, such as `amd64`. Optional: unset, every run
  produces its own, so a single pool holds every package the recipe declares and
  can be served as it stands. Name one when several architectures feed a single
  published archive, where one name and version must mean one file. Every other
  architecture then builds only its architecture-dependent packages, and a
  component whose every binary package is `Architecture: all` is skipped for
  them. See [Who builds the `Architecture: all`
  packages](cross-architecture.md#who-builds-the-architecture-all-packages).
- `version-tag` — the tag every built version carries, identifying the suite it
  was built for, such as `deb13`. Optional: src2deb derives it from the suite for
  the numbered Debian releases. Name one when the recipe targets a suite outside
  that set, such as a rolling suite or a derivative.

  It names the tag for *this recipe's* `suite`, and only that one. A `--suite`
  override supersedes it along with the suite it described, and the new suite
  derives its own tag or takes one from `--version-tag`. See
  [Package versions](package-versions.md).

  ```toml
  suite = "sid"
  version-tag = "debsid"
  ```
- `maintainer` — the identity src2deb signs a synthesized `debian/changelog`
  with, written as Debian writes it. Optional, and consulted only for a component
  that [declares its version](#components-with-no-changelog); a component's own
  `maintainer` overrides it, and a component with neither takes the `Maintainer`
  its `debian/control` declares.

  ```toml
  maintainer = "Your Name <you@example.org>"
  ```
- `mirror` — the primary archive mirror URL. Defaults to the Debian CDN. Name one
  to build against a local or regional mirror rather than `deb.debian.org`.

  ```toml
  mirror = "http://ftp.uk.debian.org/debian"
  ```

## Toolchain

The `[toolchain.rust]` table selects where the Rust compiler and Cargo come
from. It is optional; the default is the archive's own Rust.

- `provider` — `debian` (the default) or `rustup`.
  - `debian` resolves `rustc` and `cargo` from the archive as ordinary
    build-dependencies. The build is only as new as the suite's Rust.
  - `rustup` installs a pinned toolchain with `rustup` into the build root and
    prefers it on `PATH`, while the archive's `rustc` and `cargo` stay installed
    to satisfy the declared build-dependencies. This decouples the compiler from
    the suite's Rust.
- `version` — the exact toolchain version, such as `1.95.0`. Required when
  `provider = "rustup"`.

## Additional repositories

Each `[[repositories]]` entry adds an archive to resolve build-dependencies
from, beyond the primary suite and the feed-forward pool.

- `name` — a short identifier, unique within the recipe.
- `suite` — the suite to resolve from. Defaults to the recipe's primary suite,
  and follows a `--suite` override with it.

  A `suite` named here does not follow the override, because it names a specific
  archive rather than a variation on the primary one: `trixie-backports` has no
  automatic counterpart under `--suite forky`, and guessing one would resolve
  build-dependencies from the wrong release. A recipe that declares a suite here
  is a recipe for one target — leave it out to keep the recipe portable, or give
  each target its own recipe.
- `mirror` — the archive mirror URL. Defaults to the recipe's primary mirror.
- `components` — the archive components to enable. Defaults to `["main"]`.
- `trust-unsigned` — trust the repository without verifying a signature, for a
  local or `file://` archive under your control. Defaults to `false`.
- `keyring` — the path to the binary OpenPGP keyring the repository's release is
  verified against. Required for a signed repository; omitted for a
  `trust-unsigned` one.

A signed repository must name a `keyring`: the provisioner has no embedded trust
anchor for an archive other than the primary Debian one.

### Worked examples

A backports suite on the primary mirror, verified against Debian's own archive
keyring — the file `debian-archive-keyring` installs, which most Debian hosts
already have:

```toml
[[repositories]]
name = "backports"
suite = "trixie-backports"
keyring = "/usr/share/keyrings/debian-archive-keyring.gpg"
```

A `keyring` is a *binary* OpenPGP keyring: the format `gpg --export` writes, and
the format the files under `/usr/share/keyrings/` are in. An ASCII-armoured key
(`.asc`) is not one; convert it with `gpg --dearmor`. The path is read on the
host, and only the keys it holds are used to verify that repository's release.

A pool another src2deb run produced, read over `file://` and trusted without a
signature — which is what `trust-unsigned` is for, since src2deb pools are
unsigned:

```toml
[[repositories]]
name = "prior-pool"
mirror = "file:///srv/build/work/pool/trixie/amd64"
trust-unsigned = true
```

Only trust an archive unsigned when you control both the archive and the path to
it. See [Using the pool](using-the-pool.md).

## Components

Each `[[components]]` entry is one buildable component: a source tree with a
`debian/` directory.

- `name` — the component name, unique within the recipe.
- `source` — where the component's source comes from. A component names exactly
  one origin: `source.git`, `source.path`, or `source.tarball`. Naming more than
  one, or none, is refused.
  - `source.git` — the git repository URL to clone.
  - `source.git-ref` — the branch, tag, or commit to check out. Defaults to the
    remote's default branch. It qualifies `source.git`, and setting it on a path
    source is refused rather than ignored.
  - `source.path` — a tree already on disk, built without being cloned. Relative
    to the recipe's own directory, so a recipe kept beside the trees it builds
    names them relatively and moves with them; an absolute path is used as it
    stands.

    ```toml
    [[components]]
    name = "cosmic-comp"
    source.path = "../../checkouts/cosmic-comp"
    ```

    The tree is copied into the work directory and built from the copy, so a
    build never writes into it. Packages built from a path source are marked as
    such in their version and in the manifest, and `--skip-published` never
    skips one. See [Building from a tree on
    disk](sources-and-toolchain.md#building-from-a-tree-on-disk).
  - `source.tarball` — a release archive to fetch and unpack, over `https`,
    `http`, or `file`.

    ```toml
    [[components]]
    name = "foo"
    source.tarball = "https://example.org/releases/foo-1.2.3.tar.xz"
    source.sha256 = "5f2e1a9c3b8d..."
    ```

    See [Building from a release archive](#building-from-a-release-archive)
    below.
  - `source.sha256` — the SHA-256 the archive must hash to, in hexadecimal of
    either case. Required for `source.tarball`, and setting it on any other
    origin is refused rather than ignored.
  - `source.subdir` — a subdirectory within the source that holds the `debian/`
    tree, for a component that lives inside a larger superproject. The whole
    source is the tree when unset. It applies to every origin, and must stay
    inside the source: a `..` component or an absolute path is refused, since the
    tree it names is what the vendor pass binds into a cage that runs upstream's
    own `debian/rules clean`.
- `packaging` — where the component's `debian/` directory comes from, for a
  source that carries none of its own. Optional; see [Packaging
  overlays](#packaging-overlays) below. It takes the same settings `source`
  does — `packaging.git`, `packaging.git-ref`, `packaging.path`,
  `packaging.tarball`, `packaging.sha256`, `packaging.subdir` — under the same
  rules.
- `patches` — patch files applied over the resolved source tree, in the order
  given. Optional; see [Patches](#patches) below.
- `version` — the upstream version to build the component as, for packaging that
  carries no `debian/changelog`. Optional; see [Components with no
  changelog](#components-with-no-changelog) below. Exclusive with
  `version-from`.
- `version-from` — where to derive that version instead of stating it. The one
  value is `git-describe`. Exclusive with `version`.
- `maintainer` — the identity a synthesized changelog is signed with, overriding
  the recipe's. Optional.
- `extra-build-deps` — extra build-dependency package names beyond those
  `debian/control` declares. Rarely needed; most build-dependencies are
  discovered from the control file. Reach for it when a component's build needs
  something its packaging does not declare — often a tool the vendor pass runs
  before `dpkg-buildpackage` sees the tree at all.

  ```toml
  [[components]]
  name = "cosmic-comp"
  source.git = "https://github.com/pop-os/cosmic-comp"
  extra-build-deps = ["just"]
  ```

  These are installed into the build root but create no edge in the build order,
  which is derived from `debian/control` alone. Naming a package another
  component produces will not order that component first — declare it in
  `debian/control` if it needs to be.

src2deb computes the build order from the components' declared dependencies, so
they may be listed in any order.

## Packaging overlays

Not every upstream ships a `debian/` directory. For many that do not, someone
else's packaging exists — a distribution's packaging repository, or one of your
own. Point a component at it:

```toml
[[components]]
name = "foo"
source.git = "https://github.com/example/foo"

packaging.git = "https://salsa.debian.org/debian/foo"
packaging.git-ref = "debian/latest"
```

`packaging` takes the same settings `source` does, resolved the same way: a git
repository is cloned and checked out, a path is a tree already on disk, a
`git-ref` selects the revision, and a `subdir` names the directory within it
that holds `debian/`.

### What is taken, and what is not

The overlay's `debian/` directory becomes the component's. Nothing else is
taken.

That boundary matters, because a distribution's packaging repository usually
carries a copy of the upstream tree beside its packaging — and that copy is not
the source you are building, it is whichever release was last packaged. Taking
it would silently replace your source with an older one. Only `debian/` crosses
over, so a repository of either shape works: one holding packaging alone, and
one holding packaging beside a tree it happens not to be used for.

In the other direction, the overlay **replaces** any `debian/` the source ships
rather than merging with it. There is no per-file precedence to reason about:
the packaging that reaches the build is the packaging you declared, with nothing
of an abandoned one left beside it — no stale `install` file naming a path the
new packaging never builds, no `patches/series` applied by a build that was
never asked to. The source's own `debian/` is set aside for the build, not lost;
drop the `packaging` setting and the next run has it back.

### Both trees are recorded

A component with an overlay has two inputs, and both count:

- The version carries both, source first:
  `1.0.0-1+deb13.20260731.abc1234.def5678`.
- The manifest records two `[[component.source]]` entries, each naming the part
  it played — `role = "source"` and `role = "packaging"`. See [The provenance
  manifest](provenance.md).
- `--skip-published` rebuilds the component when *either* moves. New packaging
  against an unchanged source produces a new package, which is what you want the
  moment you fix a `debian/rules`.

`SOURCE_GIT_HASH`, which packaging reads to stamp a revision into what it
builds, is the *source's* commit and never the packaging repository's.

### Packaging kept beside the recipe

The other home for packaging is the recipe itself. Nothing has to be published
or maintained in a second repository, and the packaging is versioned with the
recipe that names it — which is what you want for a one-off, or for a component
whose upstream will never carry a `debian/` of its own.

```toml
[[components]]
name = "foo"
source.git = "https://github.com/example/foo"
packaging.path = "packaging/foo"
```

`packaging.path` is relative to the recipe's own directory, as `source.path` and
`patches` are, so the recipe directory holds:

```text
recipes/mine/
├── recipe.toml
└── packaging/
    └── foo/
        └── debian/
            ├── control
            ├── rules
            └── ...
```

One directory per component under `packaging/`, each holding the `debian/` tree
to overlay. Nothing enforces that layout — `packaging.path` names any directory
you like — but a recipe that follows it reads at a glance, and a component's
packaging sits where someone looking for it will look.

Nothing is ever written to a packaging source, so a path one is read where it
lies rather than copied into the work directory.

### What a path overlay is recorded as

An overlay from a repository is identified by the revision it was checked out
at. One from a path is identified by a **digest over the `debian/` tree it
supplied**:

```toml
  [[component.source]]
  role = "packaging"
  kind = "tree"
  value = "483b0e8..."
  pinned = true
```

The digest covers exactly what src2deb copied — the `debian/` directory and
nothing beside it — so a `README` next to your packaging never provokes a
rebuild, and editing `debian/rules` always does. It is over what the directory
*holds* rather than where it is, so moving the packaging or renaming the recipe
directory does not republish every component built from it.

That makes local packaging a pinned input, and a component overlaid from one is
skippable by `--skip-published` exactly as one overlaid from a repository is.
Note the contrast with `source.path`, which stays unpinned: a component's own
source is an arbitrarily large tree that src2deb copies and the build writes
into, while an overlay contributes one small directory that nothing writes to,
so it can be measured cheaply and exactly.

### Packaging from a repository, with your own changes

There is one overlay per component, not a stack of them. To take a
distribution's packaging and change part of it, overlay theirs and
[patch](#patches) the rest:

```toml
[[components]]
name = "foo"
source.git = "https://github.com/example/foo"
packaging.git = "https://salsa.debian.org/debian/foo"
patches = ["patches/0001-build-with-our-features.patch"]
```

The series is applied after the overlay, so a patch reaches the packaging the
overlay supplied. That is deliberately the only way to do it: a second overlay
winning per file would let the packaging you layered over drift out from under
you with no signal at all, while a patch that no longer applies fails the
component and names itself.

## Building from a release archive

Most projects that are not Rust ones publish a release tarball rather than
expecting you to build from a tag, and an upstream tarball beside a separate
`debian/` directory is the native Debian model. Name the archive and the digest
it must hash to:

```toml
[[components]]
name = "foo"
source.tarball = "https://example.org/releases/foo-1.2.3.tar.xz"
source.sha256 = "5f2e1a9c3b8d4e7a2f9016c5b3d8e4a71f0c9d2b6e5a8347c1b0f9e2d6a4c8b13"
packaging.path = "packaging/foo"
version = "1.2.3"
```

`https`, `http`, and `file` URLs are all fetched, so a local mirror works as well
as a release page. The archive may be uncompressed or compressed with gzip, xz,
or zstd, read from its content rather than from the URL.

A release archive ships no `debian/`, so it needs [packaging from
elsewhere](#packaging-overlays) and a [declared
version](#components-with-no-changelog). Both are shown above.

### The digest is what makes it a source

`source.sha256` is required, and the archive is verified against it before
anything is unpacked — on every run, not only the first. Nothing about the
transport is trusted: a hostile mirror, a broken proxy, and a truncated download
all produce something that does not hash to what you declared, and the component
fails rather than building it.

That is what makes an archive as good an input as a revision. What a URL serves
can change; what it hashes to cannot.

```text
pkg: the archive fetched from https://example.org/releases/foo-1.2.3.tar.xz
hashes to 91af22c..., and the recipe declares sha256 = "5f2e1a9...". Nothing
was unpacked.
```

A mismatch is worth reading twice. If the archive is the one you meant, correct
the recipe. If it is not, you have found a mirror serving something other than
what it did when the digest was written down.

### The release directory is found for you

A release archive conventionally puts everything under one directory named for
the release — `foo-1.2.3/` — and src2deb descends into it, so the version does
not go into the recipe twice. An archive laid out any other way is taken as it
stands, and `source.subdir` applies within whichever you get.

The one directory never treated as a wrapper is `debian/`: a distribution
publishes its packaging as an archive holding exactly that, and there the single
directory is what is being supplied rather than something wrapped around it.

### Fetched once, kept by digest

Archives are cached under `<work>/tarballs/`, named by their digests. Two
components naming one archive fetch it once, a recipe that changes a digest names
a different file rather than a stale one, and a host with no network — or no
`curl` — still builds from what is already there.

The unpacked tree, by contrast, is replaced on every run, so each build sees the
archive as it stands rather than the leavings of the run before.

## Components with no changelog

A component's version normally comes from its own `debian/changelog`, which
src2deb extends with the suite, the date, and the source revision. Packaging
assembled from a [packaging overlay](#packaging-overlays) often has no such
history: a `control` and a `rules` are enough to build with, and a changelog is
not something you want to maintain by hand for a package that is rebuilt from
source every time.

Declare the version instead, and src2deb writes the changelog:

```toml
[[components]]
name = "foo"
source.git = "https://github.com/example/foo"
packaging.path = "packaging/foo"
version = "1.2.3"
```

The entry it writes carries the version you declared, the source package name
`debian/control` declares, and a maintainer identity — and the ordinary stamping
path then extends it, so the package is versioned exactly as any other:

```text
foo (1.2.3+deb13.20260731.abc1234.def5678) trixie; urgency=medium
```

Everything downstream reads that changelog, including the vendor pass, so the
component builds like any other.

### Deriving the version from a tag

A project that tags its releases can state its version once, in the tag:

```toml
[[components]]
name = "foo"
source.git = "https://github.com/example/foo"
packaging.path = "packaging/foo"
version-from = "git-describe"
```

`git describe --tags` runs against the resolved source, and its output becomes a
version: a leading `v` is dropped, and hyphens become dots so the Debian revision
boundary stays where the stamp puts it.

| Tag state | `git describe` | Version |
| --- | --- | --- |
| On the tag | `v1.2.3` | `1.2.3` |
| Four commits past it | `v1.2.3-4-gabc1234` | `1.2.3.4.gabc1234` |

Those order the way the history does, so a build from a later commit supersedes
one from an earlier commit even before the build date is taken into account.

A source with no tag in its history has no version to derive, and the component
is refused rather than versioned from an abbreviated commit that would not order
against the build before it. State the version with `version` in that case.

`git describe` reads the repository the source was resolved into, not the
`source.subdir` within it — a member of a superproject takes the superproject's
tag, because that is the only tag there is.

### Where the maintainer comes from

The entry is signed with the first identity that is declared:

1. the component's own `maintainer`,
2. the recipe's `maintainer`,
3. the `Maintainer` field in the component's `debian/control`.

Debian policy makes that last field mandatory, so packaging complete enough to
build already carries an identity and most recipes declare nothing at all.
src2deb never invents one: a component that declares a version with no identity
anywhere is refused.

Write an identity as Debian writes it, `Name <email>`. An identity with no
address, or one carrying a line break or two consecutive spaces, cannot be read
back out of a changelog trailer and is refused by the recipe.

### A declared version replaces the changelog

`version` and `version-from` are the authority wherever they are set. If the
assembled tree does ship a `debian/changelog`, the declared version **replaces**
it — the same rule a packaging overlay follows, and for the same reason: one
authority for the version, with no per-entry precedence to reason about. That is
what lets you build a project whose upstream changelog has been frozen for years
at the version it actually has.

The consequence is that the shipped package carries the declared entry and the
stamped one above it, and not the history it replaced. Leave `version` out for a
component whose changelog you want kept.

Dropping the setting restores the tree's own changelog on the next run, exactly
as dropping a `packaging` overlay does.

### The declaration is part of what was built

`--skip-published` compares the declared version alongside the source
fingerprint, so editing `version` rebuilds the component even though every tree
it resolves is byte-identical. The manifest records it as `version` on the
component. See [The provenance manifest](provenance.md).

## Patches

A component may carry local fixes upstream has not taken. Declare them per
component, in the order they apply:

```toml
[[components]]
name = "cosmic-comp"
source.git = "https://github.com/pop-os/cosmic-comp"
patches = [
  "patches/cosmic-comp/0001-fix-build-on-trixie.patch",
  "patches/cosmic-comp/0002-relax-a-dependency.patch",
]
```

Each path is relative to the recipe's own directory, as `source.path` is, so a
recipe carries its patches alongside it. Keeping them under a directory named
for the component is a convention, not a requirement.

The series is applied to the tree src2deb resolved — a git checkout, or its copy
of a `source.path` tree — and never to anything of yours. It is applied last, so
a patch may change a file a [packaging overlay](#packaging-overlays) supplied,
and before anything reads the tree, so a patch may change `debian/control` and
the build order follows the patched file.

### What a patch may be

Anything `git apply` accepts: a plain unified diff, a `git format-patch` output,
a patch that adds or deletes files, one that changes a file's mode. Paths are
read at `-p1` — the `a/` and `b/` prefixes git writes — and must stay inside the
tree.

Patches apply to either kind of source, over a packaging overlay if there is
one, and to a `subdir` component they apply relative to the subdirectory that
holds `debian/`.

### A patch either applies or the component fails

There is no fuzz, no three-way merge, and no `.rej` file left behind. A patch
that no longer matches the source it was written against fails the component,
naming the patch:

```text
src2deb: FAILED cosmic-comp: resolving source for cosmic-comp: patch
recipes/cosmic-epoch/patches/cosmic-comp/0001-fix-build-on-trixie.patch does not
apply to /work/sources/cosmic-comp: error: patch failed: src/shell/mod.rs:412
```

A partly-patched tree is not something to build a package from, so the component
stops rather than continuing with whatever did apply. Under `--keep-going` the
rest of the run carries on without it, as with any other resolve failure.

### Patches are part of what a package was built from

The series is a pinned input to the component's fingerprint, identified by a
digest over its members' contents in order. That has three consequences:

- The version carries it, after the source revision:
  `1.0.0-1+deb13.20260731.abc1234.5f2e1a9`. A patched package and an unpatched
  one built from the same revision on the same day are therefore distinct, and
  ordered.
- The manifest records it as an input of kind `patches`. See [The provenance
  manifest](provenance.md).
- `--skip-published` rebuilds the component when the series changes. Editing a
  patch, adding one, removing one, or reordering them all count; renaming a
  patch file does not, since the same patches in the same order produce the same
  tree.

Removing a patch removes its effect, including any file it added. A source
checkout persists between runs and a patch's new files are untracked, so this is
not something a re-checkout would do on its own — src2deb clears what the last
run's series left, so a component always builds the tree its recipe currently
describes.

### What this is not

`patches` applies a series and nothing more. It does not manage one: there is no
command to add, refresh, or rebase a patch, and none to record one from a
modified tree. Use git for that, on a branch of the upstream source, and export
with `git format-patch`.

Nor is it `debian/patches`. src2deb applies the series directly to the tree, so
it works whatever format the source is in and whether or not the packaging uses
quilt. A component whose upstream `debian/patches` you want to extend is
better served by patching `debian/patches/series` itself with one of these.

## Recipes in this repository

Three recipes ship with src2deb. Each has a README covering what it builds, the
upstream it builds from, and how to run it — together they are the worked
examples for everything above.

| Recipe | Builds |
| --- | --- |
| [`cosmic-epoch`][epoch] | The COSMIC desktop: 27 components from Pop's `debian/` trees, with a pinned rustup toolchain |
| [`pop-desktop-data`][data] | The theme, icon, font, and metadata packages COSMIC depends on at runtime |
| [`cosmic-debian`][debian] | The `cosmic-desktop` metapackage and a compatibility package for a dependency name Debian has retired |

[epoch]: https://github.com/gregordinary/src2deb/tree/main/recipes/cosmic-epoch
[data]: https://github.com/gregordinary/src2deb/tree/main/recipes/pop-desktop-data
[debian]: https://github.com/gregordinary/src2deb/tree/main/recipes/cosmic-debian

All three belong in one pool, and share a work directory to get there. Build
them for the same suite and architecture, and
`apt install cosmic-desktop` installs the result. See
[Using the pool](using-the-pool.md).
