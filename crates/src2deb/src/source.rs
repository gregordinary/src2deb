//! Resolving a component's git source into an unpacked tree with a `debian/`
//! directory.
//!
//! src2deb owns source acquisition; ferroday-cage takes an already-unpacked
//! tree. The resolver clones (or updates) each component's repository under a
//! work directory, checks out the requested ref, initializes submodules — so a
//! submodule superproject such as cosmic-epoch resolves its members —
//! materializes Git LFS content, and returns the path to the tree that holds
//! `debian/`.
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

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result, io_error};
use crate::recipe::Component;

/// A resolved component source: the tree that holds `debian/`, and the exact
/// commit it was checked out at.
///
/// The commit is the resolved `HEAD` after checkout, so it names a concrete
/// revision even when the recipe tracked a branch or the remote's default. It
/// anchors the run's provenance manifest.
#[derive(Debug, Clone)]
pub struct ResolvedSource {
    /// The path to the tree that holds the `debian/` directory.
    pub tree: PathBuf,
    /// The full commit hash the tree was checked out at.
    pub commit: String,
}

/// Clones and checks out component sources under a work directory.
pub struct SourceResolver {
    sources_dir: PathBuf,
}

impl SourceResolver {
    /// Creates a resolver that places checkouts under `sources_dir`.
    pub fn new(sources_dir: impl Into<PathBuf>) -> SourceResolver {
        SourceResolver {
            sources_dir: sources_dir.into(),
        }
    }

    /// Resolves `component`'s source, returning the tree holding its `debian/`
    /// directory and the commit it was checked out at.
    ///
    /// Clones the repository on first use and fetches on later use, checks out
    /// the configured ref, and initializes submodules. The returned tree is the
    /// checkout, or the checkout's `subdir` when the recipe sets one, and the
    /// commit is the resolved `HEAD` for the run's provenance.
    ///
    /// A re-run always lands on the fetched remote state: a branch ref advances
    /// to its upstream tip, a tag or commit resolves to itself, and an unset ref
    /// tracks the remote's default branch. See `resolve_target`.
    pub fn resolve(&self, component: &Component) -> Result<ResolvedSource> {
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
                &[
                    "clone",
                    "--recurse-submodules",
                    &component.source.git,
                    &component.name,
                ],
            )?;
        }

        // Check out the fetched target detached, so we never pin to a stale
        // local branch that `fetch` did not move.
        let target = self.resolve_target(component, &checkout)?;
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
        let control = tree.join("debian/control");
        if !control.is_file() {
            return Err(Error::Source {
                component: component.name.clone(),
                reason: format!("{} has no debian/control", tree.display()),
            });
        }
        self.materialize_lfs(component, &checkout, &tree)?;
        let commit = self.head_commit(component, &checkout)?;
        Ok(ResolvedSource { tree, commit })
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
    fn resolve_target(&self, component: &Component, checkout: &Path) -> Result<String> {
        let Some(git_ref) = &component.source.git_ref else {
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
            Ok(git_ref.clone())
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

/// The source tree within a checkout: the checkout's `subdir` when the recipe
/// sets one — for a component nested in a superproject — or the checkout itself.
fn source_tree(checkout: &Path, subdir: Option<&Path>) -> PathBuf {
    match subdir {
        Some(subdir) => checkout.join(subdir),
        None => checkout.to_path_buf(),
    }
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
