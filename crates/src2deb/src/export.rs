//! Handing a run's packages to an archive.
//!
//! The [`pool`](crate::pool) is a build-time carrier and a local serving pool:
//! it is keyed to one suite and one architecture, and its index names one
//! version of each package. A published archive is a different shape — one
//! `Release` covering every architecture of a suite, managed by an archive tool
//! with a release process of its own. Reaching into the work directory to bridge
//! the two means depending on layout src2deb is free to change.
//!
//! An export is that bridge, and it is a contract: a directory of the files an
//! archive tool ingests, in a layout this chapter fixes.
//!
//! ```text
//! <dest>/<suite>/
//! ├── export.toml
//! ├── manifests/<recipe>/<architecture>.toml
//! ├── cosmic-comp_1.0.0+deb13.20260802.abc1234_arm64.deb
//! ├── cosmic-comp_1.0.0+deb13.20260802.abc1234_arm64.buildinfo
//! ├── cosmic-comp_1.0.0+deb13.20260802.abc1234_arm64.changes
//! └── cosmic-icons_1.0.0+deb13.20260802.abc1234_all.deb
//! ```
//!
//! The packages are flat, so the whole suite is one argument to an archive tool
//! that scans a directory. Beside them travel the `.changes` and `.buildinfo`
//! each build wrote, and a copy of each architecture's provenance manifest — so
//! what a publisher archives beside a release is the record of how it was built,
//! without reading anything under the work directory.
//!
//! # What an export carries
//!
//! Every component the recipe's manifests currently record as built, for every
//! architecture the work directory holds a manifest for. That is the archive's
//! current state rather than the last run's output: a `--skip-published` run
//! builds two components of twenty-six, and an archive wants all twenty-six.
//! The manifest carries a built record forward for exactly this reason.
//!
//! # `Architecture: all` packages
//!
//! An arch-indep package's file name carries no architecture and its stamped
//! version does not vary with one, so a recipe built for two architectures
//! produces one file name over two sets of bytes. An export carries one of
//! them, chosen by the recipe where it says. Declaring
//! [`arch-indep-owner`](crate::Recipe::arch_indep_owner) settles which, and
//! stops the second from being built at all.
//!
//! # Repeating an export
//!
//! An export replaces the one before it. [`EXPORT_INDEX`] names every file the
//! export wrote, so the next export into the same directory removes exactly
//! those and writes its own — a scheduled run stays idempotent, and a superseded
//! version never reaches the archive by being left behind.
//!
//! The index is keyed by recipe, so several recipes may export into one
//! directory and each replaces only its own files. That is the ordinary case for
//! an archive that publishes more than one recipe's packages into one suite.
//!
//! Two invariants make the replacement safe:
//!
//! - **An export removes only files an index of its own named.** A file the
//!   destination held for any other reason is left where it is.
//! - **The index is written before the files it names.** An export interrupted
//!   partway through therefore leaves nothing the next export cannot account
//!   for; the alternative order leaves orphans, which is the one outcome
//!   replacement exists to prevent.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::build;
use crate::error::{Error, Result, io_error};
use crate::manifest::{self, Manifest, STATUS_BUILT};
use crate::recipe::Recipe;

/// The name of the index an export writes at the root of its suite directory.
pub const EXPORT_INDEX: &str = "export.toml";

/// The directory within an export that holds the copied provenance manifests.
pub const EXPORT_MANIFEST_DIR: &str = "manifests";

/// What to export, and where.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// The destination root. The export is written to `<dest>/<suite>/`.
    pub dest: PathBuf,
    /// The architectures to carry, or empty for every architecture the work
    /// directory records a manifest for.
    pub architectures: Vec<String>,
}

impl ExportOptions {
    /// Exports every architecture the work directory records, to `dest`.
    pub fn to(dest: impl Into<PathBuf>) -> ExportOptions {
        ExportOptions {
            dest: dest.into(),
            architectures: Vec::new(),
        }
    }
}

/// The index an export writes, naming what it carries.
///
/// Read back by the next export into the same directory, which is what makes an
/// export replace its predecessor rather than accumulate beside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportIndex {
    /// The suite every recipe in this directory was built for.
    pub suite: String,
    /// One entry per recipe exported here, in name order.
    #[serde(rename = "recipe", default)]
    pub recipes: Vec<RecipeExport>,
}

/// One recipe's contribution to an export directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeExport {
    /// The recipe name.
    pub name: String,
    /// The architectures carried, in name order.
    pub architectures: Vec<String>,
    /// The provenance manifests copied into the export, relative to the suite
    /// directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manifests: Vec<String>,
    /// One entry per component and architecture that contributed a file.
    #[serde(rename = "component", default)]
    pub components: Vec<ExportedComponent>,
}

impl RecipeExport {
    /// Every file this recipe's export owns, relative to the suite directory.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.manifests.iter().map(String::as_str).chain(
            self.components
                .iter()
                .flat_map(|component| component.files.iter().map(String::as_str)),
        )
    }
}

/// One component's files, from the architecture whose build produced them.
///
/// A component appears once per architecture it contributed from, so a
/// component built for two architectures has two entries — and one whose
/// `Architecture: all` packages were taken from elsewhere has one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedComponent {
    /// The component name.
    pub name: String,
    /// The architecture whose build produced these files.
    pub architecture: String,
    /// The files, relative to the suite directory.
    pub files: Vec<String>,
}

