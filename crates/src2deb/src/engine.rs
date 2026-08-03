//! The build orchestrator: resolve sources, order the components, and build
//! each in turn, feeding results forward through the local pool.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Condvar, Mutex};

use crate::build::{Artifact, Binaries, BuildInfo, BuildOutcome, Builder, Target};
use crate::cancel::Cancel;
use crate::error::{Error, Result, io_error};
use crate::fingerprint::Fingerprint;
use crate::lock::WorkLock;
use crate::manifest::{self, BuildIdentity, Manifest, SandboxRecord};
use crate::observer::Stream;
use crate::plan::{self, BuildGraph};
use crate::pool::LocalPool;
use crate::provision::{
    BuildRoot, BuildRootProvider, FullReprovision, LayeredProvision, ProvisionConfig,
};
use crate::recipe::{Component, Recipe};
use crate::schedule::{Claim, Scheduler};
use crate::source::{ResolvedSource, SourceResolver, VendorPass};
use crate::version::VersionStamp;

/// A progress event from a build run, delivered to the reporter passed to
/// [`Engine::run`].
#[non_exhaustive]
pub enum Progress<'a> {
    /// The run holds the work directory and is about to begin. Reported once,
    /// before every other event.
    ///
    /// The work-directory lock is the first thing that can reject a run
    /// outright, and it is taken inside the engine, so a caller that announces
    /// what it is about to do announces it here rather than before the call —
    /// otherwise a rejected run states that it is building and is then
    /// contradicted.
    Started,
    /// The run's versions carry a pinned date rather than today's. Reported
    /// once, before anything resolves, whenever the date was given or taken
    /// from a prior manifest — so a reproduction says which date it settled on
    /// rather than leaving it to be read off the packages afterwards.
    BuildDate {
        /// The date, as `YYYY-MM-DD`.
        date: &'a str,
    },
    /// A component's source is being resolved.
    Resolving {
        /// The component name.
        component: &'a str,
    },
    /// A component's source did not resolve, and the run is going on without
    /// it because it was never going to build it: it is outside the run's
    /// selection, and was resolved only to complete the build order.
    ///
    /// Distinct from [`Failed`](Progress::Failed), which is for a component the
    /// run was asked to deliver. This one costs the run nothing and does not
    /// change its outcome.
    Unresolved {
        /// The component name.
        component: &'a str,
        /// The failure, rendered for display.
        error: &'a str,
    },
    /// The build order has been computed. Reported once: sources resolve once
    /// for the whole run, whatever it goes on to target.
    Ordered {
        /// The components in build order.
        order: &'a [String],
    },
    /// The recipe targets several architectures and names no arch-indep owner,
    /// so each of them produces its own copy of every `Architecture: all`
    /// package. Reported once, before the first architecture builds — by
    /// [`plan`](Engine::plan) as well as [`run`](Engine::run).
    ///
    /// Not a warning: complete per-architecture pools are what leaving the owner
    /// unset means, and they are what a pool served as it stands needs. It is
    /// reported because the run is about to spend the emulated time to make
    /// those copies, and because an archive that merges the architectures will
    /// have to choose between them afterwards. See
    /// [`Recipe::arch_indep_owner`](crate::Recipe::arch_indep_owner).
    ArchIndepUnowned {
        /// The architectures that each build their own copy, in build order.
        architectures: &'a [String],
    },
    /// The run has begun building for one of its target architectures. Reported
    /// once per architecture, after the shared resolve and before anything is
    /// provisioned for it.
    Architecture {
        /// The Debian architecture being built for.
        architecture: &'a str,
        /// Its 1-based position among the architectures this run builds for.
        index: usize,
        /// How many architectures this run builds for.
        total: usize,
    },
    /// The target architecture is foreign to the host, so the build runs through
    /// a `qemu-user` binfmt handler. Reported once per architecture, before
    /// provisioning, only when the target does not run natively on the host.
    ForeignArchitecture {
        /// The Debian architecture being built for.
        target: &'a str,
        /// The host's Debian architecture.
        host: &'a str,
    },
    /// The recipe hands its `Architecture: all` packages to another
    /// architecture, so a build here produces none of them. Reported once per
    /// architecture, before provisioning, only when that architecture is not the
    /// owner — by [`plan`](Engine::plan) as well as [`run`](Engine::run), since
    /// it is something to know before committing a machine to a build.
    ///
    /// Worth saying out loud: the pool ends up holding fewer packages than the
    /// recipe declares, which is right when several architectures feed one
    /// published archive and wrong when that pool is served as it stands.
    ArchIndepElsewhere {
        /// The architecture that produces the recipe's arch-indep packages.
        owner: &'a str,
    },
    /// Layered provisioning was selected: a shared base is bootstrapped once and
    /// each component's build-deps are staged into a disposable overlay upper.
    /// Reported once, before the base is prepared.
    Layered,
    /// The host cannot establish an unprivileged overlay, so full
    /// reprovisioning is used instead of the layered strategy. Reported once,
    /// before the first build.
    OverlayUnavailable {
        /// Why an overlay-rooted build layer could not be established.
        reason: &'a str,
    },
    /// A build root is being provisioned. Reported once per root, before its
    /// package work begins — including for a root that turns out to be current,
    /// where the run resolves the plan, finds it unchanged, and installs
    /// nothing.
    Provisioning {
        /// The component whose root is being provisioned, or `None` for the
        /// shared base a layered run bootstraps once.
        component: Option<&'a str>,
    },
    /// A resource is being fetched from an archive: a release file, a package
    /// index, or a package's `.deb`. Reported per URL tried, so a repository
    /// that fails over from one mirror to the next shows both.
    Fetching {
        /// The root being provisioned, or `None` for the shared base.
        component: Option<&'a str>,
        /// The URL being fetched.
        url: &'a str,
    },
    /// A root's package set was resolved against the archive and the pool, so
    /// every package the provision will install is now known.
    PackagesResolved {
        /// The root being provisioned, or `None` for the shared base.
        component: Option<&'a str>,
        /// How many packages the resolved plan installs.
        packages: usize,
    },
    /// A package is being downloaded into the shared package cache. Not
    /// reported for a package the cache already holds, so a warm cache reports
    /// only what it has to fetch.
    Downloading {
        /// The root being provisioned, or `None` for the shared base.
        component: Option<&'a str>,
        /// The binary package name.
        package: &'a str,
        /// Its 1-based position in the root's package set.
        index: usize,
        /// How many packages the set holds.
        total: usize,
    },
    /// The recipe's pinned rustup toolchain is being installed into a
    /// freshly-provisioned build root. Reported once per root that needs it, and
    /// not at all for a root reused from a prior run, which already carries it.
    InstallingToolchain {
        /// The root being provisioned, or `None` for the shared base.
        component: Option<&'a str>,
        /// The toolchain version the recipe pinned.
        version: &'a str,
    },
    /// A package is being unpacked into the build root.
    Extracting {
        /// The root being provisioned, or `None` for the shared base.
        component: Option<&'a str>,
        /// The binary package name.
        package: &'a str,
        /// Its 1-based position among the packages unpacked into this root.
        index: usize,
        /// How many packages the set holds.
        total: usize,
    },
    /// A component is being built.
    Building {
        /// The component name.
        component: &'a str,
        /// Its 1-based position among the components this run builds.
        index: usize,
        /// How many components this run builds.
        ///
        /// The run's own count, not the recipe's: a component skipped as
        /// already published or left out by `--only` is neither counted nor
        /// positioned, so a selective run over a 27-component recipe reports
        /// `1/1` rather than a position in a build order it is not following.
        total: usize,
    },
    /// A component's crates are being vendored (pass 1, with network).
    Vendoring {
        /// The component name.
        component: &'a str,
    },
    /// A line of a component's in-cage build output (from the vendor or build
    /// pass). Streamed as the build runs, one event per line.
    Output {
        /// The component whose build produced the line.
        component: &'a str,
        /// Which standard stream the line came from.
        stream: Stream,
        /// The line, without its trailing newline.
        line: &'a str,
    },
    /// A component built successfully.
    Built {
        /// The component name.
        component: &'a str,
        /// How many artifacts it produced.
        artifacts: usize,
    },
    /// A component's artifacts were published to the pool.
    Published {
        /// The component name.
        component: &'a str,
        /// How many `.debs` were published.
        debs: usize,
    },
    /// A component failed to build. Reported as it happens, so a `keep_going`
    /// run surfaces each failure at the point it occurs rather than only in the
    /// closing summary.
    Failed {
        /// The component name.
        component: &'a str,
        /// The failure, rendered for display.
        error: &'a str,
    },
    /// A component was skipped rather than built — already published, or not in
    /// the run's selection. Reported once per skipped component, before building.
    Skipped {
        /// The component name.
        component: &'a str,
        /// Why it was skipped, for display.
        reason: &'a str,
    },
    /// The run was cancelled. Reported once, after the build phase has wound
    /// down and before the manifest is written, so it reads as the reason the
    /// run stopped short rather than as a component's failure.
    Cancelled,
    /// The provenance manifest was written. Reported once, at run end.
    Manifest {
        /// The path the manifest was written to.
        path: &'a std::path::Path,
    },
}

/// An owned [`Progress`] event, for sending across the channel a parallel build
/// routes worker progress through — [`Progress`] borrows, and a worker cannot
/// hold the reporter, so each event is copied out, sent, and rebuilt on the
/// draining thread by [`report_to`](OwnedProgress::report_to).
enum OwnedProgress {
    Started,
    BuildDate(String),
    Resolving(String),
    Unresolved {
        component: String,
        error: String,
    },
    Ordered(Vec<String>),
    ArchIndepUnowned {
        architectures: Vec<String>,
    },
    Architecture {
        architecture: String,
        index: usize,
        total: usize,
    },
    ForeignArchitecture {
        target: String,
        host: String,
    },
    ArchIndepElsewhere {
        owner: String,
    },
    Layered,
    OverlayUnavailable(String),
    Provisioning {
        component: Option<String>,
    },
    Fetching {
        component: Option<String>,
        url: String,
    },
    PackagesResolved {
        component: Option<String>,
        packages: usize,
    },
    Downloading {
        component: Option<String>,
        package: String,
        index: usize,
        total: usize,
    },
    InstallingToolchain {
        component: Option<String>,
        version: String,
    },
    Extracting {
        component: Option<String>,
        package: String,
        index: usize,
        total: usize,
    },
    Building {
        component: String,
        index: usize,
        total: usize,
    },
    Vendoring(String),
    Output {
        component: String,
        stream: Stream,
        line: String,
    },
    Built {
        component: String,
        artifacts: usize,
    },
    Published {
        component: String,
        debs: usize,
    },
    Failed {
        component: String,
        error: String,
    },
    Skipped {
        component: String,
        reason: String,
    },
    Cancelled,
    Manifest(PathBuf),
}

impl OwnedProgress {
    /// Rebuilds the borrowed [`Progress`] and delivers it to `reporter`. The
    /// inverse of [`from`](OwnedProgress::from), run on the thread that owns the
    /// reporter.
    fn report_to(&self, reporter: &mut dyn FnMut(Progress)) {
        match self {
            OwnedProgress::Started => reporter(Progress::Started),
            OwnedProgress::BuildDate(date) => reporter(Progress::BuildDate { date }),
            OwnedProgress::Resolving(component) => reporter(Progress::Resolving { component }),
            OwnedProgress::Unresolved { component, error } => {
                reporter(Progress::Unresolved { component, error })
            }
            OwnedProgress::Ordered(order) => reporter(Progress::Ordered { order }),
            OwnedProgress::ArchIndepUnowned { architectures } => {
                reporter(Progress::ArchIndepUnowned { architectures })
            }
            OwnedProgress::Architecture {
                architecture,
                index,
                total,
            } => reporter(Progress::Architecture {
                architecture,
                index: *index,
                total: *total,
            }),
            OwnedProgress::ForeignArchitecture { target, host } => {
                reporter(Progress::ForeignArchitecture { target, host })
            }
            OwnedProgress::ArchIndepElsewhere { owner } => {
                reporter(Progress::ArchIndepElsewhere { owner })
            }
            OwnedProgress::Layered => reporter(Progress::Layered),
            OwnedProgress::OverlayUnavailable(reason) => {
                reporter(Progress::OverlayUnavailable { reason })
            }
            OwnedProgress::Provisioning { component } => reporter(Progress::Provisioning {
                component: component.as_deref(),
            }),
            OwnedProgress::Fetching { component, url } => reporter(Progress::Fetching {
                component: component.as_deref(),
                url,
            }),
            OwnedProgress::PackagesResolved {
                component,
                packages,
            } => reporter(Progress::PackagesResolved {
                component: component.as_deref(),
                packages: *packages,
            }),
            OwnedProgress::Downloading {
                component,
                package,
                index,
                total,
            } => reporter(Progress::Downloading {
                component: component.as_deref(),
                package,
                index: *index,
                total: *total,
            }),
            OwnedProgress::InstallingToolchain { component, version } => {
                reporter(Progress::InstallingToolchain {
                    component: component.as_deref(),
                    version,
                })
            }
            OwnedProgress::Extracting {
                component,
                package,
                index,
                total,
            } => reporter(Progress::Extracting {
                component: component.as_deref(),
                package,
                index: *index,
                total: *total,
            }),
            OwnedProgress::Building {
                component,
                index,
                total,
            } => reporter(Progress::Building {
                component,
                index: *index,
                total: *total,
            }),
            OwnedProgress::Vendoring(component) => reporter(Progress::Vendoring { component }),
            OwnedProgress::Output {
                component,
                stream,
                line,
            } => reporter(Progress::Output {
                component,
                stream: *stream,
                line,
            }),
            OwnedProgress::Built {
                component,
                artifacts,
            } => reporter(Progress::Built {
                component,
                artifacts: *artifacts,
            }),
            OwnedProgress::Published { component, debs } => reporter(Progress::Published {
                component,
                debs: *debs,
            }),
            OwnedProgress::Failed { component, error } => {
                reporter(Progress::Failed { component, error })
            }
            OwnedProgress::Skipped { component, reason } => {
                reporter(Progress::Skipped { component, reason })
            }
            OwnedProgress::Cancelled => reporter(Progress::Cancelled),
            OwnedProgress::Manifest(path) => reporter(Progress::Manifest { path }),
        }
    }
}

impl From<&Progress<'_>> for OwnedProgress {
    fn from(event: &Progress) -> OwnedProgress {
        match event {
            Progress::Started => OwnedProgress::Started,
            Progress::BuildDate { date } => OwnedProgress::BuildDate(date.to_string()),
            Progress::Resolving { component } => OwnedProgress::Resolving(component.to_string()),
            Progress::Unresolved { component, error } => OwnedProgress::Unresolved {
                component: component.to_string(),
                error: error.to_string(),
            },
            Progress::Ordered { order } => OwnedProgress::Ordered(order.to_vec()),
            Progress::ArchIndepUnowned { architectures } => OwnedProgress::ArchIndepUnowned {
                architectures: architectures.to_vec(),
            },
            Progress::Architecture {
                architecture,
                index,
                total,
            } => OwnedProgress::Architecture {
                architecture: architecture.to_string(),
                index: *index,
                total: *total,
            },
            Progress::ForeignArchitecture { target, host } => OwnedProgress::ForeignArchitecture {
                target: target.to_string(),
                host: host.to_string(),
            },
            Progress::ArchIndepElsewhere { owner } => OwnedProgress::ArchIndepElsewhere {
                owner: owner.to_string(),
            },
            Progress::Layered => OwnedProgress::Layered,
            Progress::OverlayUnavailable { reason } => {
                OwnedProgress::OverlayUnavailable(reason.to_string())
            }
            Progress::Provisioning { component } => OwnedProgress::Provisioning {
                component: component.map(str::to_string),
            },
            Progress::Fetching { component, url } => OwnedProgress::Fetching {
                component: component.map(str::to_string),
                url: url.to_string(),
            },
            Progress::PackagesResolved {
                component,
                packages,
            } => OwnedProgress::PackagesResolved {
                component: component.map(str::to_string),
                packages: *packages,
            },
            Progress::Downloading {
                component,
                package,
                index,
                total,
            } => OwnedProgress::Downloading {
                component: component.map(str::to_string),
                package: package.to_string(),
                index: *index,
                total: *total,
            },
            Progress::InstallingToolchain { component, version } => {
                OwnedProgress::InstallingToolchain {
                    component: component.map(str::to_string),
                    version: version.to_string(),
                }
            }
            Progress::Extracting {
                component,
                package,
                index,
                total,
            } => OwnedProgress::Extracting {
                component: component.map(str::to_string),
                package: package.to_string(),
                index: *index,
                total: *total,
            },
            Progress::Building {
                component,
                index,
                total,
            } => OwnedProgress::Building {
                component: component.to_string(),
                index: *index,
                total: *total,
            },
            Progress::Vendoring { component } => OwnedProgress::Vendoring(component.to_string()),
            Progress::Output {
                component,
                stream,
                line,
            } => OwnedProgress::Output {
                component: component.to_string(),
                stream: *stream,
                line: line.to_string(),
            },
            Progress::Built {
                component,
                artifacts,
            } => OwnedProgress::Built {
                component: component.to_string(),
                artifacts: *artifacts,
            },
            Progress::Published { component, debs } => OwnedProgress::Published {
                component: component.to_string(),
                debs: *debs,
            },
            Progress::Failed { component, error } => OwnedProgress::Failed {
                component: component.to_string(),
                error: error.to_string(),
            },
            Progress::Skipped { component, reason } => OwnedProgress::Skipped {
                component: component.to_string(),
                reason: reason.to_string(),
            },
            Progress::Cancelled => OwnedProgress::Cancelled,
            Progress::Manifest { path } => OwnedProgress::Manifest(path.to_path_buf()),
        }
    }
}

