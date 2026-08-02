//! Build planning: read each component's `debian/control`, work out which
//! components depend on which, and order them so every component builds after
//! the components that produce its build-dependencies.
//!
//! Build-dependency names come from ferroday-cage's
//! [`build_depend_names`]; the
//! binary package names a component *produces* are read from its `Package:`
//! stanzas here. An edge runs from a producer to a consumer, and a topological
//! sort yields the build order. Dependencies the set does not produce are left
//! to archive and pool resolution at provision time.
//!
//! # Build-dependency alternatives
//!
//! [`build_depend_names`] returns only the first alternative of each `a | b` group,
//! and src2deb uses that single list both to install build-dependencies and to
//! build the inter-component graph. An in-set edge expressed only through a later
//! alternative — a build-dep written `archive-pkg | in-set-pkg`, where the
//! in-set package is not first — is therefore not seen, so its producer would
//! not be ordered first. The current COSMIC recipe's one real edge is a direct
//! dependency, not an alternative, so this bounds how a future recipe may use
//! alternatives to express an in-set edge rather than affecting today's build.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ferroday_cage::provision::debian::build_depend_names;

use crate::error::{Error, Result};

/// Reads `tree/debian/control` as text.
pub fn read_control(component: &str, tree: &Path) -> Result<String> {
    let path = tree.join("debian/control");
    std::fs::read_to_string(&path).map_err(|err| Error::Control {
        component: component.to_string(),
        reason: format!("{}: {err}", path.display()),
    })
}

/// The build-dependency package names a `debian/control` declares, via
/// ferroday-cage's parser (`Build-Depends`, `-Arch`, and `-Indep`).
pub fn build_dependencies(control: &str) -> Vec<String> {
    build_depend_names(control)
        .into_iter()
        .map(|d| d.to_string())
        .collect()
}

/// The source package name a `debian/control` declares, or `None` when it
/// declares none.
///
/// This is the name a changelog entry opens with, so it is what a
/// [synthesized changelog](crate::version::synthesized_changelog) has to agree
/// with: `dpkg-buildpackage` reads the source package name from the changelog
/// and checks it against `control`.
pub fn source_package(control: &str) -> Option<&str> {
    source_field(control, "Source")
}

/// The maintainer identity a `debian/control` declares, or `None` when it
/// declares none.
///
/// Debian policy makes the field mandatory, so a component that can be built at
/// all carries one — which is what lets a synthesized changelog be signed
/// without src2deb inventing an identity or the recipe restating one the
/// packaging already declares.
pub fn maintainer(control: &str) -> Option<&str> {
    source_field(control, "Maintainer")
}

/// The value of field `name` in a `debian/control`'s source stanza — the first
/// stanza, which ends at the first blank line.
///
/// Bounded to that stanza deliberately. A binary stanza may carry a field
/// spelled the same for a different subject, and reading the whole file would
/// answer with whichever came first rather than with the source's own.
///
/// Field names are matched without regard to case, as `dpkg` matches them. A
/// continuation line begins with whitespace, so its leading text never matches
/// a field name and a folded value contributes nothing.
fn source_field<'a>(control: &'a str, name: &str) -> Option<&'a str> {
    control
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
        .filter(|value| !value.is_empty())
}

/// The binary package names a `debian/control` produces: the value of every
/// `Package:` field (the binary stanzas; the source stanza has none).
pub fn binary_packages(control: &str) -> Vec<String> {
    control
        .lines()
        .filter_map(|line| line.strip_prefix("Package:"))
        .map(|value| value.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Whether a `debian/control` declares any binary package that is not
/// `Architecture: all`.
///
/// This decides whether a component has anything to build for an architecture
/// that does not own the recipe's arch-indep output: a component whose every
/// binary package is `Architecture: all` has nothing left once those are taken
/// away, and `dpkg-buildpackage -B` on such a source fails outright rather than
/// producing an empty result.
///
/// Read stanza by stanza, so each `Architecture` is the one belonging to the
/// `Package` above it rather than to whichever binary happens to be declared
/// first. A binary stanza with no `Architecture` field at all is malformed
/// control; it counts as architecture-dependent, because the reading that
/// builds the component is the one that cannot lose a package.
pub fn has_architecture_dependent_packages(control: &str) -> bool {
    let (mut is_binary, mut architecture) = (false, None);
    // A trailing empty line closes the last stanza, so the judgement is written
    // once rather than repeated after the loop.
    for line in control.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if is_binary && architecture != Some("all") {
                return true;
            }
            (is_binary, architecture) = (false, None);
        } else if line.starts_with("Package:") {
            // The source stanza has none, so it never judges as a binary: its
            // fields describe the source and not a package this build emits.
            is_binary = true;
        } else if let Some(value) = line.strip_prefix("Architecture:") {
            architecture = Some(value.trim());
        }
    }
    false
}

