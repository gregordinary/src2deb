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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ferroday_cage::provision::debian::{Pool, Repository};

use crate::build::Artifact;
use crate::error::{Error, Result, io_error};

/// The directory within the work directory that holds every pool.
pub const POOL_DIR: &str = "pool";

/// The archive component every pool publishes under.
///
/// One component, because a pool is one recipe's build output served as an
/// archive, and splitting it would mean deciding which packages are `contrib`
/// on a basis src2deb has no way to know.
pub const POOL_COMPONENT: &str = "main";

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

/// What a publish stamps into the pool's `Release` beyond the coordinates: when
/// the archive was made, and who says it is theirs.
///
/// The date is not optional. apt reports `W: Invalid 'Date' entry in Release
/// file` for every pool that omits one, on every `apt update`, and a `Release`
/// with no date cannot be compared against the one a client already holds.
///
/// The three identity fields are, and have no defaults. `Origin` and `Label`
/// name an organization and its archive, and inventing either would put a name
/// in an archive that its owner did not choose; a recipe that wants them says
/// so. See [`Recipe::origin`](crate::Recipe::origin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolRelease {
    /// The `Date`, in seconds since the Unix epoch.
    ///
    /// The run's build stamp rather than the moment of the publish, so a run
    /// pinned with `--build-date` writes the same `Release` twice over. A
    /// publish clock would leave two runs of one pinned build differing in the
    /// one file the pin cannot reach. See
    /// [`BuildStamp::seconds`](crate::version::BuildStamp::seconds).
    pub date: i64,
    /// The `Origin`, or `None` to write none.
    pub origin: Option<String>,
    /// The `Label`, or `None` to write none.
    pub label: Option<String>,
    /// The `Description`, or `None` to write none.
    pub description: Option<String>,
}

/// A local `dists/`-structured `.deb` pool.
pub struct LocalPool {
    dir: PathBuf,
    suite: String,
    component: String,
    architecture: String,
    /// The release fields a publish stamps in, or `None` for a pool opened to
    /// read. See [`LocalPool::new`] and [`LocalPool::publishing`].
    release: Option<PoolRelease>,
    /// Whether the pool holds any package to resolve against. Seeded from the
    /// pool on disk at [`init`](Self::init) — so a resumed run sees a prior run's
    /// packages — and set by [`publish`](Self::publish).
    ///
    /// Atomic because parallel workers publish and read it concurrently. A
    /// worker never observes it stale in a way that matters: the scheduler
    /// releases a component only once every in-set producer's `publish` has
    /// returned, so a component that needs the pool always sees it populated.
    ///
    /// What is not fixed is what a component that does *not* need the pool sees.
    /// It is released as soon as a worker is free, so on a cold pool whether it
    /// declares the pool at all — and, warm or cold, which packages the pool
    /// held when it did — follows from how the run was scheduled. That is
    /// inherent to a feed-forward pool built in parallel rather than something
    /// this flag decides, and it is visible only where the pool carries a name
    /// the archive also carries. See `docs/src/how-a-build-runs.md`.
    has_packages: AtomicBool,
}