/// Options that shape a build run.
///
/// Defaults to the strict single-threaded behavior: one component at a time,
/// stopping at the first failure. Constructed with a struct literal and
/// [`Default`], so later options can be added without disturbing existing call
/// sites.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Keep building the remaining components after one fails, rather than
    /// stopping at the first failure. Every failure is collected into the
    /// [`RunReport`] either way.
    pub keep_going: bool,
    /// How many components to build concurrently. `0` and `1` both build
    /// sequentially; a higher count runs that many worker threads, each building
    /// a component whose in-set producers have all published.
    pub jobs: usize,
    /// Which components to build. Defaults to [`Selection::All`].
    pub selection: Selection,
    /// Skip a component whose source resolves to what a prior run recorded as
    /// built, so a re-run rebuilds only what changed. A component whose source
    /// is unpinned is never skipped, since there is nothing to compare; see
    /// [`ComponentRecord::is_built_at`](crate::manifest::ComponentRecord::is_built_at).
    pub skip_published: bool,
    /// The date every version this run stamps carries. Defaults to
    /// [`BuildDate::Now`].
    pub build_date: BuildDate,
    /// The signal that stops the run. The default is never set, so a caller
    /// that wants no cancellation supplies nothing.
    pub cancel: Cancel,
}

/// The date a run stamps into every version it produces.
///
/// `dpkg-buildpackage` derives `SOURCE_DATE_EPOCH` from the changelog entry
/// src2deb writes, so this settles not only how the packages are versioned but
/// the clock the build itself sees. Pinning it is what makes two runs from the
/// same sources comparable; leaving it at [`Now`](Self::Now) is what makes a
/// rebuild supersede its predecessor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BuildDate {
    /// The moment the run starts, in UTC.
    ///
    /// The default, and what an ordinary build wants: a rebuild of unchanged
    /// pinned sources still sorts above the build before it, so it reaches a
    /// machine that already installed that one. See [`crate::version`].
    #[default]
    Now,
    /// A fixed instant, in seconds since the Unix epoch.
    ///
    /// Two runs given the same instant stamp the same versions and hand
    /// `dpkg-buildpackage` the same `SOURCE_DATE_EPOCH`, which is what a
    /// verification rebuild needs.
    At(i64),
    /// The date the prior manifest records for this recipe, suite, and
    /// architecture.
    ///
    /// Reproduces a recorded build without transcribing its date. Fails with
    /// [`Error::BuildDate`] when no manifest records one — a work directory
    /// this recipe has never built in, or one whose runs never built anything.
    Recorded,
}

/// Which components a run builds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Selection {
    /// Every component in the recipe.
    #[default]
    All,
    /// Only the named components; their in-set dependencies must already be in
    /// the pool.
    Only(Vec<String>),
    /// The named component and every component after it in the build order.
    From(String),
}

impl Selection {
    /// The flag this selection was given as, for a message that has to name it.
    pub fn flag(&self) -> &'static str {
        match self {
            Selection::All => "the selection",
            Selection::Only(_) => "--only",
            Selection::From(_) => "--from",
        }
    }

    /// Rejects a selection naming a component `recipe` does not have.
    ///
    /// Checked against the recipe rather than the build order, so it costs
    /// nothing: component names are in `recipe.toml`, while the order is only
    /// known once every source has been cloned and every `debian/control` read.
    /// A typo is the cheapest thing a run can get wrong, so it is the first
    /// thing checked.
    pub fn validate(&self, recipe: &Recipe) -> Result<()> {
        let named = match self {
            Selection::All => return Ok(()),
            Selection::Only(names) => names.as_slice(),
            Selection::From(name) => std::slice::from_ref(name),
        };
        let known: BTreeSet<&str> = recipe
            .components
            .iter()
            .map(|component| component.name.as_str())
            .collect();
        match named.iter().find(|name| !known.contains(name.as_str())) {
            Some(unknown) => Err(Error::Selection(format!(
                "{} names unknown component {unknown:?}",
                self.flag()
            ))),
            None => Ok(()),
        }
    }

    /// Whether this selection may include `name` when the build order is not
    /// known yet.
    ///
    /// [`Selection::Only`] names components outright, so it answers exactly.
    /// [`Selection::From`] names a *position* in an order that does not exist
    /// until every component has resolved, so it answers conservatively: a
    /// component might be in the selection, and treating it as though it is not
    /// would quietly excuse a failure that matters.
    fn includes_unordered(&self, name: &str) -> bool {
        match self {
            Selection::All | Selection::From(_) => true,
            Selection::Only(names) => names.iter().any(|only| only == name),
        }
    }

    /// The set of component names this selection builds, drawn from the build
    /// `order`.
    ///
    /// Names are validated against the recipe by [`validate`](Self::validate)
    /// before anything resolves, so a name the order does not carry here is a
    /// component whose source did not resolve — already recorded as a failure,
    /// and not something this run can build. [`Selection::Only`] drops it;
    /// [`Selection::From`] cannot, because the selection is everything *after*
    /// an anchor that now has no position, so it returns
    /// [`Error::Selection`].
    fn resolve<'a>(&self, order: &'a [String]) -> Result<BTreeSet<&'a str>> {
        match self {
            Selection::All => Ok(order.iter().map(String::as_str).collect()),
            Selection::Only(names) => Ok(order
                .iter()
                .map(String::as_str)
                .filter(|name| names.iter().any(|only| only == name))
                .collect()),
            Selection::From(name) => {
                let start = order.iter().position(|n| n == name).ok_or_else(|| {
                    Error::Selection(format!(
                        "--from names component {name:?}, whose source did not resolve, so \
                         the components after it in the build order cannot be identified"
                    ))
                })?;
                Ok(order[start..].iter().map(String::as_str).collect())
            }
        }
    }
}

/// A component the run did not build, and why.
#[derive(Debug, Clone)]
pub struct Skipped {
    /// The component name.
    pub component: String,
    /// What its source resolved to.
    pub source: Fingerprint,
    /// Why it was skipped.
    pub reason: SkipReason,
}

/// Why a component was not built this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// It was already built from this source in a prior run
    /// (`--skip-published`).
    AlreadyBuilt,
    /// It was outside the run's `--only`/`--from` selection.
    NotSelected,
    /// Every binary package it declares is `Architecture: all`, and the recipe
    /// hands arch-indep output to another architecture, so this run has nothing
    /// of it to build. See
    /// [`Recipe::owns_arch_indep`](crate::Recipe::owns_arch_indep).
    ArchIndepElsewhere,
    /// The run was cancelled before it finished the component — either before
    /// it was reached at all, or partway through its build.
    Cancelled,
}

impl SkipReason {
    /// Every reason, so a caller tallying them cannot leave one out.
    pub const ALL: [SkipReason; 4] = [
        SkipReason::AlreadyBuilt,
        SkipReason::NotSelected,
        SkipReason::ArchIndepElsewhere,
        SkipReason::Cancelled,
    ];

    /// A short human-readable phrase for the reason.
    pub fn label(self) -> &'static str {
        match self {
            SkipReason::AlreadyBuilt => "already built",
            SkipReason::NotSelected => "not selected",
            SkipReason::ArchIndepElsewhere => "arch-indep owned elsewhere",
            SkipReason::Cancelled => "cancelled",
        }
    }
}

/// A produced binary package: its name and version.
#[derive(Debug, Clone)]
pub struct Package {
    /// The binary package name.
    pub name: String,
    /// The package version.
    pub version: String,
}

/// A component that built and published successfully.
#[derive(Debug, Clone)]
pub struct Built {
    /// The component name.
    pub component: String,
    /// What the component's source resolved to.
    pub source: Fingerprint,
    /// The upstream version the recipe declared for this component, when it
    /// declares one. `None` for a component that takes its version from a
    /// `debian/changelog`. See
    /// [`ResolvedSource::version`](crate::source::ResolvedSource::version).
    pub version: Option<String>,
    /// How this component's version was stamped, which decides whether its
    /// packages sort above or below the archive's own of the same version. See
    /// [`VersionStamp`].
    pub version_stamp: VersionStamp,
    /// The files the build produced, under the run's output tree. One package
    /// contributes more than one — a `.deb` and its `.ddeb` debug companion —
    /// so this is longer than [`packages`](Self::packages).
    pub artifacts: Vec<Artifact>,
    /// The `.buildinfo` the build wrote, recording what the packages were built
    /// against. Absent when the build wrote none.
    pub buildinfo: Option<BuildInfo>,
    /// The distinct packages the build produced, with their versions.
    pub packages: Vec<Package>,
}

/// A component the run could not deliver: one whose build failed, or one whose
/// source never resolved.
#[derive(Debug)]
pub struct Failed {
    /// The component name.
    pub component: String,
    /// What the component's source resolved to, recorded even though the build
    /// failed so the manifest can name the exact input. Empty for a component
    /// that failed before it had one — resolving its source, or reading its
    /// `debian/control`.
    pub source: Fingerprint,
    /// The error that ended the component.
    pub error: Error,
}

/// The outcome of a build run: what each architecture built, what failed, and
/// where the artifacts landed.
///
/// [`Engine::run`] returns this whenever it gets far enough to attempt builds —
/// even when some fail and even when a `keep_going` run is cut short — so a
/// caller can print a closing summary. A failure before anything is built
/// (resolving a source, ordering the graph, bootstrapping the base) leaves
/// nothing to summarize and surfaces as `Err` instead; the same failure met
/// after an architecture has published is in [`stopped_by`](Self::stopped_by).
///
/// The run's shape is one resolve and then one build per architecture, and this
/// splits along the same seam: the order and the sources that would not resolve
/// are the run's, recorded once, and everything a target decides is in
/// [`ArchitectureReport`].
#[derive(Debug)]
pub struct RunReport {
    /// The build order every architecture followed, with any component whose
    /// source did not resolve after it.
    ///
    /// A component's place in the order comes from its `debian/control`, so one
    /// that never resolved has no place in it — but it is still part of the
    /// recipe, and appending it keeps this list naming every component the run
    /// accounted for, which is what the summary counts and the manifest records.
    pub order: Vec<String>,
    /// The components whose source never resolved, in build order.
    ///
    /// Recorded once rather than per architecture: a run resolves its sources
    /// once, so a source that would not resolve is a fact about the run and not
    /// about any one target. Each architecture's own summary and manifest still
    /// account for them — see [`undelivered`](Self::undelivered) — since from a
    /// target's side they are components it did not get.
    pub unresolved: Vec<Failed>,
    /// What the run produced for each architecture, in the order the recipe
    /// names them.
    ///
    /// Shorter than the recipe's list when the run stopped early: cancelled, a
    /// component failed and the run was not told to keep going, or an
    /// architecture ended in [`stopped_by`](Self::stopped_by). At most the last
    /// entry is partial, since each of those stops the run where it is.
    pub architectures: Vec<ArchitectureReport>,
    /// The error that ended the run partway through an architecture, when one
    /// did.
    ///
    /// This is a failure of the kind [`Engine::run`] otherwise returns as
    /// `Err` — a build root that will not provision, a selection the pool
    /// cannot cover — reached after an earlier architecture had already built
    /// and published. That work stands and its manifest is written, so the
    /// report has to reach the caller; the error travels in it rather than in
    /// place of it. An error before anything built is `Err` as it always was.
    pub stopped_by: Option<Error>,
    /// Whether the run was cancelled before it finished. Components an
    /// architecture never reached, and one it stopped partway through, are in
    /// its [`skipped`](ArchitectureReport::skipped) with
    /// [`SkipReason::Cancelled`]; an architecture the run never started has no
    /// report at all.
    pub cancelled: bool,
}

impl RunReport {
    /// Whether every attempted component built successfully, for every
    /// architecture, and nothing ended the run partway.
    ///
    /// A cancelled run can still be successful by this measure: cancellation
    /// stops the run, it does not fail what already built. Check
    /// [`cancelled`](Self::cancelled) alongside it.
    pub fn is_success(&self) -> bool {
        self.unresolved.is_empty()
            && self.stopped_by.is_none()
            && self
                .architectures
                .iter()
                .all(|architecture| architecture.failed.is_empty())
    }

    /// The total number of artifacts produced, across every component and every
    /// architecture.
    ///
    /// What *this run* produced, not what the output trees hold: a run that
    /// skipped everything produced nothing, while the trees still hold whatever
    /// the runs before it built.
    pub fn artifact_count(&self) -> usize {
        self.architectures
            .iter()
            .map(ArchitectureReport::artifact_count)
            .sum()
    }

    /// Every component the run could not deliver for `architecture`: that
    /// architecture's failed builds, then the sources that never resolved.
    ///
    /// The two are one thing to whoever asked for the packages — a component
    /// they did not get — so this is what a summary counts and what a manifest
    /// records as failed. In build order, since an unresolved component has no
    /// place in the order and sorts after the ones that do.
    pub fn undelivered<'a>(
        &'a self,
        architecture: &'a ArchitectureReport,
    ) -> impl Iterator<Item = &'a Failed> {
        architecture.failed.iter().chain(self.unresolved.iter())
    }
}

/// What a run produced for one architecture: what built, what failed, and where
/// the artifacts landed.
#[derive(Debug)]
pub struct ArchitectureReport {
    /// The Debian architecture this was built for.
    pub architecture: String,
    /// The components that built and published, in build order.
    pub built: Vec<Built>,
    /// The components whose build failed, in build order. A component whose
    /// source never resolved is in [`RunReport::unresolved`] instead, since it
    /// failed for the run rather than for this architecture.
    pub failed: Vec<Failed>,
    /// The components this architecture did not build, in build order.
    pub skipped: Vec<Skipped>,
    /// The directory the artifacts were written under
    /// (`work/out/<suite>/<architecture>`), holding one directory per component.
    pub out_dir: PathBuf,
    /// The provenance manifest written for this architecture
    /// (`work/manifests/<recipe>/<suite>/<architecture>.toml`).
    pub manifest_path: PathBuf,
}

impl ArchitectureReport {
    /// The number of artifacts this architecture's built components produced.
    pub fn artifact_count(&self) -> usize {
        self.built.iter().map(|built| built.artifacts.len()).sum()
    }

    /// How many components were skipped for `reason`.
    ///
    /// The four reasons are not one outcome. A run that deliberately built one
    /// component of twenty-seven and a run that was cancelled after one both
    /// report twenty-six skipped, and only the reason tells them apart.
    pub fn skipped_for(&self, reason: SkipReason) -> usize {
        self.skipped
            .iter()
            .filter(|skipped| skipped.reason == reason)
            .count()
    }
}

/// The result of [`Engine::plan`]: the build order and each component's
/// resolved source and build-dependencies, with nothing built.
#[derive(Debug)]
pub struct PlanReport {
    /// The build order.
    pub order: Vec<String>,
    /// Each component, in build order.
    pub components: Vec<PlannedComponent>,
}

