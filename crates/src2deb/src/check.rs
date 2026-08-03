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
//! One pass over the archives a build root is provisioned from — the target
//! suite, the recipe's additional repositories, and the pool itself — projected
//! down to the names they offer. Every dependency of every pool package is then
//! answered against that set directly: a clause is satisfiable when the archives
//! carry any of the names it accepts.
//!
//! The projection names real packages and virtual ones alike, so a dependency an
//! archive package satisfies through `Provides` is settled the same way one
//! naming a real package is — and the providers are reported alongside, so a
//! dependency on a name several packages offer is not read as a dependency on a
//! package. It costs one release and index fetch per pool for any number of
//! names.
//!
//! # What is checked, and what is not
//!
//! `Depends` and `Pre-Depends`, which are what make a package installable. A
//! `Recommends` apt cannot satisfy is passed over by apt rather than refused, so
//! it does not belong in an answer about installability.
//!
//! Names, not versions. A dependency's version constraint and its architecture
//! qualifier are parsed away, for the reason the provisioner parses them away
//! too: a suite is internally consistent, and the version a package resolves to
//! in it is the version the suite ships. What this catches is a dependency on a
//! package that is not there at all, which is the failure that reaches a target
//! machine. A dependency on a version the suite does not carry is not caught
//! here, and never was.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ferroday_cage::provision::debian::Repository;

use crate::error::{Error, Result};
use crate::pool::LocalPool;
use crate::provision::Names;
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
/// built, and what a check has to say is something no build reports.
///
/// The event precedes the work it describes. A check's cost is entirely in
/// reading the archives — fetching each one's release and package index — which
/// is silent and takes seconds, so what it is about to do is worth more than
/// what it has finished.
#[non_exhaustive]
pub enum CheckProgress<'a> {
    /// A pool's archives are about to be read. Reported once per pool, after
    /// the pools to visit are settled — so a check with none to visit announces
    /// nothing.
    Reading {
        /// The architecture whose pool it is.
        architecture: &'a str,
        /// How many binary packages the pool holds.
        packages: usize,
    },
}

/// The relationship field a dependency was declared in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Relationship {
    /// `Pre-Depends`, which must be configured before this package unpacks.
    PreDepends,
    /// `Depends`, the ordinary runtime dependency.
    Depends,
    /// `Recommends`, which apt installs by default and passes over when it
    /// cannot satisfy.
    ///
    /// Never produced by a pool check, which answers installability and so reads
    /// only what makes a package installable. A
    /// [plan-time reading](crate::plan::runtime_relationships) reports it,
    /// because a `Recommends` nothing satisfies is a gap in what a recipe
    /// delivers even though it is not a gap in what apt will install.
    Recommends,
}