impl LocalPool {
    /// Opens the pool rooted at `dir` for `suite`/`component`/`architecture` to
    /// read: its index, and its declaration as a repository.
    ///
    /// Reading needs the coordinates alone, since they are what locate the index
    /// and name the repository. A pool opened this way carries no release
    /// fields, so publishing through it would write a `Release` with no `Date`
    /// and no identity — [`publishing`](Self::publishing) is the constructor for
    /// a pool that is written to.
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
            release: None,
            has_packages: AtomicBool::new(false),
        }
    }

    /// Opens the same pool to publish into, stamping `release` into the
    /// `Release` that [`init`](Self::init) and [`publish`](Self::publish) emit.
    pub fn publishing(
        dir: impl Into<PathBuf>,
        suite: impl Into<String>,
        component: impl Into<String>,
        architecture: impl Into<String>,
        release: PoolRelease,
    ) -> LocalPool {
        LocalPool {
            release: Some(release),
            ..LocalPool::new(dir, suite, component, architecture)
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

    /// The ferroday-cage pool writer for this pool's coordinates, carrying the
    /// release fields when the pool was opened to publish.
    ///
    /// The read-only paths go through this too, for the archive layout it
    /// computes — the index path and the `file://` mirror URL. Neither depends
    /// on the release fields, so a pool opened to read locates exactly the same
    /// files as the one that wrote them.
    fn writer(&self) -> Pool {
        let mut pool = Pool::at(&self.dir)
            .suite(self.suite.as_str())
            .component(self.component.as_str())
            .architecture(self.architecture.as_str());
        if let Some(release) = &self.release {
            pool = pool.date(release.date);
            if let Some(origin) = &release.origin {
                pool = pool.origin(origin.as_str());
            }
            if let Some(label) = &release.label {
                pool = pool.label(label.as_str());
            }
            if let Some(description) = &release.description {
                pool = pool.description(description.as_str());
            }
        }
        pool
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

    /// The pool's `Packages` index as text, empty when the pool holds nothing
    /// or does not exist yet.
    ///
    /// Read from the index directly rather than through the provisioner,
    /// because every question asked of it is about the pool as it stands rather
    /// than about a build root provisioned from it — and one of them, whether
    /// the pool already carries a package a component the run is not building
    /// would have produced, has to be settled before anything is provisioned at
    /// all. See [`Engine::run`](crate::Engine::run).
    pub fn index_text(&self) -> Result<String> {
        let path = self.index_path()?;
        match std::fs::read_to_string(&path) {
            Ok(index) => Ok(index),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(err) => Err(Error::Pool(format!("reading {}: {err}", path.display()))),
        }
    }

    /// The binary package names the pool's index lists, empty when the pool
    /// holds nothing or does not exist yet.
    pub fn indexed_packages(&self) -> Result<std::collections::BTreeSet<String>> {
        Ok(self
            .index_text()?
            .lines()
            .filter_map(|line| line.strip_prefix("Package:"))
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect())
    }

    /// The pool-relative paths of every file the pool's index names, empty when
    /// the pool holds nothing or does not exist yet.
    ///
    /// What [`prune`] must not remove.
    pub fn indexed_files(&self) -> Result<std::collections::BTreeSet<String>> {
        Ok(self
            .index_text()?
            .lines()
            .filter_map(|line| line.strip_prefix("Filename:"))
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
        self.repository().map(Some)
    }

    /// The pool as a trusted `file://` repository when its index on disk names
    /// anything, and `None` otherwise.
    ///
    /// [`as_repository`](Self::as_repository) answers from what this process has
    /// published, which a run tracks so a parallel build does not re-read the
    /// index once per component. A caller that publishes nothing has nothing to
    /// have tracked, and asks the pool itself — which is also what lets a plan
    /// read a pool an earlier run filled.
    pub fn repository_if_populated(&self) -> Result<Option<Repository>> {
        match self.index_has_packages()? {
            true => self.repository().map(Some),
            false => Ok(None),
        }
    }

    /// The pool as a trusted `file://` repository, whatever it holds.
    ///
    /// [`as_repository`](Self::as_repository) answers `None` for a pool nothing
    /// has published into, because a build must not declare a repository whose
    /// `Release` is not there yet. A caller that has read the pool's index
    /// already knows what it holds and takes the repository directly.
    pub fn repository(&self) -> Result<Repository> {
        // The pool names its own `file://` URL, so the spelling the provisioner
        // reads and the spelling the writer publishes under come from one place.
        let url = self.writer().mirror_url().map_err(Error::Debian)?;
        Repository::builder(self.suite.as_str())
            .mirror(url)
            .components([self.component.as_str()])
            .trust_unsigned(true)
            .name("src2deb-pool")
            .build()
            .map_err(Error::Debian)
    }
}

/// What to prune, and how much to keep.
#[derive(Debug, Clone)]
pub struct PruneOptions {
    /// How many versions of each binary package to keep. At least one: a pool
    /// pruned to nothing is not a pool.
    pub keep: usize,
    /// The architectures to prune, or empty for every pool the suite holds.
    pub architectures: Vec<String>,
    /// Report what would be removed without removing anything.
    pub dry_run: bool,
}

impl Default for PruneOptions {
    /// Keeps the version each pool's index names, and no other, across every
    /// architecture the suite holds.
    fn default() -> PruneOptions {
        PruneOptions {
            keep: 1,
            architectures: Vec::new(),
            dry_run: false,
        }
    }
}

/// What pruning removed, across every pool it visited.
#[derive(Debug, Clone)]
pub struct PruneReport {
    /// One entry per pool visited, in architecture order.
    pub pools: Vec<PrunedPool>,
    /// Whether anything was actually removed.
    pub dry_run: bool,
}

impl PruneReport {
    /// How many files were removed, across every pool.
    pub fn removed(&self) -> usize {
        self.pools.iter().map(|pool| pool.removed.len()).sum()
    }

    /// How many bytes were reclaimed, across every pool.
    pub fn bytes(&self) -> u64 {
        self.pools.iter().map(|pool| pool.bytes).sum()
    }
}

/// One pool's pruning.
#[derive(Debug, Clone)]
pub struct PrunedPool {
    /// The architecture the pool serves.
    pub architecture: String,
    /// The pool's directory.
    pub dir: PathBuf,
    /// The files removed, in package and then version order.
    pub removed: Vec<PrunedFile>,
    /// The bytes the removed files held.
    pub bytes: u64,
    /// The distinct binary packages the pool holds.
    pub packages: usize,
}

/// One superseded package file.
#[derive(Debug, Clone)]
pub struct PrunedFile {
    /// The binary package name.
    pub package: String,
    /// The version removed.
    pub version: String,
    /// The file's path.
    pub path: PathBuf,
}

/// Removes superseded `.deb` files from every pool the suite holds under
/// `work_dir`, keeping the newest [`keep`](PruneOptions::keep) versions of each
/// binary package.
///
/// # Why a pool accumulates
///
/// A pool's index names **one version of each package**: publishing merges the
/// new `.debs` into the index by highest version, so a package superseded by a
/// later build stops being named the moment that build publishes. The file
/// stays where it was written. Nothing resolves against it — apt reads the
/// index — and nothing removes it, so a work directory that has run nightly for
/// a month holds a month of packages behind an index naming one.
///
/// Pruning is what removes them. Keeping one version leaves the pool on disk
/// exactly matching its index; keeping more leaves a superseded `.deb` to hand
/// to someone or to roll back to, which is the only thing keeping it is good
/// for, since apt is never offered it.
///
/// # What is never removed
///
/// **A file the index names.** The index is the pool's contract with every
/// client resolving against it, and a file it names that is not there is a
/// broken archive rather than a reclaimed byte. Retention selects among the
/// versions on disk, and the indexed version is by construction the highest, so
/// this guard never fires — it is what makes that a property rather than an
/// assumption.
///
/// Because nothing indexed is removed, the index still describes the pool
/// exactly, and pruning rewrites nothing: no `Release`, no `Packages`, and no
/// signature a caller wrote over either.
///
/// # Concurrency
///
/// The caller holds the work directory. Pruning removes files a client that
/// read an *earlier* `Release` may still be fetching, so it is not safe to run
/// while a build is publishing into the same pool — which is why a build prunes
/// after its last component publishes rather than as each one does.
pub fn prune(work_dir: &Path, suite: &str, options: &PruneOptions) -> Result<PruneReport> {
    if options.keep == 0 {
        return Err(Error::Prune(
            "at least one version of each package must be kept".to_string(),
        ));
    }
    let architectures = select_architectures(work_dir, suite, &options.architectures)?;
    let mut pools = Vec::new();
    for architecture in architectures {
        pools.push(prune_pool(work_dir, suite, &architecture, options)?);
    }
    Ok(PruneReport {
        pools,
        dry_run: options.dry_run,
    })
}

/// The architectures to prune: those named, checked against the pools the work
/// directory holds, or every one it holds.
fn select_architectures(work_dir: &Path, suite: &str, named: &[String]) -> Result<Vec<String>> {
    let held = pool_architectures(work_dir, suite)?;
    if held.is_empty() {
        return Err(Error::Prune(format!(
            "there is no pool for suite {suite:?} under {}",
            work_dir.display()
        )));
    }
    if named.is_empty() {
        return Ok(held);
    }
    let mut selected = Vec::new();
    for architecture in named {
        if !held.iter().any(|known| known == architecture) {
            return Err(Error::Prune(format!(
                "there is no {suite}/{architecture} pool; the work directory holds: {}",
                held.join(", ")
            )));
        }
        if !selected.contains(architecture) {
            selected.push(architecture.clone());
        }
    }
    selected.sort();
    Ok(selected)
}

/// The architectures the work directory holds a pool for at `suite`, in name
/// order.
pub fn pool_architectures(work_dir: &Path, suite: &str) -> Result<Vec<String>> {
    let dir = work_dir.join(POOL_DIR).join(suite);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(Error::Pool(format!("reading {}: {err}", dir.display())));
        }
    };
    let mut architectures = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|err| Error::Pool(format!("reading {}: {err}", dir.display())))?;
        if entry.path().is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            architectures.push(name.to_string());
        }
    }
    architectures.sort();
    Ok(architectures)
}

