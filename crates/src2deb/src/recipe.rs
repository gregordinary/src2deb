//! A build recipe: the components to build, and where their source comes from.
//!
//! A recipe is a TOML file, `recipe.toml`, in a recipe directory. It names a
//! suite and architecture and lists components, each with a source: a git
//! repository to clone, or a tree already on disk. See `recipes/cosmic-epoch/`
//! for a worked example.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::version::VersionStamp;

/// A build recipe.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Recipe {
    /// The directory the recipe was loaded from, which a relative
    /// [`Source::path`] resolves against. See [`dir`](Self::dir).
    ///
    /// Set by [`load`](Self::load) rather than read from the file, so a recipe
    /// cannot declare a directory other than the one it lives in. A recipe
    /// deserialized directly carries none, and a relative `source.path` then
    /// resolves against the process's working directory.
    #[serde(skip)]
    dir: PathBuf,
    /// The recipe name (for example `cosmic-epoch`).
    pub name: String,
    /// The Debian suite to build for (for example `trixie` or `forky`).
    pub suite: String,
    /// The tag every built version carries, identifying the suite it was built
    /// for (for example `deb13`).
    ///
    /// Defaults to the tag for the recipe's suite, which src2deb knows for the
    /// numbered Debian releases. A recipe targeting a suite outside that set —
    /// a rolling suite, or a derivative — names its own, because a tag that
    /// does not order the way the releases do would make an upgrade between
    /// suites look like a downgrade. See [`crate::version`].
    ///
    /// It names the tag for the recipe's *own* [`suite`](Self::suite), and only
    /// that one. Retargeting a run at another suite leaves this tag describing a
    /// suite the run is no longer building for, so a caller that replaces
    /// [`suite`](Self::suite) clears this field as well and lets the new suite
    /// resolve its own tag.
    #[serde(default)]
    pub version_tag: Option<String>,
    /// The target architecture, a Debian name such as `amd64` or `arm64`.
    /// Defaults to the host's architecture, so a recipe that omits it builds
    /// natively wherever it runs; naming a foreign one builds through qemu.
    #[serde(default = "default_architecture")]
    pub architecture: String,
    /// The architecture that produces this recipe's `Architecture: all`
    /// packages, when one is named.
    ///
    /// An `Architecture: all` package's file name carries no architecture and
    /// its stamped version does not vary with one, so building the recipe for
    /// two architectures produces that package twice: the same name and version
    /// over different bytes. Naming an owner settles which architecture makes
    /// it, and every other one builds only its architecture-dependent packages.
    ///
    /// Unset, every run owns its own arch-indep output, which is the behaviour
    /// a single-architecture build wants: its pool holds every package the
    /// recipe produces and can be served as it stands. Name an owner when
    /// several architectures feed one published archive. See
    /// [`owns_arch_indep`](Self::owns_arch_indep).
    #[serde(default)]
    pub arch_indep_owner: Option<String>,
    /// How this recipe's components order against the archive's own packages of
    /// the same version, for components that do not state it themselves.
    ///
    /// Defaults to [`VersionStamp::Supersede`], which is what a build of
    /// software the archive does not carry wants. A recipe whose components are
    /// rebuilds of Debian source packages sets
    /// [`backport`](VersionStamp::Backport) here rather than on each of them.
    /// See [`Component::version_stamp`].
    #[serde(default)]
    pub version_stamp: Option<VersionStamp>,
    /// The maintainer identity a synthesized `debian/changelog` is signed with,
    /// for components whose packaging carries none of their own.
    ///
    /// Written as Debian writes it, `Name <email>`. A component may name its
    /// own with [`Component::maintainer`], and a component that names neither
    /// takes the `Maintainer` its `debian/control` declares — so this is a
    /// convenience for a recipe packaging several components under one
    /// identity, not a requirement.
    ///
    /// Unused by a component whose packaging ships a changelog: that entry's
    /// own trailer is reused, as it always has been. See [`crate::version`].
    #[serde(default)]
    pub maintainer: Option<String>,
    /// The primary archive mirror. Defaults to the Debian CDN inside the
    /// provisioner when unset.
    #[serde(default)]
    pub mirror: Option<String>,
    /// The Rust toolchain to build with. Defaults to the one the archive
    /// sources provide (`provider = "debian"`).
    #[serde(default)]
    pub toolchain: Toolchain,
    /// Additional archive repositories to resolve build-dependencies from,
    /// beyond the primary suite and the feed-forward pool. Empty by default.
    #[serde(default)]
    pub repositories: Vec<Repository>,
    /// The components to build, in any order; src2deb computes the build order
    /// from their declared dependencies.
    pub components: Vec<Component>,
}

/// The toolchain configuration for a recipe.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Toolchain {
    /// How the Rust compiler and Cargo are obtained.
    #[serde(default)]
    pub rust: RustToolchain,
}

/// Where the Rust compiler and Cargo come from.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct RustToolchain {
    /// The provider: the archive's own `rustc`/`cargo`, or a rustup-managed
    /// toolchain installed into the build root.
    #[serde(default)]
    pub provider: RustProvider,
    /// The exact toolchain version, required for [`RustProvider::Rustup`] (for
    /// example `1.95.0`) and unused for [`RustProvider::Debian`].
    #[serde(default)]
    pub version: Option<String>,
}

impl RustToolchain {
    /// The pinned rustup toolchain version to install, or `None` when the
    /// archive's own Rust is used.
    pub fn rustup_version(&self) -> Option<&str> {
        match self.provider {
            RustProvider::Debian => None,
            RustProvider::Rustup => self.version.as_deref(),
        }
    }
}

/// A source of the Rust compiler and Cargo.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RustProvider {
    /// The `rustc` and `cargo` the archive sources provide, resolved as ordinary
    /// build-dependencies. The build is only as new as the suite's Rust.
    #[default]
    Debian,
    /// A pinned toolchain installed with `rustup` into the build root, layered
    /// over the archive's Rust (which stays installed to satisfy the build's
    /// declared `rustc`/`cargo` build-dependencies) and preferred on `PATH`.
    /// Decouples the compiler from the suite's Rust cadence.
    Rustup,
}

/// An additional archive repository a recipe resolves build-dependencies from.
///
/// The primary suite and the feed-forward pool are always present; these are
/// extra archives — a backports suite, a vendor archive, a local `file://`
/// pool. A signed repository must name a [`keyring`](Repository::keyring); only
/// a [`trust_unsigned`](Repository::trust_unsigned) repository (a local or
/// `[trusted=yes]` archive) may omit one, as the provisioner has no embedded
/// trust anchor for an archive other than the primary Debian one.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Repository {
    /// A short identifier, unique within the recipe (for example `backports`).
    pub name: String,
    /// The suite to resolve from. Defaults to the recipe's primary suite.
    #[serde(default)]
    pub suite: Option<String>,
    /// The archive mirror URL. Defaults to the recipe's primary mirror, or the
    /// provisioner's default Debian mirror when that is also unset.
    #[serde(default)]
    pub mirror: Option<String>,
    /// The archive components to enable. Defaults to `["main"]`.
    #[serde(default = "default_components")]
    pub components: Vec<String>,
    /// Trust the repository without verifying a signature (apt's
    /// `[trusted=yes]`), for a local or `file://` archive under your control.
    #[serde(default)]
    pub trust_unsigned: bool,
    /// The binary OpenPGP keyring the repository's release is verified against.
    /// Required for a signed repository; omitted for a
    /// [`trust_unsigned`](Self::trust_unsigned) one.
    #[serde(default)]
    pub keyring: Option<PathBuf>,
}