/// One component in a [`PlanReport`]: what it resolved to and what it will need.
#[derive(Debug)]
pub struct PlannedComponent {
    /// The component name.
    pub name: String,
    /// What the component's source resolved to.
    pub source: Fingerprint,
    /// The upstream version the recipe declared or derived for this component,
    /// when it declares one, and `None` for a component that takes its version
    /// from a `debian/changelog`.
    ///
    /// Worth reporting because it is the one thing a plan settles that the
    /// recipe does not state outright: `version-from = "git-describe"` reads the
    /// source's tags, and a plan is where to see what they gave before a build
    /// stamps it into a package.
    pub version: Option<String>,
    /// The build-dependency package names, from `debian/control` plus the
    /// recipe's `extra-build-deps`.
    pub build_deps: Vec<String>,
}

/// A component's resolved source paired with its `debian/control` text, held in
/// recipe order between the resolve and build phases.
///
/// Borrows the recipe's own [`Component`], so the resolve phase's output indexes
/// straight to what the build phase needs rather than through a position shared
/// with the recipe — a correspondence that no longer holds once a component can
/// fail to resolve and leave the list.
struct Resolved<'a> {
    component: &'a Component,
    /// The tree that holds the component's `debian/` directory.
    tree: PathBuf,
    /// What the tree was resolved from.
    source: Fingerprint,
    /// The upstream version the recipe declared, when it declares one. See
    /// [`ResolvedSource::version`](crate::source::ResolvedSource::version).
    version: Option<String>,
    /// Whether a build of this tree still needs the vendor pass. See
    /// [`VendorPass`].
    vendor: VendorPass,
    control: String,
}

impl Resolved<'_> {
    /// The component's name.
    fn name(&self) -> &str {
        &self.component.name
    }
}

/// What the resolve phase produced: the components whose source resolved, the
/// order over them, and the ones whose source did not — split by what their
/// failure means to the run.
struct Resolution<'a> {
    trees: Vec<Resolved<'a>>,
    graph: BuildGraph,
    /// Components the run was asked to build and could not.
    failed: Vec<Failed>,
    /// Components outside the run's selection. Recorded as not selected,
    /// because that is why they were not built: the run resolved them only to
    /// complete the build order, and never intended to build them.
    excused: Vec<Skipped>,
}

/// What a component's resolve failure means to the run.
///
/// Every component is resolved whatever the run was asked to build, since the
/// order is derived from the whole set — so a failure has to be weighed against
/// what the run actually wanted from that component.
enum OnUnresolved {
    /// End the run. The default: a component the run was going to build cannot
    /// be built, and nothing was said about carrying on without it.
    Fatal,
    /// Record it as a failed component and carry on
    /// ([`RunOptions::keep_going`]).
    Fail,
    /// Record it as not selected and carry on. The run was never going to
    /// build it, so its source failing is not this run's failure — and making
    /// it one would mean a selective build could never succeed while any
    /// component of its recipe was unreachable.
    Excuse,
}

/// What every architecture of a run shares, settled once before the first of
/// them builds.
///
/// Sources resolve once for the whole run, so the trees, the order over them,
/// and the run's stamp are the run's rather than any target's — which is what
/// makes two architectures of one run a build of the same commits at the same
/// versions. Each architecture reads this unchanged and decides only what its
/// own target settles.
struct RunContext<'a> {
    recipe: &'a Recipe,
    /// Each resolved component, by name.
    resolved: BTreeMap<&'a str, &'a Resolved<'a>>,
    graph: &'a BuildGraph,
    /// Every component the run accounts for, in build order, with the ones that
    /// never resolved after them.
    order: &'a [String],
    /// The components the run's selection builds, drawn from the build order.
    selected: BTreeSet<&'a str>,
    /// Components resolved only to complete the build order and never meant to
    /// be built. Copied into each architecture's skip list, since each of them
    /// passed over the component for this same reason.
    excused: &'a [Skipped],
    /// Components whose source never resolved. Recorded as failed in every
    /// architecture's manifest: from a target's side they are components it did
    /// not get.
    unresolved: &'a [Failed],
    stamp: &'a crate::version::BuildStamp,
    /// Why the host cannot establish an unprivileged overlay under the work
    /// directory, or `None` when it can — which decides the provisioning
    /// strategy. Probed once for the run, since it is a property of the host and
    /// the work directory rather than of any target.
    overlay_blocker: Option<String>,
    options: &'a RunOptions,
}

/// One architecture's prior manifest and where it lives, read before the run
/// starts building.
///
/// Read up front for every architecture rather than one at a time, because
/// [`BuildDate::Recorded`] settles one date for the whole run and so has to see
/// all of them together.
struct Prior<'a> {
    architecture: &'a str,
    /// Where this architecture's manifest is read from and written back to.
    path: PathBuf,
    /// What the last run recorded for this architecture, or `None` for a work
    /// directory holding no build of this recipe for this target.
    manifest: Option<Manifest>,
}

/// Drives a build run over a recipe.
pub struct Engine {
    work_dir: PathBuf,
}

impl Engine {
    /// Creates an engine that keeps its sources, roots, cache, pool, and output
    /// under `work_dir`.
    pub fn new(work_dir: impl Into<PathBuf>) -> Engine {
        Engine {
            work_dir: work_dir.into(),
        }
    }

    /// Creates the work directory if needed and takes the whole-directory lock,
    /// returning the guard to hold for the operation.
    fn lock_work_dir(&self) -> Result<WorkLock> {
        std::fs::create_dir_all(&self.work_dir)
            .map_err(|err| io_error("creating", &self.work_dir, err))?;
        WorkLock::acquire(&self.work_dir)
    }

    /// Builds every component in `recipe`, in dependency order and for every
    /// architecture the recipe names, reporting progress through `reporter` and
    /// returning a [`RunReport`] of what built and what failed.
    ///
    /// Sources are resolved and ordered once, and each architecture is then
    /// built in turn against that one resolution — so two architectures of one
    /// run are built from the same commits at the same stamped versions, which
    /// a pair of separate runs cannot guarantee against a moving branch. Each
    /// architecture gets a pool, an output tree, and a manifest of its own.
    ///
    /// The architectures are built sequentially, in the order the recipe names
    /// them. [`RunOptions::jobs`] parallelizes the components within one
    /// architecture, not the architectures themselves: a foreign build is
    /// emulated, and running two of those alongside each other contends for the
    /// same cores and the same package cache without finishing sooner.
    ///
    /// A component's failure is collected into the report rather than
    /// propagated; with [`RunOptions::keep_going`] the run continues to the next
    /// component, and without it the run stops but still returns the report so
    /// far. That covers a component whose source will not resolve, whose
    /// `debian/control` or `debian/changelog` cannot be read, and whose build
    /// fails — so one unreachable repository or one malformed changelog does not
    /// deny every other component its build. A component outside the run's
    /// selection is weaker still: it was never going to be built, so its source
    /// failing to resolve is recorded and carried past whatever
    /// [`keep_going`](RunOptions::keep_going) says.
    ///
    /// What remains fatal is what leaves the run with nothing coherent to do:
    /// a selection naming a component the recipe does not have or that did not
    /// resolve, a target suite with no version tag, a dependency cycle, a
    /// selection that leaves out a producer of a selected component's
    /// build-dependencies, and bootstrapping the shared base. Each returns
    /// `Err`.
    ///
    /// A failure ends the run rather than only the architecture it happened in:
    /// without [`keep_going`](RunOptions::keep_going) the run stops where it is,
    /// and the architectures it had not started have no report. With it, every
    /// architecture is attempted and the report tallies them all.
    ///
    /// The fatal errors above end the run whatever `keep_going` says — that is
    /// what makes them fatal — and are returned as `Err` while nothing has been
    /// built. Once an architecture has published its packages and written its
    /// manifest, that work stands, so a later architecture's fatal error is
    /// carried in [`RunReport::stopped_by`](RunReport::stopped_by) rather than
    /// discarding the report along with it.
    ///
    /// Cancellation follows the same shape. Cancelled before the build loop —
    /// while sources are resolving, or while the shared base bootstraps — the
    /// run has nothing to report and returns [`Error::Cancelled`]; cancelled
    /// once building has started, it winds down and returns a report marked
    /// [`cancelled`](RunReport::cancelled), so what already built is still
    /// recorded and published.
    pub fn run(
        &mut self,
        recipe: &Recipe,
        options: &RunOptions,
        reporter: &mut dyn FnMut(Progress),
    ) -> Result<RunReport> {
        // Hold the work directory for the whole run, so a second run against the
        // same `--work` is rejected rather than corrupting the shared pool, out
        // trees, and source checkouts. One lock covers every architecture: the
        // run is one operation over one work directory, whatever it targets.
        let _lock = self.lock_work_dir()?;
        reporter(Progress::Started);

        // 1. Settle everything the run's own arguments decide, before it spends
        //    anything on the network or the archive. Each is a usage error — a
        //    `--only` naming a component the recipe does not have, a target
        //    suite with no version tag, a build date to be taken from manifests
        //    that record none — and each is answerable from `recipe.toml` and
        //    files already on disk, so none costs the run a single clone.
        options.selection.validate(recipe)?;
        // A prior run's manifest carries the state `--skip-published` consults,
        // the date `BuildDate::Recorded` reproduces, and the records this run
        // folds forward so untouched components stay recorded. There is one per
        // architecture, so a work directory shared with another recipe — or with
        // the same recipe targeted elsewhere — neither loses its provenance nor
        // offers records of packages that were never built for that target. All
        // of them are read here because the run's date is settled from them
        // together, before the first architecture builds.
        let mut priors: Vec<Prior> = Vec::new();
        for architecture in &recipe.architectures {
            let path =
                manifest::manifest_path(&self.work_dir, &recipe.name, &recipe.suite, architecture);
            let manifest = Manifest::load(&path)?;
            priors.push(Prior {
                architecture,
                path,
                manifest,
            });
        }
        // One stamp for the whole run, so every package it produces carries the
        // same build date however long the run takes, however many components
        // build at once, and however many architectures it targets. A validated
        // recipe always resolves a tag, but a caller may replace the suite after
        // that validation, so this is checked and not assumed.
        let tag = recipe
            .resolved_version_tag()
            .ok_or_else(|| Error::VersionTag {
                suite: recipe.suite.clone(),
            })?;
        let stamp = build_stamp(tag, options.build_date, &priors)?;
        if options.build_date != BuildDate::Now {
            reporter(Progress::BuildDate {
                date: &stamp.calendar_date(),
            });
        }

        // 2. Resolve each component's source, read its control, and order the
        //    build — once, whatever the run goes on to target. A source is not
        //    architecture-dependent, and resolving once is what makes every
        //    architecture of a run a build of the same commits: a second resolve
        //    against a moving branch could hand the second architecture a
        //    different tree under the same stamped version. A component that
        //    will not resolve ends the run only when the run was going to build
        //    it and was not told to carry on regardless.
        let decide = |name: &str| {
            if !options.selection.includes_unordered(name) {
                OnUnresolved::Excuse
            } else if options.keep_going {
                OnUnresolved::Fail
            } else {
                OnUnresolved::Fatal
            }
        };
        let Resolution {
            trees,
            graph,
            failed: unresolved,
            excused,
        } = self.resolve_and_order(recipe, &stamp, &decide, &options.cancel, reporter)?;

        // 3. The components this run accounts for: the derived build order, then
        //    whatever never resolved into it, in recipe order.
        let mut order: Vec<String> = graph.order().to_vec();
        let ordered: BTreeSet<&str> = graph.order().iter().map(String::as_str).collect();
        order.extend(
            recipe
                .components
                .iter()
                .map(|component| component.name.as_str())
                .filter(|name| !ordered.contains(name))
                .map(str::to_string),
        );
        // 4. Which provisioning strategy the run uses. Probed once and stated
        //    once: whether the host can establish an unprivileged overlay under
        //    this work directory is a property of the two of them, not of any
        //    target, so asking per architecture would both repeat the probe and
        //    report a run-wide fact as though it varied.
        let overlay_blocker =
            ferroday_cage::host::overlay_blocker(&self.work_dir).map(|blocker| blocker.to_string());
        match &overlay_blocker {
            None => reporter(Progress::Layered),
            Some(reason) => reporter(Progress::OverlayUnavailable { reason }),
        }
        report_arch_indep_unowned(recipe, reporter);

        let context = RunContext {
            recipe,
            resolved: trees.iter().map(|entry| (entry.name(), entry)).collect(),
            graph: &graph,
            order: &order,
            selected: options.selection.resolve(graph.order())?,
            excused: &excused,
            unresolved: &unresolved,
            stamp: &stamp,
            overlay_blocker,
            options,
        };

        // 5. Build each architecture in turn against that one resolution. A
        //    failure the run was not told to carry past stops it here rather
        //    than only inside the architecture it happened in: the run was asked
        //    for a set of packages, and it has not delivered them.
        let mut architectures: Vec<ArchitectureReport> = Vec::new();
        let mut stopped_by: Option<Error> = None;
        let total = priors.len();
        for (index, prior) in priors.into_iter().enumerate() {
            if options.cancel.requested() {
                break;
            }
            reporter(Progress::Architecture {
                architecture: prior.architecture,
                index: index + 1,
                total,
            });
            match self.run_architecture(&context, prior, reporter) {
                Ok(report) => {
                    let failed = !report.failed.is_empty();
                    architectures.push(report);
                    if failed && !options.keep_going {
                        break;
                    }
                }
                // A cancel is an outcome the report already carries, so it needs
                // no error beside it.
                Err(Error::Cancelled) if !architectures.is_empty() => break,
                // Nothing has been built, so there is nothing to report and the
                // error is the whole answer — which is what a single-architecture
                // run has always returned.
                Err(error) if architectures.is_empty() => return Err(error),
                // An architecture before this one built and published packages,
                // and its manifest is written. That stands whatever went wrong
                // here, so the report reaches the caller and carries the error
                // rather than being discarded with it.
                Err(error) => {
                    stopped_by = Some(error);
                    break;
                }
            }
        }

        // A cancel is reported once for the run, after the architecture that saw
        // it has wound down and recorded what it managed — so it reads as the
        // reason the run stopped short rather than as one target's failure.
        let cancelled = options.cancel.requested();
        if cancelled {
            reporter(Progress::Cancelled);
        }
        Ok(RunReport {
            order,
            unresolved,
            architectures,
            stopped_by,
            cancelled,
        })
    }