/// A resolved build order over a set of components, with the dependency
/// structure it was ordered from.
///
/// Beyond the linear [`order`](Self::order) a sequential build follows, the
/// graph keeps the components that depend on each one
/// ([`dependents`](Self::dependents)), so a parallel scheduler can release a
/// component the moment its producers finish.
///
/// It keeps no in-degrees. A parallel run builds a subset — a skipped or
/// unselected producer must not gate its consumer — so the scheduler counts
/// producers over the run's own items rather than over the whole graph, and a
/// count taken here would be the wrong one.
///
/// It does keep the package-to-producer mapping the edges were derived from
/// ([`producer`](Self::producer)), because a run that builds a subset needs to
/// say *which* component a package it cannot resolve would have come from.
#[derive(Debug, Clone)]
pub struct BuildGraph {
    order: Vec<String>,
    /// Each component's in-set consumers, which it must build before.
    dependents: BTreeMap<String, Vec<String>>,
    /// Which component produces each binary package the set produces.
    producers: BTreeMap<String, String>,
}

impl BuildGraph {
    /// Computes the build order for `components`, each given as its name and its
    /// `debian/control` text.
    ///
    /// Returns [`Error::Plan`] when the intra-set dependencies form a cycle.
    pub fn resolve(components: &[(String, String)]) -> Result<BuildGraph> {
        let count = components.len();

        // Which component produces each binary package. The first producer wins
        // a tie, which a coherent recipe never presents.
        let mut producers: BTreeMap<String, String> = BTreeMap::new();
        for (name, control) in components {
            for binary in binary_packages(control) {
                producers.entry(binary).or_insert_with(|| name.clone());
            }
        }

        let names: Vec<String> = components.iter().map(|(name, _)| name.clone()).collect();
        let mut in_degree: BTreeMap<String, usize> =
            names.iter().map(|name| (name.clone(), 0)).collect();
        let mut dependents: BTreeMap<String, Vec<String>> = names
            .iter()
            .map(|name| (name.clone(), Vec::new()))
            .collect();

        for (name, control) in components {
            // The in-set components this one depends on: the producers of its
            // build-deps, excluding itself.
            let mut deps: BTreeSet<String> = BTreeSet::new();
            for dep in build_dependencies(control) {
                if let Some(source) = producers.get(&dep)
                    && source != name
                {
                    deps.insert(source.clone());
                }
            }
            *in_degree.get_mut(name).expect("name is seeded") = deps.len();
            for dep in &deps {
                dependents
                    .get_mut(dep)
                    .expect("producer is a known component")
                    .push(name.clone());
            }
        }

        // Kahn's algorithm, seeding ready components in recipe order so the
        // output is deterministic.
        let mut ready: Vec<String> = names
            .iter()
            .filter(|name| in_degree[*name] == 0)
            .cloned()
            .collect();
        let mut order: Vec<String> = Vec::with_capacity(count);
        let mut cursor = 0;
        while cursor < ready.len() {
            let name = ready[cursor].clone();
            cursor += 1;
            for dependent in &dependents[&name] {
                let degree = in_degree.get_mut(dependent).expect("dependent is known");
                *degree -= 1;
                if *degree == 0 {
                    ready.push(dependent.clone());
                }
            }
            order.push(name);
        }

        if order.len() != count {
            let ordered: BTreeSet<&String> = order.iter().collect();
            let stuck: Vec<&str> = names
                .iter()
                .filter(|name| !ordered.contains(name))
                .map(String::as_str)
                .collect();
            return Err(Error::Plan(format!(
                "a dependency cycle involves: {}",
                stuck.join(", ")
            )));
        }

        Ok(BuildGraph {
            order,
            dependents,
            producers,
        })
    }

    /// The components in build order.
    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// The component that produces binary package `package`, or `None` when no
    /// component in the set does — which is every archive package, and is why
    /// the build order carries no edge for one.
    pub fn producer(&self, package: &str) -> Option<&str> {
        self.producers.get(package).map(String::as_str)
    }

