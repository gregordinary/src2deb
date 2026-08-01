//! The local `.deb` pool that carries build-dependencies from one component to
//! the next.
//!
//! Each built component publishes its `.debs` here, and the next component's
//! provisioning declares the pool as a trusted `file://` repository so those
//! packages resolve into its build root. This is the `deb-multirepo` pattern:
//! a `dists/`-structured pool trusted without a signature.
//!
//! Writing the pool uses ferroday-cage's [`Pool`]:
//! [`init`](LocalPool::init) emits a valid empty pool up front, and
//! [`publish`](LocalPool::publish) adds each component's `.debs` and regenerates
//! the index. The consume side, [`as_repository`](LocalPool::as_repository),
//! builds a trusted `file://` [`Repository`] as soon as a component has
//! published into the pool.
//!
//! # Sharing the pool between parallel builds
//!
//! `Pool::publish` excludes concurrent publishes with a lock in the pool root,
//! and a publish becomes visible to a reader all at once — the emitted `Release`
//! is renamed into place last, and the indexes it names are also written where
//! they are never rewritten, so a bootstrap that has read a `Release` keeps
//! resolving the indexes that `Release` described however many publishes land
//! while it works. Publishing into the pool while another component resolves
//! against it is therefore safe.
//!
//! [`publish`](LocalPool::publish) accordingly takes `&self`, so a parallel
//! build shares one `LocalPool` across its workers with no lock of its own.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ferroday_cage::provision::debian::{Pool, Repository};

use crate::build::Artifact;
use crate::error::{Error, Result};

/// The directory within the work directory that holds every pool.
pub const POOL_DIR: &str = "pool";

/// The path of the pool for `suite` and `architecture` under `work_dir`:
/// `pool/<suite>/<architecture>/`.
///
/// A pool is scoped to the suite and architecture it was built for.
///
/// The architecture is a hard requirement. An `Architecture: all` package's file
/// name carries no architecture, and its stamped version is identical across
/// architectures — the stamp names the suite, not the machine (see
/// [`crate::version`]) — so the same component built for amd64 and for arm64
/// produces one file name for two different files. Inside one pool that is a
/// collision: publishing regenerates only the publishing run's `Packages`, so the
/// second build would overwrite the file and leave the first architecture's index
/// holding a checksum that no longer matches, which apt reports as a hash
/// mismatch.
///
/// The suite is what keeps a pool one archive rather than a shared store. A
/// rebuild for another suite differs in its version tag, so its file names differ
/// and would not in fact collide — but a pool scoped to a single identity is a
/// unit that can be served, signed, mirrored, or discarded on its own, and whose
/// index accounts for every file beside it.
///
/// This gives up Debian's one-pool-many-dists layout, which is the right shape
/// for an archive with a release process behind it. This pool is a build-time
/// carrier and a local serving pool, so keeping storage keyed to the identity
/// that produced it is what makes each index describe the files it indexed.
///
/// Both fields are validated as single benign path segments when the recipe loads
/// ([`Recipe::load`](crate::Recipe::load)), so neither can climb out of the work
/// directory here.
pub fn pool_dir(work_dir: &Path, suite: &str, architecture: &str) -> PathBuf {
    work_dir.join(POOL_DIR).join(suite).join(architecture)
}

/// A local `dists/`-structured `.deb` pool.
pub struct LocalPool {
    dir: PathBuf,
    suite: String,
    component: String,
    architecture: String,
    /// Whether the pool holds any package to resolve against. Seeded from the
    /// pool on disk at [`init`](Self::init) — so a resumed run sees a prior run's
    /// packages — and set by [`publish`](Self::publish).
    ///
    /// Atomic because parallel workers publish and read it concurrently. A
    /// worker never observes it stale in a way that matters: the scheduler
    /// releases a component only once every in-set producer's `publish` has
    /// returned, so a component that needs the pool always sees it populated.
    has_packages: AtomicBool,
}

impl LocalPool {
    /// Creates a pool rooted at `dir` for `suite`/`component`/`architecture`.
    pub fn new(
        dir: impl Into<PathBuf>,
        suite: impl Into<String>,
        component: impl Into<String>,
        architecture: impl Into<String>,
    ) -> LocalPool {
        LocalPool {
            dir: dir.into(),
            suite: suite.into(),
            component: component.into(),
            architecture: architecture.into(),
            has_packages: AtomicBool::new(false),
        }
    }

