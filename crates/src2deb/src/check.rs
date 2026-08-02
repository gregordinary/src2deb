//! Checking that the packages a pool holds can be installed.
//!
//! A build validates the *build* graph: every component's build-dependencies
//! resolve, or it does not build. Nothing in that says the packages it produces
//! can be installed. A run can go twenty-six for twenty-six and still publish an
//! archive apt refuses, because a runtime `Depends` names something neither the
//! target suite nor the pool has — a package that only exists in Ubuntu, one
//! that was transitional in the last release and is gone from this one, or one
//! that is simply not packaged yet.
//!
//! This module answers that question against the pool, which is where the
//! packages and their fully-substituted relationships both are. `debian/control`
//! declares `${shlibs:Depends}`; the `.deb` carries what it expanded to, and
//! ferroday-cage's pool writer keeps each `.deb`'s control stanza verbatim in
//! the index. Reading the pool therefore sees the dependencies a client will
//! see, not the ones the packaging was written with.
//!
//! # How a dependency is settled
//!
//! Availability is answered by the same resolver a build root is provisioned
//! with, over the same archives: the target suite, the recipe's additional
//! repositories, and the pool itself. Two stages, in order of cost:
//!
//! 1. **One resolve of the whole pool.** Every package the pool holds is an
//!    `include`, so the resolver closes over all of their `Depends` and
//!    `Pre-Depends` and returns the install set. Every name in that set is a
//!    package that exists, so a dependency naming one is settled — as is one
//!    naming a virtual package the pool itself provides.
//! 2. **A probe for whatever is left.** The install set names real packages, so
//!    it cannot vouch for a dependency an *archive* package satisfies virtually
//!    through `Provides`: the provider is in the set under its own name. What
//!    the first stage leaves unexplained is therefore asked directly — as one
//!    resolve for the whole residue, which answers it when nothing is missing,
//!    and name by name when something is.
//!
//! The first stage is a sieve rather than an oracle, and that is what makes the
//! result independent of it: a name it explains is available, and a name it does
//! not is asked about explicitly. Nothing is reported unsatisfiable that the
//! resolver was not asked about by name.
//!
//! # What is checked, and what is not
//!
//! `Depends` and `Pre-Depends`, which are what make a package installable. A
//! `Recommends` apt cannot satisfy is passed over by apt rather than refused, so
//! it does not belong in an answer about installability.
//!
//! Names, not versions. A dependency's version constraint is not enforced, for
//! the reason the provisioner does not enforce one either: a suite is internally
//! consistent, and the version a package resolves to in it is the version the
//! suite ships. What this catches is a dependency on a package that is not there
//! at all, which is the failure that reaches a target machine.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ferroday_cage::provision::debian::{
    Debian, DebianBuilder, DebianError, Fetch, FetchError, FetchRequest, HttpFetch, Repository,
};

use crate::error::{Error, Result};
use crate::pool::LocalPool;
use crate::recipe::Recipe;

/// Which pools to check.
#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    /// The architectures to check, or empty for every pool the suite holds.
    pub architectures: Vec<String>,
}

/// A progress event from a check, delivered to the reporter passed to
/// [`Engine::check`](crate::Engine::check).
///
/// Its own vocabulary rather than the build's [`Progress`](crate::Progress),
/// because a check is not a build: nothing here is resolved, provisioned, or
/// built, and the two things a check has to say are things no build reports.
///
/// Both events precede the work they describe. A check's cost is entirely in
/// resolving against the archive — fetching a suite's package index and closing
/// over it — which is silent and takes seconds, so what it is about to do is
/// worth more than what it has finished.
#[non_exhaustive]
pub enum CheckProgress<'a> {
    /// A pool's packages are about to be resolved against the archive.
    /// Reported once per pool, after the pools to visit are settled — so a
    /// check with none to visit announces nothing.
    Resolving {
        /// The architecture whose pool it is.
        architecture: &'a str,
        /// How many binary packages the pool holds.
        packages: usize,
    },
    /// The pool's install closure did not account for every dependency it
    /// declares, so the remainder is being resolved by name.
    ///
    /// Reported once per pool and only when there is a remainder, which is the
    /// point at which a check costs more than the single resolve it usually
    /// does. Ordinarily the remainder is small and is settled in one more
    /// resolve; a remainder that holds something genuinely missing costs one
    /// resolve per name.
    ResolvingNames {
        /// How many dependency names are left to resolve.
        names: usize,
    },
}

/// The relationship field a dependency was declared in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Relationship {
    /// `Pre-Depends`, which must be configured before this package unpacks.
    PreDepends,
    /// `Depends`, the ordinary runtime dependency.
    Depends,
}