    /// The in-set consumers of `component` — the components that must build after
    /// it. Empty for a component nothing else depends on.
    pub fn dependents(&self, component: &str) -> &[String] {
        self.dependents
            .get(component)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal `debian/control` text with a source stanza (carrying any
    /// build-dependencies) followed by one binary stanza per produced package.
    fn control(source: &str, build_deps: &[&str], packages: &[&str]) -> String {
        let mut text = format!("Source: {source}\n");
        if !build_deps.is_empty() {
            text.push_str(&format!("Build-Depends: {}\n", build_deps.join(", ")));
        }
        for package in packages {
            text.push_str(&format!("\nPackage: {package}\nArchitecture: any\n"));
        }
        text
    }

    /// Resolves a build order from `(name, control)` pairs.
    fn order(components: &[(&str, &str)]) -> Result<Vec<String>> {
        let owned: Vec<(String, String)> = components
            .iter()
            .map(|(name, control)| (name.to_string(), control.to_string()))
            .collect();
        BuildGraph::resolve(&owned).map(|graph| graph.order().to_vec())
    }

    #[test]
    fn binary_packages_reads_every_binary_stanza() {
        let control = "Source: s\nBuild-Depends: debhelper\n\
                       \n\nPackage:  a \n\nPackage:\n\nPackage: b\n";
        // The source stanza contributes none, whitespace is trimmed, and an empty
        // Package value is skipped.
        assert_eq!(binary_packages(control), ["a", "b"]);
    }

    #[test]
    fn the_source_stanza_yields_the_package_name_and_the_maintainer() {
        // The two fields a synthesized changelog is written from.
        let control = "Source: cosmic-icons\n\
                       Maintainer: Someone <someone@example.invalid>\n\
                       Build-Depends: debhelper-compat (= 13)\n\
                       \nPackage: cosmic-icons\nArchitecture: all\n";
        assert_eq!(source_package(control), Some("cosmic-icons"));
        assert_eq!(
            maintainer(control),
            Some("Someone <someone@example.invalid>"),
        );
        // Read as dpkg reads them: case-insensitively, and with the value
        // trimmed of the space that follows the colon.
        assert_eq!(source_package("source:   spaced   \n"), Some("spaced"));
        assert_eq!(
            maintainer("MAINTAINER: A <a@e.invalid>\n"),
            Some("A <a@e.invalid>")
        );
    }

    #[test]
    fn a_field_a_binary_stanza_carries_is_not_the_sources_own() {
        // The reading stops at the first blank line, so a binary stanza cannot
        // answer for the source — and a source declaring neither says so.
        let control = "Source: outer\n\nPackage: inner\nArchitecture: all\n\
                       Maintainer: Wrong <wrong@example.invalid>\n";
        assert_eq!(source_package(control), Some("outer"));
        assert_eq!(maintainer(control), None);
        // An empty value is no value, rather than an identity of nothing.
        assert_eq!(maintainer("Source: s\nMaintainer:   \n"), None);
        // A folded continuation line begins with whitespace, so its text is
        // never read as a field of its own.
        assert_eq!(
            source_package("Uploaders: A <a@e.invalid>,\n Source: not-a-field <b@e.invalid>\n"),
            None,
        );
    }

    #[test]
    fn a_component_is_architecture_dependent_when_any_binary_package_is() {
        // The mixed case is the common one: a program plus its data package.
        assert!(has_architecture_dependent_packages(
            "Source: s\n\nPackage: p\nArchitecture: any\n\nPackage: p-data\nArchitecture: all\n"
        ));
        // Declared order must not decide it: the `all` stanza comes first here,
        // and the component still has an architecture-dependent package.
        assert!(has_architecture_dependent_packages(
            "Source: s\n\nPackage: p-data\nArchitecture: all\n\nPackage: p\nArchitecture: any\n"
        ));
        // A restricted architecture list is still architecture-dependent.
        assert!(has_architecture_dependent_packages(
            "Source: s\n\nPackage: p\nArchitecture: amd64 arm64\n"
        ));
    }

    #[test]
    fn a_component_of_nothing_but_arch_indep_packages_has_none() {
        // What the theme, icon, and metapackage recipes look like, and what an
        // architecture that does not own arch-indep output has nothing to build
        // of.
        assert!(!has_architecture_dependent_packages(
            "Source: s\nBuild-Depends: debhelper-compat (= 13)\n\
             \n\nPackage: theme\nArchitecture: all\nDepends: x\n\
             \nPackage: theme-doc\nArchitecture: all\n"
        ));
    }

    #[test]
    fn the_source_stanza_never_decides_architecture_dependence() {
        // It declares no binary package, so it cannot make the component either
        // dependent or independent whatever fields it carries.
        assert!(!has_architecture_dependent_packages(
            "Source: s\nBuild-Depends: debhelper\nVcs-Git: https://example/s\n"
        ));
    }

    #[test]
    fn a_binary_stanza_with_no_architecture_field_counts_as_dependent() {
        // Malformed control: `Architecture` is mandatory in a binary stanza. The
        // reading that builds the component is the one that cannot lose a
        // package, so this is never skipped as arch-indep.
        assert!(has_architecture_dependent_packages(
            "Source: s\n\nPackage: p\nDepends: x\n"
        ));
    }

    #[test]
    fn build_dependencies_keeps_the_first_alternative_of_a_group() {
        let control = control("s", &["debhelper-compat (= 13)", "foo | bar"], &["p"]);
        // Version constraints are stripped and only the first alternative of an
        // `a | b` group is kept — the ferroday-cage parser's discipline, which
        // bounds how src2deb models alternatives in the build graph.
        assert_eq!(build_dependencies(&control), ["debhelper-compat", "foo"]);
    }

    #[test]
    fn a_flat_graph_preserves_recipe_order() {
        let a = control("a", &[], &["pkg-a"]);
        let b = control("b", &[], &["pkg-b"]);
        let c = control("c", &[], &["pkg-c"]);
        assert_eq!(
            order(&[("a", &a), ("b", &b), ("c", &c)]).unwrap(),
            ["a", "b", "c"],
        );
    }

    #[test]
    fn a_producer_is_ordered_before_its_consumer() {
        let randr = control(
            "cosmic-randr",
            &[],
            &["cosmic-randr", "libcosmic-randr-dev"],
        );
        let osd = control(
            "cosmic-osd",
            &["debhelper-compat (= 13)", "libcosmic-randr-dev"],
            &["cosmic-osd"],
        );
        // Listed consumer-first, but the producer of the build-dep must come out
        // first regardless of listing order.
        assert_eq!(
            order(&[("cosmic-osd", &osd), ("cosmic-randr", &randr)]).unwrap(),
            ["cosmic-randr", "cosmic-osd"],
        );
    }

    #[test]
    fn a_dependency_the_set_does_not_produce_creates_no_edge() {
        // build-depends on an archive package no in-set component produces: no
        // edge, so recipe order stands.
        let a = control("a", &["libssl-dev"], &["pkg-a"]);
        let b = control("b", &["libssl-dev"], &["pkg-b"]);
        assert_eq!(order(&[("a", &a), ("b", &b)]).unwrap(), ["a", "b"]);
    }

    #[test]
    fn a_self_dependency_does_not_block_a_component() {
        // A component that build-depends on a package it itself produces gets no
        // self-edge, so it still orders.
        let a = control("a", &["pkg-a"], &["pkg-a"]);
        assert_eq!(order(&[("a", &a)]).unwrap(), ["a"]);
    }

    #[test]
    fn the_graph_exposes_each_components_dependents() {
        let randr = control("cosmic-randr", &[], &["libcosmic-randr-dev"]);
        let osd = control(
            "cosmic-osd",
            &["debhelper-compat (= 13)", "libcosmic-randr-dev"],
            &["cosmic-osd"],
        );
        let owned: Vec<(String, String)> = [("cosmic-osd", &osd), ("cosmic-randr", &randr)]
            .iter()
            .map(|(name, control)| (name.to_string(), control.to_string()))
            .collect();
        let graph = BuildGraph::resolve(&owned).unwrap();

        // The producer gates its consumer; the consumer gates nothing. This is
        // what the parallel scheduler releases work from, and it is per-edge, so
        // it holds whichever subset of the graph a run happens to build.
        assert_eq!(graph.dependents("cosmic-randr"), ["cosmic-osd"]);
        assert!(graph.dependents("cosmic-osd").is_empty());
        // An unknown name reads as no edges.
        assert!(graph.dependents("nope").is_empty());
    }

    #[test]
    fn a_dependency_cycle_is_reported_with_the_stuck_components() {
        let a = control("a", &["pkg-b"], &["pkg-a"]);
        let b = control("b", &["pkg-a"], &["pkg-b"]);
        let err = order(&[("a", &a), ("b", &b)]).unwrap_err();
        assert!(matches!(err, Error::Plan(_)));
        let message = format!("{err}");
        assert!(message.contains("cycle"), "{message}");
        assert!(message.contains('a') && message.contains('b'), "{message}");
    }
}