/// What an export carried.
#[derive(Debug, Clone)]
pub struct ExportReport {
    /// The suite directory written, `<dest>/<suite>`.
    pub dir: PathBuf,
    /// The architectures carried, in name order.
    pub architectures: Vec<String>,
    /// How many distinct components contributed at least one package, counted
    /// over every architecture rather than per architecture.
    pub components: usize,
    /// How many distinct binary packages were carried, likewise.
    pub packages: usize,
    /// Every file written, including the copied manifests.
    pub files: usize,
    /// The total size of the files written.
    pub bytes: u64,
    /// The files a prior export left that this one removed.
    pub removed: usize,
    /// The `Architecture: all` packages built by more than one architecture,
    /// one entry per package.
    pub duplicates: Vec<Duplicate>,
}

/// An `Architecture: all` package produced by more than one architecture, and
/// which copy the export carried.
#[derive(Debug, Clone)]
pub struct Duplicate {
    /// The binary package name.
    pub package: String,
    /// The architecture whose copy was carried.
    pub kept: String,
    /// The architectures whose copies were dropped, in name order.
    pub dropped: Vec<String>,
}

/// One file to carry: where it is now, and what it is called in the export.
#[derive(Debug, Clone)]
struct Carried {
    source: PathBuf,
    name: String,
}

/// One component's build output for one architecture, before deduplication.
#[derive(Debug)]
struct ComponentOutput {
    component: String,
    architecture: String,
    /// The `.deb` and `.ddeb` files, each with the package name it holds and
    /// whether it is architecture-independent.
    packages: Vec<PackageFile>,
    /// The `.changes` and `.buildinfo` beside them.
    records: Vec<Carried>,
}

/// One built package file.
#[derive(Debug)]
struct PackageFile {
    carried: Carried,
    package: String,
    version: String,
    /// Whether the file's name declares it `Architecture: all`.
    arch_indep: bool,
}

/// Exports every package the recipe's manifests record as built under
/// `work_dir` into `<dest>/<suite>/`.
///
/// The caller holds the work directory: an export reads the output trees and
/// manifests a run writes, so it must not run while one is writing them. See
/// [`Engine::export`](crate::Engine::export), which takes the lock.
///
/// The recipe supplies the name, the suite, and — when it declares one — the
/// architecture that owns arch-indep output. Its own `architecture` field is not
/// consulted: which architectures an export carries comes from the work
/// directory and from [`ExportOptions::architectures`], so retargeting a recipe
/// for the sake of an export would say nothing.
pub fn export(work_dir: &Path, recipe: &Recipe, options: &ExportOptions) -> Result<ExportReport> {
    let architectures = select_architectures(work_dir, recipe, &options.architectures)?;

    // Read every architecture's build output before writing anything, so an
    // export that cannot be assembled leaves the destination as it found it.
    let mut outputs = Vec::new();
    for architecture in &architectures {
        outputs.extend(read_architecture(work_dir, recipe, architecture)?);
    }
    let duplicates = deduplicate(&mut outputs, recipe.arch_indep_owner.as_deref());

    let dir = options.dest.join(&recipe.suite);
    let manifests = manifest_files(work_dir, recipe, &architectures);
    let entry = RecipeExport {
        name: recipe.name.clone(),
        architectures: architectures.clone(),
        manifests: manifests.iter().map(|file| file.name.clone()).collect(),
        components: outputs
            .iter()
            .filter(|output| !output.packages.is_empty())
            .map(|output| ExportedComponent {
                name: output.component.clone(),
                architecture: output.architecture.clone(),
                files: output.files().map(|file| file.name.clone()).collect(),
            })
            .collect(),
    };

    // The index lands before the files it names, so no file this export writes
    // is ever unaccounted for. See the module documentation.
    let prior = read_index(&dir, recipe)?;
    let index = merge_index(prior.as_ref(), &recipe.suite, entry.clone());
    std::fs::create_dir_all(&dir)
        .map_err(|err| io_error("creating the export directory", &dir, err))?;
    write_index(&dir, &index)?;

    let removed = remove_superseded(&dir, prior.as_ref(), &entry)?;

    let mut bytes = 0;
    let mut files = 0;
    for carried in manifests
        .iter()
        .chain(outputs.iter().flat_map(ComponentOutput::files))
    {
        bytes += copy_file(carried, &dir)?;
        files += 1;
    }

    // Both counted by distinct name, over every architecture: one component
    // built for two of them is one component, and a package built for two is one
    // package. Counting the index's entries instead would report a
    // single-component recipe as two.
    let mut packages: BTreeSet<&str> = BTreeSet::new();
    let mut components: BTreeSet<&str> = BTreeSet::new();
    for output in outputs.iter().filter(|out| !out.packages.is_empty()) {
        components.insert(output.component.as_str());
        packages.extend(output.packages.iter().map(|file| file.package.as_str()));
    }
    Ok(ExportReport {
        dir,
        architectures,
        components: components.len(),
        packages: packages.len(),
        files,
        bytes,
        removed,
        duplicates,
    })
}