    /// Builds every selected component for one architecture, publishes them to
    /// that architecture's pool, and writes its manifest.
    ///
    /// The back half of [`run`](Self::run), which resolves the sources and then
    /// calls this once per architecture. Everything it reads about the
    /// components is settled in `context` and shared unchanged; what it decides
    /// for itself is what the target settles — which of the recipe's binary
    /// packages to build, which pool to publish into, and which prior manifest
    /// `--skip-published` is answered from.
    fn run_architecture(
        &self,
        context: &RunContext,
        prior: Prior,
        reporter: &mut dyn FnMut(Progress),
    ) -> Result<ArchitectureReport> {
        let RunContext {
            recipe, options, ..
        } = *context;
        let architecture = prior.architecture;
        report_architecture(recipe, architecture, reporter);

        // The pool is scoped to this suite and architecture, the identity the
        // manifest and the output tree are keyed by: an `Architecture: all`
        // package's file name carries no architecture, so one pool shared across
        // them would overwrite a file and strand the other architecture's index
        // on a stale checksum. See `pool::pool_dir`.
        //
        // Its `Release` is dated with the run's own stamp, which is settled
        // before any architecture starts, so every pool a run publishes carries
        // one date and a run pinned with `--build-date` writes the same
        // `Release` every time.
        let pool = LocalPool::publishing(
            crate::pool::pool_dir(&self.work_dir, &recipe.suite, architecture),
            recipe.suite.clone(),
            crate::pool::POOL_COMPONENT,
            architecture.to_string(),
            crate::pool::PoolRelease {
                date: context.stamp.seconds(),
                origin: recipe.origin.clone(),
                label: recipe.label.clone(),
                description: recipe.description.clone(),
            },
        );

        // 1. Decide which components to build, from this architecture's prior
        //    manifest: it is what `--skip-published` consults, and what the
        //    manifest written below folds forward so untouched components stay
        //    recorded.
        let (prior_build_date, prior_sandbox, prior_archives, prior_interpreter, prior_records) =
            match prior.manifest {
                Some(manifest) => (
                    manifest.build_date,
                    manifest.sandbox,
                    manifest.archives,
                    manifest.interpreter,
                    manifest
                        .components
                        .into_iter()
                        .map(|record| (record.name.clone(), record))
                        .collect(),
                ),
                None => (
                    None,
                    None,
                    Vec::new(),
                    None,
                    BTreeMap::<String, manifest::ComponentRecord>::new(),
                ),
            };

        let builder = Builder::new(
            recipe.toolchain.rust.rustup_version().map(str::to_string),
            options.cancel.clone(),
        );
        let out_root = crate::build::output_dir(&self.work_dir, &recipe.suite, architecture);
        // Which binary packages each component's build produces. Unless the
        // recipe hands its arch-indep output to a different architecture, that
        // is all of them, which is what a pool served as it stands wants.
        let owns_arch_indep = recipe.owns_arch_indep(architecture);
        let binaries = if owns_arch_indep {
            Binaries::All
        } else {
            Binaries::ArchitectureDependent
        };
        let mut items: Vec<WorkItem> = Vec::new();
        let mut skipped: Vec<Skipped> = context.excused.to_vec();
        for name in context.graph.order() {
            let entry = context.resolved[name.as_str()];
            // Resolved per component rather than once for the run: a recipe may
            // mix rebuilds of archive packages with software the archive does
            // not carry, and the two order differently on purpose.
            let version_stamp = recipe.resolved_version_stamp(entry.component);
            let identity = BuildIdentity {
                source: &entry.source,
                version: entry.version.as_deref(),
                version_stamp,
            };
            if let Some(reason) = skip_reason(
                name,
                &identity,
                options,
                &context.selected,
                &prior_records,
                owns_arch_indep || plan::has_architecture_dependent_packages(&entry.control),
            ) {
                reporter(Progress::Skipped {
                    component: name,
                    reason: reason.label(),
                });
                skipped.push(Skipped {
                    component: name.clone(),
                    source: entry.source.clone(),
                    reason,
                });
                continue;
            }
            let component = entry.component;
            let mut build_deps = plan::build_dependencies(&entry.control);
            build_deps.extend(component.extra_build_deps.iter().cloned());
            items.push(WorkItem {
                name,
                component,
                tree: &entry.tree,
                source: &entry.source,
                version: entry.version.as_deref(),
                version_stamp,
                vendor: entry.vendor,
                binaries,
                build_deps,
                stamp: context.stamp,
                suite: &recipe.suite,
                out_root: &out_root,
            });
        }

        // 2. The last thing the run's own arrangement can get wrong, and the last
        //    that costs nothing to check: a component this architecture leaves
        //    out that produces a build-dependency of one it builds. The pool
        //    answers it from a file, where provisioning would answer it only
        //    after the shared base is bootstrapped, in the provisioner's
        //    vocabulary.
        refuse_unbuildable_run(
            &items,
            context.graph,
            &skipped,
            &options.selection,
            recipe.resolved_arch_indep_owner(architecture),
            &pool,
        )?;

        // 3. Provisioner. Prefer layered provisioning — one shared base plus a
        //    disposable per-component overlay — when the host supports an
        //    unprivileged overlay; otherwise fall back to full reprovisioning.
        //    Both share the content-addressed package cache and the local pool.
        //    The roots are keyed by target, so each architecture of a run keeps
        //    a warm base of its own rather than discarding the last one's.
        let config = ProvisionConfig::new(
            recipe.suite.clone(),
            architecture.to_string(),
            recipe.mirror.clone(),
            Some(self.work_dir.join("cache")),
            recipe.repositories.clone(),
            recipe.toolchain.rust.rustup_version().map(str::to_string),
        );
        // `+ Sync` so a parallel build can share one provider across worker
        // threads; both strategies hold only immutable configuration after
        // `prepare`, and `build_root` takes `&self`.
        let mut provider: Box<dyn BuildRootProvider + Sync> = if context.overlay_blocker.is_none() {
            Box::new(LayeredProvision::new(
                config,
                crate::provision::base_dir(&self.work_dir, &recipe.suite, architecture),
                crate::provision::uppers_dir(&self.work_dir, &recipe.suite, architecture),
                options.cancel.clone(),
            ))
        } else {
            Box::new(FullReprovision::new(
                config,
                crate::provision::roots_dir(&self.work_dir, &recipe.suite, architecture),
                options.cancel.clone(),
            ))
        };

        // 4. Prepare the shared state, but only once this architecture knows it
        //    has work. One that builds nothing — everything already published,
        //    or nothing selected — has no use for either, and both are expensive
        //    to touch: the bootstrap re-resolves and reinstalls several hundred
        //    packages whenever the archive has moved, and a publish replaces the
        //    pool's `Release`, discarding any signature it carried.
        if !items.is_empty() {
            pool.init()?;
            provider.prepare(reporter)?;
        }

        // 5. Build the selected components, sequentially or across worker threads.
        let outcomes = if options.jobs.max(1) == 1 {
            self.build_sequential(
                &items,
                provider.as_ref(),
                &builder,
                &pool,
                options,
                reporter,
            )
        } else {
            self.build_parallel(
                &items,
                provider.as_ref(),
                &builder,
                &pool,
                context.graph,
                options,
                reporter,
            )
        };

        let Outcomes {
            mut built,
            mut failed,
            sandbox,
        } = outcomes;
        // Every component's position among the ones the run accounted for, for
        // putting its outcomes back into that order.
        let position: BTreeMap<&str, usize> = context
            .order
            .iter()
            .enumerate()
            .map(|(index, name)| (name.as_str(), index))
            .collect();
        // A parallel run collects outcomes as its workers finish, so they arrive
        // in completion order — which varies run to run and by job count. Sort
        // them back into build order, so a report reads the same however the run
        // was scheduled, as the manifest and the sandbox record already do.
        in_build_order(&mut built, &position, |built| built.component.as_str());
        in_build_order(&mut failed, &position, |failed| failed.component.as_str());
        // The build-order position only served to choose between components;
        // the manifest records the record itself. An architecture that built
        // nothing keeps the prior one, exactly as a skipped component keeps its
        // prior record: the packages the manifest still calls built were built
        // under it, and dropping it would leave them unaccounted for.
        let sandbox = sandbox.map(|(_, record)| record).or(prior_sandbox);
        // The archives every root this architecture provisioned resolved
        // against, read once every root is provisioned. A run that provisioned
        // nothing resolved nothing and keeps what the prior manifest holds, the
        // same carry-forward the sandbox record gets.
        let archives: Vec<manifest::ArchiveRecord> = provider
            .archives()
            .iter()
            .map(manifest::ArchiveRecord::of)
            .collect();
        let archives = match archives.is_empty() {
            false => archives,
            true => prior_archives,
        };

        // A cancelled run stops before reaching every component it selected.
        // Record those, and the one it stopped partway through, so the manifest
        // keeps what each resolved to and the summary accounts for them rather
        // than passing over them in silence.
        if options.cancel.requested() {
            let selected: Vec<(&str, &Fingerprint)> =
                items.iter().map(|item| (item.name, item.source)).collect();
            skipped.extend(unfinished(&selected, &built, &failed));
        }
        // The skipped were collected in three passes — the components that never
        // resolved, those the build order passed over, and those a cancel never
        // reached — so restore the single order the report documents.
        in_build_order(&mut skipped, &position, |skipped| {
            skipped.component.as_str()
        });

        let report = ArchitectureReport {
            architecture: architecture.to_string(),
            built,
            failed,
            skipped,
            out_dir: out_root,
            manifest_path: prior.path,
        };
        // Record this architecture's provenance last, once every outcome is
        // known, folding in prior records for components it did not touch.
        // The run's own date when it built something, and the prior one when it
        // did not — the same carry-forward the sandbox record gets, and for the
        // same reason: the packages this manifest still calls built were stamped
        // with that date.
        let build_date = match report.built.is_empty() {
            false => Some(context.stamp.calendar_date()),
            true => prior_build_date,
        };
        // The interpreter a foreign build ran every target binary through, read
        // from the kernel's own binfmt registration. A run that built nothing
        // ran nothing through it and keeps what is already recorded; a native
        // run has none, and a native run after a foreign one for this same
        // architecture is not a thing a work directory can hold, since the
        // manifest is keyed by architecture.
        let interpreter = match report.built.is_empty() {
            false => manifest::InterpreterRecord::of(architecture),
            true => prior_interpreter,
        };
        manifest_for_architecture(
            recipe,
            context.order,
            context.unresolved,
            &report,
            &prior_records,
            &self.work_dir,
        )
        .with_sandbox(sandbox)
        .with_archives(archives)
        .with_interpreter(interpreter)
        .with_build_date(build_date)
        .write(&report.manifest_path)?;
        reporter(Progress::Manifest {
            path: &report.manifest_path,
        });
        Ok(report)
    }

    /// Resolves sources and computes the build order without building anything,
    /// returning a [`PlanReport`] of the order and each component's resolved
    /// source and build-dependencies.
    ///
    /// Like the start of [`run`](Self::run), this clones or fetches every
    /// component's source, because the build order is read from each
    /// `debian/control`; it stops before provisioning or building. Cloning is
    /// the slow part, so `cancel` stops it between components; a plan has
    /// nothing partial to return, so a cancelled one is
    /// [`Error::Cancelled`].
    ///
    /// The report itself is one order over one set of sources, whatever the
    /// recipe targets, since neither depends on an architecture. What does is
    /// reported as it goes: each architecture the recipe names is announced with
    /// what building for it would mean.
    ///
    /// Every failure is fatal, unlike a run's. A plan's whole result is the
    /// order, and an order derived from some of the components is not a partial
    /// answer but a different one — so a source that will not resolve ends the
    /// plan rather than quietly dropping a component out of it.
    pub fn plan(
        &self,
        recipe: &Recipe,
        cancel: &Cancel,
        reporter: &mut dyn FnMut(Progress),
    ) -> Result<PlanReport> {
        // Planning clones sources under the work directory, so it takes the same
        // lock a build does.
        let _lock = self.lock_work_dir()?;
        reporter(Progress::Started);
        // A resolve may write a `debian/changelog` for a component that declares
        // its version, and that entry carries the run's date. Planning stamps no
        // version of its own, so the tag this stamp holds is never read; a
        // suite with no tag is a build's error to report, not a plan's.
        let stamp = crate::version::BuildStamp::now(recipe.resolved_version_tag().unwrap_or(""));
        let Resolution { trees, graph, .. } =
            self.resolve_and_order(recipe, &stamp, &|_| OnUnresolved::Fatal, cancel, reporter)?;
        // The order is one answer for the whole recipe, but what each target
        // would cost and produce is not, so that half is reported per
        // architecture — which is the point of planning before committing a
        // machine to a build.
        report_arch_indep_unowned(recipe, reporter);
        for (index, architecture) in recipe.architectures.iter().enumerate() {
            reporter(Progress::Architecture {
                architecture,
                index: index + 1,
                total: recipe.architectures.len(),
            });
            report_architecture(recipe, architecture, reporter);
        }
        let resolved: BTreeMap<&str, &Resolved> =
            trees.iter().map(|entry| (entry.name(), entry)).collect();

        let components = graph
            .order()
            .iter()
            .map(|name| {
                let entry = resolved[name.as_str()];
                let mut build_deps = plan::build_dependencies(&entry.control);
                build_deps.extend(entry.component.extra_build_deps.iter().cloned());
                PlannedComponent {
                    name: name.clone(),
                    source: entry.source.clone(),
                    version: entry.version.clone(),
                    build_deps,
                }
            })
            .collect();
        Ok(PlanReport {
            order: graph.order().to_vec(),
            components,
        })
    }

    /// Copies every package the work directory records as built for `recipe`
    /// into an export directory an archive tool ingests, and reports what it
    /// carried.
    ///
    /// Reads the work directory and writes nothing into it, so it neither
    /// resolves a source nor provisions a root — but it reads the output trees
    /// and manifests a run writes, so it takes the same lock a build does rather
    /// than reading them while a run is replacing them.
    ///
    /// The recipe supplies the name, the suite, and the arch-indep owner. Which
    /// architectures are carried comes from the work directory; see
    /// [`crate::export`].
    pub fn export(
        &self,
        recipe: &Recipe,
        options: &crate::export::ExportOptions,
    ) -> Result<crate::export::ExportReport> {
        let _lock = self.lock_work_dir()?;
        crate::export::export(&self.work_dir, recipe, options)
    }

    /// Removes superseded packages from the pools the recipe's suite holds,
    /// keeping the newest [`keep`](crate::pool::PruneOptions::keep) versions of
    /// each binary package.
    ///
    /// Scoped to a suite rather than to a recipe: a pool is one archive, and
    /// every recipe built into a work directory for that suite and architecture
    /// publishes into it. The recipe names the suite and, through
    /// [`PruneOptions`](crate::pool::PruneOptions), narrows the architectures.
    ///
    /// Takes the work-directory lock. Pruning removes files a client that read
    /// an earlier `Release` may still be fetching, so it must not run while a
    /// build is publishing into the same pool.
    pub fn prune(
        &self,
        recipe: &Recipe,
        options: &crate::pool::PruneOptions,
    ) -> Result<crate::pool::PruneReport> {
        let _lock = self.lock_work_dir()?;
        crate::pool::prune(&self.work_dir, &recipe.suite, options)
    }

    /// Resolves the runtime dependencies of every package the recipe's suite
    /// holds a pool of, and reports the ones nothing available satisfies.
    ///
    /// Where [`run`](Self::run) validates that each component *builds*, this
    /// validates that what it produced can be *installed*: a package whose
    /// `Depends` names something neither the target suite nor the pool has
    /// builds perfectly and is refused by apt. See [`crate::check`] for how a
    /// dependency is settled.
    ///
    /// Scoped to a suite rather than to a recipe, like
    /// [`prune`](Self::prune) and for the same reason: a pool is one archive
    /// whichever recipe built into it, so what is checked is the archive as it
    /// stands rather than one recipe's share of it. The recipe names the suite,
    /// the mirror, and the additional repositories to resolve against, and
    /// [`CheckOptions`](crate::check::CheckOptions) narrows the architectures.
    ///
    /// Takes the work-directory lock: the pool it reads is the one a build
    /// publishes into.
    pub fn check(
        &self,
        recipe: &Recipe,
        options: &crate::check::CheckOptions,
        reporter: &mut dyn FnMut(crate::check::CheckProgress),
    ) -> Result<crate::check::CheckReport> {
        let _lock = self.lock_work_dir()?;
        crate::check::check(&self.work_dir, recipe, options, reporter)
    }