/// Prunes one pool.
fn prune_pool(
    work_dir: &Path,
    suite: &str,
    architecture: &str,
    options: &PruneOptions,
) -> Result<PrunedPool> {
    let dir = pool_dir(work_dir, suite, architecture);
    let pool = LocalPool::new(&dir, suite, POOL_COMPONENT, architecture);
    let indexed = pool.indexed_files()?;

    // Every `.deb` the pool holds, grouped by the package name its file name
    // carries. By name alone rather than by name and architecture: a package
    // may be `Architecture: all` at one version and architecture-dependent at
    // the next, and it is one package throughout — the index names it once, and
    // apt resolves it by name.
    let mut versions: BTreeMap<String, Vec<PoolFile>> = BTreeMap::new();
    for path in deb_files(&dir.join(POOL_DIR))? {
        let Some(file) = PoolFile::of(path) else {
            continue;
        };
        versions.entry(file.package.clone()).or_default().push(file);
    }

    let packages = versions.len();
    let mut removed = Vec::new();
    let mut bytes = 0;
    for (package, mut files) in versions {
        // Newest first, so what is kept is the head of the list. Two files may
        // carry one version — a package that was `Architecture: all` at a
        // version and architecture-dependent at the same one has two names in
        // one directory — so the path breaks the tie and the order is total.
        // Without it the order would be the directory's, which is arbitrary,
        // and which of two equal versions survived would vary between runs.
        files.sort_by(|a, b| {
            crate::version::compare(&b.version, &a.version).then_with(|| a.path.cmp(&b.path))
        });
        for file in files.into_iter().skip(options.keep) {
            // The index's own vocabulary is a pool-relative path, so the
            // comparison is made in it.
            if file
                .relative(&dir)
                .is_some_and(|relative| indexed.contains(&relative))
            {
                continue;
            }
            bytes += std::fs::metadata(&file.path)
                .map(|meta| meta.len())
                .unwrap_or(0);
            if !options.dry_run {
                std::fs::remove_file(&file.path)
                    .map_err(|err| io_error("removing a superseded package", &file.path, err))?;
            }
            removed.push(PrunedFile {
                package: package.clone(),
                version: file.version,
                path: file.path,
            });
        }
    }
    Ok(PrunedPool {
        architecture: architecture.to_string(),
        dir,
        removed,
        bytes,
        packages,
    })
}