impl ComponentOutput {
    /// Every file this component contributes, packages first.
    fn files(&self) -> impl Iterator<Item = &Carried> {
        self.packages
            .iter()
            .map(|package| &package.carried)
            .chain(self.records.iter())
    }
}

/// The architectures to export: those named, checked against what the work
/// directory records, or every one it records.
///
/// A named architecture the work directory has no manifest for is an error
/// rather than an empty export, since it is a typo or a build that has not
/// happened, and both are worth being told about.
fn select_architectures(work_dir: &Path, recipe: &Recipe, named: &[String]) -> Result<Vec<String>> {
    let recorded = recorded_architectures(work_dir, &recipe.name, &recipe.suite)?;
    if recorded.is_empty() {
        return Err(Error::Export(format!(
            "no run has recorded a build of recipe {:?} for suite {:?} under {}; \
             build it before exporting it",
            recipe.name,
            recipe.suite,
            work_dir.display()
        )));
    }
    if named.is_empty() {
        return Ok(recorded);
    }
    let mut selected: Vec<String> = Vec::new();
    for architecture in named {
        if !recorded.iter().any(|known| known == architecture) {
            return Err(Error::Export(format!(
                "no run has recorded a build of recipe {:?} for {}/{architecture}; \
                 the work directory holds: {}",
                recipe.name,
                recipe.suite,
                recorded.join(", ")
            )));
        }
        if !selected.contains(architecture) {
            selected.push(architecture.clone());
        }
    }
    selected.sort();
    Ok(selected)
}

/// The architectures the work directory holds a manifest for, in name order.
///
/// The manifests are the record of what was built, so they are what an export
/// enumerates — rather than the output tree, which holds a directory per
/// component whatever became of it.
fn recorded_architectures(work_dir: &Path, recipe: &str, suite: &str) -> Result<Vec<String>> {
    let dir = manifest::manifest_dir(work_dir, recipe, suite);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_error("reading the manifest directory", &dir, err)),
    };
    let mut architectures = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|err| io_error("reading the manifest directory", &dir, err))?
            .path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        if let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) {
            architectures.push(name.to_string());
        }
    }
    architectures.sort();
    Ok(architectures)
}

/// One architecture's built components, with the files each produced.
fn read_architecture(
    work_dir: &Path,
    recipe: &Recipe,
    architecture: &str,
) -> Result<Vec<ComponentOutput>> {
    let path = manifest::manifest_path(work_dir, &recipe.name, &recipe.suite, architecture);
    let manifest = Manifest::load(&path)?.ok_or_else(|| {
        Error::Export(format!(
            "the manifest {} is no longer there",
            path.display()
        ))
    })?;
    let out_root = build::output_dir(work_dir, &recipe.suite, architecture);

    let mut outputs = Vec::new();
    for record in &manifest.components {
        if record.status != STATUS_BUILT {
            continue;
        }
        let out_dir = out_root.join(&record.name);
        if !out_dir.is_dir() {
            return Err(missing_output(&record.name, architecture, &out_dir));
        }
        // The `.changes` is the authority for what the build produced, exactly
        // as it is when the build collects its own artifacts, so an export
        // carries the set the manifest was written from.
        let mut packages = Vec::new();
        for artifact in build::collect_artifacts(&out_dir)? {
            let name = file_name(&artifact.path);
            if !artifact.path.is_file() {
                return Err(missing_output(&record.name, architecture, &artifact.path));
            }
            packages.push(PackageFile {
                arch_indep: is_arch_indep(&name),
                package: artifact.package,
                version: artifact.version,
                carried: Carried {
                    source: artifact.path,
                    name,
                },
            });
        }
        if packages.is_empty() {
            return Err(missing_output(&record.name, architecture, &out_dir));
        }
        let mut records = Vec::new();
        for extension in ["changes", "buildinfo"] {
            if let Some(path) = build::first_with_extension(&out_dir, extension)? {
                records.push(Carried {
                    name: file_name(&path),
                    source: path,
                });
            }
        }
        outputs.push(ComponentOutput {
            component: record.name.clone(),
            architecture: architecture.to_string(),
            packages,
            records,
        });
    }
    Ok(outputs)
}

/// The error for a component the manifest calls built whose artifacts are not
/// where the manifest says they are.
///
/// An export that quietly passed over it would publish an archive missing a
/// package the manifest claims, which is worse than not exporting at all — so
/// this is fatal, and it names the remedy.
fn missing_output(component: &str, architecture: &str, path: &Path) -> Error {
    Error::Export(format!(
        "{component} is recorded as built for {architecture}, but {} is not there; \
         rebuild it, or pass --architecture to export the architectures that are",
        path.display()
    ))
}

/// Whether a package file name declares an architecture-independent package.
///
/// A Debian binary file name is `name_version_arch.deb`, so the architecture is
/// the field after the last `_`. Taken from the name rather than from the
/// package's control stanza because the name is what collides: two
/// architectures producing `Architecture: all` output write one file name over
/// two sets of bytes, and it is the name an archive stores by.
fn is_arch_indep(file_name: &str) -> bool {
    let stem = file_name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(file_name);
    stem.rsplit_once('_').is_some_and(|(_, arch)| arch == "all")
}