    /// Resolves every component's source, reads its `debian/control`, and
    /// computes the build order over the components that resolved. The shared
    /// front half of [`run`](Self::run) and [`plan`](Self::plan).
    ///
    /// Every component is resolved, whatever the run was asked to build,
    /// because the order is derived from the whole set: which component produces
    /// a package is read from that component's own control file, so the graph is
    /// only complete once every one has been read.
    ///
    /// A component's source may fail to resolve without ending the run.
    /// `decide` weighs each failure as it happens, so one that ends the run does
    /// so before the remaining components are cloned, and one that does not is
    /// reported where it occurred rather than only in the closing summary.
    /// Ordering the components that did resolve is fatal either way — a cycle is
    /// a property of the recipe, not of one component.
    ///
    /// `stamp` dates the `debian/changelog` a resolve writes for a component
    /// that declares its version, so every entry a run produces carries the run's
    /// own date. Nothing else here reads it.
    fn resolve_and_order<'a>(
        &self,
        recipe: &'a Recipe,
        stamp: &crate::version::BuildStamp,
        decide: &dyn Fn(&str) -> OnUnresolved,
        cancel: &Cancel,
        reporter: &mut dyn FnMut(Progress),
    ) -> Result<Resolution<'a>> {
        // The recipe's own directory, so a relative `source.path` resolves
        // against the recipe rather than against wherever src2deb was invoked;
        // the identity and the date a component declaring its own version is
        // given a `debian/changelog` from.
        let resolver = SourceResolver::new(
            &self.work_dir,
            recipe.dir(),
            recipe.maintainer.as_deref(),
            stamp,
        );

        let mut trees: Vec<Resolved> = Vec::new();
        let mut failed: Vec<Failed> = Vec::new();
        let mut excused: Vec<Skipped> = Vec::new();
        for component in &recipe.components {
            // A clone runs as a `git` child this does not hold a handle to, so
            // the boundary a cancel is honored at is between components.
            if cancel.requested() {
                return Err(Error::Cancelled);
            }
            reporter(Progress::Resolving {
                component: &component.name,
            });
            let outcome = resolver.resolve(component).and_then(|resolved| {
                let control = plan::read_control(&component.name, &resolved.tree)?;
                Ok((resolved, control))
            });
            let error = match outcome {
                Ok((
                    ResolvedSource {
                        tree,
                        source,
                        version,
                        vendor,
                    },
                    control,
                )) => {
                    trees.push(Resolved {
                        component,
                        tree,
                        source,
                        version,
                        vendor,
                        control,
                    });
                    continue;
                }
                Err(error) => error,
            };
            match decide(&component.name) {
                OnUnresolved::Fatal => return Err(error),
                OnUnresolved::Fail => {
                    reporter(Progress::Failed {
                        component: &component.name,
                        error: &error.to_string(),
                    });
                    failed.push(Failed {
                        component: component.name.clone(),
                        // No source: the failure is that there is no resolved
                        // tree to name one from.
                        source: Fingerprint::none(),
                        error,
                    });
                }
                OnUnresolved::Excuse => {
                    reporter(Progress::Unresolved {
                        component: &component.name,
                        error: &error.to_string(),
                    });
                    excused.push(Skipped {
                        component: component.name.clone(),
                        source: Fingerprint::none(),
                        reason: SkipReason::NotSelected,
                    });
                }
            }
        }

        let controls: Vec<(String, String)> = trees
            .iter()
            .map(|entry| (entry.name().to_string(), entry.control.clone()))
            .collect();
        let graph = BuildGraph::resolve(&controls)?;
        reporter(Progress::Ordered {
            order: graph.order(),
        });
        Ok(Resolution {
            trees,
            graph,
            failed,
            excused,
        })
    }

    /// Builds every component sequentially, in build order, collecting outcomes.
    /// A component's failure is recorded; without [`RunOptions::keep_going`] the
    /// loop stops at the first, and either way the outcomes so far are returned.
    /// A cancelled run stops too, and the component it was building is neither
    /// built nor failed — the caller records it as cancelled.
    ///
    /// Returns no error: every way a component's build can end is an outcome,
    /// which is the whole point of the build phase and what lets a run report on
    /// what it managed.
    fn build_sequential(
        &self,
        items: &[WorkItem],
        provider: &dyn BuildRootProvider,
        builder: &Builder,
        pool: &LocalPool,
        options: &RunOptions,
        reporter: &mut dyn FnMut(Progress),
    ) -> Outcomes {
        let total = items.len();
        let mut outcomes = Outcomes::default();
        for (position, item) in items.iter().enumerate() {
            if options.cancel.requested() {
                break;
            }
            reporter(Progress::Building {
                component: item.name,
                index: position + 1,
                total,
            });
            match self.build_and_publish(item, provider, builder, pool, reporter) {
                Ok(outcome) => outcomes.keep(position, item, outcome),
                // A cancelled component is neither built nor failed: the run
                // stopped it, so it is left for the caller to record as
                // cancelled alongside the components never reached.
                Err(Error::Cancelled) => break,
                Err(error) => {
                    reporter(Progress::Failed {
                        component: item.name,
                        error: &error.to_string(),
                    });
                    outcomes.failed.push(item.failed(error));
                    if !options.keep_going {
                        break;
                    }
                }
            }
        }
        outcomes
    }

    /// Builds components across `jobs` worker threads, releasing each the moment
    /// its in-set producers have published. Progress is routed to `reporter` on
    /// this thread through a channel, since a worker's reporter is not shareable.
    ///
    /// Returns no error, for the same reason
    /// [`build_sequential`](Self::build_sequential) does not.
    #[allow(clippy::too_many_arguments)]
    fn build_parallel(
        &self,
        items: &[WorkItem],
        provider: &(dyn BuildRootProvider + Sync),
        builder: &Builder,
        pool: &LocalPool,
        graph: &BuildGraph,
        options: &RunOptions,
        reporter: &mut dyn FnMut(Progress),
    ) -> Outcomes {
        // Position-indexed graph over the components being built, for the
        // scheduler. Both in-degrees and dependents are restricted to this run's
        // items: a producer that was skipped (already published, or not selected)
        // is not in the set, so it neither counts against a consumer's in-degree
        // nor gates it — the consumer resolves its packages from the pool.
        let position: BTreeMap<&str, usize> = items
            .iter()
            .enumerate()
            .map(|(i, item)| (item.name, i))
            .collect();
        let dependents: Vec<Vec<usize>> = items
            .iter()
            .map(|item| {
                graph
                    .dependents(item.name)
                    .iter()
                    .filter_map(|d| position.get(d.as_str()).copied())
                    .collect()
            })
            .collect();
        let mut in_degree = vec![0usize; items.len()];
        for edges in &dependents {
            for &dependent in edges {
                in_degree[dependent] += 1;
            }
        }

        let jobs = options.jobs.max(1).min(items.len().max(1));
        let state = Mutex::new(ParallelState {
            scheduler: Scheduler::new(in_degree, dependents),
            outcomes: Outcomes::default(),
        });
        let ready = Condvar::new();
        let (tx, rx) = mpsc::channel::<OwnedProgress>();

        std::thread::scope(|scope| {
            for _ in 0..jobs {
                let tx = tx.clone();
                scope.spawn(|| {
                    self.worker(items, provider, builder, pool, &state, &ready, options, tx);
                });
            }
            // Drop this thread's sender so the drain below ends once every worker
            // has finished and dropped its own.
            drop(tx);
            // Drain progress on this thread, which owns the non-shareable reporter.
            while let Ok(event) = rx.recv() {
                event.report_to(reporter);
            }
        });

        let state = state.into_inner().expect("no worker panicked holding it");
        state.outcomes
    }

    /// A single worker: claim a ready component, build and publish it, record the
    /// outcome, and release its dependents, until the scheduler says stop.
    ///
    /// A cancelled run stops the scheduler, so every worker sees `Stop` on its
    /// next claim and the run winds down as each in-flight build is stopped.
    #[allow(clippy::too_many_arguments)]
    fn worker(
        &self,
        items: &[WorkItem],
        provider: &dyn BuildRootProvider,
        builder: &Builder,
        pool: &LocalPool,
        state: &Mutex<ParallelState>,
        ready: &Condvar,
        options: &RunOptions,
        tx: mpsc::Sender<OwnedProgress>,
    ) {
        let total = items.len();
        // A worker cannot hold the non-shareable reporter, so it forwards every
        // progress event to the draining thread over the channel.
        let mut report = |event: Progress| {
            let _ = tx.send(OwnedProgress::from(&event));
        };
        loop {
            let position = {
                let mut guard = state.lock().expect("scheduler mutex not poisoned");
                loop {
                    if options.cancel.requested() {
                        guard.scheduler.cancel();
                    }
                    match guard.scheduler.claim() {
                        Claim::Build(position) => break position,
                        Claim::Stop => return,
                        Claim::Wait => {
                            guard = ready.wait(guard).expect("scheduler mutex not poisoned");
                        }
                    }
                }
            };

            let item = &items[position];
            report(Progress::Building {
                component: item.name,
                index: position + 1,
                total,
            });
            // A cancelled component is neither built nor failed: the run
            // stopped it, so it records no outcome and is accounted for as
            // cancelled once the run winds down.
            let outcome = match self.build_and_publish(item, provider, builder, pool, &mut report) {
                Ok(outcome) => Some(Ok(outcome)),
                Err(Error::Cancelled) => None,
                Err(error) => {
                    report(Progress::Failed {
                        component: item.name,
                        error: &error.to_string(),
                    });
                    Some(Err(item.failed(error)))
                }
            };

            let mut guard = state.lock().expect("scheduler mutex not poisoned");
            let success = !matches!(outcome, Some(Err(_)));
            match outcome {
                Some(Ok(outcome)) => guard.outcomes.keep(position, item, outcome),
                Some(Err(failed)) => guard.outcomes.failed.push(failed),
                None => {}
            }
            guard
                .scheduler
                .complete(position, success, !options.keep_going);
            if options.cancel.requested() {
                guard.scheduler.cancel();
            }
            // Releasing a producer may make several dependents ready, and a stop
            // must reach every waiter, so wake them all.
            ready.notify_all();
        }
    }

    /// Provisions, vendors, builds, commits, and publishes one component,
    /// returning its artifacts. Shared by the sequential and parallel paths.
    ///
    /// Nothing here is serialized against a concurrent worker. Provisioning
    /// downloads into the shared, content-addressed package cache, which is safe
    /// for concurrent writers of one entry; it resolves against the pool, which
    /// is safe to read while another worker publishes into it; and publishing
    /// excludes other publishes inside ferroday-cage. So every phase of a
    /// component's build — provision, vendor, build, publish — runs fully
    /// parallel with every other component's.
    fn build_and_publish(
        &self,
        item: &WorkItem,
        provider: &dyn BuildRootProvider,
        builder: &Builder,
        pool: &LocalPool,
        reporter: &mut dyn FnMut(Progress),
    ) -> Result<BuildOutcome> {
        // Rendered before anything is provisioned, so a component that cannot be
        // version-stamped fails without first paying for a build root.
        let changelog_entry = item.changelog_entry()?;
        let pool_repo = pool.as_repository()?;
        let root = provider.build_root(item.component, &item.build_deps, pool_repo, reporter)?;
        let outcome = self.assemble(root.as_ref(), item, &changelog_entry, builder, reporter)?;
        let debs = pool.publish(&outcome.artifacts)?;
        reporter(Progress::Published {
            component: item.name,
            debs,
        });
        Ok(outcome)
    }

    /// Vendors, builds, and commits a component in its provisioned `root`,
    /// stamping `changelog_entry` into the build's copy of the tree, and returns
    /// what the build produced. The pool-free half of a component's build,
    /// shared by the sequential and parallel paths.
    fn assemble(
        &self,
        root: &dyn BuildRoot,
        item: &WorkItem,
        changelog_entry: &str,
        builder: &Builder,
        reporter: &mut dyn FnMut(Progress),
    ) -> Result<BuildOutcome> {
        let name = item.name;
        let target = item.target(changelog_entry);
        // A source that already carries what its build needs skips the one pass
        // that reaches the host network, so such a component is built entirely
        // inside an isolated cage. See [`VendorPass`].
        if item.vendor == VendorPass::Run {
            reporter(Progress::Vendoring { component: name });
            builder.vendor(root, target, reporter)?;
        }

        let out_dir = item.out_root.join(name);
        let outcome = builder.build(root, target, &out_dir, reporter)?;
        // The build succeeded: mark the root reusable. For a root built in place
        // this re-records the plan marker cleared for the build; for an
        // overlay-backed root it is a no-op.
        root.commit()?;
        reporter(Progress::Built {
            component: name,
            artifacts: outcome.artifacts.len(),
        });
        Ok(outcome)
    }
}

/// One component's build inputs, held in build-order position between planning
/// and building. Borrows from the recipe and the resolved sources, which outlive
/// the build.
struct WorkItem<'a> {
    name: &'a str,
    component: &'a Component,
    tree: &'a Path,
    source: &'a Fingerprint,
    /// The upstream version the recipe declared, when it declares one. Carried
    /// only to be recorded: the tree's own `debian/changelog` already holds it,
    /// which is what the build reads.
    version: Option<&'a str>,
    /// How this component's version is stamped, resolved from the component and
    /// the recipe. Reaches the entry the build stamps the tree with, and the
    /// manifest, so a run that changes it publishes rather than skips.
    version_stamp: VersionStamp,
    /// Whether this component's build runs the vendor pass. See [`VendorPass`].
    vendor: VendorPass,
    /// Which of the component's binary packages this run builds. Run-level, not
    /// per component: it follows from which architecture owns the recipe's
    /// arch-indep output, and a component with nothing left to build under it is
    /// skipped rather than given a different setting.
    binaries: Binaries,
    build_deps: Vec<String>,
    /// The run's build stamp and the suite it targets, from which this
    /// component's `debian/changelog` entry is rendered when its build starts.
    ///
    /// Rendering is left to build time rather than done here so that a component
    /// whose changelog is missing or malformed fails as a component — recorded
    /// in the report, carried past by `--keep-going` — instead of ending the run
    /// before any component is attempted.
    stamp: &'a crate::version::BuildStamp,
    suite: &'a str,
    /// The run's output tree, under which this component's artifacts are
    /// collected. Carried per item so both the sequential and parallel paths
    /// reach it without threading the recipe's suite and architecture through
    /// every build signature.
    out_root: &'a Path,
}

impl WorkItem<'_> {
    /// The `debian/changelog` entry stamping this component's version, read from
    /// its own changelog and the run's stamp.
    ///
    /// Fails with [`Error::Changelog`] when the changelog is unreadable or does
    /// not open with a well-formed entry, which is a failure of this component
    /// and not of the run: the error travels the same path a build failure does.
    fn changelog_entry(&self) -> Result<String> {
        crate::version::stamped_entry(
            self.name,
            self.tree,
            self.stamp,
            self.suite,
            self.source,
            self.version_stamp,
        )
    }

    /// What the build passes need to know about this component, given the
    /// `changelog_entry` its build stamps the tree with.
    fn target<'a>(&'a self, changelog_entry: &'a str) -> Target<'a> {
        Target {
            component: self.name,
            tree: self.tree,
            commit: self.source.git_commit(),
            changelog_entry: Some(changelog_entry),
            binaries: self.binaries,
        }
    }

    /// The [`Built`] record for this component from what its build produced.
    fn built(&self, artifacts: Vec<Artifact>, buildinfo: Option<BuildInfo>) -> Built {
        Built {
            component: self.name.to_string(),
            source: self.source.clone(),
            version: self.version.map(str::to_string),
            version_stamp: self.version_stamp,
            packages: packages_of(&artifacts),
            artifacts,
            buildinfo,
        }
    }

    /// The [`Failed`] record for this component from the error that ended it.
    fn failed(&self, error: Error) -> Failed {
        Failed {
            component: self.name.to_string(),
            source: self.source.clone(),
            error,
        }
    }
}

/// The shared, mutex-guarded state of a parallel build: the readiness scheduler
/// and the outcomes collected so far.
struct ParallelState {
    scheduler: Scheduler,
    outcomes: Outcomes,
}

/// What the build phase produced, however it was driven: each component's
/// outcome, and the sandbox inputs the builds ran under.
#[derive(Default)]
struct Outcomes {
    built: Vec<Built>,
    failed: Vec<Failed>,
    /// The sandbox record, with the build-order position it came from.
    sandbox: Option<(usize, SandboxRecord)>,
}

impl Outcomes {
    /// Records a component's successful build at build-order `position`.
    fn keep(&mut self, position: usize, item: &WorkItem, outcome: BuildOutcome) {
        self.keep_sandbox(position, SandboxRecord::of(item.name, &outcome.inputs));
        self.built
            .push(item.built(outcome.artifacts, outcome.buildinfo));
    }

    /// Keeps `record` when `position` is earlier in build order than whatever
    /// is already held.
    ///
    /// Outcomes arrive in completion order under `--jobs N`, so choosing by
    /// build-order position rather than by arrival is what makes a parallel run
    /// record the same component a sequential run would.
    fn keep_sandbox(&mut self, position: usize, record: SandboxRecord) {
        if self
            .sandbox
            .as_ref()
            .is_none_or(|(seen, _)| position < *seen)
        {
            self.sandbox = Some((position, record));
        }
    }
}

/// The stamp a run's versions carry: `tag`, dated as `date` asks.
///
/// [`BuildDate::Recorded`] takes the date from `priors`, the manifests for this
/// recipe and suite at each architecture the run targets. One stamp dates the
/// whole run, so the manifests have to agree: a manifest that records none, and
/// two that record different dates, are both [`Error::BuildDate`] rather than a
/// silent fall back to today or to one of them, either of which would produce a
/// build that looks like a reproduction and is not.
fn build_stamp(tag: &str, date: BuildDate, priors: &[Prior]) -> Result<crate::version::BuildStamp> {
    use crate::version::BuildStamp;

    let seconds = match date {
        BuildDate::Now => return Ok(BuildStamp::now(tag)),
        BuildDate::At(seconds) => seconds,
        BuildDate::Recorded => recorded_date(priors)?,
    };
    Ok(BuildStamp::at(tag, seconds))
}