/// One buildable component: a source tree with a `debian/` directory.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Component {
    /// The component name, unique within the recipe.
    pub name: String,
    /// Where the component's source comes from.
    ///
    /// Defaulted rather than required so that a component naming no origin is
    /// refused by [`Recipe::load`] with the two settings that would fix it,
    /// instead of by the parser with the name of a table.
    #[serde(default)]
    pub source: Source,
    /// Where the component's `debian/` directory comes from, for a source that
    /// carries none of its own.
    ///
    /// Resolved exactly as [`source`](Self::source) is — a git repository to
    /// clone, or a tree already on disk — and its `debian/` directory becomes
    /// the component's, replacing whatever the source ships. Nothing outside
    /// `debian/` is taken from it, so a packaging repository that also carries
    /// a copy of the upstream tree contributes only its packaging.
    ///
    /// The overlay is a second input to the component's fingerprint: the
    /// manifest names both revisions, the version stamp carries both, and
    /// moving either triggers a rebuild under `--skip-published`. See
    /// [`crate::fingerprint`].
    #[serde(default)]
    pub packaging: Option<Source>,
    /// Patch files applied over the resolved source tree, in the order given.
    ///
    /// Each is relative to the recipe's own directory ([`Recipe::dir`]), as
    /// [`Source::path`] is, so a recipe carries its patches alongside it. The
    /// series is applied before anything reads the tree, so a patch may change
    /// `debian/control` and the build order follows the patched file.
    ///
    /// The series is a pinned input to the component's fingerprint: editing,
    /// adding, removing, or reordering a patch changes what the component was
    /// built from, so it is stamped into the version and triggers a rebuild
    /// under `--skip-published`. See [`crate::fingerprint`].
    #[serde(default)]
    pub patches: Vec<PathBuf>,
    /// The upstream version to build this component as, for packaging that
    /// carries no `debian/changelog` to take one from.
    ///
    /// Exclusive with [`version_from`](Self::version_from), which derives the
    /// same thing rather than stating it. Declaring either makes src2deb write
    /// the component's `debian/changelog`, **replacing** whatever the assembled
    /// tree holds — the same rule a [`packaging`](Self::packaging) overlay
    /// follows, and for the same reason: one authority for the version, with no
    /// per-entry precedence to reason about.
    ///
    /// The value stands where a changelog's own version would, so it may carry
    /// an epoch and a Debian revision. src2deb stamps the suite, date, and
    /// source fingerprint onto it as it does any other. See
    /// [`crate::version`].
    #[serde(default)]
    pub version: Option<String>,
    /// Where to derive this component's upstream version from, for packaging
    /// that carries no `debian/changelog` and a source that names its releases.
    ///
    /// Exclusive with [`version`](Self::version). See [`VersionFrom`].
    #[serde(default)]
    pub version_from: Option<VersionFrom>,
    /// How this component's stamped version orders against the archive's own
    /// package of the same version, overriding the recipe's
    /// [`version_stamp`](Recipe::version_stamp).
    ///
    /// Unset — and unset on the recipe — a build supersedes the version it was
    /// built from, which is right for software the archive does not carry.
    /// `version-stamp = "backport"` puts it below instead, which is what a
    /// rebuild of a source the archive also ships wants: the archive's own
    /// package wins wherever it is available, and the rebuild fills the gap
    /// where it is not.
    ///
    /// A component built from a [`source.dsc`](Source::dsc) is the usual case
    /// for it, but the setting is about the package rather than about how its
    /// source was fetched: a component built from a git repository of the same
    /// software wants exactly the same thing. It is therefore stated rather than
    /// implied, so that changing how a source is fetched never silently changes
    /// how its packages order. See [`VersionStamp`].
    #[serde(default)]
    pub version_stamp: Option<VersionStamp>,
    /// The maintainer identity this component's synthesized `debian/changelog`
    /// is signed with, overriding the recipe's
    /// [`maintainer`](Recipe::maintainer).
    ///
    /// Written as Debian writes it, `Name <email>`. A component naming neither
    /// takes the `Maintainer` its `debian/control` declares.
    #[serde(default)]
    pub maintainer: Option<String>,
    /// Extra build-dependency package names beyond those `debian/control`
    /// declares, given in the recipe as `extra-build-deps`. Rarely needed; most
    /// build-deps are discovered from control.
    #[serde(default)]
    pub extra_build_deps: Vec<String>,
}

/// Where a component's upstream version is derived from, when the recipe does
/// not state it outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VersionFrom {
    /// `git describe --tags`, run against the component's resolved source.
    ///
    /// The nearest tag on a tagged commit, and `<tag>-<commits>-g<hash>`
    /// anywhere after one, rewritten into a Debian version — see
    /// [`version_from_describe`](crate::version::version_from_describe). A
    /// source with no tag in its history has no version to derive, and the
    /// component is refused rather than given one that does not order.
    ///
    /// The repository described is the one the source was resolved into, not
    /// the [`subdir`](Source::subdir) within it: a member of a superproject
    /// takes the superproject's tag, because that is the only tag there is.
    GitDescribe,
}

/// Where a component's upstream version comes from, as a single choice rather
/// than two fields that could contradict each other.
///
/// Produced by [`Component::version_source`] once a recipe has been validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionSource<'a> {
    /// The version the recipe states outright.
    Declared(&'a str),
    /// A version derived from the resolved source.
    Derived(VersionFrom),
}

/// Where a tree a component is built from comes from.
///
/// Serves both of a component's inputs: its own [`source`](Component::source),
/// and the [`packaging`](Component::packaging) overlay that supplies a
/// `debian/` directory when the source carries none. Each names exactly one
/// origin — [`git`](Self::git) or [`path`](Self::path) — which
/// [`Recipe::load`] enforces, so [`origin`](Self::origin) answers for a
/// validated recipe. The remaining fields qualify whichever was named.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Source {
    /// The git repository URL to clone. Exclusive with [`path`](Self::path).
    #[serde(default)]
    pub git: Option<String>,
    /// The branch, tag, or commit to check out. Defaults to the remote's
    /// default branch when unset, and applies only to [`git`](Self::git).
    #[serde(default)]
    pub git_ref: Option<String>,
    /// A release archive to fetch and unpack. Exclusive with
    /// [`git`](Self::git) and [`path`](Self::path), and requires
    /// [`sha256`](Self::sha256).
    ///
    /// `http`, `https`, and `file` URLs are fetched; a redirect may reach only
    /// the first two. The archive may be uncompressed or compressed with gzip,
    /// xz, or zstd, which is read from its content rather than from the URL.
    ///
    /// The digest pins it, so an archive source is as reproducible as a
    /// revision: what the URL serves may change, and a build that consumed
    /// something else fails rather than producing a package.
    #[serde(default)]
    pub tarball: Option<String>,
    /// The SHA-256 the [`tarball`](Self::tarball) or [`dsc`](Self::dsc) must
    /// hash to, in hexadecimal of either case.
    ///
    /// Required for a fetched artefact and refused on any other origin, since it
    /// says nothing about a revision or a directory. Verified before anything is
    /// unpacked, on every run.
    #[serde(default)]
    pub sha256: Option<String>,
    /// A Debian source package to fetch and unpack, named by the URL of its
    /// `.dsc`. Exclusive with the other origins, and requires
    /// [`sha256`](Self::sha256).
    ///
    /// The declared digest pins the `.dsc`, and the `.dsc` declares the digest
    /// of every file it names — so one hash in the recipe pins the whole source
    /// package. The files are fetched from the directory the `.dsc` sits in.
    ///
    /// A component built from one **skips the vendor pass**: a Debian source
    /// package already carries what its build needs, so the build is hermetic
    /// from start to finish. Such a component ordinarily also names
    /// [`version_stamp`](Component::version_stamp), since the archive ships the
    /// package it rebuilds.
    ///
    /// `3.0 (quilt)`, `3.0 (native)`, and native `1.0` source packages are
    /// built. A `1.0` package carrying a `.diff.gz` is refused, naming the
    /// alternative.
    #[serde(default)]
    pub dsc: Option<String>,
    /// A tree already on disk, built without being cloned. Exclusive with
    /// [`git`](Self::git).
    ///
    /// Relative to the recipe's own directory ([`Recipe::dir`]), so a recipe
    /// kept beside the trees it builds names them relatively and moves with
    /// them. An absolute path is used as it stands.
    ///
    /// A path is not a pinned input: it says where a tree was read from and
    /// nothing about what it held. Builds from one are recorded as
    /// unreproducible and are never skipped by `--skip-published`. See
    /// [`crate::fingerprint`].
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// A subdirectory within the source that holds the `debian/` tree, for a
    /// component that lives inside a larger superproject or monorepo. The whole
    /// source is the tree when unset.
    ///
    /// On a [`packaging`](Component::packaging) overlay it names the directory
    /// holding the `debian/` tree to overlay, which is the same thing said of
    /// the other input.
    #[serde(default)]
    pub subdir: Option<PathBuf>,
}

/// Where a tree comes from, as a single choice rather than a set of fields
/// that could contradict each other.
///
/// Produced by [`Source::origin`] once a recipe has been validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin<'a> {
    /// A git repository, cloned under the work directory and checked out.
    Git {
        /// The repository URL.
        url: &'a str,
        /// The branch, tag, or commit to check out, or `None` to track the
        /// remote's default branch.
        git_ref: Option<&'a str>,
    },
    /// A tree already on disk, copied under the work directory as it stands.
    Path(&'a Path),
    /// A release archive, fetched and unpacked under the work directory.
    Tarball {
        /// The URL to fetch the archive from.
        url: &'a str,
        /// The SHA-256 the archive is verified against before it is unpacked.
        sha256: &'a str,
    },
    /// A Debian source package, fetched and unpacked under the work directory.
    Dsc {
        /// The URL of the `.dsc`.
        url: &'a str,
        /// The SHA-256 the `.dsc` is verified against before it is read. The
        /// `.dsc` then carries the digest of every file it names.
        sha256: &'a str,
    },
}

impl Source {
    /// Where this tree comes from, or `None` when it does not name exactly one
    /// origin.
    ///
    /// Never `None` for a validated recipe: [`Recipe::load`] refuses a
    /// component whose source or packaging overlay names no origin, refuses one
    /// that names more than one, and refuses an archive with no digest to
    /// verify it against — so exactly one arm applies. A caller that builds a
    /// [`Source`] itself is outside that guarantee, which is why this reports
    /// rather than asserts.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match (&self.git, &self.path, &self.tarball, &self.dsc) {
            (Some(url), None, None, None) => Some(Origin::Git {
                url,
                git_ref: self.git_ref.as_deref(),
            }),
            (None, Some(path), None, None) => Some(Origin::Path(path)),
            // A fetched artefact with no digest names no origin: there would be
            // nothing to verify what was fetched against, and the tree would be
            // whatever the URL happened to serve.
            (None, None, Some(url), None) => Some(Origin::Tarball {
                url,
                sha256: self.sha256.as_deref()?,
            }),
            (None, None, None, Some(url)) => Some(Origin::Dsc {
                url,
                sha256: self.sha256.as_deref()?,
            }),
            _ => None,
        }
    }
}