/// Drops every `Architecture: all` package carried by more than one
/// architecture, keeping one copy, and reports each one it had to choose
/// between.
///
/// An archive merges a suite's architectures under one `Release`, so the same
/// name and version may appear once. Which copy is carried follows the recipe
/// where it says: a declared
/// [`arch-indep-owner`](crate::Recipe::arch_indep_owner) is the authority, and
/// its copy is kept even where another architecture built a later one — that is
/// what declaring an owner means. With none declared, the later version wins,
/// and architecture order breaks a tie, so an export is a function of the work
/// directory and not of the order it was read in.
///
/// Deduplication runs whether or not an owner is declared, because a recipe with
/// none produces every arch-indep package once per architecture — see
/// [`Recipe::owns_arch_indep`](crate::Recipe::owns_arch_indep) — and hands the
/// export the duplicates to resolve.
///
/// The `.changes` and `.buildinfo` of a build whose arch-indep output was
/// dropped still travel, and still name it. They record what that build
/// produced, which is the truth an archive keeps them for; the export is what
/// decides what to carry. Declaring an owner removes the divergence at its
/// source, since a non-owner then never builds the package at all.
fn deduplicate(outputs: &mut [ComponentOutput], owner: Option<&str>) -> Vec<Duplicate> {
    // Every architecture that offers each arch-indep package, with the version
    // it offers, in the order the outputs were read — which is architecture
    // order, since that is how they are enumerated.
    let mut offers: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for output in outputs.iter() {
        for package in output.packages.iter().filter(|file| file.arch_indep) {
            offers
                .entry(package.package.as_str())
                .or_default()
                .push((output.architecture.as_str(), package.version.as_str()));
        }
    }

    let mut keep: BTreeMap<String, String> = BTreeMap::new();
    let mut duplicates = Vec::new();
    for (package, offered) in offers {
        if offered.len() < 2 {
            continue;
        }
        let kept = offered
            .iter()
            .find(|(architecture, _)| Some(*architecture) == owner)
            .copied()
            .unwrap_or_else(|| {
                // No owner offers it: the later version wins, and the first
                // architecture in name order breaks a tie.
                *offered
                    .iter()
                    .reduce(|best, next| match crate::version::compare(next.1, best.1) {
                        std::cmp::Ordering::Greater => next,
                        _ => best,
                    })
                    .expect("a non-empty offer list")
            })
            .0;
        keep.insert(package.to_string(), kept.to_string());
        duplicates.push(Duplicate {
            package: package.to_string(),
            kept: kept.to_string(),
            dropped: offered
                .iter()
                .map(|(architecture, _)| (*architecture).to_string())
                .filter(|architecture| architecture != kept)
                .collect(),
        });
    }

    for output in outputs.iter_mut() {
        output.packages.retain(|file| {
            keep.get(&file.package)
                .is_none_or(|kept| kept == &output.architecture)
        });
        // A component whose every package went elsewhere contributes nothing,
        // not even the records beside them: they describe a build whose output
        // this export does not carry.
        if output.packages.is_empty() {
            output.records.clear();
        }
    }
    duplicates
}

/// Each architecture's provenance manifest, named for where it lands in the
/// export.
///
/// Under `manifests/<recipe>/`, mirroring the work directory's own layout, so
/// several recipes exporting into one directory keep their records apart.
fn manifest_files(work_dir: &Path, recipe: &Recipe, architectures: &[String]) -> Vec<Carried> {
    architectures
        .iter()
        .map(|architecture| Carried {
            name: format!("{EXPORT_MANIFEST_DIR}/{}/{architecture}.toml", recipe.name),
            source: manifest::manifest_path(work_dir, &recipe.name, &recipe.suite, architecture),
        })
        .collect()
}

/// The index a prior export left in `dir`, or `None` when there is none.
///
/// An index for another suite is an error: the destination holds a different
/// suite's export, so the caller named the wrong directory, and replacing files
/// on that basis would delete an archive's worth of another suite's packages.
fn read_index(dir: &Path, recipe: &Recipe) -> Result<Option<ExportIndex>> {
    let path = dir.join(EXPORT_INDEX);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_error("reading the export index", &path, err)),
    };
    let index: ExportIndex = toml::from_str(&text).map_err(|err| {
        Error::Export(format!(
            "the index {} does not parse: {err}",
            path.display()
        ))
    })?;
    if index.suite != recipe.suite {
        return Err(Error::Export(format!(
            "{} holds an export for suite {:?}, not {:?}",
            path.display(),
            index.suite,
            recipe.suite
        )));
    }
    Ok(Some(index))
}

