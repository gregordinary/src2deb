//! Resolving a component's source into an unpacked tree with a `debian/`
//! directory.
//!
//! src2deb owns source acquisition; ferroday-cage takes an already-unpacked
//! tree. Whichever [`Origin`] a component names, the resolver puts the tree
//! under the work directory and returns the path to the part of it that holds
//! `debian/`.
//!
//! For a **git** source it clones (or updates) the repository under the work
//! directory, checks out the requested ref, initializes submodules — so a
//! submodule superproject such as cosmic-epoch resolves its members — and
//! materializes Git LFS content.
//!
//! For a **path** source it copies the tree on disk into the work directory and
//! builds from the copy. See [Path sources](#path-sources).
//!
//! # Path sources
//!
//! A path source is never built where it lies. The vendor pass binds the source
//! tree read-write and runs the component's own `debian/rules clean` in it,
//! which leaves a `vendor.tar` and a `vendor/` behind and deletes whatever that
//! target is written to delete. For a git source that tree is a checkout src2deb
//! made and owns; for a path source it is someone's working directory, and doing
//! either to it would be a surprise. So the tree is copied under the work
//! directory first, exactly as it stands, and the copy is what the passes see.
//!
//! The copy is made afresh each run, which is what a git source gets from
//! `git checkout --force`: a file deleted upstream really disappears, and no
//! state survives from the run before. It costs a full traversal of the tree
//! every run, so a path source pointed at a directory holding a large build
//! output pays for that output on every build.
//!
//! A path names where a tree was read from and nothing about what it held, so a
//! build from one is recorded as unpinned. See [`crate::fingerprint`].
//!
//! # Git LFS
//!
//! A repository may keep large assets outside itself with Git LFS, leaving a
//! short text pointer in the tree. A checkout made without LFS support writes
//! those pointers verbatim, and they are ordinary valid files: the build embeds
//! or installs one without complaint and the package is produced successfully.
//! The substitution surfaces only when the installed program reads the asset and
//! finds a stub where its data should be.
//!
//! The resolver therefore treats a pointer left in the built tree as a source
//! resolution failure rather than something to pass along. It fetches the real
//! content, then verifies none remains, so a build either has the content it was
//! written against or does not run.
//!
//! The scan covers the files git tracks, which is where a pointer can come from
//! and nowhere else. That keeps it off everything a previous build left in the
//! tree — vendored crates above all, which are numerous and are not the
//! component's assets.
//!
//! A path source is scanned but never fetched for. The tree belongs to whoever
//! pointed src2deb at it, so a pointer found there fails the component with the
//! command that fixes it rather than being pulled behind their back.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result, io_error};
use crate::fingerprint::{Fingerprint, SourceInput};
use crate::recipe::{Component, Origin};

/// A resolved component source: the tree that holds `debian/`, and what the
/// tree was resolved from.
///
/// The fingerprint of a git source is the resolved `HEAD` after checkout, so it
/// names a concrete revision even when the recipe tracked a branch or the
/// remote's default; the fingerprint of a path source is the path it was read
/// from, which pins nothing. It anchors the run's provenance manifest, the
/// version stamp, and the `--skip-published` comparison.
#[derive(Debug, Clone)]
pub struct ResolvedSource {
    /// The path to the tree that holds the `debian/` directory.
    pub tree: PathBuf,
    /// What the tree was resolved from.
    pub source: Fingerprint,
}

/// Puts component sources under a work directory: cloning and checking out a
/// git source, and copying a path source.
pub struct SourceResolver {
    sources_dir: PathBuf,
    recipe_dir: PathBuf,
}

impl SourceResolver {
    /// Creates a resolver that places sources under `sources_dir` and resolves a
    /// relative [`Source::path`](crate::Source::path) against `recipe_dir`.
    pub fn new(sources_dir: impl Into<PathBuf>, recipe_dir: impl Into<PathBuf>) -> SourceResolver {
        SourceResolver {
            sources_dir: sources_dir.into(),
            recipe_dir: recipe_dir.into(),
        }
    }

    /// Resolves `component`'s source, returning the tree holding its `debian/`
    /// directory and the fingerprint of what it was resolved from.
    ///
    /// The returned tree is the resolved source, or its `subdir` when the recipe
    /// sets one. Which work the resolve does follows the component's [`Origin`]:
    /// a git source is cloned or fetched and checked out, landing on the fetched
    /// remote state and fingerprinted by the resolved `HEAD`; a path source is
    /// copied under the work directory and fingerprinted, unpinned, by the path
    /// it was read from. See [Path sources](self#path-sources).
    ///
    /// A component that names no origin — or two — cannot be resolved. A
    /// validated recipe has neither ([`Recipe::load`](crate::Recipe::load)
    /// refuses both), so this is reported rather than assumed away.
    pub fn resolve(&self, component: &Component) -> Result<ResolvedSource> {
        match component.source.origin() {
            Some(Origin::Git { url, git_ref }) => self.resolve_git(component, url, git_ref),
            Some(Origin::Path(path)) => self.resolve_path(component, path),
            None => Err(Error::Source {
                component: component.name.clone(),
                reason: "the component names no single source; set source.git to \
                         clone a repository, or source.path to build a tree on disk"
                    .to_string(),
            }),
        }
    }