impl Component {
    /// Where this component's upstream version comes from, or `None` when the
    /// recipe states neither — which is every component whose packaging ships a
    /// `debian/changelog`, and so the ordinary case.
    ///
    /// Never both for a validated recipe: [`Recipe::load`] refuses a component
    /// naming a version and a derivation at once, so exactly one arm applies
    /// when either is set. A caller that builds a [`Component`] itself is
    /// outside that guarantee, which is why this reports rather than asserts.
    pub fn version_source(&self) -> Option<VersionSource<'_>> {
        match (&self.version, self.version_from) {
            (Some(version), None) => Some(VersionSource::Declared(version)),
            (None, Some(from)) => Some(VersionSource::Derived(from)),
            (Some(_), Some(_)) | (None, None) => None,
        }
    }
}

impl Recipe {
    /// Loads the recipe from `recipe.toml` in `dir`.
    pub fn load(dir: impl AsRef<Path>) -> Result<Recipe> {
        let path = dir.as_ref().join("recipe.toml");
        let text = std::fs::read_to_string(&path).map_err(|err| Error::Recipe {
            path: path.clone(),
            reason: err.to_string(),
        })?;
        let mut recipe: Recipe = toml::from_str(&text).map_err(|err| Error::Recipe {
            path: path.clone(),
            reason: err.to_string(),
        })?;
        // Recorded from where the file was found rather than from anything in
        // it, so a relative `source.path` resolves against the recipe's own
        // directory. See [`Source::path`].
        recipe.dir = dir.as_ref().to_path_buf();
        recipe.validate(&path)?;
        Ok(recipe)
    }

    /// The directory the recipe was loaded from.
    ///
    /// A relative [`Source::path`] resolves against it, so a recipe kept beside
    /// the trees it builds names them relatively and moves with them. Empty for
    /// a recipe that was not loaded from disk.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The version tag builds from this recipe carry: the explicit
    /// [`version_tag`](Self::version_tag) when set, otherwise the tag for the
    /// recipe's suite.
    ///
    /// Never `None` for a validated recipe: [`Recipe::load`](Self::load)
    /// rejects a suite with no known tag and no override, so that a missing tag
    /// is a recipe error rather than a surprise partway through a run. A caller
    /// that replaces [`suite`](Self::suite) after loading is outside that
    /// guarantee and checks the result again — which is what
    /// [`crate::Error::VersionTag`] reports.
    pub fn resolved_version_tag(&self) -> Option<&str> {
        self.version_tag
            .as_deref()
            .or_else(|| crate::version::suite_tag(&self.suite))
    }

    /// The architecture that produces this recipe's `Architecture: all`
    /// packages: the declared [`arch_indep_owner`](Self::arch_indep_owner), or
    /// the recipe's own architecture when none is declared.
    pub fn resolved_arch_indep_owner(&self) -> &str {
        self.arch_indep_owner
            .as_deref()
            .unwrap_or(&self.architecture)
    }

    /// How `component`'s stamped version orders against the archive's own
    /// package of the same version: the component's own
    /// [`version_stamp`](Component::version_stamp), then the recipe's, then the
    /// default.
    ///
    /// The same precedence [`maintainer`](Self::maintainer) follows, and for the
    /// same reason: a recipe of rebuilds says it once, and a mixed recipe says
    /// it per component.
    pub fn resolved_version_stamp(&self, component: &Component) -> VersionStamp {
        component
            .version_stamp
            .or(self.version_stamp)
            .unwrap_or_default()
    }

    /// Whether the architecture this recipe targets produces its
    /// `Architecture: all` packages.
    ///
    /// True unless the recipe hands arch-indep output to another architecture,
    /// so an ordinary single-architecture build produces every package its
    /// recipe declares.
    pub fn owns_arch_indep(&self) -> bool {
        self.resolved_arch_indep_owner() == self.architecture
    }

    /// Rejects a structurally invalid recipe: no components; an unsafe recipe
    /// name, suite, architecture, or component name; a duplicate component or
    /// repository name; a component naming no source or two, or whose source or
    /// packaging overlay is otherwise unusable (see [`source_error`]); a rustup
    /// toolchain missing its version; or a signed repository missing its
    /// keyring.
    ///
    /// Recipes are trusted input, so the checks on values that become paths or
    /// subprocess arguments are defense in depth: they keep a typo from doing
    /// something far away from what it looks like, rather than standing between
    /// a recipe and the machine.
    fn validate(&self, path: &Path) -> Result<()> {
        let bad = |reason: String| Error::Recipe {
            path: path.to_path_buf(),
            reason,
        };

        if self.components.is_empty() {
            return Err(bad("the recipe lists no components".to_string()));
        }

        // The recipe name and the suite are both path segments of the run's
        // manifest, and the suite is additionally a path segment in the local
        // pool and a field in the `deb` lines the provisioner writes to
        // `sources.list`, where whitespace would be read as another field.
        if let Some(reason) = name_error(&self.name) {
            return Err(bad(format!("recipe name {:?} {reason}", self.name)));
        }
        if let Some(reason) = name_error(&self.suite) {
            return Err(bad(format!("suite {:?} {reason}", self.suite)));
        }

        // The version tag ends up inside a package version, so it is checked
        // here rather than at build time: a run that cannot stamp a version is
        // better refused before it resolves a single source tree.
        match &self.version_tag {
            Some(tag) => {
                if let Some(reason) = version_tag_error(tag) {
                    return Err(bad(format!("version-tag {tag:?} {reason}")));
                }
            }
            None if crate::version::suite_tag(&self.suite).is_none() => {
                return Err(bad(format!(
                    "suite {:?} is not a numbered Debian release, so it has no \
                     known version tag; set version-tag to the tag builds for \
                     this suite should carry (for example \"deb13\")",
                    self.suite
                )));
            }
            None => {}
        }

        // The architecture becomes a path segment in the local pool and a field
        // in the build root's plan key, so an unsafe one is refused before it
        // can corrupt either.
        if let Some(reason) = crate::arch::architecture_name_error(&self.architecture) {
            return Err(bad(format!(
                "architecture {:?} {reason}",
                self.architecture
            )));
        }
        // The owner is compared against the target architecture, and a name
        // that could never be one would hand arch-indep output to nothing.
        if let Some(owner) = &self.arch_indep_owner
            && let Some(reason) = crate::arch::architecture_name_error(owner)
        {
            return Err(bad(format!("arch-indep-owner {owner:?} {reason}")));
        }

        let mut seen = std::collections::BTreeSet::new();
        for component in &self.components {
            // A component name becomes a path segment under the work directory
            // and the checkout argument to git, so an unsafe one is rejected
            // before it can escape the work directory or be read as an option.
            if let Some(reason) = name_error(&component.name) {
                return Err(bad(format!("component name {:?} {reason}", component.name)));
            }
            if !seen.insert(component.name.as_str()) {
                return Err(bad(format!(
                    "duplicate component name {:?}",
                    component.name
                )));
            }
            // A component has one source. Naming two would leave which one it
            // is built from to whichever the resolver happened to look at
            // first, and naming none leaves nothing to build.
            match named_origins("source", &component.source).as_slice() {
                [_] => {}
                [] => {
                    return Err(bad(format!(
                        "component {:?} declares no source; set source.git to clone \
                         a repository, source.path to build a tree on disk, \
                         source.tarball to build a release archive, or source.dsc \
                         to rebuild a Debian source package",
                        component.name
                    )));
                }
                named => {
                    return Err(bad(format!(
                        "component {:?} declares {}; a component is built from one \
                         source",
                        component.name,
                        named.join(" and "),
                    )));
                }
            }
            if let Some(reason) = source_error("source", &component.source) {
                return Err(bad(format!("component {:?} {reason}", component.name)));
            }

            // A packaging overlay is resolved the same way the source is, so it
            // is held to the same rules. Its cardinality is stated separately
            // because the remedy is: an overlay is optional, and a table that
            // exists without naming an origin is a half-written one.
            if let Some(packaging) = &component.packaging {
                match named_origins("packaging", packaging).as_slice() {
                    [_] => {}
                    [] => {
                        return Err(bad(format!(
                            "component {:?} declares a packaging overlay with no \
                             source; set packaging.git to clone a repository, \
                             packaging.path to overlay a tree on disk, \
                             packaging.tarball to overlay a release archive, or \
                             packaging.dsc to take a Debian source package's \
                             packaging",
                            component.name
                        )));
                    }
                    named => {
                        return Err(bad(format!(
                            "component {:?} declares {}; a packaging overlay comes \
                             from one source",
                            component.name,
                            named.join(" and "),
                        )));
                    }
                }
                if let Some(reason) = source_error("packaging", packaging) {
                    return Err(bad(format!("component {:?} {reason}", component.name)));
                }
            }

            // A patch path is joined onto the recipe's directory as a source
            // path is, where an empty one would name that directory rather than
            // a file.
            if component.patches.iter().any(|p| p.as_os_str().is_empty()) {
                return Err(bad(format!(
                    "component {:?} lists an empty patch path",
                    component.name
                )));
            }

            // A component's version is stated or derived, never both: the two
            // would give one component two versions, and which won would be
            // whichever the resolver consulted first.
            if component.version.is_some() && component.version_from.is_some() {
                return Err(bad(format!(
                    "component {:?} declares both version and version-from; a \
                     component's version is stated or derived, not both",
                    component.name
                )));
            }
            // Checked here rather than at build time: a version that cannot be
            // stamped is a recipe error, and a run is better refused before it
            // resolves a single source tree.
            if let Some(version) = &component.version
                && let Some(reason) = crate::version::declared_version_error(version)
            {
                return Err(bad(format!(
                    "component {:?} version {version:?} {reason}",
                    component.name
                )));
            }
            if let Some(maintainer) = &component.maintainer
                && let Some(reason) = maintainer_error(maintainer)
            {
                return Err(bad(format!(
                    "component {:?} maintainer {maintainer:?} {reason}",
                    component.name
                )));
            }
        }

        // The recipe's own identity, held to the same rules as a component's.
        if let Some(maintainer) = &self.maintainer
            && let Some(reason) = maintainer_error(maintainer)
        {
            return Err(bad(format!("maintainer {maintainer:?} {reason}")));
        }

        // A rustup toolchain must pin an exact version, so the build is
        // reproducible and the archive's own Rust cannot silently stand in.
        if self.toolchain.rust.provider == RustProvider::Rustup
            && self.toolchain.rust.version.is_none()
        {
            return Err(bad(
                "toolchain.rust.provider = \"rustup\" requires toolchain.rust.version".to_string(),
            ));
        }

        let mut repository_names = std::collections::BTreeSet::new();
        for repository in &self.repositories {
            if !repository_names.insert(repository.name.as_str()) {
                return Err(bad(format!(
                    "duplicate repository name {:?}",
                    repository.name
                )));
            }
            // The provisioner has no embedded trust anchor for a non-primary
            // archive, so a signed one must name the keyring its release is
            // verified against.
            if !repository.trust_unsigned && repository.keyring.is_none() {
                return Err(bad(format!(
                    "repository {:?} is signed but names no keyring; add a keyring, or set \
                     trust-unsigned = true for a local archive",
                    repository.name
                )));
            }
        }

        Ok(())
    }
}