/// The index to write: this recipe's entry, over whatever other recipes the
/// prior index recorded.
fn merge_index(prior: Option<&ExportIndex>, suite: &str, entry: RecipeExport) -> ExportIndex {
    let mut recipes: Vec<RecipeExport> = prior
        .map(|index| {
            index
                .recipes
                .iter()
                .filter(|recipe| recipe.name != entry.name)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    recipes.push(entry);
    recipes.sort_by(|a, b| a.name.cmp(&b.name));
    ExportIndex {
        suite: suite.to_string(),
        recipes,
    }
}

/// Writes the index, atomically, so an interrupted export never leaves one an
/// export cannot read.
fn write_index(dir: &Path, index: &ExportIndex) -> Result<()> {
    let path = dir.join(EXPORT_INDEX);
    let staging = dir.join(format!(".{EXPORT_INDEX}.{}.partial", std::process::id()));
    // The index is composed of strings, none of which TOML serialization can
    // reject.
    let text = toml::to_string(index).expect("an index of strings serializes to TOML");
    std::fs::write(&staging, text)
        .map_err(|err| io_error("writing the export index", &staging, err))?;
    std::fs::rename(&staging, &path).map_err(|err| {
        let _ = std::fs::remove_file(&staging);
        io_error("writing the export index", &path, err)
    })
}

/// Removes the files this recipe's prior export left that this one does not
/// carry, and returns how many were removed.
///
/// A file the new export carries under the same name is left where it is and
/// overwritten by the copy, so a name that has not moved is never briefly
/// absent from the destination.
fn remove_superseded(
    dir: &Path,
    prior: Option<&ExportIndex>,
    entry: &RecipeExport,
) -> Result<usize> {
    let Some(prior) = prior else {
        return Ok(0);
    };
    let Some(previous) = prior
        .recipes
        .iter()
        .find(|recipe| recipe.name == entry.name)
    else {
        return Ok(0);
    };
    let current: BTreeSet<&str> = entry.files().collect();
    let mut removed = 0;
    for file in previous.files().filter(|file| !current.contains(file)) {
        // The index is src2deb's own and names files it wrote relative to the
        // suite directory, but a path is still checked before it is removed:
        // an index that has been edited must not be able to delete outside the
        // export.
        let Some(path) = safe_join(dir, file) else {
            continue;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error("removing a superseded export file", &path, err)),
        }
    }
    Ok(removed)
}

/// `dir` joined with a relative export path, or `None` when the path is not one
/// an export could have written.
///
/// Every component must be an ordinary name: no absolute path, no `..`, no root
/// or prefix component. The index is src2deb's own, so this is defense in depth
/// against an edited one rather than a check on untrusted input.
fn safe_join(dir: &Path, relative: &str) -> Option<PathBuf> {
    let mut path = dir.to_path_buf();
    if relative.is_empty() {
        return None;
    }
    for component in Path::new(relative).components() {
        match component {
            std::path::Component::Normal(name) => path.push(name),
            _ => return None,
        }
    }
    Some(path)
}

/// Copies one file into the export, and returns its size.
///
/// The copy lands beside its destination and is renamed into place, so a file
/// under its final name is always complete — an export interrupted partway
/// through leaves no truncated `.deb` for an archive tool to ingest.
fn copy_file(carried: &Carried, dir: &Path) -> Result<u64> {
    let destination = safe_join(dir, &carried.name).ok_or_else(|| {
        Error::Export(format!(
            "{:?} is not a name an export may write",
            carried.name
        ))
    })?;
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| io_error("creating an export directory", parent, err))?;
    }
    let staging = staging_path(&destination);
    let bytes = std::fs::copy(&carried.source, &staging)
        .map_err(|err| io_error("copying into the export", &carried.source, err))?;
    std::fs::rename(&staging, &destination).map_err(|err| {
        let _ = std::fs::remove_file(&staging);
        io_error("writing into the export", &destination, err)
    })?;
    Ok(bytes)
}

/// The path a file is staged at before being renamed over `path`: a sibling, so
/// the rename stays within one filesystem, named for the writing process, so two
/// exports into one directory cannot stage over each other.
fn staging_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let staged = format!(".{name}.{}.partial", std::process::id());
    match path.parent() {
        Some(parent) => parent.join(staged),
        None => PathBuf::from(staged),
    }
}

