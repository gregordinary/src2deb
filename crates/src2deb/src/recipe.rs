//! A build recipe: the components to build, and where their source comes from.
//!
//! A recipe is a TOML file, `recipe.toml`, in a recipe directory. It names a
//! suite and architecture and lists components, each with a source: a git
//! repository to clone, or a tree already on disk. See `recipes/cosmic-epoch/`
//! for a worked example.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

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
#[derive(Debug, Clone, Deserialize)]
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
    /// Extra build-dependency package names beyond those `debian/control`
    /// declares, given in the recipe as `extra-build-deps`. Rarely needed; most
    /// build-deps are discovered from control.
    #[serde(default)]
    pub extra_build_deps: Vec<String>,
}

/// Where a component's source comes from.
///
/// A component names exactly one origin — [`git`](Self::git) or
/// [`path`](Self::path) — which [`Recipe::load`] enforces, so
/// [`origin`](Self::origin) answers for a validated recipe. The remaining
/// fields qualify whichever was named.
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
    #[serde(default)]
    pub subdir: Option<PathBuf>,
}

/// Where a component's source comes from, as a single choice rather than a set
/// of fields that could contradict each other.
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
}

impl Source {
    /// Where this source comes from, or `None` when it names neither a git
    /// repository nor a path.
    ///
    /// Never `None` for a validated recipe: [`Recipe::load`] refuses a
    /// component that names no origin, and refuses one that names both, so
    /// exactly one arm applies. A caller that builds a [`Source`] itself is
    /// outside that guarantee, which is why this reports rather than asserts.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match (&self.git, &self.path) {
            (Some(url), None) => Some(Origin::Git {
                url,
                git_ref: self.git_ref.as_deref(),
            }),
            (None, Some(path)) => Some(Origin::Path(path)),
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
    /// repository name; a component naming no source or two, whose git URL or
    /// ref would be read as an option, whose `git-ref` qualifies a source that
    /// is not a repository, or whose `subdir` leaves the source; a rustup
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
            // A component has one source. Naming both would leave which one it
            // is built from to whichever the resolver happened to look at
            // first, and naming neither leaves nothing to build.
            match (&component.source.git, &component.source.path) {
                (Some(_), Some(_)) => {
                    return Err(bad(format!(
                        "component {:?} declares both source.git and source.path; \
                         a component is built from one source",
                        component.name
                    )));
                }
                (None, None) => {
                    return Err(bad(format!(
                        "component {:?} declares no source; set source.git to clone \
                         a repository, or source.path to build a tree on disk",
                        component.name
                    )));
                }
                _ => {}
            }
            // The git URL and ref are passed to git as positional arguments, so
            // an option-like value would be read as a flag rather than as what
            // it names.
            if let Some(git) = &component.source.git
                && let Some(reason) = argument_error(git)
            {
                return Err(bad(format!(
                    "component {:?} source.git {:?} {reason}",
                    component.name, git
                )));
            }
            if let Some(git_ref) = &component.source.git_ref {
                if let Some(reason) = argument_error(git_ref) {
                    return Err(bad(format!(
                        "component {:?} source.git-ref {:?} {reason}",
                        component.name, git_ref
                    )));
                }
                // A ref selects a revision of a repository, so it says nothing
                // about a tree on disk. Refused rather than ignored: a recipe
                // switched from git to a path keeps its ref, and silently
                // dropping it would build something other than what it reads as
                // building.
                if component.source.git.is_none() {
                    return Err(bad(format!(
                        "component {:?} sets source.git-ref, which applies only to \
                         source.git",
                        component.name
                    )));
                }
            }
            // The path is joined onto the recipe's directory and then copied
            // into the work directory, so an empty one would name the recipe
            // directory itself rather than a source tree.
            if let Some(path) = &component.source.path
                && path.as_os_str().is_empty()
            {
                return Err(bad(format!(
                    "component {:?} source.path is empty",
                    component.name
                )));
            }
            // A patch path is joined onto the recipe's directory the same way,
            // where an empty one would name that directory rather than a file.
            if component.patches.iter().any(|p| p.as_os_str().is_empty()) {
                return Err(bad(format!(
                    "component {:?} lists an empty patch path",
                    component.name
                )));
            }
            // The subdir is joined onto the resolved source to give the tree the
            // vendor pass binds read-write into a cage that runs the component's
            // own `debian/rules clean` with the host network. An absolute subdir
            // would not extend that path but replace it, and a `..` would climb
            // out of it, so either would hand that pass a tree outside the work
            // directory.
            if let Some(subdir) = &component.source.subdir
                && let Some(reason) = subdir_error(subdir)
            {
                return Err(bad(format!(
                    "component {:?} source.subdir {:?} {reason}",
                    component.name,
                    subdir.display()
                )));
            }
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
            format!("{err}").contains("declares both source.git and source.path"),
            "{err}"
        );

        let err =
            load("name = \"r\"\nsuite = \"trixie\"\n[[components]]\nname = \"c\"\n").unwrap_err();
        assert!(format!("{err}").contains("declares no source"), "{err}");
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