/// A `.deb` in the pool, with the package and version its file name names.
#[derive(Debug)]
struct PoolFile {
    path: PathBuf,
    package: String,
    version: String,
}

impl PoolFile {
    /// Reads a pool file's identity from its name, or `None` when the name is
    /// not `name_version_arch.deb`.
    ///
    /// From the name rather than from the `.deb`'s own control stanza: the pool
    /// stores a package at a path built from that name, so it is what tells two
    /// versions apart, and reading a control member out of every archive in the
    /// pool to learn what the path already says would cost a run's worth of
    /// decompression.
    fn of(path: PathBuf) -> Option<PoolFile> {
        let name = path.file_name()?.to_str()?;
        let stem = name.strip_suffix(".deb")?;
        let mut fields = stem.splitn(3, '_');
        let package = fields.next()?;
        let version = fields.next()?;
        // The pool encodes an epoch's colon in the file name, since a `:` is
        // not a character every filesystem carries; the version compares in its
        // own spelling.
        fields.next()?;
        Some(PoolFile {
            package: package.to_string(),
            version: version.replace("%3a", ":"),
            path,
        })
    }

    /// The file's path relative to the pool root, as the index spells it.
    fn relative(&self, pool_dir: &Path) -> Option<String> {
        Some(self.path.strip_prefix(pool_dir).ok()?.to_str()?.to_string())
    }
}