/// A path's file name as text, lossily — every path an export carries is one
/// src2deb composed or dpkg wrote.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::fingerprint::{Fingerprint, SourceInput, SourceRole};
    use crate::manifest::{ComponentRecord, PackageRecord};

    /// A unique scratch directory for one test.
    fn scratch(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("src2deb-export-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A recipe named `r` for trixie, with `owner` as its arch-indep owner.
    fn recipe(owner: Option<&str>) -> Recipe {
        let owner = owner
            .map(|owner| format!("arch-indep-owner = \"{owner}\"\n"))
            .unwrap_or_default();
        toml::from_str(&format!(
            "name = \"r\"\nsuite = \"trixie\"\n{owner}\
             [[components]]\nname = \"c\"\nsource.git = \"https://example.invalid/c\"\n"
        ))
        .unwrap()
    }

    /// Writes a component's build output into the work directory: one file per
    /// name given, each holding its own name so a copy can be checked.
    fn build_output(work: &Path, architecture: &str, component: &str, files: &[&str]) {
        let dir = build::output_dir(work, "trixie", architecture).join(component);
        std::fs::create_dir_all(&dir).unwrap();
        for name in files {
            std::fs::write(dir.join(name), name.as_bytes()).unwrap();
        }
    }

    /// Records `components` as built for `architecture` in the work directory's
    /// manifest.
    fn manifest_for(work: &Path, architecture: &str, components: &[&str]) {
        let records = components
            .iter()
            .map(|name| ComponentRecord {
                name: (*name).to_string(),
                status: STATUS_BUILT.to_string(),
                error: None,
                version: None,
                buildinfo: None,
                source: Fingerprint::of(SourceInput::git(SourceRole::Source, "abc1234")),
                packages: vec![PackageRecord {
                    name: (*name).to_string(),
                    version: "1.0".to_string(),
                }],
            })
            .collect();
        Manifest::new("r", "trixie", architecture, records)
            .write(&manifest::manifest_path(work, "r", "trixie", architecture))
            .unwrap();
    }

    /// The file names an export directory holds, recursively, relative to it.
    fn listing(dir: &Path) -> Vec<String> {
        fn walk(dir: &Path, prefix: &str, into: &mut Vec<String>) {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for path in entries {
                let name = format!("{prefix}{}", file_name(&path));
                if path.is_dir() {
                    walk(&path, &format!("{name}/"), into);
                } else {
                    into.push(name);
                }
            }
        }
        let mut names = Vec::new();
        walk(dir, "", &mut names);
        names
    }

    #[test]
    fn an_architecture_independent_package_is_recognized_by_its_file_name() {
        assert!(is_arch_indep("cosmic-icons_1.0_all.deb"));
        // The architecture is the last field, so a package whose name or
        // version merely contains "all" is not one.
        assert!(!is_arch_indep("libinstall_1.0_amd64.deb"));
        assert!(!is_arch_indep("c_1.0-all_arm64.deb"));
        assert!(!is_arch_indep("c_1.0_arm64.ddeb"));
        // A name that is not a Debian binary name at all declares nothing.
        assert!(!is_arch_indep("Packages"));
    }

    /// A component output holding one package, for the deduplication tests.
    fn output(
        component: &str,
        architecture: &str,
        package: &str,
        version: &str,
    ) -> ComponentOutput {
        let name = format!("{package}_{version}_all.deb");
        ComponentOutput {
            component: component.to_string(),
            architecture: architecture.to_string(),
            packages: vec![PackageFile {
                carried: Carried {
                    source: PathBuf::from(&name),
                    name: name.clone(),
                },
                package: package.to_string(),
                version: version.to_string(),
                arch_indep: true,
            }],
            records: vec![Carried {
                source: PathBuf::from("c.changes"),
                name: "c.changes".to_string(),
            }],
        }
    }

    #[test]
    fn a_declared_owner_supplies_the_arch_indep_package_whatever_the_other_built() {
        let mut outputs = vec![
            output("c", "amd64", "c-data", "2.0"),
            output("c", "arm64", "c-data", "1.0"),
        ];
        // The owner's copy is carried even though the other architecture built
        // a later version: declaring an owner is declaring the authority, not a
        // preference to be overridden by whichever build ran most recently.
        let duplicates = deduplicate(&mut outputs, Some("arm64"));
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].kept, "arm64");
        assert_eq!(duplicates[0].dropped, ["amd64"]);
        assert!(outputs[0].packages.is_empty());
        assert_eq!(outputs[1].packages.len(), 1);
        // The dropped architecture contributes nothing at all, not even the
        // records describing a build whose output is not carried.
        assert!(outputs[0].records.is_empty());
        assert_eq!(outputs[1].records.len(), 1);
    }

    #[test]
    fn without_an_owner_the_later_version_is_carried_and_a_tie_is_broken_by_name() {
        let mut outputs = vec![
            output("c", "amd64", "c-data", "1.0"),
            output("c", "arm64", "c-data", "2.0"),
        ];
        let duplicates = deduplicate(&mut outputs, None);
        assert_eq!(duplicates[0].kept, "arm64");

        // Equal versions: the first architecture in name order, so an export is
        // a function of what the work directory holds rather than of the order
        // it happened to be read in.
        let mut tied = vec![
            output("c", "amd64", "c-data", "1.0"),
            output("c", "arm64", "c-data", "1.0"),
        ];
        let duplicates = deduplicate(&mut tied, None);
        assert_eq!(duplicates[0].kept, "amd64");
        assert_eq!(tied[0].packages.len(), 1);
        assert!(tied[1].packages.is_empty());
    }

    #[test]
    fn a_package_only_one_architecture_built_is_not_a_duplicate() {
        let mut outputs = vec![output("c", "amd64", "c-data", "1.0")];
        assert!(deduplicate(&mut outputs, None).is_empty());
        assert_eq!(outputs[0].packages.len(), 1);
    }

    #[test]
    fn an_export_carries_every_recorded_architecture_and_names_what_it_wrote() {
        let root = scratch("carry");
        let work = root.join("work");
        let dest = root.join("drop");
        for architecture in ["amd64", "arm64"] {
            manifest_for(&work, architecture, &["c"]);
            build_output(
                &work,
                architecture,
                "c",
                &[
                    &format!("c_1.0_{architecture}.deb"),
                    &format!("c_1.0_{architecture}.changes"),
                    &format!("c_1.0_{architecture}.buildinfo"),
                ],
            );
        }

        let report = export(&work, &recipe(None), &ExportOptions::to(&dest)).unwrap();
        assert_eq!(report.architectures, ["amd64", "arm64"]);
        // One component and one package, counted over both architectures: a
        // component built twice is not two components.
        assert_eq!(report.components, 1);
        assert_eq!(report.packages, 1);
        assert_eq!(report.removed, 0);
        assert!(report.duplicates.is_empty());

        // The packages are flat and the manifests are kept under the recipe, so
        // the suite directory is one argument to an archive tool and two
        // recipes' records do not collide.
        assert_eq!(
            listing(&report.dir),
            [
                "c_1.0_amd64.buildinfo",
                "c_1.0_amd64.changes",
                "c_1.0_amd64.deb",
                "c_1.0_arm64.buildinfo",
                "c_1.0_arm64.changes",
                "c_1.0_arm64.deb",
                "export.toml",
                "manifests/r/amd64.toml",
                "manifests/r/arm64.toml",
            ]
        );
        // The copy is the file, not a reference to it.
        assert_eq!(
            std::fs::read_to_string(report.dir.join("c_1.0_amd64.deb")).unwrap(),
            "c_1.0_amd64.deb"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_changes_file_is_what_says_which_packages_a_build_produced() {
        let root = scratch("changes");
        let work = root.join("work");
        manifest_for(&work, "arm64", &["c"]);
        // A `.changes` as dpkg-buildpackage writes one. The build collects its
        // artifacts from it, so an export must too, or the two disagree about
        // what a component produced — and a `.deb` left in the output tree by
        // something else would travel.
        build_output(
            &work,
            "arm64",
            "c",
            &[
                "c_1.0_arm64.deb",
                "c-dbgsym_1.0_arm64.ddeb",
                "c_1.0_arm64.buildinfo",
            ],
        );
        let out_dir = build::output_dir(&work, "trixie", "arm64").join("c");
        std::fs::write(
            out_dir.join("c_1.0_arm64.changes"),
            "Format: 1.8\nSource: c\nFiles:\n \
             d41d8cd 12 admin optional c_1.0_arm64.deb\n \
             e99a18c 34 admin optional c-dbgsym_1.0_arm64.ddeb\n \
             c3fcd3d 56 admin optional c_1.0_arm64.buildinfo\n",
        )
        .unwrap();

        let report = export(&work, &recipe(None), &ExportOptions::to(root.join("drop"))).unwrap();
        // The `.deb` and its `.ddeb` companion are two files and two packages;
        // the `.buildinfo` the `Files:` section also names is carried as the
        // record it is, not as a package.
        assert_eq!(report.packages, 2);
        assert_eq!(
            listing(&report.dir),
            [
                "c-dbgsym_1.0_arm64.ddeb",
                "c_1.0_arm64.buildinfo",
                "c_1.0_arm64.changes",
                "c_1.0_arm64.deb",
                "export.toml",
                "manifests/r/arm64.toml",
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_arch_indep_package_built_twice_is_carried_once() {
        let root = scratch("dedup");
        let work = root.join("work");
        let dest = root.join("drop");
        for architecture in ["amd64", "arm64"] {
            manifest_for(&work, architecture, &["c"]);
            build_output(
                &work,
                architecture,
                "c",
                &[
                    &format!("c_1.0_{architecture}.deb"),
                    // The same name over different bytes on each architecture:
                    // the collision an archive merging them cannot hold.
                    "c-data_1.0_all.deb",
                    &format!("c_1.0_{architecture}.changes"),
                ],
            );
        }
        let report = export(&work, &recipe(Some("arm64")), &ExportOptions::to(&dest)).unwrap();

        assert_eq!(report.duplicates.len(), 1);
        assert_eq!(report.duplicates[0].kept, "arm64");
        assert_eq!(
            listing(&report.dir),
            [
                "c-data_1.0_all.deb",
                "c_1.0_amd64.changes",
                "c_1.0_amd64.deb",
                "c_1.0_arm64.changes",
                "c_1.0_arm64.deb",
                "export.toml",
                "manifests/r/amd64.toml",
                "manifests/r/arm64.toml",
            ]
        );
        // The one copy is the owner's, so which bytes were carried follows the
        // recipe rather than which architecture was copied last.
        assert_eq!(
            std::fs::read(
                build::output_dir(&work, "trixie", "arm64")
                    .join("c")
                    .join("c-data_1.0_all.deb")
            )
            .unwrap(),
            std::fs::read(report.dir.join("c-data_1.0_all.deb")).unwrap()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_export_replaces_the_one_before_it_and_leaves_other_recipes_alone() {
        let root = scratch("replace");
        let work = root.join("work");
        let dest = root.join("drop");
        manifest_for(&work, "arm64", &["c"]);
        build_output(&work, "arm64", "c", &["c_1.0_arm64.deb"]);
        let report = export(&work, &recipe(None), &ExportOptions::to(&dest)).unwrap();

        // A file another recipe's export left, which this recipe's export must
        // not touch: the drop directory for a suite holds every recipe that
        // publishes into it.
        let other = report.dir.join("other_1.0_arm64.deb");
        std::fs::write(&other, b"other").unwrap();

        // The version moves, so the file name moves with it.
        std::fs::remove_dir_all(build::output_dir(&work, "trixie", "arm64").join("c")).unwrap();
        build_output(&work, "arm64", "c", &["c_2.0_arm64.deb"]);
        let report = export(&work, &recipe(None), &ExportOptions::to(&dest)).unwrap();

        assert_eq!(report.removed, 1);
        assert_eq!(
            listing(&report.dir),
            [
                "c_2.0_arm64.deb",
                "export.toml",
                "manifests/r/arm64.toml",
                "other_1.0_arm64.deb",
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn two_recipes_share_a_destination_and_each_replaces_only_its_own() {
        let root = scratch("recipes");
        let work = root.join("work");
        let dest = root.join("drop");
        let second: Recipe = toml::from_str(
            "name = \"s\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"d\"\nsource.git = \"https://example.invalid/d\"\n",
        )
        .unwrap();

        manifest_for(&work, "arm64", &["c"]);
        build_output(&work, "arm64", "c", &["c_1.0_arm64.deb"]);
        export(&work, &recipe(None), &ExportOptions::to(&dest)).unwrap();

        Manifest::new(
            "s",
            "trixie",
            "arm64",
            vec![ComponentRecord {
                name: "d".to_string(),
                status: STATUS_BUILT.to_string(),
                error: None,
                version: None,
                buildinfo: None,
                source: Fingerprint::of(SourceInput::git(SourceRole::Source, "def5678")),
                packages: vec![PackageRecord {
                    name: "d".to_string(),
                    version: "1.0".to_string(),
                }],
            }],
        )
        .write(&manifest::manifest_path(&work, "s", "trixie", "arm64"))
        .unwrap();
        build_output(&work, "arm64", "d", &["d_1.0_arm64.deb"]);
        let report = export(&work, &second, &ExportOptions::to(&dest)).unwrap();

        // The second recipe's export left the first's packages where they were,
        // and the index now names both.
        assert_eq!(report.removed, 0);
        assert_eq!(
            listing(&report.dir),
            [
                "c_1.0_arm64.deb",
                "d_1.0_arm64.deb",
                "export.toml",
                "manifests/r/arm64.toml",
                "manifests/s/arm64.toml",
            ]
        );
        let index: ExportIndex =
            toml::from_str(&std::fs::read_to_string(report.dir.join(EXPORT_INDEX)).unwrap())
                .unwrap();
        let names: Vec<&str> = index
            .recipes
            .iter()
            .map(|recipe| recipe.name.as_str())
            .collect();
        assert_eq!(names, ["r", "s"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_export_into_another_suites_directory_is_refused() {
        let root = scratch("suite");
        let work = root.join("work");
        let dest = root.join("drop");
        manifest_for(&work, "arm64", &["c"]);
        build_output(&work, "arm64", "c", &["c_1.0_arm64.deb"]);
        let report = export(&work, &recipe(None), &ExportOptions::to(&dest)).unwrap();

        // The index declares the suite it was written for. A destination whose
        // index names another suite means the caller named the wrong directory,
        // and replacing files on that basis would delete another suite's
        // archive.
        let index = report.dir.join(EXPORT_INDEX);
        let text = std::fs::read_to_string(&index)
            .unwrap()
            .replace("trixie", "forky");
        std::fs::write(&index, text).unwrap();
        let err = export(&work, &recipe(None), &ExportOptions::to(&dest)).unwrap_err();
        assert!(
            format!("{err}").contains("suite \"forky\""),
            "unexpected error: {err}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_component_recorded_as_built_whose_artifacts_are_gone_fails_the_export() {
        let root = scratch("missing");
        let work = root.join("work");
        manifest_for(&work, "arm64", &["c"]);
        // No output tree at all: the manifest claims a package the work
        // directory cannot produce, and an export that passed over it would
        // publish an archive quietly missing it.
        let err = export(&work, &recipe(None), &ExportOptions::to(root.join("drop"))).unwrap_err();
        let message = format!("{err}");
        assert!(message.contains("recorded as built"), "{message}");
        assert!(message.contains("rebuild it"), "{message}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_architecture_no_run_recorded_is_a_usage_error_naming_what_there_is() {
        let root = scratch("unknown-arch");
        let work = root.join("work");
        manifest_for(&work, "arm64", &["c"]);
        build_output(&work, "arm64", "c", &["c_1.0_arm64.deb"]);
        let options = ExportOptions {
            dest: root.join("drop"),
            architectures: vec!["amd64".to_string()],
        };
        let err = export(&work, &recipe(None), &options).unwrap_err();
        assert!(format!("{err}").contains("arm64"), "{err}");

        // And a work directory holding nothing for the recipe says so rather
        // than writing an empty export.
        let empty = scratch("empty");
        let err = export(
            &empty,
            &recipe(None),
            &ExportOptions::to(empty.join("drop")),
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("build it before exporting"),
            "{err}"
        );
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&empty).unwrap();
    }

    #[test]
    fn an_index_path_that_climbs_out_of_the_export_is_refused() {
        // The index is src2deb's own, so this is defense in depth: an edited one
        // must not be able to remove or write a file outside the export.
        let dir = Path::new("/drop/trixie");
        assert_eq!(
            safe_join(dir, "manifests/r/arm64.toml"),
            Some(PathBuf::from("/drop/trixie/manifests/r/arm64.toml"))
        );
        assert_eq!(safe_join(dir, "../../etc/passwd"), None);
        assert_eq!(safe_join(dir, "/etc/passwd"), None);
        assert_eq!(safe_join(dir, ""), None);
    }
}