    /// Resolves a git source: clones the repository on first use and fetches on
    /// later use, checks out `git_ref`, and initializes submodules.
    ///
    /// The fingerprint names the resolved `HEAD` for the run's provenance, so it
    /// is a concrete revision even when the recipe tracked a branch or the
    /// remote's default.
    ///
    /// A re-run always lands on the fetched remote state: a branch ref advances
    /// to its upstream tip, a tag or commit resolves to itself, and an unset ref
    /// tracks the remote's default branch. See `resolve_target`.
    fn resolve_git(
        &self,
        component: &Component,
        url: &str,
        git_ref: Option<&str>,
    ) -> Result<ResolvedSource> {
        std::fs::create_dir_all(&self.sources_dir).map_err(|err| self.fail(component, err))?;
        let checkout = self.sources_dir.join(&component.name);

        if checkout.join(".git").is_dir() {
            // Fetch updates the remote-tracking refs (`origin/*`) and tags, but
            // never the local branches; the checkout below targets that fetched
            // remote state so a re-run picks up upstream. `--force` lets a moved
            // tag update rather than being rejected.
            self.git(
                component,
                &checkout,
                &["fetch", "--tags", "--prune", "--force"],
            )?;
        } else {
            self.git(
                component,
                &self.sources_dir,
                &["clone", "--recurse-submodules", url, &component.name],
            )?;
        }

        // Check out the fetched target detached, so we never pin to a stale
        // local branch that `fetch` did not move.
        let target = self.resolve_target(component, &checkout, git_ref)?;
        self.git(
            component,
            &checkout,
            &["checkout", "--force", "--detach", &target],
        )?;
        // Re-sync submodules to the checked-out commit, forcing a checkout so a
        // submodule that moved between refs is updated rather than left behind.
        self.git(
            component,
            &checkout,
            &["submodule", "update", "--init", "--recursive", "--force"],
        )?;

        let tree = source_tree(&checkout, component.source.subdir.as_deref());
        refuse_without_control(component, &tree)?;
        // A checkout persists between runs, so anything a prior run's series
        // created is still there — and would refuse to be created again, or
        // stay behind a patch the recipe has since dropped.
        self.discard_prior_patch_output(component, &checkout)?;
        self.materialize_lfs(component, &checkout, &tree)?;
        let patches = self.apply_patches(component, &tree)?;
        let commit = self.head_commit(component, &checkout)?;
        Ok(ResolvedSource {
            tree,
            source: Fingerprint::over(
                std::iter::once(SourceInput::git(commit))
                    .chain(patches)
                    .collect(),
            ),
        })
    }

    /// Resolves a path source: copies the tree at `declared` under the work
    /// directory and builds from the copy.
    ///
    /// `declared` is taken relative to the recipe's own directory, so a recipe
    /// kept beside the trees it builds moves with them; an absolute path is used
    /// as it stands. The fingerprint is the canonical path the tree was read
    /// from, which is unpinned — see [`crate::fingerprint`].
    ///
    /// The copy is what makes this safe to run against a working tree, and the
    /// order of the work here is what makes it cheap to get wrong: the tree is
    /// checked for a `debian/control` and scanned for Git LFS pointers *before*
    /// anything is copied, so a misdirected path fails at once rather than after
    /// a full traversal of whatever it pointed at.
    fn resolve_path(&self, component: &Component, declared: &Path) -> Result<ResolvedSource> {
        // `join` takes an absolute `declared` whole, so this is the relative
        // case and the absolute one at once.
        let joined = self.recipe_dir.join(declared);
        // Canonicalized so the record names one path however the recipe reached
        // it, and so the overlap check below compares two paths that are both
        // absolute and free of symlinks.
        let origin = joined.canonicalize().map_err(|err| Error::Source {
            component: component.name.clone(),
            reason: format!("source.path {} cannot be read: {err}", joined.display()),
        })?;
        if !origin.is_dir() {
            return Err(Error::Source {
                component: component.name.clone(),
                reason: format!("source.path {} is not a directory", origin.display()),
            });
        }

        let subdir = component.source.subdir.as_deref();
        let tree_origin = source_tree(&origin, subdir);
        refuse_without_control(component, &tree_origin)?;
        self.refuse_lfs_pointers(component, &tree_origin)?;

        std::fs::create_dir_all(&self.sources_dir).map_err(|err| self.fail(component, err))?;
        // The destination a git source would get, so every component's tree sits
        // at the same place under the work directory however it was resolved.
        let checkout = self.sources_dir.join(&component.name);
        // The overlap check needs two paths it can compare, and the destination
        // does not exist yet — so it is canonicalized through the directory that
        // holds it, which does. The component name is a single benign path
        // segment, checked when the recipe loaded.
        let canonical_checkout = self
            .sources_dir
            .canonicalize()
            .map_err(|err| self.fail(component, err))?
            .join(&component.name);
        refuse_overlap(component, &origin, &canonical_checkout)?;

        // Afresh, so the copy is the tree as it now stands rather than that tree
        // over the leavings of the run before — the same guarantee `git checkout
        // --force` gives a git source, which is also what stops a prior run's
        // `vendor.tar` from being handed to the next one.
        if checkout.exists() {
            std::fs::remove_dir_all(&checkout)
                .map_err(|err| io_error("clearing", &checkout, err))?;
        }
        copy_tree(&origin, &checkout)?;

        // The copy is fresh, so the series applies over the tree as it stands
        // with nothing to discard first.
        let tree = source_tree(&checkout, subdir);
        let patches = self.apply_patches(component, &tree)?;
        Ok(ResolvedSource {
            tree,
            source: Fingerprint::over(
                std::iter::once(SourceInput::path(origin.to_string_lossy()))
                    .chain(patches)
                    .collect(),
            ),
        })
    }

    /// Applies `component`'s patch series to `tree`, and returns the input it
    /// contributes to the component's fingerprint — `None` when the component
    /// declares no patches.
    ///
    /// The series is applied with `git apply`, one patch at a time and in the
    /// order the recipe lists them, so a failure names the patch that caused it
    /// rather than the set. `git apply` needs no repository, so it serves a
    /// checkout and a copied tree alike, and it refuses a patch reaching outside
    /// the tree without being asked to.
    ///
    /// A patch that does not apply fails the component. There is no fuzzing,
    /// no three-way merge, and no `.rej` file left behind: a patch that no
    /// longer matches its upstream is a patch that needs looking at, and a
    /// partly-patched tree is not something to build a package from.
    fn apply_patches(&self, component: &Component, tree: &Path) -> Result<Option<SourceInput>> {
        if component.patches.is_empty() {
            return Ok(None);
        }
        let series = self.read_patches(component)?;
        for (path, contents) in &series {
            self.apply_patch(component, tree, path, contents)?;
        }
        Ok(Some(SourceInput::patches(series_digest(&series))))
    }