/// Reports why a suite name is unsafe, or `None` when it is safe.
///
/// The suite is a path segment of the run's manifest, pool, and output tree, and
/// a field in the `deb` lines the provisioner writes to `sources.list`. This is
/// the same check [`Recipe::load`] applies, exposed so a `--suite` override can
/// be rejected as a usage error against the flag rather than as an error against
/// a recipe that is itself fine — the counterpart of
/// [`architecture_name_error`](crate::arch::architecture_name_error).
pub fn suite_name_error(suite: &str) -> Option<&'static str> {
    name_error(suite)
}

/// Reports why a version tag is unusable, or `None` when it is usable.
///
/// This is the same check [`Recipe::load`] applies, exposed so a
/// `--version-tag` override can be rejected as a usage error against the flag
/// rather than as an error against a recipe that is itself fine — the
/// counterpart of [`suite_name_error`] and
/// [`architecture_name_error`](crate::arch::architecture_name_error).
///
/// The tag is spliced into the Debian revision of every version a run produces,
/// where the grammar allows only alphanumerics and `+`, `.`, `~`. A tag
/// containing anything else would produce versions `dpkg` refuses, surfacing as
/// an opaque build failure rather than as the recipe error it is.
///
/// The sharp case the grammar catches is `-`: a version's Debian revision
/// begins at its *last* hyphen, so a tag carrying one silently moves that
/// boundary and splits the version somewhere other than where it reads as
/// splitting. `1.0.0-1` tagged `deb-13` yields upstream `1.0.0-1+deb` with
/// revision `13.20260731.abc1234`, which still compares — just not as anything
/// anyone intended.
pub fn version_tag_error(tag: &str) -> Option<&'static str> {
    if tag.is_empty() {
        Some("is empty")
    } else if !tag
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '~')
    {
        Some("contains a character a Debian version may not")
    } else {
        None
    }
}

/// Reports why a maintainer identity cannot sign a changelog entry, or `None`
/// when it can.
///
/// The value is written verbatim into a changelog trailer,
/// ` -- Name <email>  Date`, which `dpkg` parses. Four rules follow from that
/// one line:
///
/// - **`Name <email>`**, because that is the form the trailer takes; an
///   identity with no address is a trailer `dpkg-parsechangelog` rejects.
/// - **No line break**, which would end the trailer early and leave the rest of
///   the identity standing as a changelog line of its own.
/// - **No two consecutive spaces**, which are what separate the identity from
///   the date: an identity carrying a pair would read back truncated, and the
///   remainder would be read as part of the date.
/// - **No leading or trailing whitespace**, since the trailer supplies its own
///   and the value is not trimmed on the way in.
///
/// This is the check [`Recipe::load`] applies to a declared identity. The
/// `Maintainer` a `debian/control` declares is held to it too, at the point it
/// is used, since it reaches the same line by another route.
pub fn maintainer_error(maintainer: &str) -> Option<&'static str> {
    if maintainer.is_empty() {
        Some("is empty")
    } else if maintainer != maintainer.trim() {
        Some("has leading or trailing whitespace")
    } else if maintainer.contains(['\n', '\r']) {
        Some("contains a line break")
    } else if maintainer.contains("  ") {
        Some(
            "contains two consecutive spaces, which end the identity in a \
             changelog trailer",
        )
    } else if !maintainer.ends_with('>')
        || !maintainer
            .split_once('<')
            .is_some_and(|(name, _)| !name.trim().is_empty())
    {
        Some("is not of the form \"Name <email>\"")
    } else {
        None
    }
}

/// Reports why a recipe's name, suite, or component name is unsafe to use as a
/// path segment, or `None` when it is safe.
///
/// All three are joined into work paths — a component's checkout, build root,
/// and output directory; the recipe and suite in the run's manifest path; the
/// suite again in the local pool — and a component name is additionally passed
/// to `git clone` as the directory to create. Each must therefore be a single,
/// benign path segment: non-empty, free of separators and `..` traversal, not
/// option-like, and free of whitespace, which would blur it into a neighbouring
/// field wherever a name is written to a line the tools reparse. Recipes are
/// trusted input, so this is defense in depth rather than a security boundary.
fn name_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("is empty")
    } else if name == "." {
        Some("is a directory reference")
    } else if name.contains("..") {
        Some("contains \"..\"")
    } else if name.contains(['/', '\\', '\0']) {
        Some("contains a path separator")
    } else if name.starts_with('-') {
        Some("starts with '-'")
    } else if name.contains(char::is_whitespace) {
        Some("contains whitespace")
    } else {
        None
    }
}

/// Reports why one of a component's declared trees is unusable, or `None` when
/// it is usable.
///
/// `field` names the table the settings came from — `source` or `packaging` —
/// so one set of rules serves both inputs while each message names the setting
/// the recipe actually wrote. Cardinality is not checked here: naming no origin
/// is a different mistake for a required source than for an optional overlay,
/// and each caller says so in its own words.
fn source_error(field: &str, source: &Source) -> Option<String> {
    // The git URL and ref are passed to git as positional arguments, so an
    // option-like value would be read as a flag rather than as what it names.
    if let Some(git) = &source.git
        && let Some(reason) = argument_error(git)
    {
        return Some(format!("{field}.git {git:?} {reason}"));
    }
    // A fetched artefact's URL reaches curl the same way, and is checked here
    // for the same reason. Both kinds are pinned by a digest, and a recipe that
    // names one with nothing to verify it against would build whatever the URL
    // happened to serve — the one thing a fetched source exists to rule out.
    // Refused at load, so a run does not fetch before discovering it cannot
    // trust what it fetched.
    for (setting, url) in [("tarball", &source.tarball), ("dsc", &source.dsc)] {
        let Some(url) = url else { continue };
        if let Some(reason) = argument_error(url) {
            return Some(format!("{field}.{setting} {url:?} {reason}"));
        }
        if source.sha256.is_none() {
            return Some(format!(
                "sets {field}.{setting} with no {field}.sha256 to verify it \
                 against; a fetched source is pinned by its digest"
            ));
        }
    }
    if let Some(sha256) = &source.sha256 {
        if let Some(reason) = sha256_error(sha256) {
            return Some(format!("{field}.sha256 {sha256:?} {reason}"));
        }
        // A digest identifies a fetched artefact, so it says nothing about a
        // revision or a directory. Refused rather than ignored, as `git-ref`
        // is: a recipe switched from an archive to a repository keeps its
        // digest, and passing over it would build something other than what it
        // reads as building.
        if source.tarball.is_none() && source.dsc.is_none() {
            return Some(format!(
                "sets {field}.sha256, which applies only to {field}.tarball and \
                 {field}.dsc"
            ));
        }
    }
    if let Some(git_ref) = &source.git_ref {
        if let Some(reason) = argument_error(git_ref) {
            return Some(format!("{field}.git-ref {git_ref:?} {reason}"));
        }
        // A ref selects a revision of a repository, so it says nothing about a
        // tree on disk. Refused rather than ignored: a recipe switched from git
        // to a path keeps its ref, and silently dropping it would build
        // something other than what it reads as building.
        if source.git.is_none() {
            return Some(format!(
                "sets {field}.git-ref, which applies only to {field}.git"
            ));
        }
    }
    // The path is joined onto the recipe's directory, so an empty one would
    // name the recipe directory itself rather than a tree.
    if let Some(path) = &source.path
        && path.as_os_str().is_empty()
    {
        return Some(format!("{field}.path is empty"));
    }
    // The subdir is joined onto the resolved tree. For a source that gives the
    // tree the vendor pass binds read-write into a cage running the component's
    // own `debian/rules clean` with the host network; an absolute subdir would
    // not extend that path but replace it, and a `..` would climb out of it, so
    // either would hand that pass a tree outside the work directory.
    if let Some(subdir) = &source.subdir
        && let Some(reason) = subdir_error(subdir)
    {
        return Some(format!("{field}.subdir {:?} {reason}", subdir.display()));
    }
    None
}

