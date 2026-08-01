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
- `source` — where the component's source comes from:
  - `source.git` — the git repository URL to clone. Required.
  - `source.git-ref` — the branch, tag, or commit to check out. Defaults to the
    remote's default branch.
  - `source.subdir` — a subdirectory within the checkout that holds the
    `debian/` tree, for a component that lives inside a larger superproject. The
    whole checkout is the source tree when unset. It must stay inside the
    checkout: a `..` component or an absolute path is refused, since the source
    tree it names is what the vendor pass binds into a cage that runs upstream's
    own `debian/rules clean`.
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
