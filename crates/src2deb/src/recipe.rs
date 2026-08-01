//! A build recipe: the components to build, and where their source comes from.
//!
//! A recipe is a TOML file, `recipe.toml`, in a recipe directory. It names a
//! suite and architecture and lists components, each with a git source. See
//! `recipes/cosmic-epoch/` for a worked example.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

/// A build recipe.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Recipe {
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
    pub source: Source,
    /// Extra build-dependency package names beyond those `debian/control`
    /// declares, given in the recipe as `extra-build-deps`. Rarely needed; most
    /// build-deps are discovered from control.
    #[serde(default)]
    pub extra_build_deps: Vec<String>,
}

/// A component's git source.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Source {
    /// The git repository URL to clone.
    pub git: String,
    /// The branch, tag, or commit to check out. Defaults to the remote's
    /// default branch when unset.
    #[serde(default)]
    pub git_ref: Option<String>,
    /// A subdirectory within the checkout that holds the `debian/` tree, for a
    /// component that lives inside a larger superproject or monorepo. The whole
    /// checkout is the source tree when unset.
    #[serde(default)]
    pub subdir: Option<PathBuf>,
}

impl Recipe {
    /// Loads the recipe from `recipe.toml` in `dir`.
    pub fn load(dir: impl AsRef<Path>) -> Result<Recipe> {
        let path = dir.as_ref().join("recipe.toml");
        let text = std::fs::read_to_string(&path).map_err(|err| Error::Recipe {
            path: path.clone(),
            reason: err.to_string(),
        })?;
        let recipe: Recipe = toml::from_str(&text).map_err(|err| Error::Recipe {
            path: path.clone(),
            reason: err.to_string(),
        })?;
        recipe.validate(&path)?;
        Ok(recipe)
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

    /// Rejects a structurally invalid recipe: no components; an unsafe recipe
    /// name, suite, architecture, or component name; a duplicate component or
    /// repository name; a component whose git URL or ref would be read as an
    /// option, or whose `subdir` leaves the checkout; a rustup toolchain missing
    /// its version; or a signed repository missing its keyring.
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
            // The git URL and ref are passed to git as positional arguments, so
            // an option-like value would be read as a flag rather than as what
            // it names.
            if let Some(reason) = argument_error(&component.source.git) {
                return Err(bad(format!(
                    "component {:?} source.git {:?} {reason}",
                    component.name, component.source.git
                )));
            }
            if let Some(git_ref) = &component.source.git_ref
                && let Some(reason) = argument_error(git_ref)
            {
                return Err(bad(format!(
                    "component {:?} source.git-ref {:?} {reason}",
                    component.name, git_ref
                )));
            }
            // The subdir is joined onto the checkout to give the source tree,
            // which the vendor pass binds read-write into a cage that runs the
            // component's own `debian/rules clean` with the host network. An
            // absolute subdir would not extend the checkout path but replace it,
            // and a `..` would climb out of it, so either would hand that pass a
            // tree outside the work directory.
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

/// Reports why a component's `subdir` is unsafe to join onto its checkout, or
/// `None` when it is safe.
///
/// The subdir names a tree *within* the checkout, and the result is what the
/// vendor pass binds read-write into a cage that runs upstream's own
/// `debian/rules clean` with the host network. [`Path::join`] replaces the whole
/// path when given an absolute one rather than extending it, so an absolute
/// subdir would silently redirect that bind to anywhere on the host; a `..`
/// component would climb out of the checkout to the same effect. Both are
/// refused, leaving a subdir that can only descend.
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