    /// Reads `component`'s patch series, in the order the recipe lists it.
    ///
    /// Each path is taken relative to the recipe's own directory, as
    /// [`Source::path`](crate::Source::path) is. Read once and carried, since
    /// the contents both identify the series and are what gets applied.
    fn read_patches(&self, component: &Component) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        component
            .patches
            .iter()
            .map(|declared| {
                let path = self.recipe_dir.join(declared);
                let contents = std::fs::read(&path).map_err(|err| Error::Source {
                    component: component.name.clone(),
                    reason: format!("patch {} cannot be read: {err}", path.display()),
                })?;
                Ok((path, contents))
            })
            .collect()
    }

    /// Applies one patch to `tree`, failing the component with git's own
    /// account of what went wrong.
    ///
    /// A patch git declined to apply at all fails the component too, even though
    /// git exits successfully for it. See [`SKIPPED`].
    fn apply_patch(
        &self,
        component: &Component,
        tree: &Path,
        path: &Path,
        contents: &[u8],
    ) -> Result<()> {
        let output = self.git_apply(component, tree, &["--verbose"], contents)?;
        let report = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            return Err(Error::Source {
                component: component.name.clone(),
                reason: format!(
                    "patch {} does not apply to {}: {}",
                    path.display(),
                    tree.display(),
                    report.trim(),
                ),
            });
        }
        if report.contains(SKIPPED) {
            return Err(Error::Source {
                component: component.name.clone(),
                reason: format!(
                    "patch {} was skipped rather than applied to {}: {}. A patch \
                     that is not applied would produce a package the version \
                     stamp says was patched and the contents say was not, so the \
                     build stops here",
                    path.display(),
                    tree.display(),
                    report.trim(),
                ),
            });
        }
        Ok(())
    }

    /// Runs `git apply` over `tree` with `patch` on standard input, returning
    /// git's output for the caller to interpret.
    ///
    /// The patch is fed on standard input rather than named as an argument, so
    /// it reaches git the same way wherever the file lives and however its own
    /// path is spelled.
    ///
    /// Two parts of the environment are set deliberately.
    ///
    /// `GIT_CEILING_DIRECTORIES` puts any repository *enclosing* the tree out of
    /// view. `git apply` run from a subdirectory of a repository prefixes every
    /// patch path with that subdirectory — and then silently skips a
    /// git-format patch that creates a file, because the name in its
    /// `diff --git` header does not carry the prefix it now expects. It exits
    /// zero having written nothing. A work directory inside a checkout is an
    /// ordinary arrangement, so without this a real build would quietly drop
    /// patches. Stopping git's search above the tree leaves the plain-patch
    /// behaviour: paths relative to the tree, and no repository consulted. A
    /// tree that is itself a repository root is unaffected either way, since
    /// there is no prefix to prepend.
    ///
    /// `LC_ALL` fixes the language of git's report, which is read back rather
    /// than only shown.
    fn git_apply(
        &self,
        component: &Component,
        tree: &Path,
        args: &[&str],
        patch: &[u8],
    ) -> Result<std::process::Output> {
        use std::io::Write;
        use std::process::Stdio;

        let mut command = Command::new("git");
        command
            .arg("apply")
            .args(args)
            .arg("-")
            .current_dir(tree)
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Absolute and symlink-free, which is the form git compares against.
        // A tree with no parent is the filesystem root, which holds no
        // repository to hide.
        if let Some(parent) = tree
            .canonicalize()
            .map_err(|err| self.fail(component, err))?
            .parent()
        {
            command.env("GIT_CEILING_DIRECTORIES", parent);
        }

        let mut child = command.spawn().map_err(|err| self.fail(component, err))?;
        child
            .stdin
            .take()
            .expect("the child was spawned with a piped stdin")
            .write_all(patch)
            .map_err(|err| self.fail(component, err))?;
        child
            .wait_with_output()
            .map_err(|err| self.fail(component, err))
    }

    /// Removes whatever a patch series left in `checkout` on an earlier run, so
    /// this run's series applies to the tree upstream ships.
    ///
    /// Only a git checkout needs this, and only for the files a series
    /// *creates*. `git checkout --force` restores every tracked file a series
    /// modified, but it leaves untracked files where they are — deliberately,
    /// since that is what carries the vendor pass's output between runs. So a
    /// patch that adds a file finds it already there on the second run and
    /// refuses to create it.
    ///
    /// The paths to discard are the ones the patches themselves name, taken from
    /// `git apply --numstat`, which parses a patch without applying it. Cleaning
    /// exactly those leaves everything else — the vendored crates above all —
    /// where it was.
    ///
    /// Two series are consulted, not one: this run's, and whatever the last run
    /// recorded ([`PATCH_TARGETS`]). A patch dropped from the recipe is no
    /// longer there to name the file it added, and without the record that file
    /// would stay in the tree and be built into the package — a change to the
    /// recipe that the build silently does not follow.
    fn discard_prior_patch_output(&self, component: &Component, checkout: &Path) -> Result<()> {
        let record = checkout.join(PATCH_TARGETS);
        if component.patches.is_empty() && !record.is_file() {
            return Ok(());
        }

        let tree = source_tree(checkout, component.source.subdir.as_deref());
        let mut current = Vec::new();
        for (path, contents) in self.read_patches(component)? {
            current.extend(self.patch_targets(component, &tree, &path, &contents)?);
        }
        // The union, so a path this run no longer touches is still cleared once.
        let mut targets = read_patch_targets(&record)?;
        targets.extend(current.iter().cloned());
        targets.sort();
        targets.dedup();
        write_patch_targets(&record, &current)?;

        if targets.is_empty() {
            return Ok(());
        }
        // `-x` reaches an ignored file too, which a patch may well create; the
        // pathspecs keep that bounded to what a series touches. A pathspec
        // matching nothing is not an error, so a series that only modifies
        // tracked files costs one no-op call. A tracked file is never removed
        // whatever a pathspec says, so the modifications stay the checkout's to
        // restore.
        let mut args: Vec<&std::ffi::OsStr> = vec![
            "clean".as_ref(),
            "--force".as_ref(),
            "-d".as_ref(),
            "-x".as_ref(),
            "--quiet".as_ref(),
            "--".as_ref(),
        ];
        args.extend(targets.iter().map(|path| path.as_os_str()));
        let output = Command::new("git")
            .args(&args)
            .current_dir(&tree)
            .output()
            .map_err(|err| self.fail(component, err))?;
        if output.status.success() {
            return Ok(());
        }
        Err(Error::Source {
            component: component.name.clone(),
            reason: format!(
                "clearing what a prior patch series left in {} failed: {}",
                tree.display(),
                String::from_utf8_lossy(&output.stderr).trim(),
            ),
        })
    }

    /// The paths within `tree` that one patch touches, read from
    /// `git apply --numstat` without applying anything.
    ///
    /// `-z` so a path holding a newline or invalid UTF-8 survives. Each record
    /// is NUL-terminated and reads `added\tdeleted\tpath`, so the path is
    /// whatever follows the second tab. A rename is reported at its new path
    /// alone, which is the one that gets created and so the one to discard.
    fn patch_targets(
        &self,
        component: &Component,
        tree: &Path,
        path: &Path,
        contents: &[u8],
    ) -> Result<Vec<PathBuf>> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let output = self.git_apply(component, tree, &["--numstat", "-z"], contents)?;
        if !output.status.success() {
            return Err(Error::Source {
                component: component.name.clone(),
                reason: format!(
                    "patch {} could not be read as a patch: {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr).trim(),
                ),
            });
        }
        Ok(output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
            .filter_map(numstat_path)
            .map(|path| PathBuf::from(OsStr::from_bytes(path)))
            .collect())
    }

    /// Fails the component when `tree` holds a Git LFS pointer, naming the
    /// command that materializes it.
    ///
    /// The scan is the one a git source gets, run against the tree on disk and
    /// bounded the same way — to the files git tracks under it. What differs is
    /// the remedy: a git source's checkout belongs to src2deb, so a pointer
    /// there is pulled; a path source's tree belongs to whoever pointed src2deb
    /// at it, so it is reported and left alone.
    ///
    /// A tree that is not part of a git working tree carries no pointers by
    /// definition and is passed over, as is one on a host with no `git` — a path
    /// source is the one kind that needs none.
    fn refuse_lfs_pointers(&self, component: &Component, tree: &Path) -> Result<()> {
        if !is_git_work_tree(tree) {
            return Ok(());
        }
        // Bounded to `tree`'s own subtree even when the working tree's root is
        // an ancestor of it: `git ls-files` with no pathspec lists what is under
        // the directory it runs in.
        let tracked = self.tracked_files(component, tree)?;
        let pointers = lfs_pointers(tree, &tracked)?;
        if pointers.is_empty() {
            return Ok(());
        }
        Err(Error::Source {
            component: component.name.clone(),
            reason: format!(
                "{} is a Git LFS pointer; run `git lfs pull` in {} so the build \
                 gets the real content instead of the pointer stub standing in \
                 for it{}",
                describe_pointers(&pointers),
                tree.display(),
                POINTER_CONSEQUENCE,
            ),
        })
    }

    /// Replaces any Git LFS pointer in `tree` with the content it stands for.
    ///
    /// Does nothing for the common case of a component that uses no LFS, which
    /// is established by scanning the built tree rather than by reading
    /// `.gitattributes`: what matters is whether a stub would reach the build,
    /// not what declared it.
    ///
    /// Fetching is repository-wide because that is the unit `git lfs` works on,
    /// while the check either side of it is scoped to `tree` — the subdirectory
    /// actually built — and within it to the files git tracks, which is the only
    /// place a pointer can come from. See [`tracked_files`](Self::tracked_files).
    /// A pointer elsewhere in a superproject belongs to a component this build is
    /// not producing and is not this build's concern.
    ///
    /// Returns [`Error::Source`] when the content cannot be fetched, so a
    /// package is never produced from stubs.
    fn materialize_lfs(&self, component: &Component, checkout: &Path, tree: &Path) -> Result<()> {
        let tracked = self.tracked_files(component, tree)?;
        let pointers = lfs_pointers(tree, &tracked)?;
        if pointers.is_empty() {
            return Ok(());
        }

        // Probed rather than assumed so the failure names the missing tool. Left
        // to `git lfs pull`, the run would fail with git's "'lfs' is not a git
        // command", which reads as a malformed invocation rather than an
        // uninstalled dependency.
        if !self.git_probe(component, checkout, &["lfs", "version"])? {
            return Err(Error::Source {
                component: component.name.clone(),
                reason: format!(
                    "{} is stored with Git LFS, but `git lfs` is not available; \
                     install git-lfs so the real content is fetched instead of \
                     the pointer stubs standing in for it{}",
                    describe_pointers(&pointers),
                    POINTER_CONSEQUENCE,
                ),
            });
        }

        // `pull` fetches the objects the checked-out commit needs and swaps the
        // pointers for them. It does not descend into submodules, so each is
        // pulled in its own right; `foreach` is a no-op when there are none.
        self.git(component, checkout, &["lfs", "pull"])?;
        self.git(
            component,
            checkout,
            &["submodule", "foreach", "--recursive", "git lfs pull"],
        )?;

        let residual = lfs_pointers(tree, &tracked)?;
        if !residual.is_empty() {
            return Err(Error::Source {
                component: component.name.clone(),
                reason: format!(
                    "{} is still a Git LFS pointer after `git lfs pull`; the \
                     content could not be fetched from the LFS server{}",
                    describe_pointers(&residual),
                    POINTER_CONSEQUENCE,
                ),
            });
        }
        Ok(())
    }

    /// The files git tracks under `tree`, as paths relative to it.
    ///
    /// This is what bounds the Git LFS scan, and only tracked files can be in
    /// it: a pointer is what git itself writes in place of an asset it could not
    /// materialize, so nothing else in the tree can be one.
    ///
    /// The distinction matters because the tree is not clean. `git checkout
    /// --force` leaves untracked files where they are, so from the second run
    /// onward the tree still holds pass 1's vendoring output — `vendor.tar`, an
    /// unpacked `vendor/` tree, a `.cargo/config` — which for a COSMIC component
    /// is tens of thousands of small files. Walking the tree would open every one
    /// of them on every resolve of every component. It would also read them: a
    /// crate vendored from a git dependency that uses LFS can carry a pointer of
    /// its own, and `git lfs pull` against the *component's* repository, which
    /// has never heard of that file, could not replace it — failing the component
    /// permanently, and naming a path with no visible connection to it.
    ///
    /// `--recurse-submodules` descends into submodules, so a superproject's
    /// members are covered exactly as the checkout covers them.
    fn tracked_files(&self, component: &Component, tree: &Path) -> Result<Vec<PathBuf>> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let args = ["ls-files", "-z", "--recurse-submodules"];
        let output = Command::new("git")
            .args(args)
            .current_dir(tree)
            .output()
            .map_err(|err| self.fail(component, err))?;
        if !output.status.success() {
            return Err(Error::Source {
                component: component.name.clone(),
                reason: format!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        // `-z` so a path holding a newline or invalid UTF-8 survives the round
        // trip; git writes a trailing NUL, so the final split is empty.
        Ok(output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| PathBuf::from(OsStr::from_bytes(path)))
            .collect())
    }

    /// The commit `HEAD` resolves to in `checkout`, for the run's provenance.
    fn head_commit(&self, component: &Component, checkout: &Path) -> Result<String> {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(checkout)
            .output()
            .map_err(|err| self.fail(component, err))?;
        if !output.status.success() {
            return Err(Error::Source {
                component: component.name.clone(),
                reason: format!(
                    "git rev-parse HEAD failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Resolves the commit-ish to check out for `component`, always naming the
    /// fetched remote state rather than a stale local branch.
    ///
    /// A configured branch resolves to its remote-tracking tip (`origin/<ref>`),
    /// so a re-run advances to the upstream branch head; a tag or commit resolves
    /// to itself, which the fetch already made current; and an unset ref resolves
    /// to the remote's default branch (`origin/HEAD`, set at clone time).
    fn resolve_target(
        &self,
        component: &Component,
        checkout: &Path,
        git_ref: Option<&str>,
    ) -> Result<String> {
        let Some(git_ref) = git_ref else {
            return Ok("origin/HEAD".to_string());
        };
        // A remote-tracking branch of this name means the ref is a branch, so
        // build its fetched tip; otherwise it is a tag or commit and is used
        // as-is. `rev-parse --verify --quiet` probes without failing the run.
        let remote = format!("origin/{git_ref}");
        if self.git_probe(
            component,
            checkout,
            &["rev-parse", "--verify", "--quiet", &remote],
        )? {
            Ok(remote)
        } else {
            Ok(git_ref.to_string())
        }
    }

    /// Runs a git command in `cwd`, mapping any failure to [`Error::Source`].
    fn git(&self, component: &Component, cwd: &Path, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|err| self.fail(component, err))?;
        if output.status.success() {
            return Ok(());
        }
        Err(Error::Source {
            component: component.name.clone(),
            reason: format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        })
    }

    /// Runs a git command in `cwd` as a boolean probe: `Ok(true)` when it exits
    /// successfully, `Ok(false)` when it exits non-zero, and an error only when
    /// git cannot be launched. Used to test whether a ref resolves without
    /// treating a clean "no" as a failure.
    fn git_probe(&self, component: &Component, cwd: &Path, args: &[&str]) -> Result<bool> {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|err| self.fail(component, err))?;
        Ok(output.status.success())
    }

    fn fail(&self, component: &Component, err: std::io::Error) -> Error {
        Error::Source {
            component: component.name.clone(),
            reason: err.to_string(),
        }
    }
}

/// The bytes every Git LFS pointer file begins with.
///
/// The spec fixes this as the first line, so it identifies a pointer without
/// parsing the rest of it.
const LFS_POINTER_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/";

/// The largest a Git LFS pointer file can be, bounding what the scan opens.
///
/// The format is three short lines — version, oid, size — so a file above this
/// holds real content and is skipped without being read.
const LFS_POINTER_MAX_BYTES: u64 = 1024;

/// Appended to both pointer failures, which differ in cause but not in stakes.
const POINTER_CONSEQUENCE: &str = ". Building against a pointer produces a \
     package that installs cleanly and fails at runtime, so the build stops here";

/// Renders found pointers for an error message: every path when there are few,
/// and a bounded list with a count when a whole directory of assets is stubbed.
fn describe_pointers(pointers: &[PathBuf]) -> String {
    const SHOWN: usize = 5;
    let shown = pointers
        .iter()
        .take(SHOWN)
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    match pointers.len().checked_sub(SHOWN) {
        Some(rest) if rest > 0 => format!("{shown} and {rest} more"),
        _ => shown,
    }
}

/// The Git LFS pointer files among `files` — paths relative to `tree` — sorted
/// so a failure reports them in a stable order.
///
/// A symlink is never a pointer: LFS materializes content in place, and
/// following a link risks leaving the tree. A file the index names but the
/// working tree does not hold is not one either, and is passed over rather than
/// failing the resolve, since a file that is absent cannot reach the build.
fn lfs_pointers(tree: &Path, files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for relative in files {
        let path = tree.join(relative);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(io_error("inspecting", &path, err)),
        };
        // The format is three short lines, so anything larger holds real content
        // and is skipped without being opened.
        if !metadata.is_file() || metadata.len() > LFS_POINTER_MAX_BYTES {
            continue;
        }
        if is_lfs_pointer(&path)? {
            found.push(relative.clone());
        }
    }
    found.sort();
    Ok(found)
}

/// Whether `path` begins with the Git LFS pointer signature.
///
/// A file shorter than the signature cannot be a pointer, so a short read is a
/// clean negative rather than an error.
fn is_lfs_pointer(path: &Path) -> Result<bool> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).map_err(|err| io_error("opening", path, err))?;
    let mut head = [0u8; LFS_POINTER_PREFIX.len()];
    match file.read_exact(&mut head) {
        Ok(()) => Ok(head == LFS_POINTER_PREFIX),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(io_error("reading", path, err)),
    }
}