/// The date every architecture's prior manifest records, as seconds since the
/// Unix epoch.
///
/// A manifest is a file on disk that anything may have edited, so a date that
/// does not parse is refused rather than rounded off to today.
fn recorded_date(priors: &[Prior]) -> Result<i64> {
    let mut agreed: Option<(&str, &str)> = None;
    for prior in priors {
        let recorded = prior
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.build_date.as_deref());
        let Some(recorded) = recorded else {
            return Err(Error::BuildDate(format!(
                "the build date was to be taken from the prior manifest, and the one for \
                 {} records none; the work directory holds no build of this recipe for \
                 that suite and architecture",
                prior.architecture
            )));
        };
        match agreed {
            // Two architectures built on different days have no one date to
            // reproduce them both, and stamping either onto the other would
            // reproduce neither. Refused, naming both, since the way out is to
            // reproduce them one architecture at a time.
            Some((architecture, date)) if date != recorded => {
                return Err(Error::BuildDate(format!(
                    "the build date was to be taken from the prior manifests, and they \
                     disagree: {architecture} records {date}, {} records {recorded}; \
                     reproduce one architecture at a time, or name the date outright",
                    prior.architecture
                )));
            }
            Some(_) => {}
            None => agreed = Some((prior.architecture, recorded)),
        }
    }
    // A validated recipe names at least one architecture, so there is always a
    // manifest to have agreed on by here.
    let (_, date) = agreed.ok_or_else(|| {
        Error::BuildDate("the run targets no architecture to take a date from".to_string())
    })?;
    crate::version::epoch_at_date(date).ok_or_else(|| {
        Error::BuildDate(format!(
            "the prior manifest records build-date {date:?}, which is not a YYYY-MM-DD date"
        ))
    })
}

/// Reports [`Progress::ArchIndepUnowned`] when `recipe` builds for several
/// architectures and names no owner, so each of them will produce its own copy
/// of every `Architecture: all` package.
///
/// By [`Engine::plan`] as well as [`Engine::run`]: it is a cost to know before
/// committing a machine to a build, which is what planning is for.
fn report_arch_indep_unowned(recipe: &Recipe, reporter: &mut dyn FnMut(Progress)) {
    if recipe.architectures.len() > 1 && recipe.arch_indep_owner.is_none() {
        reporter(Progress::ArchIndepUnowned {
            architectures: &recipe.architectures,
        });
    }
}

/// Reports what building `recipe` for `architecture` means, before anything is
/// provisioned for it.
///
/// [`Progress::ForeignArchitecture`] when the target does not run natively on
/// the host, so a cross-architecture build is announced rather than surfacing
/// only as a binfmt error deep in the bootstrap; ferroday-cage still enforces
/// the actual binfmt requirement. [`Progress::ArchIndepElsewhere`] when the
/// recipe hands its `Architecture: all` packages to a different architecture, so
/// a build that will produce fewer packages than its recipe declares says so
/// first.
fn report_architecture(recipe: &Recipe, architecture: &str, reporter: &mut dyn FnMut(Progress)) {
    let host = crate::arch::host_architecture();
    if crate::arch::is_foreign(&host, architecture) {
        reporter(Progress::ForeignArchitecture {
            target: architecture,
            host: &host,
        });
    }
    if !recipe.owns_arch_indep(architecture) {
        reporter(Progress::ArchIndepElsewhere {
            owner: recipe.resolved_arch_indep_owner(architecture),
        });
    }
}

/// Sorts a run's outcomes into build order, given each component's position in
/// it and how to read a component's name off an outcome.
///
/// A sequential run collects outcomes in build order already; a parallel one
/// collects them as its workers finish, which varies with the job count and with
/// how long each build happens to take. Sorting here is what makes the two
/// report identically.
///
/// A name the order does not carry sorts last rather than panicking. Every
/// outcome's component comes from the build order, so this does not arise; it
/// keeps a report from being the thing that fails a run that otherwise finished.
fn in_build_order<T>(
    outcomes: &mut [T],
    position: &BTreeMap<&str, usize>,
    component: impl Fn(&T) -> &str,
) {
    outcomes.sort_by_key(|outcome| {
        position
            .get(component(outcome))
            .copied()
            .unwrap_or(usize::MAX)
    });
}

/// The components a cancelled run did not finish: every component it selected
/// that is neither built nor failed. `selected` is in build order, so the
/// result is too.
///
/// That covers both the components the run never reached and the one it stopped
/// partway through — from the outside they are the same thing, a selected
/// component with no outcome, and both are recorded with what their source
/// resolved to so the manifest keeps naming the exact input.
fn unfinished(
    selected: &[(&str, &Fingerprint)],
    built: &[Built],
    failed: &[Failed],
) -> Vec<Skipped> {
    let reached: BTreeSet<&str> = built
        .iter()
        .map(|built| built.component.as_str())
        .chain(failed.iter().map(|failed| failed.component.as_str()))
        .collect();
    selected
        .iter()
        .filter(|(name, _)| !reached.contains(name))
        .map(|(name, source)| Skipped {
            component: name.to_string(),
            source: (*source).clone(),
            reason: SkipReason::Cancelled,
        })
        .collect()
}

/// Refuses a run that builds a component whose build-dependency is produced by
/// a component the run leaves out, when the pool does not already hold that
/// package.
///
/// Whatever the components being built depend on has to come from somewhere. An
/// archive package comes from the archive; a package another component of the
/// recipe produces comes from the pool, and is there only because an earlier run
/// put it there. Left unchecked, the mismatch surfaces when the consumer's build
/// root is provisioned — after a shared base of several hundred packages, and
/// phrased as a resolver failure rather than as the arrangement error it is.
///
/// Everything the check needs is settled before any of that: the graph knows
/// which component produces the package, `skipped` knows the run left it out and
/// why, and the pool's index is a file on disk.
///
/// Two reasons for a producer's absence are grounds for refusal, and each names
/// its own remedy: the selection left it out, or the recipe leaves its
/// `Architecture: all` output to another architecture. A producer skipped as
/// already built is not, because its packages are in the pool — that is what
/// made it skippable — and one that failed to resolve produces nothing the graph
/// knows about, so it never reaches here. That keeps a `--keep-going` run's
/// tolerance intact: a component the run has already given up on cannot then end
/// it.
fn refuse_unbuildable_run(
    items: &[WorkItem],
    graph: &BuildGraph,
    skipped: &[Skipped],
    selection: &Selection,
    arch_indep_owner: &str,
    pool: &LocalPool,
) -> Result<()> {
    let absent: BTreeMap<&str, SkipReason> = skipped
        .iter()
        .map(|skipped| (skipped.component.as_str(), skipped.reason))
        .collect();
    // (consumer, build-dependency, producer, why the producer is absent) for
    // every in-set dependency this run leaves nothing to produce.
    let unproduced: Vec<(&str, &str, &str, SkipReason)> = items
        .iter()
        .flat_map(|item| {
            item.build_deps.iter().filter_map(|dep| {
                let producer = graph.producer(dep)?;
                if producer == item.name {
                    return None;
                }
                let reason = absent.get(producer).copied()?;
                matches!(
                    reason,
                    SkipReason::NotSelected | SkipReason::ArchIndepElsewhere
                )
                .then_some((item.name, dep.as_str(), producer, reason))
            })
        })
        .collect();
    if unproduced.is_empty() {
        return Ok(());
    }

    // Read the index only once something might be missing from it, so an
    // ordinary run never touches it.
    let indexed = pool.indexed_packages()?;
    let Some((consumer, dep, producer, reason)) = unproduced
        .into_iter()
        .find(|(_, dep, _, _)| !indexed.contains(*dep))
    else {
        return Ok(());
    };
    // Why the producer is absent, and what settles it — the two halves that
    // differ by reason, in a sentence that is otherwise the same either way.
    //
    // Building for the owner first does not settle the arch-indep case, so the
    // remedy does not offer it: a pool is per architecture, so the owner's copy
    // lands in the owner's pool and this build never resolves against it. What
    // does settle it is dropping the owner, which is why the message says so
    // rather than leaving it to be worked out.
    let (absence, remedy) = match reason {
        SkipReason::ArchIndepElsewhere => (
            format!(
                "produces only Architecture: all packages, left to \
                 {arch_indep_owner:?} by this recipe"
            ),
            format!(
                "Stop naming an arch-indep owner, so this architecture builds \
                 {producer:?} itself; the owner's copy is published to the owner's \
                 pool, which this build does not resolve against"
            ),
        ),
        _ => (
            format!("{flag} leaves out", flag = selection.flag()),
            format!("Select {producer:?} as well, or build it first"),
        ),
    };
    Err(Error::BuildDependency(format!(
        "this run builds {consumer:?}, which build-depends on {dep:?}; that package is \
         produced by component {producer:?}, which {absence}, and the pool does not hold \
         it. {remedy}"
    )))
}

/// Why `name` (resolved to `source` at `version`) should be skipped this run, or
/// `None` to build it.
///
/// A component outside the selection is skipped. So is one with `nothing_to
/// _build` — a component whose every binary package is `Architecture: all`
/// where the recipe hands arch-indep output elsewhere — which outranks
/// `--skip-published`, because it is a property of the run's target rather than
/// of what a prior run happened to do. Otherwise a selected component is skipped
/// only under `--skip-published`, and only when a prior run recorded it as built
/// from the same source at the same declared version — see
/// [`ComponentRecord::is_built_at`](manifest::ComponentRecord::is_built_at).
fn skip_reason(
    name: &str,
    identity: &BuildIdentity,
    options: &RunOptions,
    selected: &BTreeSet<&str>,
    prior: &BTreeMap<String, manifest::ComponentRecord>,
    has_anything_to_build: bool,
) -> Option<SkipReason> {
    if !selected.contains(name) {
        return Some(SkipReason::NotSelected);
    }
    if !has_anything_to_build {
        return Some(SkipReason::ArchIndepElsewhere);
    }
    if options.skip_published
        && prior
            .get(name)
            .is_some_and(|record| record.is_built_at(identity))
    {
        return Some(SkipReason::AlreadyBuilt);
    }
    None
}

/// Assembles one architecture's manifest: each component in build order,
/// recorded from what that architecture did with it, or carried forward from
/// `prior` when it did not touch it — so a built component stays recorded as
/// built across selective runs and the manifest describes the whole recipe, not
/// just what this run built.
///
/// A component whose source never resolved is recorded as failed here, though it
/// failed for the run rather than for this architecture: what a manifest
/// describes is the state of this target, and from that side a component nothing
/// could resolve is one it does not have.
///
/// `work_dir` anchors the paths the manifest names, which are recorded relative
/// to it.
fn manifest_for_architecture(
    recipe: &Recipe,
    order: &[String],
    unresolved: &[Failed],
    report: &ArchitectureReport,
    prior: &BTreeMap<String, manifest::ComponentRecord>,
    work_dir: &Path,
) -> Manifest {
    let built: BTreeMap<&str, &Built> = report
        .built
        .iter()
        .map(|built| (built.component.as_str(), built))
        .collect();
    let failed: BTreeMap<&str, &Failed> = report
        .failed
        .iter()
        .chain(unresolved)
        .map(|failed| (failed.component.as_str(), failed))
        .collect();
    let skipped: BTreeMap<&str, &Skipped> = report
        .skipped
        .iter()
        .map(|skipped| (skipped.component.as_str(), skipped))
        .collect();

    let records = order
        .iter()
        .map(|name| {
            let key = name.as_str();
            if let Some(built) = built.get(key) {
                manifest::ComponentRecord {
                    name: name.clone(),
                    status: manifest::STATUS_BUILT.to_string(),
                    error: None,
                    version: built.version.clone(),
                    version_stamp: built.version_stamp,
                    buildinfo: built
                        .buildinfo
                        .as_ref()
                        .map(|buildinfo| manifest::BuildInfoRecord::of(buildinfo, work_dir)),
                    source: built.source.clone(),
                    packages: built
                        .packages
                        .iter()
                        .map(|package| manifest::PackageRecord {
                            name: package.name.clone(),
                            version: package.version.clone(),
                        })
                        .collect(),
                }
            } else if let Some(failed) = failed.get(key) {
                manifest::ComponentRecord {
                    name: name.clone(),
                    status: manifest::STATUS_FAILED.to_string(),
                    error: Some(failed.error.to_string()),
                    // A failed component records no version: only a built
                    // record is ever compared against one, and a component that
                    // failed may not have reached the point of declaring it.
                    version: None,
                    // ...nor a version stamp, for the same reason.
                    version_stamp: VersionStamp::default(),
                    // A failed build produced no packages to record one for.
                    buildinfo: None,
                    source: failed.source.clone(),
                    packages: Vec::new(),
                }
            } else if let Some(prior) = prior.get(name) {
                // Skipped, but recorded before: keep the prior record so a
                // built-then-skipped component stays built across runs.
                prior.clone()
            } else {
                // Skipped and never recorded (for example `--only` on a fresh pool).
                manifest::ComponentRecord {
                    name: name.clone(),
                    status: manifest::STATUS_SKIPPED.to_string(),
                    error: None,
                    version: None,
                    version_stamp: VersionStamp::default(),
                    buildinfo: None,
                    source: skipped
                        .get(key)
                        .map(|skipped| skipped.source.clone())
                        .unwrap_or_default(),
                    packages: Vec::new(),
                }
            }
        })
        .collect();

    Manifest::new(
        recipe.name.clone(),
        recipe.suite.clone(),
        report.architecture.clone(),
        records,
    )
}

