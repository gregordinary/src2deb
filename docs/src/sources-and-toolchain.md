# Sources and the toolchain

Where the source, the packages, and the compiler come from is declared in the
recipe, along separate axes. The [recipe reference](recipes.md) lists the fields;
this chapter explains the model behind them.

Source and archives are different questions. A component's *source* is the tree
that gets built; an *archive source* is where its build-dependencies are
resolved from. The first two sections below take them in that order.

The guiding constraint for archives is that the underlying resolver is
highest-version-wins with no pin priorities. src2deb earns determinism by
controlling the resolver's inputs — the exact set of archives it sees — rather
than by layering a preferences engine on top.

## Component sources

Each component names one source: a git repository to clone, a tree already on
disk, or a release archive to fetch. What gets built is assembled from that
source, then any packaging overlay, then any patch series — in that order,
before anything reads the tree.

A **git source** is cloned into the work directory on first use and fetched on
later use, then checked out detached at the requested ref. A branch or an unset
ref advances to the fetched upstream tip on every run; a tag or a commit resolves
to itself. What the run records is the `HEAD` the tree ended up at, so the
revision in a package's version and in the manifest is always a concrete commit,
never the moving ref that named it.

A **path source** is a tree already on disk, built without being cloned. It is
the difference between "edit, commit, push, run" and "run" while working on a
packaging tree.

An **archive source** is a release tarball, fetched and unpacked. It is how most
projects that are not Rust ones publish, and an upstream tarball beside a
separate `debian/` is the native Debian model.