/// The source tree within a resolved source: its `subdir` when the recipe sets
/// one — for a component nested in a superproject — or the whole of it.
fn source_tree(checkout: &Path, subdir: Option<&Path>) -> PathBuf {
    match subdir {
        Some(subdir) => checkout.join(subdir),
        None => checkout.to_path_buf(),
    }
}

/// Where a checkout records the paths its last patch series named, relative to
/// the checkout.
///
/// Kept inside `.git`, so it belongs to the checkout rather than to the source
/// tree: it is removed when the checkout is, it cannot collide with a file the
/// component ships, and nothing that reads the tree — the LFS scan, the build —
/// ever sees it.
const PATCH_TARGETS: &str = ".git/src2deb-patch-targets";

/// The paths a `PATCH_TARGETS` record holds, or none when there is no record.
///
/// NUL-separated, so a path holding a newline or invalid UTF-8 survives the
/// round trip. A record that cannot be read is a failure rather than an empty
/// answer: treating it as empty would silently give up the cleanup it exists to
/// drive.
fn read_patch_targets(record: &Path) -> Result<Vec<PathBuf>> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let bytes = match std::fs::read(record) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_error("reading", record, err)),
    };
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .collect())
}

/// Records the paths this run's patch series named, replacing any earlier
/// record, and removes the record entirely when the series names none.
fn write_patch_targets(record: &Path, targets: &[PathBuf]) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    if targets.is_empty() {
        return match std::fs::remove_file(record) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_error("removing", record, err)),
        };
    }
    let mut bytes = Vec::new();
    for target in targets {
        bytes.extend_from_slice(target.as_os_str().as_bytes());
        bytes.push(0);
    }
    std::fs::write(record, bytes).map_err(|err| io_error("writing", record, err))
}