impl Relationship {
    /// The field name, spelled as `debian/control` spells it.
    pub fn field(self) -> &'static str {
        match self {
            Relationship::PreDepends => "Pre-Depends",
            Relationship::Depends => "Depends",
            Relationship::Recommends => "Recommends",
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

/// One dependency satisfied by a name that something in the archives provides.
///
/// Ordinarily that is the whole story: nothing carries the name, and the clause
/// installs only because apt picks one of several providers — which is apt's
/// decision rather than the packaging's, and a weaker satisfaction than a direct
/// one. A dependency on `awk` is satisfiable everywhere and is a different fact
/// from a dependency on `gawk`.
///
/// Policy 7.5 also permits a package to provide a name a *real* package carries,
/// which is what a transitional package does. Such a clause is satisfied
/// directly and appears here all the same: the projection the check reads names
/// what the archives offer and who provides what, without saying which of the
/// two a given name is. So this reports that a name is provided, not that it is
/// only provided.
#[derive(Debug, Clone)]
pub struct Provided {
    /// The package that declares it.
    pub package: String,
    /// The field the dependency was declared in.
    pub relationship: Relationship,
    /// The dependency as the field spells it.
    pub clause: String,
    /// The alternative that is satisfied, which is the first the clause names
    /// that the archives offer — the order apt itself prefers them in.
    pub name: String,
    /// The real packages that provide [`name`](Self::name), in the archives'
    /// own order.
    pub providers: Vec<String>,
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
    /// The clauses satisfied by a name something provides, in the same order.
    ///
    /// Not a problem, and reported separately for that reason: these install.
    /// They are what a reader has to know to tell "this dependency names a
    /// package" from "this dependency names something several packages offer".
    /// See [`Provided`] for the one case where the second reading is too
    /// strong.
    pub provided: Vec<Provided>,
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
    let mut pools = Vec::new();
    for architecture in architectures {
        pools.push(check_pool(work_dir, recipe, &architecture, reporter)?);
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
    // A pool holding nothing declares no dependency, and reading the archives
    // would be reading them to answer no question.
    let (unsatisfied, provided) = if packages.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        reporter(CheckProgress::Reading {
            architecture,
            packages: packages.len(),
        });
        let mut archive = DebianArchive::new(recipe, architecture, pool.repository()?);
        examine(&packages, &mut archive)?
    };
    Ok(CheckedPool {
        architecture: architecture.to_string(),
        dir,
        packages: packages.len(),
        clauses,
        unsatisfied,
        provided,
    })
}

/// Answers every clause `packages` declare against the names `archive` offers:
/// what nothing satisfies, and what a provider does.
///
/// One pass over the archives, then a membership question per alternative. The
/// projection already carries every virtual name and the pool's own packages
/// among them, so there is no residue to take apart and no second question to
/// ask.
fn examine(
    packages: &[PoolPackage],
    archive: &mut dyn Archive,
) -> Result<(Vec<Unsatisfied>, Vec<Provided>)> {
    let available = archive.available()?;

    let mut unsatisfied = Vec::new();
    let mut provided = Vec::new();
    for package in packages {
        for relation in &package.relations {
            // The first alternative the archives offer, which is the one apt
            // prefers: a clause names its alternatives in preference order.
            let Some(name) = relation
                .alternatives
                .iter()
                .find(|name| available.contains(name))
            else {
                unsatisfied.push(Unsatisfied {
                    package: package.name.clone(),
                    version: package.version.clone(),
                    relationship: relation.relationship,
                    clause: relation.clause.clone(),
                    alternatives: relation.alternatives.clone(),
                });
                continue;
            };
            // A package is never a provider of itself, so a non-empty list
            // means something else offers this name. That is the virtual case
            // whenever no real package carries the name — which is the ordinary
            // one — and a transitional `Provides` over a real name otherwise.
            // The projection does not separate the two, and [`Provided`] is
            // worded for what it can say.
            let providers = available.providers(name);
            if !providers.is_empty() {
                provided.push(Provided {
                    package: package.name.clone(),
                    relationship: relation.relationship,
                    clause: relation.clause.clone(),
                    name: name.clone(),
                    providers,
                });
            }
        }
    }

    // A stable sort, so the clauses of one field stay in the order the field
    // declared them while the packages and fields themselves come out in an
    // order the pool's index cannot vary.
    unsatisfied.sort_by(|a, b| (&a.package, a.relationship).cmp(&(&b.package, b.relationship)));
    provided.sort_by(|a, b| (&a.package, a.relationship).cmp(&(&b.package, b.relationship)));
    Ok((unsatisfied, provided))
}

/// One binary package as the pool's index describes it.
///
/// It carries no `Provides` of its own. The pool is one of the archives read,
/// so a virtual name a pool package offers is in the projection already,
/// alongside the ones the suite's packages offer.
#[derive(Debug)]
struct PoolPackage {
    name: String,
    version: String,
    /// Its `Pre-Depends` then `Depends` clauses, in declaration order.
    relations: Vec<Relation>,
}

/// One dependency clause: what it accepts, and how it was written.
#[derive(Debug)]
struct Relation {
    relationship: Relationship,
    /// The clause verbatim, for reporting.
    clause: String,
    /// The package names it would accept, in declaration order, which is the
    /// order apt prefers them in.
    alternatives: Vec<String>,
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
pub(crate) fn stanzas(text: &str) -> Vec<BTreeMap<String, String>> {
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
pub(crate) fn package_name(atom: &str) -> Option<&str> {
    let atom = atom.trim();
    let end = atom
        .find(|c: char| c.is_whitespace() || matches!(c, '(' | ':' | '[' | '<'))
        .unwrap_or(atom.len());
    let name = &atom[..end];
    (!name.is_empty()).then_some(name)
}

/// The archives a package resolves against, asked what names they offer.
///
/// A seam rather than a direct call to the provisioner, so the reading above can
/// be exercised over a known set of names. Only [`DebianArchive`] implements it
/// outside the tests.
trait Archive {
    /// Every name the archives offer, real or provided.
    fn available(&mut self) -> Result<Box<dyn Names>>;
}

/// [`Archive`] over the Debian provisioner: the target suite, the recipe's
/// additional repositories, and the pool.
///
/// Exactly the archives a build root is provisioned from, so a dependency counts
/// as available where the recipe says packages come from. Reading them downloads
/// no package, unpacks nothing, and resolves nothing; it fetches each archive's
/// release and index and projects them to their names, which is why a foreign
/// architecture is read as readily as the host's.
struct DebianArchive<'a> {
    recipe: &'a Recipe,
    architecture: &'a str,
    pool: Repository,
}

impl<'a> DebianArchive<'a> {
    /// Creates the archive for `recipe` at `architecture`, reading `pool`
    /// alongside it.
    fn new(recipe: &'a Recipe, architecture: &'a str, pool: Repository) -> DebianArchive<'a> {
        DebianArchive {
            recipe,
            architecture,
            pool,
        }
    }
}

impl Archive for DebianArchive<'_> {
    fn available(&mut self) -> Result<Box<dyn Names>> {
        crate::provision::available_names(self.recipe, self.architecture, Some(self.pool.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An [`Archive`] over a fixed set of names, counting how many times it was
    /// read so a check's cost can be asserted as well as its answer.
    struct Fixed {
        names: Names_,
        /// How many times [`Archive::available`] was called.
        reads: std::cell::Cell<usize>,
    }

    /// The projection [`Fixed`] hands back: real names, and providers for the
    /// virtual ones.
    #[derive(Clone, Default)]
    struct Names_ {
        real: std::collections::BTreeSet<String>,
        providers: BTreeMap<String, Vec<String>>,
    }

    impl Fixed {
        /// An archive offering `real` and nothing virtual.
        fn new(real: &[&str]) -> Fixed {
            Fixed {
                names: Names_ {
                    real: real.iter().map(|name| name.to_string()).collect(),
                    providers: BTreeMap::new(),
                },
                reads: std::cell::Cell::new(0),
            }
        }

        /// The same, with `name` offered virtually by `providers`.
        fn providing(mut self, name: &str, providers: &[&str]) -> Fixed {
            self.names.providers.insert(
                name.to_string(),
                providers.iter().map(|name| name.to_string()).collect(),
            );
            self
        }
    }

    impl Names for Names_ {
        fn contains(&self, name: &str) -> bool {
            self.real.contains(name) || self.providers.contains_key(name)
        }

        fn providers(&self, name: &str) -> Vec<String> {
            self.providers.get(name).cloned().unwrap_or_default()
        }
    }

    impl Archive for Fixed {
        fn available(&mut self) -> Result<Box<dyn Names>> {
            self.reads.set(self.reads.get() + 1);
            Ok(Box::new(self.names.clone()))
        }
    }

    /// Runs a check over an index, returning what it could not satisfy.
    fn sieve(index: &str, archive: &mut Fixed) -> Vec<Unsatisfied> {
        examine(&read_index(index), archive).unwrap().0
    }

    /// Runs a check over an index, returning what a provider satisfies.
    fn provided(index: &str, archive: &mut Fixed) -> Vec<Provided> {
        examine(&read_index(index), archive).unwrap().1
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
    fn a_whole_pool_is_answered_by_one_read_of_the_archives() {
        let index = "Package: cosmic-comp\nVersion: 1.0\nDepends: libc6, libwayland-server0\n\
                     \nPackage: cosmic-term\nVersion: 1.0\nDepends: libc6, awk\n";
        let mut archive =
            Fixed::new(&["libc6", "libwayland-server0"]).providing("awk", &["gawk", "mawk"]);
        assert!(sieve(index, &mut archive).is_empty());
        // The whole point: any number of names costs one pass over the index,
        // where a resolve per name re-fetched and re-parsed it every time.
        assert_eq!(archive.reads.get(), 1);
    }

    #[test]
    fn a_dependency_on_another_pool_package_is_satisfied() {
        // The dbgsym case, and the one in-set build edge. The pool is one of the
        // archives read, so its own packages are among the names offered.
        let index = "Package: cosmic-comp\nVersion: 1.0\n\
                     \nPackage: cosmic-comp-dbgsym\nVersion: 1.0\nDepends: cosmic-comp (= 1.0)\n";
        let mut archive = Fixed::new(&["cosmic-comp", "cosmic-comp-dbgsym"]);
        assert!(sieve(index, &mut archive).is_empty());
    }

    #[test]
    fn a_virtual_the_pool_provides_itself_is_satisfied() {
        // Likewise for a virtual name: the pool's index carries its `Provides`,
        // so the projection knows the name and knows what offers it.
        let index = "Package: cosmic-session\nVersion: 1.0\nDepends: cosmic-wm\n\
                     \nPackage: cosmic-comp\nVersion: 1.0\nProvides: cosmic-wm\n";
        let mut archive =
            Fixed::new(&["cosmic-session", "cosmic-comp"]).providing("cosmic-wm", &["cosmic-comp"]);
        assert!(sieve(index, &mut archive).is_empty());
    }

    #[test]
    fn a_virtual_dependency_names_what_provides_it() {
        // What a resolver-based probe could not report at all: the dependency is
        // satisfied, and by something other than a package of that name.
        let index = "Package: cosmic-term\nVersion: 1.0\nDepends: libc6, awk\n";
        let mut archive = Fixed::new(&["libc6"]).providing("awk", &["gawk", "mawk"]);
        assert!(sieve(index, &mut archive).is_empty());

        let provided = provided(index, &mut archive);
        assert_eq!(provided.len(), 1);
        assert_eq!(provided[0].package, "cosmic-term");
        assert_eq!(provided[0].name, "awk");
        assert_eq!(provided[0].providers, ["gawk", "mawk"]);
        assert_eq!(provided[0].clause, "awk");
    }

    #[test]
    fn a_dependency_on_a_real_package_names_no_provider() {
        // A package is not a provider of itself, so a direct satisfaction is
        // reported as one rather than as a virtual with a single provider.
        let index = "Package: p\nVersion: 1\nDepends: libc6\n";
        let mut archive = Fixed::new(&["libc6"]);
        assert!(provided(index, &mut archive).is_empty());
    }

    #[test]
    fn a_name_that_is_both_real_and_provided_is_reported_as_provided() {
        // Debian policy 7.5 permits a package to provide a name a real package
        // also carries, which is what a transitional package does: `git-core`
        // provides `git`, and `git` is a package. The clause is satisfied
        // directly, and the archives are read as the names they offer rather
        // than as a catalogue separating real from virtual — so the entry says
        // the name is provided, which is all it can say, and nothing gates on
        // it either way.
        let index = "Package: p\nVersion: 1\nDepends: git\n";
        let mut archive = Fixed::new(&["git", "git-core"]).providing("git", &["git-core"]);
        assert!(sieve(index, &mut archive).is_empty());

        let provided = provided(index, &mut archive);
        assert_eq!(provided.len(), 1);
        assert_eq!(provided[0].name, "git");
        assert_eq!(provided[0].providers, ["git-core"]);
    }

    #[test]
    fn a_missing_dependency_is_reported_against_the_package_that_declares_it() {
        let index = "Package: cosmic-settings\nVersion: 1.0+deb13\n\
                     Depends: libc6, network-manager-gnome\n";
        let mut archive = Fixed::new(&["libc6"]);
        let unsatisfied = sieve(index, &mut archive);
        assert_eq!(unsatisfied.len(), 1);
        assert_eq!(unsatisfied[0].package, "cosmic-settings");
        assert_eq!(unsatisfied[0].version, "1.0+deb13");
        assert_eq!(unsatisfied[0].relationship, Relationship::Depends);
        assert_eq!(unsatisfied[0].clause, "network-manager-gnome");
    }

    #[test]
    fn one_missing_name_does_not_condemn_the_others() {
        // Each name is answered on its own, so a pool depending on a virtual
        // that exists and a package that does not reports only the second.
        let index = "Package: p\nVersion: 1\nDepends: awk, casper\n";
        let mut archive = Fixed::new(&[]).providing("awk", &["gawk"]);
        let unsatisfied = sieve(index, &mut archive);
        assert_eq!(unsatisfied.len(), 1);
        assert_eq!(unsatisfied[0].clause, "casper");
    }

    #[test]
    fn an_alternative_that_is_available_satisfies_its_clause() {
        let index = "Package: p\nVersion: 1\nDepends: gone | present, missing | also-missing\n";
        let mut archive = Fixed::new(&["present"]);
        let unsatisfied = sieve(index, &mut archive);
        assert_eq!(unsatisfied.len(), 1);
        // Both alternatives are reported, since neither is available and the
        // packaging may be fixed by either.
        assert_eq!(unsatisfied[0].clause, "missing | also-missing");
        assert_eq!(unsatisfied[0].alternatives, ["missing", "also-missing"]);
    }

    #[test]
    fn the_first_available_alternative_is_the_one_reported_on() {
        // A clause names its alternatives in preference order, and apt takes the
        // first it can. A later alternative being virtual says nothing about the
        // clause, so the reading stops at the one apt would take.
        let index = "Package: p\nVersion: 1\nDepends: present | awk\n";
        let mut archive = Fixed::new(&["present"]).providing("awk", &["gawk"]);
        assert!(sieve(index, &mut archive).is_empty());
        assert!(provided(index, &mut archive).is_empty());
    }

    #[test]
    fn a_pre_depends_is_checked_and_reported_as_one() {
        let index = "Package: p\nVersion: 1\nPre-Depends: missing-early\nDepends: libc6\n";
        let mut archive = Fixed::new(&["libc6"]);
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
        let mut archive = Fixed::new(&[]);
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
        let mut archive = Fixed::new(&[]);
        assert!(sieve(index, &mut archive).is_empty());
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
                    provided: Vec::new(),
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
                    provided: Vec::new(),
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
}