impl Relationship {
    /// The field name, spelled as `debian/control` spells it.
    pub fn field(self) -> &'static str {
        match self {
            Relationship::PreDepends => "Pre-Depends",
            Relationship::Depends => "Depends",
        }
    }
}

/// One dependency of one package that nothing available can satisfy.
#[derive(Debug, Clone)]
pub struct Unsatisfied {
    /// The package that declares it.
    pub package: String,
    /// That package's version, as the pool's index names it.
    pub version: String,
    /// The field the dependency was declared in.
    pub relationship: Relationship,
    /// The dependency as the field spells it, alternatives and version
    /// constraints included — so what is reported can be found in the packaging
    /// by searching for it.
    pub clause: String,
    /// The package names the clause would accept, none of which is available.
    pub alternatives: Vec<String>,
}

/// One pool's check.
#[derive(Debug, Clone)]
pub struct CheckedPool {
    /// The architecture the pool serves.
    pub architecture: String,
    /// The pool's directory.
    pub dir: PathBuf,
    /// How many binary packages the pool's index names.
    pub packages: usize,
    /// How many dependency clauses were checked across them.
    pub clauses: usize,
    /// The clauses nothing satisfies, ordered by package name and then by
    /// field, `Pre-Depends` before `Depends`.
    ///
    /// Sorted rather than reported in the order the pool's index happens to
    /// list its packages in, so a report is a function of what the pool holds
    /// rather than of how it was read.
    pub unsatisfied: Vec<Unsatisfied>,
}

/// What a check found, across every pool it visited.
#[derive(Debug, Clone)]
pub struct CheckReport {
    /// The suite every pool was checked against.
    pub suite: String,
    /// One entry per pool visited, in architecture order.
    pub pools: Vec<CheckedPool>,
}

impl CheckReport {
    /// Whether every dependency of every package in every pool can be
    /// satisfied.
    pub fn is_clean(&self) -> bool {
        self.pools.iter().all(|pool| pool.unsatisfied.is_empty())
    }

    /// How many dependency clauses could not be satisfied, across every pool.
    pub fn unsatisfied(&self) -> usize {
        self.pools.iter().map(|pool| pool.unsatisfied.len()).sum()
    }

    /// How many binary packages were checked, across every pool.
    ///
    /// One built for two architectures counts once per pool rather than once
    /// overall: each pool is an archive of its own, and it is that archive's
    /// copy that was resolved.
    pub fn packages(&self) -> usize {
        self.pools.iter().map(|pool| pool.packages).sum()
    }
}

/// Checks every pool the recipe's suite holds under `work_dir`, reporting the
/// runtime dependencies nothing available satisfies.
///
/// The caller holds the work directory: this reads a pool a build publishes
/// into, so it must not run while one is writing it. See
/// [`Engine::check`](crate::Engine::check), which takes the lock.
///
/// The recipe supplies the suite, the mirror, and its additional repositories —
/// the archives a build root resolves against, so a dependency is judged
/// available exactly where the recipe says packages come from. Its own
/// `architecture` field is not consulted: which pools are checked comes from the
/// work directory and from [`CheckOptions::architectures`], the same as
/// [`prune`](crate::pool::prune) and [`export`](crate::export::export).
pub fn check(
    work_dir: &Path,
    recipe: &Recipe,
    options: &CheckOptions,
    reporter: &mut dyn FnMut(CheckProgress),
) -> Result<CheckReport> {
    let architectures = select_architectures(work_dir, &recipe.suite, &options.architectures)?;
    // One cache for the whole check, so the release and index every resolve
    // needs are fetched once however many architectures are checked and however
    // many probes the residue costs.
    let cache = Cache::default();
    let mut pools = Vec::new();
    for architecture in architectures {
        pools.push(check_pool(
            work_dir,
            recipe,
            &architecture,
            &cache,
            reporter,
        )?);
    }
    Ok(CheckReport {
        suite: recipe.suite.clone(),
        pools,
    })
}