/// What `git apply --verbose` reports for a patch it declined to apply.
///
/// It is not a failure to git: the command exits zero having written nothing.
/// It is a failure here, because a patch that does not reach the tree produces
/// a package whose version says it was patched and whose contents say it was
/// not. The environment `git_apply` sets should leave nothing to skip; this is
/// what makes that a claim the run checks rather than one it assumes.
const SKIPPED: &str = "Skipped patch";

/// The path within one `git apply --numstat -z` record: whatever follows the
/// added and deleted line counts, which are tab-separated before it.
///
/// A record that does not carry two tabs is not one git wrote in this format,
/// and is passed over rather than guessed at — the paths only bound a cleanup,
/// so leaving one out costs nothing that a misread path would not cost more.
fn numstat_path(record: &[u8]) -> Option<&[u8]> {
    let mut fields = record.splitn(3, |byte| *byte == b'\t');
    fields.next()?;
    fields.next()?;
    fields.next()
}

/// The identity of a patch series: a SHA-256 over each member's contents, in
/// series order, with each length-prefixed.
///
/// Contents alone, not file names: renaming a patch does not change the tree it
/// produces, so it must not change what the component is recorded as built
/// from. The length prefix keeps the series unambiguous, so that splitting one
/// patch into two — which applies differently — does not hash the same as the
/// original.
fn series_digest(series: &[(PathBuf, Vec<u8>)]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    for (_, contents) in series {
        hasher.update((contents.len() as u64).to_be_bytes());
        hasher.update(contents);
    }
    crate::fingerprint::hex(&hasher.finalize())
}