A **Debian source package** is a `.dsc` and the tarballs it names, fetched and
assembled. It is how you rebuild a package one suite ships for another, and it is
the one source that carries its own packaging, its own changelog, and everything
its build needs — so it is also the one built with no host network at any point.
See [Rebuilding a Debian source
package](recipes.md#rebuilding-a-debian-source-package).

### Building from a tree on disk

A path source is copied into the work directory and built from the copy. Nothing
writes into the tree the recipe named.

That matters more than it might sound. The vendor pass binds the source tree
read-write and runs the component's own `debian/rules clean` in it, which is what
triggers the vendoring idiom — leaving a `vendor.tar` and a `vendor/` behind, and
deleting whatever that target is written to delete. For a git source, the tree
that happens to is a checkout src2deb made and owns. For a path source it would
be your working directory.

The copy is made afresh on every run, which is what a git source gets from `git
checkout --force`: a file you delete really disappears from the build, and no
state survives from the run before. The cost follows the size of the tree, so a
path pointed at a directory that also holds a large build output — a `target/`,
say — pays for that output on every run.

Two further rules follow from a path naming *where* a tree was read from and
nothing about *what* it held:

- The build is recorded as unpinned. The manifest writes `pinned = false` against
  the input, and the package version carries `local` where a git build carries an
  abbreviated commit — so a local build is unmistakable in `apt policy` output.
  See [Package versions](package-versions.md) and [The provenance
  manifest](provenance.md).
- `--skip-published` never skips it. There is nothing to compare a path against,
  so a component built from one is rebuilt on every run.

If the tree is a git checkout holding unmaterialized Git LFS pointers, the
component fails rather than building a package around the stubs, and the error
names the tree to run `git lfs pull` in. src2deb does not fetch on your behalf
here: the tree is yours.

### Building from a release archive

An archive source names a URL and the SHA-256 the archive must hash to. `https`,
`http`, and `file` URLs are fetched with `curl`, and the archive may be
uncompressed or compressed with gzip, xz, or zstd — read from its content rather
than from the URL.

The digest is the whole of the trust: the archive is verified against it before
anything is unpacked, on every run and not only the first, so a hostile mirror, a
broken proxy, and a truncated download each fail the component rather than
building something no one asked for. Nothing about the transport carries any part
of that claim, which is what makes `curl` an acceptable answer to fetching over
TLS rather than a compromise.

Archives are cached under the work directory, named by the digest that pins them,
so two components naming one archive fetch it once and a re-run fetches nothing —
a host with no network, or no `curl`, still builds from what is there. The
unpacked tree, by contrast, is replaced on every run, so each build sees the
archive as it stands.

A release archive ships no `debian/` of its own, so it needs packaging from
elsewhere and a declared version. See [Building from a release
archive](recipes.md#building-from-a-release-archive).

### Packaging from somewhere else

A component's tree has to hold a `debian/` directory, and not every upstream
ships one. A component may therefore name a second tree — resolved by the same
two origins, under the same rules — whose `debian/` becomes the component's.

The rule is a narrow one in both directions, and both halves are deliberate.
Only `debian/` is taken, because a distribution's packaging repository usually
carries a copy of the upstream tree beside its packaging, and that copy is
whichever release was last packaged rather than the source you are building.
And what is taken *replaces* the source's own `debian/` rather than merging with
it, so there is no per-file precedence to reason about and nothing of an
abandoned packaging tree left beside the declared one.

An overlay is a build input like any other: both inputs reach the version stamp
and the manifest, and `--skip-published` rebuilds when either moves. An overlay
from a repository is identified by its revision, and one from a directory on
disk by a digest over the `debian/` tree it supplied — so packaging kept beside
the recipe is as comparable from run to run as packaging kept in a repository,
and editing it publishes a new package. See [Packaging
overlays](recipes.md#packaging-overlays).

### Versions for packaging with no history

Packaging assembled this way often has no `debian/changelog` — a `control` and a
`rules` are enough to build with, and a release history is not something worth
maintaining by hand for a package rebuilt from source each time. The version
stamp has nothing to extend in that case, so the component names its version in
the recipe and src2deb writes the changelog: one entry, over an identity the
recipe or the packaging's own `Maintainer` field supplies. The ordinary stamping
path then extends that entry, so one code path produces every version src2deb
stamps.

`version` states the version outright and `version-from = "git-describe"` derives
it from the source's tags. See [Components with no
changelog](recipes.md#components-with-no-changelog).

### Patches over the assembled tree

Either kind of source may carry a patch series — local fixes upstream has not
taken — applied to the tree src2deb resolved rather than to anything of yours.
The series is applied last, after any packaging overlay, so a patch is the way
to fix packaging you do not control; and it is applied before any
`debian/control` is read, so a patch may change what a component build-depends
on and the build order follows.

A series is a pinned input in its own right, identified by a digest over its
members' contents in order. It is stamped into the package version alongside the
source revision, recorded in the manifest, and compared by `--skip-published`, so
editing a patch rebuilds the component and produces a version that supersedes the
one built without it. See [Patches](recipes.md#patches).

## Archive sources

The primary suite and the feed-forward pool are always present. A recipe may add
named archives — a backports suite, a vendor archive, a local `file://` pool —
each with its own suite, mirror, and components. Every added archive is threaded
into provisioning for the shared base, each layer, and each full root.

A signed archive must name the keyring its release is verified against: the
provisioner has no embedded trust anchor for an archive other than the primary
Debian one. A local archive under your control may instead be trusted without a
signature.

## The toolchain

The Rust compiler and Cargo are selected separately from the archive list,
because a rustup toolchain is not a Debian archive:

- The **Debian** provider, the default, resolves `rustc` and `cargo` from the
  archive as ordinary build-dependencies. The build is only as new as the suite's
  Rust.
- The **rustup** provider installs a pinned toolchain into the build root and
  prefers it on `PATH`, while the archive's `rustc` and `cargo` stay installed to
  satisfy the component's declared build-dependencies. This decouples the
  compiler from the suite's Rust cadence — for example, building current COSMIC,
  which needs a newer `rustc` than Debian Trixie ships, on Trixie.

The rustup provider fetches the upstream installer from `https://sh.rustup.rs`
over pinned TLS (`--proto '=https' --tlsv1.2`) and installs the exact toolchain
version the recipe names. The installer script itself is not checksum-pinned —
this is the standard rustup bootstrap — so a rustup toolchain trusts that fetch
in addition to the archive. The Debian provider avoids it, resolving `rustc` and
`cargo` from the signed archive alone.

The install happens while a build root is being provisioned, not while a build is
running, so the toolchain is fetched once per root. Under the layered strategy
that means once per run for the whole recipe, rather than once for every
component: a build pass writes into a per-component overlay that is discarded
when the component finishes, so anything a pass installed would have to be
installed again for the next one. The pinned version is part of the root's cache
key, so repinning a recipe's toolchain provisions a fresh root rather than
reusing one holding the version it replaced. See
[Build roots](build-roots.md).