/// The architectures to check: those named, checked against the pools the work
/// directory holds, or every one it holds.
///
/// A named architecture with no pool is an error rather than an empty result,
/// since it is a typo or a build that has not happened, and both are worth being
/// told about. This is [`select_architectures`](crate::pool) in the pool module
/// phrased for a check; the two are kept apart because the messages name the
/// operation, and a shared one would name neither.
fn select_architectures(work_dir: &Path, suite: &str, named: &[String]) -> Result<Vec<String>> {
    let held = crate::pool::pool_architectures(work_dir, suite)?;
    if held.is_empty() {
        return Err(Error::Check(format!(
            "there is no pool for suite {suite:?} under {}; build it before checking it",
            work_dir.display()
        )));
    }
    if named.is_empty() {
        return Ok(held);
    }
    let mut selected = Vec::new();
    for architecture in named {
        if !held.iter().any(|known| known == architecture) {
            return Err(Error::Check(format!(
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

/// Checks one architecture's pool.
fn check_pool(
    work_dir: &Path,
    recipe: &Recipe,
    architecture: &str,
    cache: &Cache,
    reporter: &mut dyn FnMut(CheckProgress),
) -> Result<CheckedPool> {
    let dir = crate::pool::pool_dir(work_dir, &recipe.suite, architecture);
    let pool = LocalPool::new(
        &dir,
        recipe.suite.clone(),
        crate::pool::POOL_COMPONENT,
        architecture.to_string(),
    );
    let packages = read_index(&pool.index_text()?);
    let clauses = packages
        .iter()
        .map(|package| package.relations.len())
        .sum::<usize>();
    // A pool holding nothing has nothing to resolve, and asking the archive
    // would be asking about the base system alone.
    let unsatisfied = if packages.is_empty() {
        Vec::new()
    } else {
        reporter(CheckProgress::Resolving {
            architecture,
            packages: packages.len(),
        });
        let mut archive = DebianArchive::new(recipe, architecture, pool.repository()?, cache);
        unsatisfied(&packages, &mut archive, reporter)?
    };
    Ok(CheckedPool {
        architecture: architecture.to_string(),
        dir,
        packages: packages.len(),
        clauses,
        unsatisfied,
    })
}

/// The clauses `packages` declare that nothing `archive` offers can satisfy.
///
/// The two-stage sieve the module documentation describes: the install closure
/// of the whole pool settles most clauses at the cost of one resolve, and
/// whatever it leaves unexplained is asked about by name.
fn unsatisfied(
    packages: &[PoolPackage],
    archive: &mut dyn Archive,
    reporter: &mut dyn FnMut(CheckProgress),
) -> Result<Vec<Unsatisfied>> {
    let names: Vec<String> = packages
        .iter()
        .map(|package| package.name.clone())
        .collect();
    // The closure names every real package the pool's dependencies reach; the
    // pool's own `Provides` cover the virtual names it satisfies itself. Every
    // pool package is an include, so a provider of a pool virtual is always in
    // the closure and needs no further check.
    let mut available = archive.closure(&names)?;
    available.extend(
        packages
            .iter()
            .flat_map(|package| package.provides.iter().cloned()),
    );

    // What the closure did not explain: every alternative of every clause no
    // alternative of which is available. A clause satisfied by one alternative
    // contributes nothing, so the residue is only what has to be asked about.
    let residue: Vec<String> = packages
        .iter()
        .flat_map(|package| package.relations.iter())
        .filter(|relation| !relation.satisfied_by(&available))
        .flat_map(|relation| relation.alternatives.iter().cloned())
        .filter(|name| !available.contains(name))
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();

    if !residue.is_empty() {
        reporter(CheckProgress::ResolvingNames {
            names: residue.len(),
        });
        // One resolve answers the whole residue whenever every name in it
        // exists, which is the ordinary case: the residue is what an archive
        // package provides virtually. Only a residue that fails has to be taken
        // apart, and it is the small set by construction.
        if archive.available(&residue)? {
            available.extend(residue);
        } else {
            for name in residue {
                if archive.available(std::slice::from_ref(&name))? {
                    available.insert(name);
                }
            }
        }
    }

    let mut unsatisfied: Vec<Unsatisfied> = packages
        .iter()
        .flat_map(|package| {
            package
                .relations
                .iter()
                .filter(|relation| !relation.satisfied_by(&available))
                .map(|relation| Unsatisfied {
                    package: package.name.clone(),
                    version: package.version.clone(),
                    relationship: relation.relationship,
                    clause: relation.clause.clone(),
                    alternatives: relation.alternatives.clone(),
                })
        })
        .collect();
    // A stable sort, so the clauses of one field stay in the order the field
    // declared them while the packages and fields themselves come out in an
    // order the pool's index cannot vary.
    unsatisfied.sort_by(|a, b| (&a.package, a.relationship).cmp(&(&b.package, b.relationship)));
    Ok(unsatisfied)
}

/// One binary package as the pool's index describes it.
#[derive(Debug)]
struct PoolPackage {
    name: String,
    version: String,
    /// The virtual package names it provides.
    provides: Vec<String>,
    /// Its `Pre-Depends` then `Depends` clauses, in declaration order.
    relations: Vec<Relation>,
}

/// One dependency clause: what it accepts, and how it was written.
#[derive(Debug)]
struct Relation {
    relationship: Relationship,
    /// The clause verbatim, for reporting.
    clause: String,
    /// The package names it would accept, in declaration order.
    alternatives: Vec<String>,
}

impl Relation {
    /// Whether any alternative is among the available names.
    fn satisfied_by(&self, available: &BTreeSet<String>) -> bool {
        self.alternatives
            .iter()
            .any(|name| available.contains(name))
    }
}

/// Reads the packages a pool's `Packages` index describes.
///
/// The index carries each `.deb`'s control stanza verbatim, so the relationships
/// here are the ones a client resolves against — `${shlibs:Depends}` already
/// expanded into the library packages the build linked against.
fn read_index(index: &str) -> Vec<PoolPackage> {
    stanzas(index)
        .into_iter()
        .filter_map(|stanza| {
            let name = stanza.get("package")?.clone();
            let mut relations = Vec::new();
            for (field, relationship) in [
                ("pre-depends", Relationship::PreDepends),
                ("depends", Relationship::Depends),
            ] {
                if let Some(value) = stanza.get(field) {
                    relations.extend(clauses(value, relationship));
                }
            }
            Some(PoolPackage {
                name,
                version: stanza.get("version").cloned().unwrap_or_default(),
                provides: stanza
                    .get("provides")
                    .map(|value| {
                        value
                            .split(',')
                            .filter_map(package_name)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                relations,
            })
        })
        .collect()
}

/// Splits a deb822 document into stanzas, each mapping a lowercased field name
/// to its value.
///
/// Field names are matched without regard to case, as dpkg matches them, so they
/// are lowercased once here rather than at each lookup. A continuation line —
/// one beginning with whitespace — is folded onto its field with a single space
/// between, which is exactly right for a relationship field wrapped across
/// lines and lossy only for a `Description`, which nothing here reads.
fn stanzas(text: &str) -> Vec<BTreeMap<String, String>> {
    let mut stanzas = Vec::new();
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut last: Option<String> = None;
    // A trailing empty line closes the last stanza, so a document that does not
    // end with a blank line still yields its final stanza.
    for line in text.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if !fields.is_empty() {
                stanzas.push(std::mem::take(&mut fields));
            }
            last = None;
        } else if line.starts_with([' ', '\t']) {
            if let Some(field) = last.as_ref().and_then(|name| fields.get_mut(name)) {
                field.push(' ');
                field.push_str(line.trim());
            }
        } else if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            fields.insert(name.clone(), value.trim().to_string());
            last = Some(name);
        }
    }
    stanzas
}

/// The clauses a relationship field's value declares, in order.
///
/// A clause with no readable package name in it — which well-formed control
/// never produces — contributes nothing rather than an unsatisfiable entry
/// naming nothing.
fn clauses(value: &str, relationship: Relationship) -> Vec<Relation> {
    value
        .split(',')
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .filter_map(|clause| {
            let alternatives: Vec<String> = clause
                .split('|')
                .filter_map(package_name)
                .map(str::to_string)
                .collect();
            (!alternatives.is_empty()).then(|| Relation {
                relationship,
                clause: clause.to_string(),
                alternatives,
            })
        })
        .collect()
}

/// The package name one alternative names, without whatever qualifies it.
///
/// A name runs to the first character that begins something else: a version
/// constraint (`(>= 1.2)`), a multi-arch qualifier (`:any`), an architecture
/// restriction (`[linux-any]`), or a build profile (`<!nocheck>`). The last two
/// belong to source relationships rather than binary ones, and cost nothing to
/// pass over here.
fn package_name(atom: &str) -> Option<&str> {
    let atom = atom.trim();
    let end = atom
        .find(|c: char| c.is_whitespace() || matches!(c, '(' | ':' | '[' | '<'))
        .unwrap_or(atom.len());
    let name = &atom[..end];
    (!name.is_empty()).then_some(name)
}

/// The archives a package resolves against, asked whether names are available.
///
/// A seam rather than a direct call to the provisioner, so the sieve above can
/// be exercised over a known set of names. Only [`DebianArchive`] implements it
/// outside the tests.
trait Archive {
    /// The names of every package in the install closure of `packages`,
    /// including `packages` themselves.
    ///
    /// A dependency the closure cannot satisfy is dropped from it rather than
    /// failing it — the resolver's own discipline, since dpkg configures with
    /// `--force-depends` — which is what makes the closure a sieve: it names
    /// what is available and stays silent about what is not.
    fn closure(&mut self, packages: &[String]) -> Result<BTreeSet<String>>;

    /// Whether every one of `names` is available, as a real package or as a
    /// virtual one something provides.
    fn available(&mut self, names: &[String]) -> Result<bool>;
}

/// [`Archive`] over the Debian provisioner: the target suite, the recipe's
/// additional repositories, and the pool.
///
/// Exactly the archives a build root is provisioned from, so a dependency counts
/// as available where the recipe says packages come from. Resolving downloads no
/// package and unpacks nothing; it fetches each archive's release and index and
/// closes over them, which is why a foreign architecture is checked as readily
/// as the host's.
struct DebianArchive<'a> {
    suite: &'a str,
    architecture: &'a str,
    mirror: Option<&'a str>,
    repositories: &'a [crate::recipe::Repository],
    pool: Repository,
    cache: &'a Cache,
}

impl<'a> DebianArchive<'a> {
    /// Creates the archive for `recipe` at `architecture`, resolving against
    /// `pool` alongside it and sharing `cache` between every resolve.
    fn new(
        recipe: &'a Recipe,
        architecture: &'a str,
        pool: Repository,
        cache: &'a Cache,
    ) -> DebianArchive<'a> {
        DebianArchive {
            suite: &recipe.suite,
            architecture,
            mirror: recipe.mirror.as_deref(),
            repositories: &recipe.repositories,
            pool,
            cache,
        }
    }

    /// A provisioner including `packages`, over every archive this resolves
    /// against.
    fn resolving(&self, packages: &[String]) -> Result<Debian<'static>> {
        let builder: DebianBuilder<'static> = Debian::builder(self.suite.to_string())
            .architecture(self.architecture.to_string())
            .fetcher(Box::new(self.cache.fetcher()));
        let builder = match self.mirror {
            Some(mirror) => builder.mirror(mirror.to_string()),
            None => builder,
        };
        crate::provision::add_repositories(builder, self.repositories, self.suite, self.mirror)?
            .repository(self.pool.clone())
            .include(packages.iter().cloned())
            .build()
            .map_err(Error::Debian)
    }
}

impl Archive for DebianArchive<'_> {
    fn closure(&mut self, packages: &[String]) -> Result<BTreeSet<String>> {
        let plan = self.resolving(packages)?.resolve().map_err(Error::Debian)?;
        Ok(plan
            .packages
            .into_iter()
            .map(|package| package.name)
            .collect())
    }

    fn available(&mut self, names: &[String]) -> Result<bool> {
        match self.resolving(names)?.resolve() {
            Ok(_) => Ok(true),
            // The resolver fails an `include` that is neither a real package nor
            // a provided virtual one, which is the answer this asks for. Its
            // other two resolution failures both concern excluded packages, and
            // nothing here excludes any, so a resolution failure means a name is
            // not there.
            Err(DebianError::Resolve { .. }) => Ok(false),
            Err(err) => Err(Error::Debian(err)),
        }
    }
}

/// The bodies fetched so far, keyed by URL, shared between every resolve a check
/// makes.
///
/// A resolve fetches each archive's release and package index, and a check makes
/// between one and a handful of them — one for the pool's closure, and one per
/// name when a residue has to be taken apart. Without this each would refetch
/// the same index, which for a Debian suite is the whole cost of the check
/// several times over.
///
/// Caching for the life of one check also gives every resolve in it one
/// consistent view of the archive, so a mirror that changes underneath a check
/// cannot make its answers disagree with each other.
#[derive(Debug, Default, Clone)]
struct Cache {
    /// `None` records a resource the archive does not have, which the
    /// provisioner asks about deliberately — it tries each index compression in
    /// turn — and which is as worth remembering as a body.
    bodies: Arc<Mutex<BTreeMap<String, Option<Vec<u8>>>>>,
}

impl Cache {
    /// A transport that answers from this cache, filling it on a miss.
    ///
    /// One per resolve, since a provisioner owns its transport; they share the
    /// cache itself.
    fn fetcher(&self) -> CachingFetch {
        CachingFetch {
            bodies: Arc::clone(&self.bodies),
            inner: HttpFetch::new(),
        }
    }
}

/// The transport a check fetches through: the default HTTP and `file://`
/// fetcher behind a per-check cache.
struct CachingFetch {
    bodies: Arc<Mutex<BTreeMap<String, Option<Vec<u8>>>>>,
    inner: HttpFetch,
}

impl Fetch for CachingFetch {
    fn fetch(
        &mut self,
        request: &FetchRequest<'_>,
        sink: &mut dyn Write,
    ) -> std::result::Result<(), FetchError> {
        let url = request.url();
        // Held only long enough to read, so a fetch never runs under the lock.
        let cached = self
            .bodies
            .lock()
            .ok()
            .and_then(|bodies| bodies.get(url).cloned());
        if let Some(cached) = cached {
            return match cached {
                Some(body) => sink
                    .write_all(&body)
                    .map_err(|err| FetchError::io(url, "writing the cached body", err)),
                None => Err(FetchError::not_found(url)),
            };
        }

        let mut body = Vec::new();
        let outcome = self.inner.fetch(request, &mut body);
        // Only a definite absence is remembered. A transport failure may be
        // transient, and caching one would turn a retry into the same failure.
        let store = match &outcome {
            Ok(()) => Some(Some(body.clone())),
            Err(FetchError::NotFound { .. }) => Some(None),
            Err(_) => None,
        };
        if let Some(entry) = store
            && let Ok(mut bodies) = self.bodies.lock()
        {
            bodies.insert(url.to_string(), entry);
        }
        outcome?;
        sink.write_all(&body)
            .map_err(|err| FetchError::io(url, "writing the body", err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An [`Archive`] over a fixed set of available names, recording what it was
    /// asked so the sieve's cost can be asserted as well as its answer.
    struct Fixed {
        /// Every name that is available, real or virtual.
        available: BTreeSet<String>,
        /// The names each closure would select, keyed by the include set's
        /// first name — enough to model "the closure explains these".
        closure: BTreeSet<String>,
        /// One entry per `available` call, holding what it was asked.
        probes: Vec<Vec<String>>,
    }

    impl Fixed {
        fn new(available: &[&str], closure: &[&str]) -> Fixed {
            Fixed {
                available: available.iter().map(|name| name.to_string()).collect(),
                closure: closure.iter().map(|name| name.to_string()).collect(),
                probes: Vec::new(),
            }
        }
    }

    impl Archive for Fixed {
        fn closure(&mut self, packages: &[String]) -> Result<BTreeSet<String>> {
            let mut names = self.closure.clone();
            names.extend(packages.iter().cloned());
            Ok(names)
        }

        fn available(&mut self, names: &[String]) -> Result<bool> {
            self.probes.push(names.to_vec());
            Ok(names.iter().all(|name| self.available.contains(name)))
        }
    }

    /// Runs the sieve over an index, returning what it could not satisfy and the
    /// probes it made.
    fn sieve(index: &str, archive: &mut Fixed) -> Vec<Unsatisfied> {
        let packages = read_index(index);
        unsatisfied(&packages, archive, &mut |_| {}).unwrap()
    }

    #[test]
    fn a_stanza_folds_its_continuation_lines() {
        let index = "Package: cosmic-comp\nVersion: 1.0\n\
                     Depends: libc6 (>= 2.41),\n \
                     libwayland-server0,\n\tlibxkbcommon0\n\
                     \nPackage: other\nVersion: 2\n";
        let packages = read_index(index);
        assert_eq!(packages.len(), 2);
        // Both continuation forms fold in, and the clause count is the one the
        // field declares rather than the line count.
        let names: Vec<&str> = packages[0]
            .relations
            .iter()
            .flat_map(|relation| relation.alternatives.iter().map(String::as_str))
            .collect();
        assert_eq!(names, ["libc6", "libwayland-server0", "libxkbcommon0"]);
    }

    #[test]
    fn a_field_is_read_however_it_is_spelled() {
        // dpkg matches field names case-insensitively, so a control stanza that
        // spells one unconventionally is still read.
        let packages = read_index("PACKAGE: p\nversion: 1\nPre-DEPENDS: init-system-helpers\n");
        assert_eq!(packages[0].name, "p");
        assert_eq!(packages[0].version, "1");
        assert_eq!(packages[0].relations.len(), 1);
        assert_eq!(
            packages[0].relations[0].relationship,
            Relationship::PreDepends
        );
    }

    #[test]
    fn a_stanza_with_no_package_field_is_not_a_package() {
        // The index's own header, or anything else that is not a package
        // stanza, contributes nothing rather than a nameless entry.
        assert!(read_index("Architecture: amd64\nVersion: 1\n").is_empty());
    }

    #[test]
    fn a_name_is_read_without_what_qualifies_it() {
        assert_eq!(package_name("libc6 (>= 2.41)"), Some("libc6"));
        assert_eq!(package_name("  python3:any  "), Some("python3"));
        assert_eq!(package_name("gcc [linux-any]"), Some("gcc"));
        assert_eq!(package_name("pkg <!nocheck>"), Some("pkg"));
        assert_eq!(package_name("plain"), Some("plain"));
        // Nothing readable as a name yields none, so a malformed clause is
        // passed over rather than reported against a name of nothing.
        assert_eq!(package_name("   "), None);
        assert_eq!(package_name("(>= 1)"), None);
    }

    #[test]
    fn a_clause_keeps_its_spelling_and_its_alternatives() {
        let relations = clauses(
            "default-dbus-session-bus | dbus-session-bus, libc6 (>= 2.41)",
            Relationship::Depends,
        );
        assert_eq!(relations.len(), 2);
        assert_eq!(
            relations[0].alternatives,
            ["default-dbus-session-bus", "dbus-session-bus"]
        );
        // The clause is reported as written, so it can be found in the packaging
        // by searching for it.
        assert_eq!(relations[1].clause, "libc6 (>= 2.41)");
        assert_eq!(relations[1].alternatives, ["libc6"]);
        // An empty field declares nothing.
        assert!(clauses("", Relationship::Depends).is_empty());
        assert!(clauses("  ,  ", Relationship::Depends).is_empty());
    }

    #[test]
    fn a_dependency_the_closure_names_costs_no_probe() {
        let index = "Package: cosmic-comp\nVersion: 1.0\nDepends: libc6, libwayland-server0\n";
        let mut archive = Fixed::new(&[], &["libc6", "libwayland-server0"]);
        assert!(sieve(index, &mut archive).is_empty());
        // The whole point of the first stage: a pool whose dependencies all
        // resolve costs exactly one resolve.
        assert!(archive.probes.is_empty());
    }

    #[test]
    fn a_dependency_on_another_pool_package_is_satisfied() {
        // The dbgsym case, and the one in-set build edge: the closure includes
        // every package the pool holds, because they are all included.
        let index = "Package: cosmic-comp\nVersion: 1.0\n\
                     \nPackage: cosmic-comp-dbgsym\nVersion: 1.0\nDepends: cosmic-comp (= 1.0)\n";
        let mut archive = Fixed::new(&[], &[]);
        assert!(sieve(index, &mut archive).is_empty());
        assert!(archive.probes.is_empty());
    }

    #[test]
    fn a_virtual_the_pool_provides_itself_is_satisfied() {
        let index = "Package: cosmic-session\nVersion: 1.0\nDepends: cosmic-wm\n\
                     \nPackage: cosmic-comp\nVersion: 1.0\nProvides: cosmic-wm, x-window-manager\n";
        let mut archive = Fixed::new(&[], &[]);
        assert!(sieve(index, &mut archive).is_empty());
        assert!(archive.probes.is_empty());
    }

    #[test]
    fn a_virtual_the_archive_provides_is_settled_by_one_probe() {
        // The closure names the provider under its own name, so the dependency's
        // own name is not in it. One probe for the whole residue answers it,
        // which is the ordinary shape of a clean pool with a virtual dependency.
        let index = "Package: cosmic-term\nVersion: 1.0\nDepends: x-terminal-emulator, awk\n";
        let mut archive = Fixed::new(&["x-terminal-emulator", "awk"], &[]);
        assert!(sieve(index, &mut archive).is_empty());
        assert_eq!(archive.probes, [["awk", "x-terminal-emulator"]]);
    }

    #[test]
    fn a_missing_dependency_is_reported_against_the_package_that_declares_it() {
        let index = "Package: cosmic-settings\nVersion: 1.0+deb13\n\
                     Depends: libc6, network-manager-gnome\n";
        let mut archive = Fixed::new(&[], &["libc6"]);
        let unsatisfied = sieve(index, &mut archive);
        assert_eq!(unsatisfied.len(), 1);
        assert_eq!(unsatisfied[0].package, "cosmic-settings");
        assert_eq!(unsatisfied[0].version, "1.0+deb13");
        assert_eq!(unsatisfied[0].relationship, Relationship::Depends);
        assert_eq!(unsatisfied[0].clause, "network-manager-gnome");
        // The residue failed as a batch, so it was taken apart name by name.
        assert_eq!(
            archive.probes,
            [
                vec!["network-manager-gnome".to_string()],
                vec!["network-manager-gnome".to_string()],
            ]
        );
    }

    #[test]
    fn one_missing_name_does_not_condemn_the_rest_of_the_residue() {
        // The reason the batch probe falls back to one probe per name rather
        // than reporting the whole residue: a residue of a virtual that exists
        // and a package that does not must report only the second.
        let index = "Package: p\nVersion: 1\nDepends: awk, casper\n";
        let mut archive = Fixed::new(&["awk"], &[]);
        let unsatisfied = sieve(index, &mut archive);
        assert_eq!(unsatisfied.len(), 1);
        assert_eq!(unsatisfied[0].clause, "casper");
    }

    #[test]
    fn an_alternative_that_is_available_satisfies_its_clause() {
        let index = "Package: p\nVersion: 1\nDepends: gone | present, missing | also-missing\n";
        let mut archive = Fixed::new(&["present"], &[]);
        let unsatisfied = sieve(index, &mut archive);
        assert_eq!(unsatisfied.len(), 1);
        // Both alternatives are reported, since neither is available and the
        // packaging may be fixed by either.
        assert_eq!(unsatisfied[0].clause, "missing | also-missing");
        assert_eq!(unsatisfied[0].alternatives, ["missing", "also-missing"]);
    }

    #[test]
    fn a_pre_depends_is_checked_and_reported_as_one() {
        let index = "Package: p\nVersion: 1\nPre-Depends: missing-early\nDepends: libc6\n";
        let mut archive = Fixed::new(&[], &["libc6"]);
        let unsatisfied = sieve(index, &mut archive);
        assert_eq!(unsatisfied.len(), 1);
        assert_eq!(unsatisfied[0].relationship, Relationship::PreDepends);
        assert_eq!(unsatisfied[0].relationship.field(), "Pre-Depends");
    }

    #[test]
    fn findings_are_ordered_by_package_and_then_by_field() {
        // Declared back to front, and with `Depends` before `Pre-Depends`, so
        // neither the index's order nor the stanza's can decide the report's.
        let index = "Package: zed\nVersion: 1\nDepends: missing-z1, missing-z2\n\
                     \nPackage: alpha\nVersion: 1\n\
                     Depends: missing-a\nPre-Depends: missing-early\n";
        let mut archive = Fixed::new(&[], &[]);
        let found: Vec<String> = sieve(index, &mut archive)
            .into_iter()
            .map(|entry| {
                format!(
                    "{} {} {}",
                    entry.package,
                    entry.relationship.field(),
                    entry.clause
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                "alpha Pre-Depends missing-early",
                "alpha Depends missing-a",
                // Within one field the field's own order stands.
                "zed Depends missing-z1",
                "zed Depends missing-z2",
            ],
        );
    }

    #[test]
    fn a_recommends_is_not_checked() {
        // apt passes over a Recommends it cannot satisfy rather than refusing
        // the package, so it is not part of an answer about installability.
        let index = "Package: p\nVersion: 1\nRecommends: nothing-provides-this\n";
        let mut archive = Fixed::new(&[], &[]);
        assert!(sieve(index, &mut archive).is_empty());
        assert!(archive.probes.is_empty());
    }

    #[test]
    fn the_report_counts_what_it_found() {
        let report = CheckReport {
            suite: "trixie".to_string(),
            pools: vec![
                CheckedPool {
                    architecture: "amd64".to_string(),
                    dir: PathBuf::from("/work/pool/trixie/amd64"),
                    packages: 27,
                    clauses: 210,
                    unsatisfied: Vec::new(),
                },
                CheckedPool {
                    architecture: "arm64".to_string(),
                    dir: PathBuf::from("/work/pool/trixie/arm64"),
                    packages: 27,
                    clauses: 210,
                    unsatisfied: vec![Unsatisfied {
                        package: "cosmic-settings".to_string(),
                        version: "1.0".to_string(),
                        relationship: Relationship::Depends,
                        clause: "network-manager-gnome".to_string(),
                        alternatives: vec!["network-manager-gnome".to_string()],
                    }],
                },
            ],
        };
        assert!(!report.is_clean());
        assert_eq!(report.unsatisfied(), 1);
        assert_eq!(report.packages(), 54);
        // One clean pool does not make the report clean: the archive is only as
        // installable as its least installable architecture.
        assert!(report.pools[0].unsatisfied.is_empty());
    }

    #[test]
    fn a_cached_body_is_served_without_a_second_fetch() {
        let dir = std::env::temp_dir().join(format!("src2deb-check-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Release");
        std::fs::write(&path, b"first").unwrap();
        let url = format!("file://{}", path.display());

        let cache = Cache::default();
        let mut body = Vec::new();
        cache
            .fetcher()
            .fetch(&FetchRequest::new(&url), &mut body)
            .unwrap();
        assert_eq!(body, b"first");

        // The file changes underneath the check; the cache keeps every resolve
        // seeing the archive as the first one found it.
        std::fs::write(&path, b"second").unwrap();
        let mut again = Vec::new();
        cache
            .fetcher()
            .fetch(&FetchRequest::new(&url), &mut again)
            .unwrap();
        assert_eq!(again, b"first");

        // An absence is remembered too, since the provisioner asks after index
        // compressions the archive may not carry.
        let missing = format!("file://{}", dir.join("Packages.xz").display());
        for _ in 0..2 {
            let outcome = cache
                .fetcher()
                .fetch(&FetchRequest::new(&missing), &mut Vec::new());
            assert!(matches!(outcome, Err(FetchError::NotFound { .. })));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