/// Fails the component when `tree` holds no `debian/control`, which is what
/// makes it a component at all: the build order, the build-dependencies, and the
/// set of binary packages are all read from it.
fn refuse_without_control(component: &Component, tree: &Path) -> Result<()> {
    if tree.join("debian/control").is_file() {
        return Ok(());
    }
    Err(Error::Source {
        component: component.name.clone(),
        reason: format!("{} has no debian/control", tree.display()),
    })
}

/// Fails the component when a path source and the work directory's copy of it
/// would sit inside one another.
///
/// Both directions are ruinous rather than merely wrong. A destination inside
/// the source means the copy walks into its own output and never terminates; a
/// source inside the destination means the wipe that precedes the copy deletes
/// the tree it was about to read. Neither can arise from an ordinary recipe, and
/// both are cheap to rule out.
///
/// Compares canonical paths, so a symlinked route to the same directory is
/// caught rather than passed over.
fn refuse_overlap(component: &Component, origin: &Path, destination: &Path) -> Result<()> {
    let reason = if destination.starts_with(origin) {
        "would be copied into itself"
    } else if origin.starts_with(destination) {
        "lies inside the work directory it would be copied into"
    } else {
        return Ok(());
    };
    Err(Error::Source {
        component: component.name.clone(),
        reason: format!(
            "source.path {} {reason} ({}); point it at a tree outside the work \
             directory",
            origin.display(),
            destination.display(),
        ),
    })
}