/// Every `.deb` under `dir`, recursively. An absent directory holds none.
fn deb_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_error("reading the pool", dir, err)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|err| io_error("reading the pool", dir, err))?
            .path();
        if path.is_dir() {
            files.extend(deb_files(&path)?);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("deb") {
            files.push(path);
        }
    }
    Ok(files)
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
    fn a_published_release_is_dated_and_names_the_identity_the_recipe_gave_it() {
        let dir = scratch("release");
        let pool = LocalPool::publishing(
            &dir,
            "forky",
            "main",
            "arm64",
            PoolRelease {
                // 2026-07-31 00:00:00 UTC.
                date: 1_785_456_000,
                origin: Some("Texor".to_string()),
                label: Some("COSMIC for Debian".to_string()),
                description: Some("COSMIC desktop packages for Debian forky".to_string()),
            },
        );
        pool.init().unwrap();
        let release = std::fs::read_to_string(pool.writer().release_path().unwrap()).unwrap();

        // The date apt reads. Without it every `apt update` against the pool
        // reports `W: Invalid 'Date' entry in Release file`.
        assert!(
            release.contains("Date: Fri, 31 Jul 2026 00:00:00 UTC"),
            "{release}",
        );
        // The fields `apt policy` renders and an apt pin matches on.
        assert!(release.contains("Origin: Texor"), "{release}");
        assert!(release.contains("Label: COSMIC for Debian"), "{release}");
        assert!(
            release.contains("Description: COSMIC desktop packages for Debian forky"),
            "{release}",
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_recipe_that_names_no_identity_gets_a_release_carrying_none() {
        // The three identity fields have no defaults: an origin names the
        // organization behind an archive, and src2deb has none to offer. The
        // date is written regardless, since apt warns on a pool that omits it.
        let dir = scratch("anonymous");
        let pool = LocalPool::publishing(
            &dir,
            "forky",
            "main",
            "arm64",
            PoolRelease {
                date: 1_785_456_000,
                origin: None,
                label: None,
                description: None,
            },
        );
        pool.init().unwrap();
        let release = std::fs::read_to_string(pool.writer().release_path().unwrap()).unwrap();

        assert!(release.contains("Date: "), "{release}");
        for absent in ["Origin:", "Label:", "Description:"] {
            assert!(!release.contains(absent), "{absent} in {release}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_publishes_at_one_build_date_write_one_release() {
        // What the date being the run's build stamp rather than the publish
        // clock buys: a run pinned with `--build-date` produces a pool that is
        // byte-identical to the one the run before it produced, `Release`
        // included. A publish clock would leave the one file the pin cannot
        // reach differing every time.
        let release_at = |label: &str| {
            let dir = scratch(label);
            let pool = LocalPool::publishing(
                &dir,
                "forky",
                "main",
                "arm64",
                PoolRelease {
                    date: 1_785_456_000,
                    origin: Some("Texor".to_string()),
                    label: None,
                    description: None,
                },
            );
            pool.init().unwrap();
            let text = std::fs::read_to_string(pool.writer().release_path().unwrap()).unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            text
        };
        assert_eq!(release_at("pinned-a"), release_at("pinned-b"));
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

#[cfg(test)]
mod prune_tests {
    use super::*;

    /// A unique scratch work directory for one test.
    fn scratch(label: &str) -> PathBuf {
        use std::sync::atomic::AtomicUsize;
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("src2deb-prune-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes a `.deb` into the pool at the layout the writer uses, holding its
    /// own name so its size varies and its identity is checkable.
    fn published(
        work: &Path,
        architecture: &str,
        name: &str,
        version: &str,
        arch: &str,
    ) -> PathBuf {
        let file = format!("{name}_{}_{arch}.deb", version.replace(':', "%3a"));
        let prefix = name.chars().next().unwrap().to_string();
        let dir = pool_dir(work, "trixie", architecture)
            .join(POOL_DIR)
            .join(POOL_COMPONENT)
            .join(prefix)
            .join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(&file);
        std::fs::write(&path, file.as_bytes()).unwrap();
        path
    }

    /// Writes a `Packages` index naming `files`, as the writer would.
    fn index(work: &Path, architecture: &str, files: &[&Path]) {
        let dir = pool_dir(work, "trixie", architecture);
        let binary = dir
            .join("dists")
            .join("trixie")
            .join(POOL_COMPONENT)
            .join(format!("binary-{architecture}"));
        std::fs::create_dir_all(&binary).unwrap();
        let text: String = files
            .iter()
            .map(|path| {
                let relative = path.strip_prefix(&dir).unwrap().to_str().unwrap();
                format!("Package: p\nFilename: {relative}\n\n")
            })
            .collect();
        std::fs::write(binary.join("Packages"), text).unwrap();
    }

    #[test]
    fn pruning_keeps_the_newest_versions_in_debian_order() {
        let work = scratch("order");
        // Deliberately out of both alphabetical and creation order: retention
        // is by version comparison, not by name or by mtime. `1.10` above `1.9`
        // is the case a lexical sort gets wrong.
        for version in ["1.9", "1.10", "1.0~rc1", "1.2"] {
            published(&work, "arm64", "p", version, "arm64");
        }
        let newest = published(&work, "arm64", "p", "2.0", "arm64");
        index(&work, "arm64", &[&newest]);

        let report = prune(&work, "trixie", &PruneOptions::default()).unwrap();
        assert_eq!(report.removed(), 4);
        assert!(newest.is_file());
        let left: Vec<String> = deb_files(&pool_dir(&work, "trixie", "arm64").join(POOL_DIR))
            .unwrap()
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["p_2.0_arm64.deb"]);

        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn keeping_more_than_one_leaves_the_versions_below_the_newest() {
        let work = scratch("keep");
        for version in ["1.0", "1.1", "1.2"] {
            published(&work, "arm64", "p", version, "arm64");
        }
        let newest = published(&work, "arm64", "p", "1.3", "arm64");
        index(&work, "arm64", &[&newest]);

        let options = PruneOptions {
            keep: 2,
            ..PruneOptions::default()
        };
        let report = prune(&work, "trixie", &options).unwrap();
        let removed: Vec<&str> = report.pools[0]
            .removed
            .iter()
            .map(|file| file.version.as_str())
            .collect();
        assert_eq!(removed, ["1.1", "1.0"]);
        assert_eq!(report.pools[0].packages, 1);
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn two_files_at_one_version_are_ordered_by_path_rather_than_by_the_directory() {
        let work = scratch("tie");
        // A package that was `Architecture: all` at a version and
        // architecture-dependent at the same one: two names in one directory,
        // one version. Which survives must not depend on the order the
        // directory happens to list them in.
        let all = published(&work, "arm64", "p", "1.0", "all");
        let arm = published(&work, "arm64", "p", "1.0", "arm64");
        index(&work, "arm64", &[]);

        let report = prune(&work, "trixie", &PruneOptions::default()).unwrap();
        assert_eq!(report.removed(), 1);
        // Sorted by path within the tie, so the `all` file is the one kept —
        // and it is kept whichever order the filesystem enumerated them in.
        assert!(all.is_file());
        assert!(!arm.exists());
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn a_file_the_index_names_is_never_removed() {
        let work = scratch("indexed");
        // An index naming a version that is not the newest on disk cannot arise
        // from publishing, which indexes by highest version. The guard is what
        // makes "pruning never breaks the index" a property of the code rather
        // than a consequence of how the pool happens to be written.
        let older = published(&work, "arm64", "p", "1.0", "arm64");
        published(&work, "arm64", "p", "2.0", "arm64");
        index(&work, "arm64", &[&older]);

        let report = prune(&work, "trixie", &PruneOptions::default()).unwrap();
        assert_eq!(report.removed(), 0);
        assert!(older.is_file());
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn a_package_is_one_package_whatever_architecture_its_versions_carry() {
        let work = scratch("archall");
        // A component whose package became `Architecture: all` between builds:
        // one package, so retention keeps the newest across both spellings
        // rather than the newest of each.
        let old = published(&work, "arm64", "p", "1.0", "arm64");
        let new = published(&work, "arm64", "p", "2.0", "all");
        index(&work, "arm64", &[&new]);

        let report = prune(&work, "trixie", &PruneOptions::default()).unwrap();
        assert_eq!(report.removed(), 1);
        assert!(!old.exists());
        assert!(new.is_file());
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn an_epoch_orders_by_its_value_and_not_by_its_encoded_name() {
        let work = scratch("epoch");
        // The pool encodes the colon, so the file names read `1%3a1.0`. Sorted
        // as written, `2.0` would look like the later version; as versions,
        // the epoch outranks it.
        let plain = published(&work, "arm64", "p", "2.0", "arm64");
        let epoch = published(&work, "arm64", "p", "1:1.0", "arm64");
        index(&work, "arm64", &[&epoch]);

        let report = prune(&work, "trixie", &PruneOptions::default()).unwrap();
        assert_eq!(report.removed(), 1);
        assert_eq!(report.pools[0].removed[0].version, "2.0");
        assert!(!plain.exists());
        assert!(epoch.is_file());
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn a_dry_run_reports_what_it_would_remove_and_removes_nothing() {
        let work = scratch("dry");
        let old = published(&work, "arm64", "p", "1.0", "arm64");
        let new = published(&work, "arm64", "p", "2.0", "arm64");
        index(&work, "arm64", &[&new]);

        let options = PruneOptions {
            dry_run: true,
            ..PruneOptions::default()
        };
        let report = prune(&work, "trixie", &options).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.removed(), 1);
        assert_eq!(report.bytes(), std::fs::metadata(&old).unwrap().len());
        assert!(old.is_file());
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn every_architecture_is_pruned_unless_one_is_named() {
        let work = scratch("arches");
        for architecture in ["amd64", "arm64"] {
            published(&work, architecture, "p", "1.0", architecture);
            let new = published(&work, architecture, "p", "2.0", architecture);
            index(&work, architecture, &[&new]);
        }
        let report = prune(&work, "trixie", &PruneOptions::default()).unwrap();
        assert_eq!(report.pools.len(), 2);
        assert_eq!(report.removed(), 2);

        // Named, only that pool is visited.
        for architecture in ["amd64", "arm64"] {
            published(&work, architecture, "p", "1.5", architecture);
        }
        let options = PruneOptions {
            architectures: vec!["arm64".to_string()],
            ..PruneOptions::default()
        };
        let report = prune(&work, "trixie", &options).unwrap();
        assert_eq!(report.pools.len(), 1);
        assert_eq!(report.pools[0].architecture, "arm64");
        assert_eq!(report.removed(), 1);
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn a_suite_or_architecture_with_no_pool_says_so() {
        let work = scratch("none");
        let err = prune(&work, "trixie", &PruneOptions::default()).unwrap_err();
        assert!(format!("{err}").contains("no pool for suite"), "{err}");

        published(&work, "arm64", "p", "1.0", "arm64");
        let options = PruneOptions {
            architectures: vec!["amd64".to_string()],
            ..PruneOptions::default()
        };
        let err = prune(&work, "trixie", &options).unwrap_err();
        assert!(format!("{err}").contains("arm64"), "{err}");

        // Keeping nothing is refused rather than emptying the pool.
        let options = PruneOptions {
            keep: 0,
            ..PruneOptions::default()
        };
        let err = prune(&work, "trixie", &options).unwrap_err();
        assert!(format!("{err}").contains("at least one"), "{err}");
        std::fs::remove_dir_all(&work).unwrap();
    }
}