/// The distinct `(package, version)` pairs among a component's artifacts, in
/// first-seen order.
///
/// A single build produces several files for one package — the `.deb` and its
/// `.ddeb` debug companion share a name and version — so the manifest records
/// each package once rather than once per file.
fn packages_of(artifacts: &[Artifact]) -> Vec<Package> {
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    let mut packages = Vec::new();
    for artifact in artifacts {
        if seen.insert(artifact.package.as_str(), ()).is_none() {
            packages.push(Package {
                name: artifact.package.clone(),
                version: artifact.version.clone(),
            });
        }
    }
    packages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ComponentRecord, PackageRecord, STATUS_BUILT, STATUS_FAILED};

    fn order(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// Renders every field of an event, so a round trip can be compared without
    /// [`Progress`] having to carry `PartialEq` for the sake of a test.
    ///
    /// The match is exhaustive on purpose. [`Progress`] and [`OwnedProgress`]
    /// are hand-mirrored, and this is where that mirror rots: a variant added
    /// to one and not the other stops compiling here, rather than silently
    /// dropping events on the parallel path.
    fn describe(event: &Progress) -> String {
        match event {
            Progress::Started => "Started".to_string(),
            Progress::BuildDate { date } => format!("BuildDate {date}"),
            Progress::Resolving { component } => format!("Resolving {component}"),
            Progress::Unresolved { component, error } => {
                format!("Unresolved {component} {error}")
            }
            Progress::Ordered { order } => format!("Ordered {}", order.join(",")),
            Progress::ArchIndepUnowned { architectures } => {
                format!("ArchIndepUnowned {}", architectures.join(","))
            }
            Progress::Architecture {
                architecture,
                index,
                total,
            } => format!("Architecture {architecture} {index}/{total}"),
            Progress::ForeignArchitecture { target, host } => {
                format!("ForeignArchitecture {target} {host}")
            }
            Progress::ArchIndepElsewhere { owner } => format!("ArchIndepElsewhere {owner}"),
            Progress::Layered => "Layered".to_string(),
            Progress::OverlayUnavailable { reason } => format!("OverlayUnavailable {reason}"),
            Progress::Provisioning { component } => {
                format!("Provisioning {}", root(*component))
            }
            Progress::Fetching { component, url } => {
                format!("Fetching {} {url}", root(*component))
            }
            Progress::PackagesResolved {
                component,
                packages,
            } => format!("PackagesResolved {} {packages}", root(*component)),
            Progress::Downloading {
                component,
                package,
                index,
                total,
            } => format!("Downloading {} {package} {index}/{total}", root(*component)),
            Progress::InstallingToolchain { component, version } => {
                format!("InstallingToolchain {} {version}", root(*component))
            }
            Progress::Extracting {
                component,
                package,
                index,
                total,
            } => format!("Extracting {} {package} {index}/{total}", root(*component)),
            Progress::Building {
                component,
                index,
                total,
            } => format!("Building {component} {index}/{total}"),
            Progress::Vendoring { component } => format!("Vendoring {component}"),
            Progress::Output {
                component,
                stream,
                line,
            } => format!("Output {component} {stream:?} {line}"),
            Progress::Built {
                component,
                artifacts,
            } => format!("Built {component} {artifacts}"),
            Progress::Published { component, debs } => format!("Published {component} {debs}"),
            Progress::Failed { component, error } => format!("Failed {component} {error}"),
            Progress::Skipped { component, reason } => format!("Skipped {component} {reason}"),
            Progress::Cancelled => "Cancelled".to_string(),
            Progress::Manifest { path } => format!("Manifest {}", path.display()),
        }
    }

    /// The label for an event's optional root, so the shared base and a
    /// component named `base` are still told apart.
    fn root(component: Option<&str>) -> String {
        match component {
            None => "<base>".to_string(),
            Some(component) => component.to_string(),
        }
    }

    /// Asserts an event survives being copied into an [`OwnedProgress`] and
    /// rebuilt — the path every event takes on a parallel build.
    fn assert_round_trips(event: Progress) {
        let expected = describe(&event);
        let mut seen = String::new();
        OwnedProgress::from(&event).report_to(&mut |rebuilt| seen = describe(&rebuilt));
        assert_eq!(seen, expected);
    }

    #[test]
    fn every_progress_event_survives_the_owned_round_trip() {
        let order = order(&["a", "b"]);
        assert_round_trips(Progress::Started);
        assert_round_trips(Progress::BuildDate { date: "2026-07-31" });
        assert_round_trips(Progress::Resolving { component: "a" });
        assert_round_trips(Progress::Unresolved {
            component: "a",
            error: "no such repository",
        });
        assert_round_trips(Progress::Ordered { order: &order });
        assert_round_trips(Progress::ArchIndepUnowned {
            architectures: &order,
        });
        assert_round_trips(Progress::Architecture {
            architecture: "arm64",
            index: 2,
            total: 2,
        });
        assert_round_trips(Progress::ForeignArchitecture {
            target: "arm64",
            host: "amd64",
        });
        assert_round_trips(Progress::ArchIndepElsewhere { owner: "amd64" });
        assert_round_trips(Progress::Layered);
        assert_round_trips(Progress::OverlayUnavailable { reason: "no idmap" });
        // Both roots a provisioning event can name: the shared base, and a
        // component's own. The `Option` is what makes a parallel run's
        // interleaved output attributable, so it round-trips either way.
        assert_round_trips(Progress::Provisioning { component: None });
        assert_round_trips(Progress::Provisioning {
            component: Some("a"),
        });
        assert_round_trips(Progress::Fetching {
            component: None,
            url: "http://deb.debian.org/debian/dists/trixie/Release",
        });
        assert_round_trips(Progress::PackagesResolved {
            component: Some("a"),
            packages: 262,
        });
        assert_round_trips(Progress::Downloading {
            component: None,
            package: "libc6",
            index: 45,
            total: 262,
        });
        assert_round_trips(Progress::InstallingToolchain {
            component: None,
            version: "1.97.0",
        });
        assert_round_trips(Progress::Extracting {
            component: Some("a"),
            package: "libc6",
            index: 130,
            total: 262,
        });
        assert_round_trips(Progress::Building {
            component: "a",
            index: 1,
            total: 2,
        });
        assert_round_trips(Progress::Vendoring { component: "a" });
        assert_round_trips(Progress::Output {
            component: "a",
            stream: Stream::Stderr,
            line: "warning: unused",
        });
        assert_round_trips(Progress::Built {
            component: "a",
            artifacts: 3,
        });
        assert_round_trips(Progress::Published {
            component: "a",
            debs: 2,
        });
        assert_round_trips(Progress::Failed {
            component: "a",
            error: "boom",
        });
        assert_round_trips(Progress::Skipped {
            component: "a",
            reason: "already built",
        });
        assert_round_trips(Progress::Cancelled);
        assert_round_trips(Progress::Manifest {
            path: Path::new("/work/manifest.toml"),
        });
    }

    /// A recipe listing `names` as its components, for the checks that read
    /// only which components a recipe has.
    fn recipe(names: &[&str]) -> Recipe {
        let components: String = names
            .iter()
            .map(|name| {
                format!(
                    "[[components]]\nname = \"{name}\"\n\
                     source.git = \"https://example.invalid/{name}\"\n"
                )
            })
            .collect();
        toml::from_str(&format!("name = \"r\"\nsuite = \"trixie\"\n{components}")).unwrap()
    }

    fn built_record(name: &str, commit: &str) -> ComponentRecord {
        ComponentRecord {
            name: name.to_string(),
            status: STATUS_BUILT.to_string(),
            error: None,
            version: None,
            version_stamp: VersionStamp::default(),
            buildinfo: None,
            source: git(commit),
            packages: vec![PackageRecord {
                name: name.to_string(),
                version: "1.0".to_string(),
            }],
        }
    }

    /// A git source at `commit`, the shape the resolver produces.
    /// The identity a run would build a component at, defaulted to what an
    /// ordinary component carries.
    fn identity<'a>(source: &'a Fingerprint, version: Option<&'a str>) -> BuildIdentity<'a> {
        BuildIdentity {
            source,
            version,
            version_stamp: VersionStamp::default(),
        }
    }

    fn git(commit: &str) -> Fingerprint {
        Fingerprint::of(crate::fingerprint::SourceInput::git(
            crate::fingerprint::SourceRole::Source,
            commit,
        ))
    }

    /// `count` stand-in artifacts, for a report whose test only counts them.
    fn artifacts(count: usize) -> Vec<Artifact> {
        (0..count)
            .map(|n| Artifact {
                package: format!("pkg-{n}"),
                version: "1.0".to_string(),
                path: PathBuf::from(format!("/out/pkg-{n}_1.0_amd64.deb")),
            })
            .collect()
    }

    #[test]
    fn selection_all_takes_every_component() {
        let names = order(&["a", "b", "c"]);
        let selected = Selection::All.resolve(&names).unwrap();
        assert_eq!(selected, ["a", "b", "c"].into_iter().collect());
    }

    #[test]
    fn selection_only_takes_the_named() {
        let names = order(&["a", "b", "c"]);
        let selected = Selection::Only(vec!["c".into(), "a".into()])
            .resolve(&names)
            .unwrap();
        assert_eq!(selected, ["a", "c"].into_iter().collect());
    }

    #[test]
    fn selection_from_takes_the_component_and_the_order_tail() {
        let names = order(&["a", "b", "c", "d"]);
        let selected = Selection::From("c".into()).resolve(&names).unwrap();
        assert_eq!(selected, ["c", "d"].into_iter().collect());
    }

    #[test]
    fn an_unknown_component_is_rejected_against_the_recipe_not_the_order() {
        // The recipe is what a typo is caught against, because it is known
        // before a single source is cloned. Both flags are checked, and a name
        // the recipe does have passes whatever the order turns out to be.
        let recipe = recipe(&["a", "b"]);
        for selection in [
            Selection::Only(vec!["a".into(), "z".into()]),
            Selection::From("z".into()),
        ] {
            let err = selection.validate(&recipe).unwrap_err();
            assert!(matches!(err, Error::Selection(_)));
            assert!(format!("{err}").contains("\"z\""), "{err}");
        }
        assert!(Selection::Only(vec!["b".into()]).validate(&recipe).is_ok());
        assert!(Selection::From("a".into()).validate(&recipe).is_ok());
        assert!(Selection::All.validate(&recipe).is_ok());
    }

    #[test]
    fn a_component_the_order_lacks_did_not_resolve_and_is_not_selected() {
        // Names are validated against the recipe before anything resolves, so a
        // name missing from the order here is a component whose source failed —
        // already recorded as a failure. `--only` drops it and builds the rest.
        let names = order(&["a", "c"]);
        let selected = Selection::Only(vec!["a".into(), "b".into()])
            .resolve(&names)
            .unwrap();
        assert_eq!(selected, ["a"].into_iter().collect());

        // `--from` cannot: its selection is everything after an anchor that now
        // has no position, so there is no tail to take.
        let err = Selection::From("b".into()).resolve(&names).unwrap_err();
        assert!(matches!(err, Error::Selection(_)));
        assert!(format!("{err}").contains("did not resolve"), "{err}");
    }

    #[test]
    fn only_answers_membership_without_an_order_and_from_assumes_the_worst() {
        // Whether a component's resolve failure may be carried past is decided
        // before the order exists. `--only` names components outright, so it
        // answers exactly; `--from` names a position, so it cannot, and it
        // answers the way that does not excuse a failure that matters.
        let only = Selection::Only(vec!["a".into()]);
        assert!(only.includes_unordered("a"));
        assert!(!only.includes_unordered("b"));
        assert!(Selection::All.includes_unordered("b"));
        assert!(Selection::From("a".into()).includes_unordered("b"));
    }

    #[test]
    fn a_selection_leaving_out_a_producer_is_refused_before_anything_is_provisioned() {
        // pkg-b build-depends on liba-dev, which pkg-a produces.
        let recipe = recipe(&["pkg-a", "pkg-b"]);
        let graph = BuildGraph::resolve(&[
            (
                "pkg-a".to_string(),
                "Source: pkg-a\n\nPackage: liba-dev\n".to_string(),
            ),
            (
                "pkg-b".to_string(),
                "Source: pkg-b\nBuild-Depends: liba-dev\n\nPackage: pkg-b\n".to_string(),
            ),
        ])
        .unwrap();
        let stamp = crate::version::BuildStamp::at("deb13", 0);
        let out_root = PathBuf::from("/out");
        let source = git("abc");
        let items = [WorkItem {
            name: "pkg-b",
            component: &recipe.components[1],
            tree: Path::new("/sources/pkg-b"),
            source: &source,
            version: None,
            version_stamp: VersionStamp::default(),
            vendor: VendorPass::Run,
            binaries: Binaries::All,
            build_deps: vec!["debhelper".to_string(), "liba-dev".to_string()],
            stamp: &stamp,
            suite: "trixie",
            out_root: &out_root,
        }];
        let selection = Selection::Only(vec!["pkg-b".to_string()]);
        // pkg-a is the producer the run leaves out, and why decides the remedy
        // the message offers.
        let left_out = |reason| {
            vec![Skipped {
                component: "pkg-a".to_string(),
                source: git("abc"),
                reason,
            }]
        };

        // An empty pool: nothing supplies liba-dev, so the run is refused —
        // and the message names the component to add rather than the package
        // the provisioner would later fail to resolve.
        let empty = LocalPool::new(scratch("empty-pool"), "trixie", "main", "amd64");
        let err = refuse_unbuildable_run(
            &items,
            &graph,
            &left_out(SkipReason::NotSelected),
            &selection,
            "amd64",
            &empty,
        )
        .unwrap_err();
        let message = format!("{err}");
        assert!(matches!(err, Error::BuildDependency(_)));
        for expected in ["--only", "\"pkg-b\"", "\"liba-dev\"", "\"pkg-a\""] {
            assert!(message.contains(expected), "{message}");
        }

        // The same gap reached another way: the producer declares only
        // `Architecture: all` packages and this architecture does not own them.
        // The remedy is a different one, so the message names that instead of
        // pointing at a selection flag nobody passed.
        let err = refuse_unbuildable_run(
            &items,
            &graph,
            &left_out(SkipReason::ArchIndepElsewhere),
            &Selection::All,
            "amd64",
            &empty,
        )
        .unwrap_err();
        let message = format!("{err}");
        assert!(message.contains("Architecture: all"), "{message}");
        assert!(message.contains("\"amd64\""), "{message}");
        assert!(!message.contains("--only"), "{message}");

        // Building the producer settles it: nothing is skipped, so nothing is
        // missing.
        assert!(refuse_unbuildable_run(&items, &graph, &[], &selection, "amd64", &empty).is_ok());

        // So does a producer skipped as already built, whose packages are in the
        // pool — that is what made it skippable, and a `--keep-going` run must
        // not be ended by a component it has already accounted for.
        assert!(
            refuse_unbuildable_run(
                &items,
                &graph,
                &left_out(SkipReason::AlreadyBuilt),
                &selection,
                "amd64",
                &empty,
            )
            .is_ok()
        );

        // So does a pool that already holds the package, which is the state a
        // prior run leaves behind and what makes a narrow re-run work at all.
        let dir = scratch("warm-pool");
        let index = dir.join("dists/trixie/main/binary-amd64");
        std::fs::create_dir_all(&index).unwrap();
        std::fs::write(index.join("Packages"), "Package: liba-dev\nVersion: 1.0\n").unwrap();
        let warm = LocalPool::new(&dir, "trixie", "main", "amd64");
        assert!(
            refuse_unbuildable_run(
                &items,
                &graph,
                &left_out(SkipReason::NotSelected),
                &selection,
                "amd64",
                &warm,
            )
            .is_ok()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A unique scratch directory path for a pool. Not created: a pool that
    /// does not exist is exactly the empty one these checks have to handle.
    fn scratch(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("src2deb-engine-{label}-{}-{n}", std::process::id()))
    }

    #[test]
    fn skip_reason_skips_the_unselected_and_the_already_built() {
        let selected: BTreeSet<&str> = ["a"].into_iter().collect();
        let mut prior = BTreeMap::new();
        prior.insert("a".to_string(), built_record("a", "abc"));
        let skip = RunOptions {
            skip_published: true,
            ..Default::default()
        };
        let no_skip = RunOptions::default();

        // Outside the selection: skipped regardless of the flag.
        assert_eq!(
            skip_reason(
                "b",
                &identity(&git("abc"), None),
                &skip,
                &selected,
                &prior,
                true
            ),
            Some(SkipReason::NotSelected)
        );
        // Selected, --skip-published, prior built from the same source: skipped.
        assert_eq!(
            skip_reason(
                "a",
                &identity(&git("abc"), None),
                &skip,
                &selected,
                &prior,
                true
            ),
            Some(SkipReason::AlreadyBuilt)
        );
        // Selected but the source moved: built.
        assert_eq!(
            skip_reason(
                "a",
                &identity(&git("moved"), None),
                &skip,
                &selected,
                &prior,
                true
            ),
            None
        );
        // Selected, no --skip-published: built even though the prior matches.
        assert_eq!(
            skip_reason(
                "a",
                &identity(&git("abc"), None),
                &no_skip,
                &selected,
                &prior,
                true
            ),
            None
        );
    }

    #[test]
    fn a_component_whose_recipe_renamed_its_version_is_built_even_when_nothing_moved() {
        // A declared version does not reach the fingerprint — it is a name the
        // recipe gave, not a tree the build consumed — so the skip decision has
        // to consult it in its own right, or editing `version` would publish
        // nothing.
        let selected: BTreeSet<&str> = ["a"].into_iter().collect();
        let mut prior = BTreeMap::new();
        prior.insert(
            "a".to_string(),
            ComponentRecord {
                version: Some("1.2.3".to_string()),
                ..built_record("a", "abc")
            },
        );
        let skip = RunOptions {
            skip_published: true,
            ..Default::default()
        };
        assert_eq!(
            skip_reason(
                "a",
                &identity(&git("abc"), Some("1.2.3")),
                &skip,
                &selected,
                &prior,
                true
            ),
            Some(SkipReason::AlreadyBuilt),
        );
        assert_eq!(
            skip_reason(
                "a",
                &identity(&git("abc"), Some("1.2.4")),
                &skip,
                &selected,
                &prior,
                true
            ),
            None,
        );
    }

    #[test]
    fn an_unpinned_source_is_built_every_run_even_under_skip_published() {
        // There is nothing to compare: the record names where the tree was read
        // from, not what it held. A prior record that matches exactly is still
        // not evidence the source stood still.
        let selected: BTreeSet<&str> = ["a"].into_iter().collect();
        let working_tree = Fingerprint::of(crate::fingerprint::SourceInput::path(
            crate::fingerprint::SourceRole::Source,
            "/home/someone/a",
        ));
        let mut prior = BTreeMap::new();
        prior.insert(
            "a".to_string(),
            ComponentRecord {
                source: working_tree.clone(),
                ..built_record("a", "unused")
            },
        );
        let skip = RunOptions {
            skip_published: true,
            ..Default::default()
        };
        assert_eq!(
            skip_reason(
                "a",
                &identity(&working_tree, None),
                &skip,
                &selected,
                &prior,
                true
            ),
            None
        );
    }

    #[test]
    fn a_component_with_nothing_but_arch_indep_packages_is_skipped_for_a_non_owner() {
        // Its every binary package belongs to another architecture, so this run
        // has nothing of it to build — and `dpkg-buildpackage -B` on such a
        // source fails outright rather than producing an empty result.
        let selected: BTreeSet<&str> = ["a"].into_iter().collect();
        let mut prior = BTreeMap::new();
        prior.insert("a".to_string(), built_record("a", "abc"));
        let skip = RunOptions {
            skip_published: true,
            ..Default::default()
        };
        assert_eq!(
            skip_reason(
                "a",
                &identity(&git("abc"), None),
                &skip,
                &selected,
                &prior,
                false
            ),
            Some(SkipReason::ArchIndepElsewhere),
        );
        // It outranks `--skip-published`, which would otherwise claim the
        // component was skipped because a prior run had built it — a reason that
        // says nothing about why this run could never build it.
        assert_eq!(
            skip_reason(
                "a",
                &identity(&git("abc"), None),
                &RunOptions::default(),
                &selected,
                &prior,
                false
            ),
            Some(SkipReason::ArchIndepElsewhere),
        );
        // Outside the selection still outranks both: it was never asked for.
        assert_eq!(
            skip_reason(
                "z",
                &identity(&git("abc"), None),
                &skip,
                &selected,
                &prior,
                false
            ),
            Some(SkipReason::NotSelected),
        );
    }

    #[test]
    fn a_cancelled_run_accounts_for_every_component_it_did_not_finish() {
        let sources: Vec<(&str, Fingerprint)> = ["aaa", "bbb", "ccc", "ddd"]
            .iter()
            .zip(["a", "b", "c", "d"])
            .map(|(commit, name)| (name, git(commit)))
            .collect();
        let selected: Vec<(&str, &Fingerprint)> = sources
            .iter()
            .map(|(name, source)| (*name, source))
            .collect();
        let built = vec![Built {
            component: "a".to_string(),
            source: git("aaa"),
            version: None,
            version_stamp: VersionStamp::default(),
            artifacts: artifacts(1),
            buildinfo: None,
            packages: Vec::new(),
        }];
        let failed = vec![Failed {
            component: "b".to_string(),
            source: git("bbb"),
            error: Error::Plan("boom".to_string()),
        }];

        // `c` was the component the cancel stopped partway through and `d` was
        // never reached; neither has an outcome, so both are recorded as
        // cancelled, in build order and keeping the source each resolved to.
        let unfinished = unfinished(&selected, &built, &failed);
        let names: Vec<&str> = unfinished
            .iter()
            .map(|skipped| skipped.component.as_str())
            .collect();
        assert_eq!(names, ["c", "d"]);
        assert_eq!(unfinished[0].source, git("ccc"));
        assert!(
            unfinished
                .iter()
                .all(|skipped| skipped.reason == SkipReason::Cancelled)
        );
    }

    #[test]
    fn outcomes_are_reported_in_build_order_however_they_arrived() {
        // A parallel run collects outcomes as workers finish, so `d` may land
        // before `a`. The report has to read the same as a sequential run's.
        let order = order(&["a", "b", "c", "d"]);
        let position: BTreeMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(index, name)| (name.as_str(), index))
            .collect();

        let mut built: Vec<Built> = ["d", "a", "c"]
            .iter()
            .map(|name| Built {
                component: name.to_string(),
                source: git("abc"),
                version: None,
                version_stamp: VersionStamp::default(),
                artifacts: artifacts(1),
                buildinfo: None,
                packages: Vec::new(),
            })
            .collect();
        in_build_order(&mut built, &position, |built| built.component.as_str());
        let names: Vec<&str> = built.iter().map(|built| built.component.as_str()).collect();
        assert_eq!(names, ["a", "c", "d"]);

        // A component the order does not carry sorts last instead of panicking,
        // so a stray name cannot fail a run that otherwise finished.
        let mut failed: Vec<Failed> = ["stray", "b"]
            .iter()
            .map(|name| Failed {
                component: name.to_string(),
                source: git("abc"),
                error: Error::Plan("boom".to_string()),
            })
            .collect();
        in_build_order(&mut failed, &position, |failed| failed.component.as_str());
        let names: Vec<&str> = failed
            .iter()
            .map(|failed| failed.component.as_str())
            .collect();
        assert_eq!(names, ["b", "stray"]);
    }

    #[test]
    fn the_sandbox_record_comes_from_the_earliest_component_in_build_order() {
        // Under `--jobs N` these arrive in completion order, which is not build
        // order; the manifest must not depend on which worker won the race.
        let mut outcomes = Outcomes::default();
        outcomes.keep_sandbox(3, sandbox_record("third"));
        assert_eq!(held(&outcomes), "third");
        // An earlier component displaces a later one...
        outcomes.keep_sandbox(1, sandbox_record("first"));
        assert_eq!(held(&outcomes), "first");
        // ...and a later one does not displace it back, however late it lands.
        outcomes.keep_sandbox(5, sandbox_record("fifth"));
        assert_eq!(held(&outcomes), "first");
    }

    /// The component the held sandbox record was taken from.
    fn held(outcomes: &Outcomes) -> &str {
        &outcomes
            .sandbox
            .as_ref()
            .expect("a record is held")
            .1
            .component
    }

    /// A record standing in for one taken from a real build pass. Only
    /// `component` is read here: what these tests settle is which component's
    /// record a run keeps, not what a record holds.
    fn sandbox_record(component: &str) -> SandboxRecord {
        SandboxRecord {
            component: component.to_string(),
            root: manifest::RootRecord::Plain {
                path: "/work/roots/c".to_string(),
            },
            identity: manifest::IdentityRecord::Single,
            network: "isolated".to_string(),
            rlimits: Vec::new(),
            hardening: manifest::HardeningRecord::Unavailable,
            env: BTreeMap::new(),
            mounts: Vec::new(),
        }
    }

    #[test]
    fn a_run_that_finished_everything_has_nothing_to_account_for() {
        let built = vec![Built {
            component: "a".to_string(),
            source: git("aaa"),
            version: None,
            version_stamp: VersionStamp::default(),
            artifacts: artifacts(1),
            buildinfo: None,
            packages: Vec::new(),
        }];
        assert!(unfinished(&[("a", &git("aaa"))], &built, &[]).is_empty());
    }

    /// An architecture's report, with everything a manifest does not read left
    /// at its empty value.
    fn architecture_report(
        architecture: &str,
        built: Vec<Built>,
        failed: Vec<Failed>,
        skipped: Vec<Skipped>,
    ) -> ArchitectureReport {
        ArchitectureReport {
            architecture: architecture.to_string(),
            built,
            failed,
            skipped,
            out_dir: PathBuf::from("/out"),
            manifest_path: PathBuf::from("/m"),
        }
    }

    #[test]
    fn the_manifest_carries_prior_records_for_untouched_components() {
        let recipe: Recipe = toml::from_str(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"a\"\nsource.git = \"x\"\n\
             [[components]]\nname = \"b\"\nsource.git = \"y\"\n",
        )
        .unwrap();
        // Prior run built a at commit abc.
        let mut prior = BTreeMap::new();
        prior.insert("a".to_string(), built_record("a", "abc"));
        // This run builds b, skips a (not selected), and fails nothing.
        let report = architecture_report(
            "amd64",
            vec![Built {
                component: "b".to_string(),
                source: git("def"),
                version: None,
                version_stamp: VersionStamp::default(),
                artifacts: artifacts(1),
                buildinfo: None,
                packages: vec![Package {
                    name: "b".to_string(),
                    version: "2.0".to_string(),
                }],
            }],
            Vec::new(),
            vec![Skipped {
                component: "a".to_string(),
                source: git("abc"),
                reason: SkipReason::NotSelected,
            }],
        );

        let manifest = manifest_for_architecture(
            &recipe,
            &order(&["a", "b"]),
            &[],
            &report,
            &prior,
            Path::new("/w"),
        );
        let records = manifest.records_by_name();
        assert_eq!(manifest.architecture, "amd64");
        // a is carried forward from the prior run: still built at abc.
        assert!(records["a"].is_built_at(&identity(&git("abc"), None)));
        // b is recorded fresh from this run.
        assert_eq!(records["b"].status, STATUS_BUILT);
        assert_eq!(records["b"].source, git("def"));
        assert_eq!(records["b"].packages[0].version, "2.0");
    }

    /// The priors a run over `architectures` reads, each recording `date` when
    /// it has one at all.
    fn priors<'a>(architectures: &'a [(&'a str, Option<&str>)]) -> Vec<Prior<'a>> {
        architectures
            .iter()
            .map(|(architecture, date)| Prior {
                architecture,
                path: PathBuf::from(format!("/w/{architecture}.toml")),
                manifest: Some(
                    Manifest::new("r", "trixie", *architecture, Vec::new())
                        .with_build_date(date.map(str::to_string)),
                ),
            })
            .collect()
    }

    #[test]
    fn a_run_dates_its_versions_as_it_was_told_to() {
        let seconds = crate::version::epoch_at_date("2026-07-31").unwrap();
        // An explicit date is used as given, and reads no manifest at all.
        let stamp = build_stamp("deb13", BuildDate::At(seconds), &[]).unwrap();
        assert_eq!(stamp.date(), "20260731");

        // A recorded date is taken from the manifests for this target, in the
        // form they write it. One stamp dates the whole run, so every
        // architecture's manifest is consulted and they have to agree.
        let stamp = build_stamp(
            "deb13",
            BuildDate::Recorded,
            &priors(&[("amd64", Some("2026-07-31")), ("arm64", Some("2026-07-31"))]),
        )
        .unwrap();
        assert_eq!(stamp.date(), "20260731");
        assert_eq!(stamp.calendar_date(), "2026-07-31");
    }

    #[test]
    fn a_recorded_date_that_is_not_there_fails_rather_than_falling_back_to_today() {
        // Falling back would produce a build that looks like a reproduction of a
        // recorded one and is not.
        for prior in [
            // No manifest at all for this target.
            Prior {
                architecture: "amd64",
                path: PathBuf::from("/w/amd64.toml"),
                manifest: None,
            },
            // A manifest whose runs never built anything records no date.
            Prior {
                architecture: "amd64",
                path: PathBuf::from("/w/amd64.toml"),
                manifest: Some(Manifest::new("r", "trixie", "amd64", Vec::new())),
            },
        ] {
            let err = build_stamp("deb13", BuildDate::Recorded, &[prior]).unwrap_err();
            assert!(matches!(err, Error::BuildDate(_)), "{err}");
            assert!(format!("{err}").contains("build date"), "{err}");
        }

        // Nor is a manifest edited to hold something that is not a date rounded
        // off to one.
        let err = build_stamp(
            "deb13",
            BuildDate::Recorded,
            &priors(&[("amd64", Some("yesterday"))]),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("YYYY-MM-DD"), "{err}");
    }

    #[test]
    fn architectures_recorded_on_different_days_have_no_one_date_to_reproduce_them() {
        // A run stamps one date into every version it produces, so there is no
        // honest answer here: either date reproduces one architecture and
        // restamps the other. Both are named, since the way out is to reproduce
        // them one at a time.
        let err = build_stamp(
            "deb13",
            BuildDate::Recorded,
            &priors(&[("amd64", Some("2026-07-30")), ("arm64", Some("2026-07-31"))]),
        )
        .unwrap_err();
        let message = format!("{err}");
        for expected in ["amd64", "arm64", "2026-07-30", "2026-07-31"] {
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn the_manifest_names_the_buildinfo_a_component_produced() {
        let recipe: Recipe = toml::from_str(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"a\"\nsource.git = \"x\"\n",
        )
        .unwrap();
        let report = architecture_report(
            "amd64",
            vec![Built {
                component: "a".to_string(),
                source: git("abc"),
                version: None,
                version_stamp: VersionStamp::default(),
                artifacts: artifacts(1),
                buildinfo: Some(BuildInfo {
                    path: PathBuf::from("/w/out/trixie/amd64/a/a_1.0_amd64.buildinfo"),
                    sha256: "abc123".to_string(),
                }),
                packages: Vec::new(),
            }],
            Vec::new(),
            Vec::new(),
        );

        let manifest = manifest_for_architecture(
            &recipe,
            &order(&["a"]),
            &[],
            &report,
            &BTreeMap::new(),
            Path::new("/w"),
        );
        let buildinfo = manifest.records_by_name()["a"]
            .buildinfo
            .clone()
            .expect("the record names the buildinfo");
        assert_eq!(buildinfo.path, "out/trixie/amd64/a/a_1.0_amd64.buildinfo");
        assert_eq!(buildinfo.sha256, "abc123");
    }

    #[test]
    fn the_manifest_records_a_failed_component_however_it_failed() {
        let recipe: Recipe = toml::from_str(
            "name = \"r\"\nsuite = \"trixie\"\n\
             [[components]]\nname = \"a\"\nsource.git = \"x\"\n\
             [[components]]\nname = \"b\"\nsource.git = \"y\"\n",
        )
        .unwrap();
        let report = architecture_report(
            "amd64",
            Vec::new(),
            vec![Failed {
                component: "a".to_string(),
                source: git("abc"),
                error: Error::Plan("boom".to_string()),
            }],
            Vec::new(),
        );
        // `b`'s source never resolved, which is the run's failure rather than
        // this architecture's — but this architecture does not have `b` either,
        // so its manifest records it as failed alongside `a`.
        let unresolved = [Failed {
            component: "b".to_string(),
            source: Fingerprint::none(),
            error: Error::Plan("no such repository".to_string()),
        }];
        let manifest = manifest_for_architecture(
            &recipe,
            &order(&["a", "b"]),
            &unresolved,
            &report,
            &BTreeMap::new(),
            Path::new("/w"),
        );
        let records = manifest.records_by_name();
        assert_eq!(records["a"].status, STATUS_FAILED);
        assert!(records["a"].error.as_deref().unwrap().contains("boom"));
        assert_eq!(records["b"].status, STATUS_FAILED);
        assert!(
            records["b"]
                .error
                .as_deref()
                .unwrap()
                .contains("no such repository")
        );
    }

    #[test]
    fn a_run_tallies_every_architecture_and_the_sources_none_of_them_got() {
        // The report splits along the seam the run does: one resolve, then one
        // build per architecture. So an unresolved source is counted once...
        let report = RunReport {
            order: order(&["a", "b"]),
            unresolved: vec![Failed {
                component: "b".to_string(),
                source: Fingerprint::none(),
                error: Error::Plan("no such repository".to_string()),
            }],
            architectures: vec![
                architecture_report(
                    "amd64",
                    vec![Built {
                        component: "a".to_string(),
                        source: git("abc"),
                        version: None,
                        version_stamp: VersionStamp::default(),
                        artifacts: artifacts(3),
                        buildinfo: None,
                        packages: Vec::new(),
                    }],
                    Vec::new(),
                    Vec::new(),
                ),
                architecture_report(
                    "arm64",
                    vec![Built {
                        component: "a".to_string(),
                        source: git("abc"),
                        version: None,
                        version_stamp: VersionStamp::default(),
                        artifacts: artifacts(2),
                        buildinfo: None,
                        packages: Vec::new(),
                    }],
                    Vec::new(),
                    Vec::new(),
                ),
            ],
            stopped_by: None,
            cancelled: false,
        };
        // ...the artifacts are summed across the architectures that produced
        // them...
        assert_eq!(report.artifact_count(), 5);
        assert_eq!(report.architectures[0].artifact_count(), 3);
        // ...and a source nothing could resolve fails the run, however well each
        // architecture did with the rest.
        assert!(!report.is_success());
        // Each architecture accounts for it in its own right, since from its
        // side it is a component it did not get.
        for architecture in &report.architectures {
            let undelivered: Vec<&str> = report
                .undelivered(architecture)
                .map(|failed| failed.component.as_str())
                .collect();
            assert_eq!(undelivered, ["b"]);
        }
    }
}