/// Whether `dir` is inside a git working tree.
///
/// A directory that is not carries no Git LFS pointers, since a pointer is what
/// git writes in place of content it could not materialize. A host with no `git`
/// at all answers the same way: a path source is the one source kind that does
/// not need one, so failing to launch git is not a reason to fail the component.
fn is_git_work_tree(dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Copies the tree at `src` to `dst`, which must not exist or must be empty.
///
/// Files are copied with their permissions, and symlinks are recreated as
/// symlinks rather than followed — so a link into the tree still points within
/// the copy, and one out of it still points where it did. Anything that is
/// neither a file, a directory, nor a symlink — a socket, a fifo, a device node
/// — is passed over: it is not source, and cannot be copied by reading it.
///
/// A directory is given its source's permissions plus `u+rwx`. Preserving a
/// directory that its owner cannot write would leave a copy the next run cannot
/// clear, wedging the component until the work directory is removed by hand; the
/// modes that reach a package are set by the packaging's own install steps, not
/// carried from the source tree.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dst).map_err(|err| io_error("creating", dst, err))?;
    for entry in std::fs::read_dir(src).map_err(|err| io_error("reading", src, err))? {
        let entry = entry.map_err(|err| io_error("reading", src, err))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let metadata =
            std::fs::symlink_metadata(&from).map_err(|err| io_error("inspecting", &from, err))?;
        let kind = metadata.file_type();
        if kind.is_symlink() {
            let target =
                std::fs::read_link(&from).map_err(|err| io_error("reading", &from, err))?;
            std::os::unix::fs::symlink(&target, &to)
                .map_err(|err| io_error("linking", &to, err))?;
        } else if kind.is_dir() {
            copy_tree(&from, &to)?;
            // Applied after the directory is populated, so a source directory
            // its owner cannot write does not stop its own contents being
            // copied into the counterpart.
            let mode = metadata.permissions().mode() | 0o700;
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode))
                .map_err(|err| io_error("setting permissions on", &to, err))?;
        } else if kind.is_file() {
            std::fs::copy(&from, &to).map_err(|err| io_error("copying", &from, err))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A representative pointer, as a checkout without LFS support writes it.
    const POINTER: &str = "version https://git-lfs.github.com/spec/v1\n\
         oid sha256:ae15dde8fe7213dd8f3cd2ca2fd4e226d8342bd06a6501613ecf111280fd4f7b\n\
         size 9402799\n";

    /// A unique scratch directory. Unit tests have no `CARGO_TARGET_TMPDIR`, so
    /// this builds on the system temp directory instead.
    fn scratch(label: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("src2deb-lfs-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn a_pointer_file_is_recognized_by_its_signature() {
        let root = scratch("signature");
        let path = root.join("cities.bitcode-v0-6");
        write(&path, POINTER);
        assert!(is_lfs_pointer(&path).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ordinary_content_is_not_a_pointer() {
        let root = scratch("ordinary");
        let path = root.join("notes.txt");
        write(&path, "version 1 of some perfectly ordinary short file\n");
        assert!(!is_lfs_pointer(&path).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_shorter_than_the_signature_is_not_a_pointer() {
        // Reading such a file hits EOF mid-signature, which must read as a
        // clean negative rather than an I/O failure.
        let root = scratch("short");
        let path = root.join("tiny");
        write(&path, "version\n");
        assert!(!is_lfs_pointer(&path).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The paths of `files`, as the tracked-file listing gives them.
    fn tracked(files: &[&str]) -> Vec<PathBuf> {
        files.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn scanning_reports_sorted_paths_relative_to_the_tree() {
        let root = scratch("relative");
        write(&root.join("res/themes/mocha-dark.png"), POINTER);
        write(&root.join("res/cities.bitcode-v0-6"), POINTER);
        write(&root.join("src/main.rs"), "fn main() {}\n");

        let files = tracked(&[
            "res/themes/mocha-dark.png",
            "res/cities.bitcode-v0-6",
            "src/main.rs",
        ]);
        assert_eq!(
            lfs_pointers(&root, &files).unwrap(),
            vec![
                PathBuf::from("res/cities.bitcode-v0-6"),
                PathBuf::from("res/themes/mocha-dark.png"),
            ],
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scanning_reads_only_the_files_it_is_given() {
        // The tree keeps a prior build's vendored crates, which git does not
        // track. One of them opening with a pointer signature — a crate vendored
        // from a repository that itself uses LFS — must not fail the component:
        // the component's own repository could never pull a file it has never
        // heard of, so the failure would be permanent and would name a path with
        // no connection to the component's assets.
        let root = scratch("untracked");
        write(&root.join("res/logo.png"), POINTER);
        write(&root.join("vendor/some-crate/assets/mesh.bin"), POINTER);
        write(&root.join("vendor.tar"), POINTER);

        let files = tracked(&["res/logo.png"]);
        assert_eq!(
            lfs_pointers(&root, &files).unwrap(),
            vec![PathBuf::from("res/logo.png")],
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scanning_skips_files_too_large_to_be_a_pointer() {
        let root = scratch("large");
        let mut padded = String::from(POINTER);
        padded.push_str(&"x".repeat(LFS_POINTER_MAX_BYTES as usize));
        write(&root.join("real-asset.bin"), &padded);
        assert!(
            lfs_pointers(&root, &tracked(&["real-asset.bin"]))
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scanning_passes_over_a_tracked_file_the_tree_does_not_hold() {
        // A file the index names but the checkout does not carry cannot reach
        // the build, so it is neither a pointer nor a reason to fail.
        let root = scratch("absent");
        write(&root.join("src/main.rs"), "fn main() {}\n");
        assert!(
            lfs_pointers(&root, &tracked(&["src/main.rs", "res/gone.png"]))
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_tree_with_no_pointers_scans_clean() {
        let root = scratch("clean");
        write(&root.join("src/main.rs"), "fn main() {}\n");
        write(&root.join("debian/control"), "Source: pkg\n");
        assert!(
            lfs_pointers(&root, &tracked(&["src/main.rs", "debian/control"]))
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_short_pointer_list_names_every_file() {
        let pointers = vec![PathBuf::from("res/a.png"), PathBuf::from("res/b.png")];
        assert_eq!(describe_pointers(&pointers), "res/a.png, res/b.png");
    }

    #[test]
    fn a_long_pointer_list_is_bounded_with_a_count() {
        let pointers: Vec<PathBuf> = (0..8).map(|n| PathBuf::from(format!("{n}.png"))).collect();
        assert_eq!(
            describe_pointers(&pointers),
            "0.png, 1.png, 2.png, 3.png, 4.png and 3 more",
        );
    }

    /// A component naming a path source, for the guards that take one.
    fn path_component(path: &Path) -> crate::recipe::Component {
        crate::recipe::Component {
            name: "pkg".to_string(),
            source: crate::recipe::Source {
                path: Some(path.to_path_buf()),
                ..crate::recipe::Source::default()
            },
            patches: Vec::new(),
            extra_build_deps: Vec::new(),
        }
    }

    #[test]
    fn a_copy_reproduces_files_directories_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("copy");
        let src = root.join("src");
        write(&src.join("debian/control"), "Source: pkg\n");
        write(&src.join("debian/rules"), "#!/usr/bin/make -f\n");
        std::fs::set_permissions(
            src.join("debian/rules"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("debian/control", src.join("link")).unwrap();
        std::fs::create_dir_all(src.join("empty")).unwrap();

        let dst = root.join("dst");
        copy_tree(&src, &dst).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.join("debian/control")).unwrap(),
            "Source: pkg\n",
        );
        // An executable `debian/rules` that arrived as a plain file would fail
        // the build for a reason nothing would name.
        assert_eq!(
            std::fs::metadata(dst.join("debian/rules"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
        );
        // A symlink stays one, so a link within the tree still points inside the
        // copy rather than being replaced by a second copy of its target.
        let link = dst.join("link");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("debian/control")
        );
        assert!(dst.join("empty").is_dir(), "an empty directory was dropped");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_copied_directory_stays_traversable_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        // A directory copied at a mode its owner cannot write would leave a
        // tree the next run's wipe cannot clear, wedging the component until the
        // work directory is removed by hand.
        let root = scratch("copy-modes");
        let src = root.join("src");
        write(&src.join("locked/file"), "content\n");
        std::fs::set_permissions(src.join("locked"), std::fs::Permissions::from_mode(0o500))
            .unwrap();

        let dst = root.join("dst");
        copy_tree(&src, &dst).unwrap();
        let mode = std::fs::metadata(dst.join("locked"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o700, 0o700, "the copy is not writable by its owner");
        // The contents came across all the same, which is what applying the mode
        // after populating the directory buys.
        assert_eq!(
            std::fs::read_to_string(dst.join("locked/file")).unwrap(),
            "content\n",
        );

        // Restore write access so the scratch directory can be removed even if
        // the assertions above had failed to hold.
        std::fs::set_permissions(src.join("locked"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_source_and_its_copy_may_not_contain_one_another() {
        // A destination inside the source means the copy walks into its own
        // output; a source inside the destination means the wipe that precedes
        // the copy deletes the tree it was about to read.
        let component = path_component(Path::new("/home/someone/tree"));
        let err = refuse_overlap(
            &component,
            Path::new("/work"),
            Path::new("/work/sources/pkg"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("copied into itself"), "{err}");

        let err = refuse_overlap(
            &component,
            Path::new("/work/sources/pkg/nested"),
            Path::new("/work/sources/pkg"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("lies inside the work directory"), "{err}");

        // The same path by both routes is caught as well.
        assert!(
            refuse_overlap(
                &component,
                Path::new("/work/sources/pkg"),
                Path::new("/work/sources/pkg")
            )
            .is_err()
        );
        // An ordinary arrangement passes.
        refuse_overlap(
            &component,
            Path::new("/home/someone/tree"),
            Path::new("/work/sources/pkg"),
        )
        .expect("a tree outside the work directory is fine");
        // A shared prefix that is not a parent is not an overlap.
        refuse_overlap(
            &component,
            Path::new("/work/sources/pkg-extra"),
            Path::new("/work/sources/pkg"),
        )
        .expect("a sibling whose name extends this one is not inside it");
    }

    /// A series of patch contents, as the digest takes them.
    fn series(patches: &[(&str, &str)]) -> Vec<(PathBuf, Vec<u8>)> {
        patches
            .iter()
            .map(|(name, body)| (PathBuf::from(name), body.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn a_series_is_identified_by_its_contents_in_order() {
        let one = series(&[("a.patch", "first\n")]);
        let two = series(&[("a.patch", "first\n"), ("b.patch", "second\n")]);

        // A hash, so the value is the shape the pinned kinds take.
        let digest = series_digest(&one);
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));

        // Adding a patch changes the series...
        assert_ne!(series_digest(&one), series_digest(&two));
        // ...and so does reordering it, since a series is applied in order and
        // two orders need not produce the same tree.
        let reversed = series(&[("b.patch", "second\n"), ("a.patch", "first\n")]);
        assert_ne!(series_digest(&two), series_digest(&reversed));
        // ...and so does editing one in place.
        assert_ne!(
            series_digest(&one),
            series_digest(&series(&[("a.patch", "amended\n")])),
        );
    }

    #[test]
    fn renaming_a_patch_does_not_change_the_series() {
        // The file name is not an input to the build: the same patches in the
        // same order produce the same tree, so a rename must not present itself
        // as a different source and provoke a rebuild.
        assert_eq!(
            series_digest(&series(&[("a.patch", "first\n")])),
            series_digest(&series(&[("renamed.patch", "first\n")])),
        );
    }

    #[test]
    fn a_series_cannot_be_confused_with_a_differently_split_one() {
        // Two patches applied in turn need not do what one patch of the same
        // bytes does, so the digest length-prefixes each member rather than
        // running them together.
        assert_ne!(
            series_digest(&series(&[("a.patch", "onetwo")])),
            series_digest(&series(&[("a.patch", "one"), ("b.patch", "two")])),
        );
    }

    #[test]
    fn a_numstat_record_yields_the_path_after_its_two_counts() {
        // `added\tdeleted\tpath`, and a path may itself hold anything but a NUL.
        assert_eq!(
            numstat_path(b"1\t1\tsrc/main.rs"),
            Some(&b"src/main.rs"[..])
        );
        assert_eq!(numstat_path(b"0\t0\ta\tb.txt"), Some(&b"a\tb.txt"[..]));
        // A record in some other shape is passed over rather than guessed at.
        assert_eq!(numstat_path(b"nonsense"), None);
        assert_eq!(numstat_path(b"1\tonly-one-tab"), None);
    }

    #[test]
    fn a_tree_without_a_control_file_is_not_a_component() {
        let root = scratch("no-control");
        write(&root.join("src/main.rs"), "fn main() {}\n");
        let err = refuse_without_control(&path_component(&root), &root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("debian/control"), "{err}");

        write(&root.join("debian/control"), "Source: pkg\n");
        refuse_without_control(&path_component(&root), &root).expect("a control file is enough");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn source_tree_is_the_checkout_when_no_subdir_is_set() {
        assert_eq!(
            source_tree(Path::new("/work/sources/cosmic-comp"), None),
            Path::new("/work/sources/cosmic-comp"),
        );
    }

    #[test]
    fn source_tree_descends_into_the_subdir_when_set() {
        assert_eq!(
            source_tree(
                Path::new("/work/sources/superproject"),
                Some(Path::new("members/cosmic-comp")),
            ),
            Path::new("/work/sources/superproject/members/cosmic-comp"),
        );
    }
}