/// The origin settings a tree names, spelled as the recipe writes them —
/// `source.git`, `packaging.tarball`, and so on.
///
/// Exactly one makes a resolvable tree. Reported as a list rather than as a
/// count so an error quotes the recipe's own words back, and `field` names the
/// table they came from so one rule serves a component's source and its
/// packaging overlay alike.
fn named_origins(field: &str, source: &Source) -> Vec<String> {
    [
        source.git.is_some().then_some("git"),
        source.path.is_some().then_some("path"),
        source.tarball.is_some().then_some("tarball"),
        source.dsc.is_some().then_some("dsc"),
    ]
    .into_iter()
    .flatten()
    .map(|setting| format!("{field}.{setting}"))
    .collect()
}

/// Reports why a declared SHA-256 cannot identify an archive, or `None` when it
/// can.
///
/// A digest is 32 bytes written as 64 hexadecimal characters. Either case is
/// accepted, since the two spell the same value and a digest is copied from
/// wherever the release published it; src2deb records the lowercase form it
/// measures.
///
/// Anything else is a mistake worth catching at load: a truncated digest, a
/// digest of another algorithm, or the whole line of a `sha256sum` output with
/// the file name still attached would each fail every fetch it was compared
/// against, and would do so after the archive had been downloaded.
fn sha256_error(sha256: &str) -> Option<&'static str> {
    /// A SHA-256 is 32 bytes, and a byte is two hexadecimal characters.
    const DIGITS: usize = 64;

    if sha256.len() != DIGITS {
        Some("is not 64 characters, which a SHA-256 in hexadecimal is")
    } else if !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        Some("is not hexadecimal")
    } else {
        None
    }
}

/// Reports why a value is unsafe to pass to a subprocess as a positional
/// argument, or `None` when it is safe.
///
/// A git URL and ref reach `git clone` and `git checkout` as positional
/// arguments. A value beginning with `-` would be read as an option instead —
/// `git clone --upload-pack=... <dir>` runs a command of the recipe's choosing —
/// so an option-like value is refused rather than passed through. Recipes are
/// trusted input, so this is defense in depth rather than a security boundary.
fn argument_error(value: &str) -> Option<&'static str> {
    if value.is_empty() {
        Some("is empty")
    } else if value.starts_with('-') {
        Some("starts with '-', which git would read as an option")
    } else {
        None
    }
}

/// Reports why a component's `subdir` is unsafe to join onto its resolved
/// source, or `None` when it is safe.
///
/// The subdir names a tree *within* the resolved source — a git checkout, or the
/// work directory's copy of a path source — and the result is what the vendor
/// pass binds read-write into a cage that runs upstream's own `debian/rules
/// clean` with the host network. [`Path::join`] replaces the whole path when
/// given an absolute one rather than extending it, so an absolute subdir would
/// silently redirect that bind to anywhere on the host; a `..` component would
/// climb out of the source to the same effect. Both are refused, leaving a
/// subdir that can only descend.
fn subdir_error(subdir: &Path) -> Option<&'static str> {
    use std::path::Component;

    if subdir.as_os_str().is_empty() {
        return Some("is empty");
    }
    for component in subdir.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => return Some("is an absolute path"),
            Component::ParentDir => return Some("contains \"..\""),
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    None
}

/// The default archive components for a [`Repository`]: `main` alone.
fn default_components() -> Vec<String> {
    vec!["main".to_string()]
}