    /// Creates the pool directory and writes a valid empty pool so the first
    /// component can declare it as a repository.
    ///
    /// Publishing is incremental, so on a work directory reused from a prior
    /// run this preserves the packages already in the pool; the on-disk index is
    /// then read to decide whether the pool has anything to resolve against, so a
    /// resumed or selective build sees earlier components' packages.
    ///
    /// Takes `&self`, like [`publish`](Self::publish): the only state it settles
    /// is the atomic recording whether the pool has anything to resolve against.
    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|err| Error::Pool(format!("creating {}: {err}", self.dir.display())))?;
        // Emit a valid empty pool so the first component may declare it as a
        // trusted repository before anything has been published into it — the
        // provisioner validates every declared repository's Release at bootstrap.
        self.writer()
            .publish(Vec::<PathBuf>::new())
            .map_err(Error::Debian)?;
        let seeded = self.index_has_packages()?;
        self.has_packages.store(seeded, Ordering::Release);
        Ok(())
    }

    /// The ferroday-cage pool writer for this pool's coordinates.
    fn writer(&self) -> Pool {
        Pool::at(&self.dir)
            .suite(self.suite.as_str())
            .component(self.component.as_str())
            .architecture(self.architecture.as_str())
    }

    /// Publishes freshly-built artifacts to the pool, adding each `.deb` and
    /// regenerating the index. Returns how many `.debs` were added.
    ///
    /// Takes `&self`, and is safe to call from several workers at once: the
    /// publishes exclude each other inside ferroday-cage.
    pub fn publish(&self, artifacts: &[Artifact]) -> Result<usize> {
        let debs: Vec<PathBuf> = artifacts
            .iter()
            .map(|artifact| artifact.path.clone())
            .collect();
        self.writer().publish(debs).map_err(Error::Debian)?;
        // Mark populated only after the write succeeds, so a failed publish does
        // not leave `as_repository` advertising a pool that holds nothing.
        if !artifacts.is_empty() {
            self.has_packages.store(true, Ordering::Release);
        }
        Ok(artifacts.len())
    }

    /// Whether the pool's `Packages` index lists at least one package.
    ///
    /// The pool is empty exactly when its index is empty, so an empty (or absent)
    /// index means nothing to resolve against.
    fn index_has_packages(&self) -> Result<bool> {
        match std::fs::metadata(self.index_path()?) {
            Ok(meta) => Ok(meta.len() > 0),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Error::Pool(format!("reading the index: {err}"))),
        }
    }

    /// The binary package names the pool's index lists, empty when the pool
    /// holds nothing or does not exist yet.
    ///
    /// Read from the index directly rather than through the provisioner, because
    /// the question this answers — does the pool already carry the packages a
    /// component the run is not building would have produced — has to be settled
    /// before anything is provisioned. See
    /// [`Engine::run`](crate::Engine::run).
    pub fn indexed_packages(&self) -> Result<std::collections::BTreeSet<String>> {
        let path = self.index_path()?;
        let index = match std::fs::read_to_string(&path) {
            Ok(index) => index,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
            Err(err) => {
                return Err(Error::Pool(format!("reading {}: {err}", path.display())));
            }
        };
        Ok(index
            .lines()
            .filter_map(|line| line.strip_prefix("Package:"))
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect())
    }

    /// The path of this pool's `Packages` index for its own component and
    /// architecture.
    fn index_path(&self) -> Result<PathBuf> {
        Ok(self
            .writer()
            .dists_dir()
            .map_err(Error::Debian)?
            .join(&self.component)
            .join(format!("binary-{}", self.architecture))
            .join("Packages"))
    }

    /// The pool as a trusted `file://` repository for provisioning, or `None`
    /// when it holds nothing to resolve against yet.
    pub fn as_repository(&self) -> Result<Option<Repository>> {
        if !self.has_packages.load(Ordering::Acquire) {
            return Ok(None);
        }
        // The pool names its own `file://` URL, so the spelling the provisioner
        // reads and the spelling the writer publishes under come from one place.
        let url = self.writer().mirror_url().map_err(Error::Debian)?;
        let repository = Repository::builder(self.suite.as_str())
            .mirror(url)
            .components([self.component.as_str()])
            .trust_unsigned(true)
            .name("src2deb-pool")
            .build()
            .map_err(Error::Debian)?;
        Ok(Some(repository))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch directory whose name carries a space, so a path that a
    /// URL would ordinarily escape is exercised.
    fn scratch(label: &str) -> PathBuf {
        use std::sync::atomic::AtomicUsize;
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("src2deb pool-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_pool_names_itself_by_a_verbatim_path() {
        let dir = scratch("url");
        let pool = LocalPool::new(&dir, "forky", "main", "arm64");
        // ferroday-cage's `file://` fetch strips the scheme and opens the
        // remainder as a raw filesystem path, without percent-decoding. A pool
        // under a path holding a space therefore has to be named verbatim:
        // escaping it would send the provisioner looking for a literal `%20`.
        assert_eq!(
            pool.writer().mirror_url().unwrap(),
            format!("file://{}", dir.display()),
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_pool_is_not_offered_as_a_repository() {
        let dir = scratch("empty");
        let pool = LocalPool::new(&dir, "forky", "main", "arm64");
        // A valid but empty pool is written, and holds nothing to resolve
        // against, so no component is told to declare it.
        pool.init().unwrap();
        assert!(pool.writer().release_path().unwrap().is_file());
        assert!(pool.as_repository().unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn each_suite_and_architecture_gets_its_own_pool() {
        let work = Path::new("/w");
        let base = pool_dir(work, "forky", "arm64");
        assert_eq!(base, Path::new("/w/pool/forky/arm64"));
        // Varying either field moves the pool. The architecture has to: an
        // `Architecture: all` package's file name carries no architecture and
        // its stamped version is the same across them, so two architectures
        // would otherwise write one file name for different bytes.
        for other in [
            pool_dir(work, "trixie", "arm64"),
            pool_dir(work, "forky", "amd64"),
        ] {
            assert_ne!(base, other);
        }
        // Nested rather than joined into one name, so identities that would
        // collide when flattened stay apart: "a" / "b-c" is not "a-b" / "c".
        assert_ne!(pool_dir(work, "a", "b-c"), pool_dir(work, "a-b", "c"));
    }

    #[test]
    fn the_pool_and_the_output_tree_share_the_run_identity() {
        // The two are keyed alike on purpose: the output tree is what feeds the
        // pool, so a run's artifacts and the pool they publish into have to be
        // reachable by the same identity. Were they to diverge, one of them
        // would be the collision the other avoids.
        let work = Path::new("/w");
        assert_eq!(
            pool_dir(work, "forky", "arm64").strip_prefix("/w/pool"),
            crate::build::output_dir(work, "forky", "arm64").strip_prefix("/w/out"),
        );
    }
}