/// The default target architecture when a recipe does not set one: the host's,
/// so an unqualified recipe builds natively wherever it runs.
fn default_architecture() -> String {
    crate::arch::host_architecture()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a recipe from TOML and validates it, as [`Recipe::load`] does
    /// without the file read.
    fn load(toml: &str) -> Result<Recipe> {
        let recipe: Recipe = toml::from_str(toml).map_err(|err| Error::Recipe {
            path: PathBuf::from("<test>"),
            reason: err.to_string(),
        })?;
        recipe.validate(Path::new("<test>"))?;
        Ok(recipe)
    }

    const ONE_COMPONENT: &str =
        "\n[[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n";

    #[test]
    fn a_known_suite_resolves_its_version_tag_without_the_recipe_naming_one() {
        for (suite, tag) in [
            ("trixie", "deb13"),
            ("forky", "deb14"),
            // A qualified suite takes the tag of the release it qualifies.
            ("trixie-backports", "deb13"),
        ] {
            let recipe = load(&format!(
                "name = \"r\"\nsuite = \"{suite}\"\n{ONE_COMPONENT}"
            ))
            .unwrap_or_else(|err| panic!("{suite} should load: {err}"));
            assert_eq!(recipe.resolved_version_tag(), Some(tag));
        }
    }

    #[test]
    fn a_suite_with_no_known_tag_is_refused_unless_the_recipe_names_one() {
        // A rolling suite carries no release number, so src2deb has no tag that
        // would order against the numbered releases. Guessing is what the tag
        // exists to avoid, so the recipe is rejected rather than stamped wrong.
        let err = load(&format!("name = \"r\"\nsuite = \"sid\"\n{ONE_COMPONENT}"))
            .expect_err("sid has no known tag");
        assert!(
            err.to_string().contains("version-tag"),
            "the error should point at the fix: {err}"
        );

        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"sid\"\nversion-tag = \"deb99\"\n{ONE_COMPONENT}"
        ))
        .expect("an explicit tag makes the suite buildable");
        assert_eq!(recipe.resolved_version_tag(), Some("deb99"));
    }

    #[test]
    fn an_explicit_tag_overrides_the_one_the_suite_would_imply() {
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\nversion-tag = \"pop13\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert_eq!(recipe.resolved_version_tag(), Some("pop13"));
    }

    #[test]
    fn a_tag_may_use_every_character_a_debian_revision_allows() {
        // `~` among them: it is grammatically valid, and because the stamp joins
        // with `+`, a tag carrying one still sorts above the base version. It
        // only orders that tag below tags without one, which is a choice the
        // recipe made.
        for tag in ["deb13", "~bpo13", "deb13+1", "1.0"] {
            load(&format!(
                "name = \"r\"\nsuite = \"trixie\"\nversion-tag = \"{tag}\"\n{ONE_COMPONENT}"
            ))
            .unwrap_or_else(|err| panic!("{tag:?} should be accepted: {err}"));
        }
    }

    #[test]
    fn a_tag_outside_the_debian_version_grammar_is_rejected() {
        for (tag, needle) in [
            // A hyphen moves the Debian revision boundary, since it begins at
            // the version's *last* hyphen: `1.0.0-1` tagged `deb-13` splits as
            // upstream `1.0.0-1+deb`, revision `13.20260731.abc1234`.
            ("deb-13", "may not"),
            ("deb 13", "may not"),
            ("deb/13", "may not"),
            ("deb_13", "may not"),
            ("", "is empty"),
        ] {
            let err = load(&format!(
                "name = \"r\"\nsuite = \"trixie\"\nversion-tag = \"{tag}\"\n{ONE_COMPONENT}"
            ))
            .expect_err(&format!("{tag:?} should be rejected"));
            assert!(
                err.to_string().contains(needle),
                "{tag:?}: expected {needle:?} in {err}"
            );
        }
    }

    #[test]
    fn a_recipe_without_a_toolchain_block_uses_the_archive_rust() {
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert_eq!(recipe.toolchain.rust.provider, RustProvider::Debian);
        assert_eq!(recipe.toolchain.rust.rustup_version(), None);
        assert!(recipe.repositories.is_empty());
    }

    #[test]
    fn a_rustup_toolchain_parses_and_reports_its_version() {
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [toolchain.rust]\nprovider = \"rustup\"\nversion = \"1.95.0\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert_eq!(recipe.toolchain.rust.provider, RustProvider::Rustup);
        assert_eq!(recipe.toolchain.rust.rustup_version(), Some("1.95.0"));
    }

    #[test]
    fn a_rustup_toolchain_without_a_version_is_rejected() {
        let err = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [toolchain.rust]\nprovider = \"rustup\"\n{ONE_COMPONENT}"
        ))
        .unwrap_err();
        assert!(format!("{err}").contains("requires toolchain.rust.version"));
    }

    #[test]
    fn a_repository_defaults_components_to_main_and_carries_its_fields() {
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[repositories]]\nname = \"backports\"\nsuite = \"trixie-backports\"\n\
             keyring = \"/k.gpg\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        let repo = &recipe.repositories[0];
        assert_eq!(repo.name, "backports");
        assert_eq!(repo.suite.as_deref(), Some("trixie-backports"));
        assert_eq!(repo.components, ["main"]);
        assert!(!repo.trust_unsigned);
    }

    #[test]
    fn a_signed_repository_must_name_a_keyring_but_a_trusted_one_need_not() {
        // Signed (the default) with no keyring is rejected...
        let err = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[repositories]]\nname = \"backports\"\n{ONE_COMPONENT}"
        ))
        .unwrap_err();
        assert!(format!("{err}").contains("signed but names no keyring"));
        // ...but a trust-unsigned local archive may omit it.
        load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[repositories]]\nname = \"pool\"\ntrust-unsigned = true\n\
             mirror = \"file:///srv/pool\"\n{ONE_COMPONENT}"
        ))
        .expect("a trusted repository needs no keyring");
    }

    #[test]
    fn an_unsafe_component_name_is_rejected() {
        for (name, needle) in [
            ("../evil", "contains \"..\""),
            ("a/b", "contains a path separator"),
            ("", "is empty"),
            ("-rf", "starts with '-'"),
            (".", "is a directory reference"),
        ] {
            let err = load(&format!(
                "name = \"r\"\nsuite = \"trixie\"\n\
                 [[components]]\nname = \"{name}\"\nsource.git = \"https://example/c\"\n"
            ))
            .unwrap_err();
            let message = format!("{err}");
            assert!(message.contains(needle), "name {name:?} gave: {message}");
        }
    }

    #[test]
    fn a_plain_component_name_is_accepted() {
        load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"xdg-desktop-portal-cosmic\"\n\
             source.git = \"https://example/c\"\n{ONE_COMPONENT}"
        ))
        .expect("an ordinary hyphenated component name is safe");
    }

    #[test]
    fn an_unsafe_recipe_name_or_suite_is_rejected() {
        // Both are path segments of the run's manifest, and the suite is one in
        // the local pool and a field in a `sources.list` line besides.
        for (name, needle) in [
            ("../evil", "contains \"..\""),
            ("a/b", "contains a path separator"),
            ("", "is empty"),
            ("-rf", "starts with '-'"),
            ("two words", "contains whitespace"),
        ] {
            let err = load(&format!(
                "name = \"{name}\"\nsuite = \"trixie\"\n{ONE_COMPONENT}"
            ))
            .unwrap_err();
            let message = format!("{err}");
            assert!(
                message.contains("recipe name") && message.contains(needle),
                "recipe name {name:?} gave: {message}"
            );

            let err = load(&format!(
                "name = \"r\"\nsuite = \"{name}\"\n{ONE_COMPONENT}"
            ))
            .unwrap_err();
            let message = format!("{err}");
            assert!(
                message.contains("suite") && message.contains(needle),
                "suite {name:?} gave: {message}"
            );
        }
    }

    #[test]
    fn ordinary_recipe_names_and_suites_are_accepted() {
        for (name, suite) in [
            ("cosmic-epoch", "trixie"),
            ("adw-gtk3", "forky"),
            ("r", "trixie-backports"),
        ] {
            load(&format!(
                "name = \"{name}\"\nsuite = \"{suite}\"\n{ONE_COMPONENT}"
            ))
            .unwrap_or_else(|err| panic!("{name}/{suite} should be safe: {err}"));
        }
    }

    #[test]
    fn a_subdir_that_leaves_the_checkout_is_rejected() {
        // The joined result is what the vendor pass binds read-write into a cage
        // running upstream code with the host network, so a subdir that does not
        // stay inside the checkout never reaches it. An absolute one is the sharp
        // case: `Path::join` replaces the checkout rather than extending it.
        for (subdir, needle) in [
            ("/etc", "is an absolute path"),
            ("/", "is an absolute path"),
            ("../../elsewhere", "contains \"..\""),
            ("members/../../elsewhere", "contains \"..\""),
            ("", "is empty"),
        ] {
            let err = load(&format!(
                "name = \"r\"\nsuite = \"trixie\"\n\
                 [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
                 source.subdir = \"{subdir}\"\n"
            ))
            .unwrap_err();
            let message = format!("{err}");
            assert!(
                message.contains("source.subdir") && message.contains(needle),
                "subdir {subdir:?} gave: {message}"
            );
        }
    }

    #[test]
    fn a_subdir_that_descends_is_accepted() {
        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             source.subdir = \"members/cosmic-comp\"\n",
        )
        .expect("a subdir inside the checkout is safe");
        assert_eq!(
            recipe.components[0].source.subdir.as_deref(),
            Some(Path::new("members/cosmic-comp"))
        );
    }

    #[test]
    fn an_option_like_git_url_or_ref_is_rejected() {
        // Both reach git as positional arguments, where a leading '-' would be
        // read as a flag: `git clone --upload-pack=...` runs a chosen command.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"--upload-pack=touch /tmp/x\"\n",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("source.git"), "{err}");
        assert!(
            format!("{err}").contains("would read as an option"),
            "{err}"
        );

        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             source.git-ref = \"--output=/tmp/x\"\n",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("source.git-ref"), "{err}");
    }

    #[test]
    fn a_component_names_one_source_and_reports_which() {
        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             source.git-ref = \"master\"\n",
        )
        .unwrap();
        assert_eq!(
            recipe.components[0].source.origin(),
            Some(Origin::Git {
                url: "https://example/c",
                git_ref: Some("master"),
            }),
        );

        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.path = \"../cosmic-comp\"\n",
        )
        .unwrap();
        assert_eq!(
            recipe.components[0].source.origin(),
            Some(Origin::Path(Path::new("../cosmic-comp"))),
        );
    }

    #[test]
    fn a_component_naming_no_source_or_two_is_rejected() {
        // Two would leave which one it is built from to whichever the resolver
        // looked at first; none leaves nothing to build.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             source.path = \"/home/someone/c\"\n",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("declares source.git and source.path"),
            "{err}"
        );

        // The error quotes back exactly the settings that were written, however
        // many there are.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             source.path = \"/home/someone/c\"\n\
             source.tarball = \"https://example/c.tar.gz\"\n",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("declares source.git and source.path and source.tarball"),
            "{err}"
        );

        let err =
            load("name = \"r\"\nsuite = \"trixie\"\n[[components]]\nname = \"c\"\n").unwrap_err();
        assert!(format!("{err}").contains("declares no source"), "{err}");
        // ...and names every way to fix it, archives included.
        assert!(format!("{err}").contains("source.tarball"), "{err}");
    }

    /// A representative digest, as a release publishes one.
    const DIGEST: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0";

    #[test]
    fn an_archive_source_names_its_url_and_the_digest_it_is_pinned_by() {
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\n\
             source.tarball = \"https://example/c-1.2.3.tar.xz\"\n\
             source.sha256 = \"{DIGEST}\"\n"
        ))
        .expect("an archive with a digest is a source");
        assert_eq!(
            recipe.components[0].source.origin(),
            Some(Origin::Tarball {
                url: "https://example/c-1.2.3.tar.xz",
                sha256: DIGEST,
            }),
        );
    }

    #[test]
    fn an_archive_with_nothing_to_verify_it_against_is_rejected() {
        // The one thing an archive source exists to rule out is a tree that is
        // whatever the URL happened to serve. Caught at load, so a run does not
        // fetch before finding out it cannot trust what it fetched.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\n\
             source.tarball = \"https://example/c.tar.gz\"\n",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("no source.sha256 to verify it against"),
            "{err}"
        );
    }

    #[test]
    fn a_digest_on_a_source_that_is_not_an_archive_is_rejected_rather_than_ignored() {
        // The shape a recipe takes when it is switched from an archive to a
        // repository and the digest is left behind — the same failure a
        // leftover `git-ref` is, and refused for the same reason.
        let err = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             source.sha256 = \"{DIGEST}\"\n"
        ))
        .unwrap_err();
        assert!(
            format!("{err}").contains("applies only to source.tarball"),
            "{err}"
        );
    }

    #[test]
    fn a_digest_that_is_not_a_sha256_is_rejected() {
        for (sha256, needle) in [
            // Truncated, or a digest of another algorithm.
            ("9f8e7d6c", "64 characters"),
            ("d41d8cd98f00b204e9800998ecf8427e", "64 characters"),
            // A whole `sha256sum` line, file name and all.
            (
                "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0  c.tar.gz",
                "64 characters",
            ),
            (
                "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
                "not hexadecimal",
            ),
        ] {
            let err = load(&format!(
                "name = \"r\"\nsuite = \"trixie\"\n\
                 [[components]]\nname = \"c\"\n\
                 source.tarball = \"https://example/c.tar.gz\"\n\
                 source.sha256 = \"{sha256}\"\n"
            ))
            .unwrap_err();
            let message = format!("{err}");
            assert!(
                message.contains("source.sha256") && message.contains(needle),
                "{sha256:?} gave: {message}"
            );
        }

        // Either case spells the same value, and a digest is copied from
        // wherever the release published it.
        load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\n\
             source.tarball = \"https://example/c.tar.gz\"\n\
             source.sha256 = \"{}\"\n",
            DIGEST.to_uppercase()
        ))
        .expect("an uppercase digest is the same digest");
    }

    #[test]
    fn a_packaging_overlay_may_come_from_an_archive_too() {
        // `packaging` takes the same settings `source` does, which is a rule
        // worth keeping true as origins are added rather than one to except
        // the newest from.
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             packaging.tarball = \"https://deb.example/c.debian.tar.xz\"\n\
             packaging.sha256 = \"{DIGEST}\"\n"
        ))
        .expect("an archive is a packaging source");
        assert_eq!(
            recipe.components[0].packaging.as_ref().unwrap().origin(),
            Some(Origin::Tarball {
                url: "https://deb.example/c.debian.tar.xz",
                sha256: DIGEST,
            }),
        );

        // ...and it is held to the same rules, named against its own setting.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             packaging.tarball = \"https://deb.example/c.debian.tar.xz\"\n",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("no packaging.sha256 to verify it against"),
            "{err}"
        );
    }

    #[test]
    fn an_option_like_archive_url_is_rejected() {
        // It reaches curl as a positional argument, where a leading '-' would
        // be read as a flag.
        let err = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\n\
             source.tarball = \"--output=/tmp/x\"\nsource.sha256 = \"{DIGEST}\"\n"
        ))
        .unwrap_err();
        assert!(format!("{err}").contains("source.tarball"), "{err}");
        assert!(
            format!("{err}").contains("would read as an option"),
            "{err}"
        );
    }

    #[test]
    fn a_git_ref_on_a_path_source_is_rejected_rather_than_ignored() {
        // The shape a recipe takes on when it is switched from git to a path and
        // the ref is left behind. Ignoring it would build something other than
        // what the recipe reads as building.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.path = \"/home/someone/c\"\n\
             source.git-ref = \"master\"\n",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("applies only to source.git"),
            "{err}"
        );
    }

    #[test]
    fn an_empty_source_path_is_rejected() {
        // It would name the recipe's own directory rather than a source tree.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.path = \"\"\n",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("source.path is empty"), "{err}");
    }

    #[test]
    fn a_source_path_may_climb_out_of_the_recipe_directory() {
        // Unlike `subdir`, which names a tree inside the resolved source, a path
        // names one anywhere on the host: a recipe kept beside the trees it
        // builds reaches them with `..`.
        for path in ["../cosmic-comp", "/home/someone/cosmic-comp", "./tree"] {
            load(&format!(
                "name = \"r\"\nsuite = \"trixie\"\n\
                 [[components]]\nname = \"c\"\nsource.path = \"{path}\"\n"
            ))
            .unwrap_or_else(|err| panic!("{path:?} should be accepted: {err}"));
        }
    }

    #[test]
    fn a_subdir_applies_to_a_path_source_as_it_does_to_a_checkout() {
        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.path = \"../superproject\"\n\
             source.subdir = \"members/cosmic-comp\"\n",
        )
        .expect("a subdir inside a path source is safe");
        assert_eq!(
            recipe.components[0].source.subdir.as_deref(),
            Some(Path::new("members/cosmic-comp")),
        );

        // ...and it is bounded the same way, since it names a tree within the
        // copy the vendor pass binds.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.path = \"../superproject\"\n\
             source.subdir = \"../elsewhere\"\n",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("source.subdir"), "{err}");
    }

    #[test]
    fn a_component_carries_its_patch_series_in_the_order_declared() {
        // The order is the applying order, so it is part of what the recipe
        // says rather than a set the loader is free to rearrange.
        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             patches = [\"patches/0002-second.patch\", \"patches/0001-first.patch\"]\n",
        )
        .unwrap();
        assert_eq!(
            recipe.components[0].patches,
            [
                PathBuf::from("patches/0002-second.patch"),
                PathBuf::from("patches/0001-first.patch"),
            ],
        );
        // A component that declares none carries none, which is what makes the
        // whole series step a no-op for the ordinary case.
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert!(recipe.components[0].patches.is_empty());
    }

    #[test]
    fn a_packaging_overlay_names_its_own_origin_and_qualifiers() {
        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             packaging.git = \"https://salsa.example/debian/c\"\n\
             packaging.git-ref = \"debian/latest\"\n\
             packaging.subdir = \"packaging\"\n",
        )
        .expect("a packaging overlay beside a source is valid");
        let packaging = recipe.components[0]
            .packaging
            .as_ref()
            .expect("the overlay parsed");
        assert_eq!(
            packaging.origin(),
            Some(Origin::Git {
                url: "https://salsa.example/debian/c",
                git_ref: Some("debian/latest"),
            }),
        );
        assert_eq!(packaging.subdir.as_deref(), Some(Path::new("packaging")));

        // A tree on disk serves as well, and a component that declares no
        // overlay carries none — which is what keeps the whole step a no-op for
        // a source that ships its own packaging.
        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             packaging.path = \"packaging/c\"\n",
        )
        .unwrap();
        assert_eq!(
            recipe.components[0].packaging.as_ref().unwrap().origin(),
            Some(Origin::Path(Path::new("packaging/c"))),
        );
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert!(recipe.components[0].packaging.is_none());
    }

    #[test]
    fn a_packaging_overlay_naming_no_source_or_two_is_rejected() {
        // An overlay is optional, so a table that exists without naming an
        // origin is a half-written one rather than a missing setting.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             packaging.subdir = \"packaging\"\n",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("declares a packaging overlay with no source"),
            "{err}"
        );

        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             packaging.git = \"https://salsa.example/debian/c\"\n\
             packaging.path = \"packaging/c\"\n",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("declares packaging.git and packaging.path"),
            "{err}"
        );
    }

    #[test]
    fn a_packaging_overlay_is_held_to_the_rules_its_source_is() {
        // One set of checks serves both trees, and each message names the
        // setting the recipe actually wrote rather than the one it resembles.
        for (setting, needle) in [
            (
                "packaging.git = \"--upload-pack=touch /tmp/x\"",
                "packaging.git",
            ),
            (
                "packaging.git = \"https://e/c\"\npackaging.git-ref = \"--output=/tmp/x\"",
                "packaging.git-ref",
            ),
            (
                "packaging.path = \"p\"\npackaging.git-ref = \"debian/latest\"",
                "applies only to packaging.git",
            ),
            ("packaging.path = \"\"", "packaging.path is empty"),
            (
                "packaging.path = \"p\"\npackaging.subdir = \"../elsewhere\"",
                "packaging.subdir",
            ),
        ] {
            let err = load(&format!(
                "name = \"r\"\nsuite = \"trixie\"\n\
                 [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n{setting}\n"
            ))
            .expect_err(&format!("{setting} should be rejected"));
            let message = format!("{err}");
            assert!(
                message.contains("component \"c\"") && message.contains(needle),
                "{setting} gave: {message}"
            );
        }
    }

    #[test]
    fn a_component_states_its_version_or_derives_it_and_reports_which() {
        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             version = \"1.2.3\"\n",
        )
        .unwrap();
        assert_eq!(
            recipe.components[0].version_source(),
            Some(VersionSource::Declared("1.2.3")),
        );

        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             version-from = \"git-describe\"\n",
        )
        .unwrap();
        assert_eq!(
            recipe.components[0].version_source(),
            Some(VersionSource::Derived(VersionFrom::GitDescribe)),
        );

        // The ordinary case: neither, and the component's own changelog is the
        // authority as it always has been.
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert!(recipe.components[0].version_source().is_none());
    }

    #[test]
    fn a_component_stating_a_version_and_deriving_one_is_rejected() {
        // Which won would be whichever the resolver consulted first.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             version = \"1.2.3\"\nversion-from = \"git-describe\"\n",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("declares both version and version-from"),
            "{err}"
        );
    }

    #[test]
    fn a_declared_version_a_package_could_not_carry_is_rejected_by_the_recipe() {
        // Checked at load, not at build: a run is better refused before it
        // resolves a single source tree.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             version = \"v1.2.3\"\n",
        )
        .unwrap_err();
        let message = format!("{err}");
        assert!(
            message.contains("component \"c\" version \"v1.2.3\""),
            "{message}"
        );
        assert!(message.contains("begin with a digit"), "{message}");
    }

    #[test]
    fn an_unknown_version_derivation_is_rejected_rather_than_ignored() {
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             version-from = \"git-tag\"\n",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("git-describe"), "{err}");
    }

    #[test]
    fn a_maintainer_is_taken_at_the_recipe_or_the_component() {
        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             maintainer = \"Recipe Owner <owner@example.invalid>\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             maintainer = \"Component Owner <c@example.invalid>\"\n",
        )
        .unwrap();
        assert_eq!(
            recipe.maintainer.as_deref(),
            Some("Recipe Owner <owner@example.invalid>"),
        );
        assert_eq!(
            recipe.components[0].maintainer.as_deref(),
            Some("Component Owner <c@example.invalid>"),
        );
        // A recipe that names none leaves the component's `debian/control` to
        // answer, which is the common case.
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert!(recipe.maintainer.is_none());
        assert!(recipe.components[0].maintainer.is_none());
    }

    #[test]
    fn an_identity_that_could_not_sign_a_changelog_trailer_is_rejected() {
        // Every rule follows from the one line the value is written into:
        // ` -- Name <email>  Date`.
        for (value, needle) in [
            ("", "is empty"),
            ("Someone", "Name <email>"),
            ("<someone@example.invalid>", "Name <email>"),
            ("Someone <someone@example.invalid> ", "whitespace"),
            // Two spaces are what separate the identity from the date, so an
            // identity carrying a pair reads back truncated.
            ("Some  One <s@example.invalid>", "two consecutive spaces"),
            ("Some\nOne <s@example.invalid>", "line break"),
        ] {
            assert!(
                maintainer_error(value).is_some_and(|reason| reason.contains(needle)),
                "{value:?} should be refused for {needle:?}, got {:?}",
                maintainer_error(value),
            );
            // ...and the recipe refuses it wherever it is declared: at the
            // recipe, and at a component overriding it.
            for setting in [
                format!("maintainer = {value:?}\n{ONE_COMPONENT}"),
                format!("{ONE_COMPONENT}maintainer = {value:?}\n"),
            ] {
                let err = load(&format!("name = \"r\"\nsuite = \"trixie\"\n{setting}"))
                    .expect_err(&format!("{setting:?} should be refused"));
                assert!(format!("{err}").contains(needle), "{setting:?} gave: {err}");
            }
        }
        // An ordinary identity is accepted.
        assert_eq!(maintainer_error("Someone <someone@example.invalid>"), None);
    }

    #[test]
    fn an_empty_patch_path_is_rejected() {
        // Joined onto the recipe's directory, it would name that directory
        // rather than a file.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             patches = [\"fix.patch\", \"\"]\n",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("empty patch path"), "{err}");
    }

    #[test]
    fn patches_apply_to_either_kind_of_source() {
        for source in [
            "source.git = \"https://example/c\"",
            "source.path = \"../tree\"",
        ] {
            load(&format!(
                "name = \"r\"\nsuite = \"trixie\"\n\
                 [[components]]\nname = \"c\"\n{source}\n\
                 patches = [\"patches/fix.patch\"]\n"
            ))
            .unwrap_or_else(|err| panic!("{source} with patches should load: {err}"));
        }
    }

    #[test]
    fn a_recipe_loaded_from_disk_records_the_directory_it_came_from() {
        // What a relative `source.path` resolves against, taken from where the
        // file was found rather than from anything the file says.
        let dir = std::env::temp_dir().join(format!("src2deb-recipe-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("recipe.toml"),
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.path = \"../tree\"\n",
        )
        .unwrap();

        let recipe = Recipe::load(&dir).expect("the recipe loads");
        assert_eq!(recipe.dir(), dir);
        // A recipe that was never loaded carries none, and says so.
        let bare: Recipe = toml::from_str(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert_eq!(bare.dir(), Path::new(""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recipe_may_not_declare_its_own_directory() {
        // The field is set from where the file was found, so a file naming one
        // is refused rather than believed.
        let err = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\ndir = \"/elsewhere\"\n{ONE_COMPONENT}"
        ))
        .unwrap_err();
        assert!(format!("{err}").contains("unknown field"), "{err}");
    }

    #[test]
    fn ordinary_git_urls_and_refs_are_accepted() {
        load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\n\
             source.git = \"https://github.com/pop-os/cosmic-comp\"\n\
             source.git-ref = \"4d370cdf92b5d96d78032593947f4ad9eae793bf\"\n",
        )
        .expect("an ordinary URL and commit are safe");
        // A ref may be a branch or a tag, including one with punctuation.
        load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"git@example:c.git\"\n\
             source.git-ref = \"release/1.0\"\n",
        )
        .expect("an ssh remote and a slashed branch are safe");
    }

    #[test]
    fn extra_build_deps_is_a_kebab_case_key() {
        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             extra-build-deps = [\"libfoo-dev\"]\n",
        )
        .unwrap();
        assert_eq!(recipe.components[0].extra_build_deps, ["libfoo-dev"]);

        // The snake_case spelling a user might reach for is refused outright by
        // deny_unknown_fields, rather than silently ignored.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"c\"\nsource.git = \"https://example/c\"\n\
             extra_build_deps = [\"libfoo-dev\"]\n",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("unknown field"));
    }

    #[test]
    fn an_omitted_architecture_defaults_to_the_host() {
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert_eq!(recipe.architecture, crate::arch::host_architecture());
    }

    #[test]
    fn every_run_owns_its_own_arch_indep_output_unless_an_owner_is_named() {
        // The default a single-architecture build wants: its pool holds every
        // package the recipe declares and can be served as it stands.
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\narchitecture = \"arm64\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert_eq!(recipe.resolved_arch_indep_owner(), "arm64");
        assert!(recipe.owns_arch_indep());

        // Named elsewhere, this architecture builds only its own packages.
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\narchitecture = \"arm64\"\n\
             arch-indep-owner = \"amd64\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert_eq!(recipe.resolved_arch_indep_owner(), "amd64");
        assert!(!recipe.owns_arch_indep());

        // Naming this architecture as the owner is the same as naming none.
        let recipe = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\narchitecture = \"amd64\"\n\
             arch-indep-owner = \"amd64\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert!(recipe.owns_arch_indep());
    }

    #[test]
    fn a_version_stamp_falls_back_from_the_component_to_the_recipe_to_the_default() {
        // The precedence `maintainer` follows: a recipe of rebuilds says it
        // once, a mixed recipe says it per component, and a recipe that says
        // nothing supersedes — which is what a build of software the archive
        // does not carry wants.
        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\nversion-stamp = \"backport\"\n\
             [[components]]\nname = \"inherits\"\nsource.git = \"https://e.invalid/a\"\n\
             [[components]]\nname = \"overrides\"\nsource.git = \"https://e.invalid/b\"\n\
             version-stamp = \"supersede\"\n",
        )
        .unwrap();
        assert_eq!(
            recipe.resolved_version_stamp(&recipe.components[0]),
            VersionStamp::Backport,
        );
        assert_eq!(
            recipe.resolved_version_stamp(&recipe.components[1]),
            VersionStamp::Supersede,
        );

        // With nothing declared anywhere, and with only the component naming it.
        let plain = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n{ONE_COMPONENT}"
        ))
        .unwrap();
        assert_eq!(
            plain.resolved_version_stamp(&plain.components[0]),
            VersionStamp::Supersede,
        );
        let only_component = load(
            "name = \"r\"\nsuite = \"trixie\"\n[[components]]\nname = \"c\"\n\
             source.git = \"https://e.invalid/a\"\nversion-stamp = \"backport\"\n",
        )
        .unwrap();
        assert_eq!(
            only_component.resolved_version_stamp(&only_component.components[0]),
            VersionStamp::Backport,
        );
    }

    #[test]
    fn a_source_package_is_pinned_by_the_digest_of_its_dsc() {
        // `source.dsc` takes the same digest `source.tarball` does, and is
        // refused without one for the same reason: there would be nothing to
        // verify what was fetched against.
        let recipe = load(
            "name = \"r\"\nsuite = \"trixie\"\n[[components]]\nname = \"c\"\n\
             source.dsc = \"https://e.invalid/c_1.0-1.dsc\"\n\
             source.sha256 = \"5f2e1a9c3b8d4e7a2f9016c5b3d8e4a71f0c9d2b6e5a8347c1b0f9e2d6a4c8b1\"\n",
        )
        .unwrap();
        assert!(matches!(
            recipe.components[0].source.origin(),
            Some(Origin::Dsc { .. })
        ));

        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n[[components]]\nname = \"c\"\n\
             source.dsc = \"https://e.invalid/c_1.0-1.dsc\"\n",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("source.dsc with no source.sha256"),
            "{err}"
        );

        // And it is one origin among four, not a qualifier on another.
        let err = load(
            "name = \"r\"\nsuite = \"trixie\"\n[[components]]\nname = \"c\"\n\
             source.git = \"https://e.invalid/c\"\n\
             source.dsc = \"https://e.invalid/c_1.0-1.dsc\"\n",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("source.git and source.dsc"),
            "{err}"
        );
    }

    #[test]
    fn an_unsafe_arch_indep_owner_is_rejected() {
        // It is compared against an architecture name, so a value that could
        // never be one would hand arch-indep output to nothing.
        let err = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\narch-indep-owner = \"../evil\"\n{ONE_COMPONENT}"
        ))
        .unwrap_err();
        assert!(
            format!("{err}").contains("arch-indep-owner \"../evil\" contains \"..\""),
            "{err}"
        );
    }

    #[test]
    fn an_unsafe_architecture_is_rejected() {
        let err = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\narchitecture = \"../evil\"\n{ONE_COMPONENT}"
        ))
        .unwrap_err();
        assert!(format!("{err}").contains("architecture \"../evil\" contains \"..\""));
    }

    #[test]
    fn duplicate_repository_names_are_rejected() {
        let err = load(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[repositories]]\nname = \"dup\"\ntrust-unsigned = true\n\
             [[repositories]]\nname = \"dup\"\ntrust-unsigned = true\n{ONE_COMPONENT}"
        ))
        .unwrap_err();
        assert!(format!("{err}").contains("duplicate repository name"));
    }
}
