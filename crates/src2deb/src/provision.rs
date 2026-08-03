//! Provisioning a build root for a component — the main ferroday-cage seam.
//!
//! [`BuildRootProvider`] abstracts "give me a build root for this component."
//! The engine is written against it, so the strategy can change without
//! touching the pipeline.
//!
//! [`LayeredProvision`] is the strategy used when the host supports an
//! unprivileged overlay: it bootstraps a shared base once with the toolchain
//! baked in, then stages only each component's build-dependency delta into a
//! disposable overlay upper (`base_layer`/`stage_layer` yields a `BuildLayer`)
//! and roots the build cage on an `overlay_rootfs(base, upper)` of
//! `base + increment`. One heavy bootstrap serves every component and the base
//! is never mutated.
//!
//! [`FullReprovision`] is the fallback for a host without overlay support: it
//! bakes a fresh rootfs per component (toolchain plus the component's
//! build-deps, resolved against the archive and the local pool) with the
//! Debian provisioner. The content-addressed package cache keeps it from
//! re-downloading the shared base each time. The engine picks between the two
//! with [`host::overlay_blocker`](ferroday_cage::host::overlay_blocker).
//!
//! Either way, a root is complete when it is handed over: when the recipe pins a
//! rustup toolchain, it is installed ([`crate::toolchain`]) while the root is
//! being provisioned, so a build pass finds it already there rather than
//! fetching it. For the layered strategy that is the difference between one
//! download per run and one per component per run, since the upper a pass writes
//! into is discarded when the component finishes.
//!
//! # One resolve per root
//!
//! A bootstrap resolves once. [`Debian::resolve`] reports the exact,
//! archive-verified package set a bootstrap would install, and that one plan is
//! used twice over: as the cache key below, and as the install set the bootstrap
//! is pinned to (`pinned_provisioner`). Resolving a second time inside the
//! bootstrap would leave a window in which the archive could publish, handing
//! the root a package set that its recorded key does not describe.
//!
//! A layered increment already resolves once — `stage_layer` computes its own
//! delta against the base's installed set — so only the two bootstrap paths,
//! [`LayeredProvision`]'s shared base and [`FullReprovision`]'s per-component
//! roots, had a second resolve to remove.
//!
//! # The build-root cache
//!
//! A build root that a build does not mutate is cached on its resolved plan:
//! `ensure_for_plan` provisions the root when the plan's `plan_key` differs from
//! the one recorded beside it, and `record_plan_key` writes the key once the
//! root is genuinely reusable. A later run reuses the root only when the key
//! still matches, and rebuilds from clean when the dependency set changed — the
//! staleness [`ensure`](ferroday_cage::provision::ensure), keyed only on the
//! directory existing, would miss. This keys [`FullReprovision`]'s per-component
//! roots and [`LayeredProvision`]'s shared base.
//!
//! The plan key names the suite and the architecture, so a root provisioned for
//! one target never matches another's key. Roots are therefore kept per target
//! on disk as well ([`base_dir`], [`roots_dir`], [`uppers_dir`]): a work
//! directory building for two architectures keeps a warm base for each, rather
//! than each run discarding the other's.
//!
//! Recording is deferred to the point a root is actually reusable, which differs
//! by strategy. The shared base is never written by a build, so it is recorded
//! the moment it is provisioned. A full-reprovision root is written in place by
//! its build, so its marker is cleared for the build and re-recorded only on
//! success (via [`BuildRoot::commit`]) — a build that fails partway leaves no marker
//! and rebuilds cleanly next run. The layered per-component *upper* is not cached
//! at all: the build writes into it through the overlay, so it is re-staged fresh
//! each run to stay hermetic.
//!
//! # Progress and cancellation
//!
//! Provisioning is the longest stretch of a run — a cold bootstrap resolves,
//! fetches, and unpacks several hundred packages — so both strategies report
//! what they are doing through the run's reporter, labeled with the root each
//! event belongs to.
//!
//! There are two seams for that, because src2deb provisions two ways.
//! A bootstrap goes through [`Provision`], which carries both progress and
//! cancellation, so the shared base and a fully-reprovisioned root can be
//! stopped at a package boundary. A layered increment is staged with
//! `stage_layer`, which is not a `Provision` at all and is observed through
//! [`Debian::observe`]; that seam reports but cannot be stopped, so an
//! increment already under way runs to completion. A cancelled run therefore
//! declines to *start* one instead.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ferroday_cage::provision::debian::{
    Available, BuildLayer, Debian, DebianBuilder, DebianEvent, Plan, Repository, ResolvedArchive,
};
use ferroday_cage::provision::{self, Provision, ProvisionEvent, ProvisionObserver};
use ferroday_cage::{Cage, CageBuilder};

use crate::cancel::Cancel;
use crate::engine::Progress;
use crate::error::{Error, Result, io_error};
use crate::recipe::{Component, Repository as RecipeRepository};

/// The mirror an additional repository resolves from when neither it nor the
/// recipe names one — the same default, scheme included, that the provisioner
/// uses for the primary suite, so the archive is defaulted one way throughout.
/// The archive's authenticity comes from its OpenPGP signature, not the
/// transport, which is why plain `http` is the archive-wide convention.
const DEFAULT_MIRROR: &str = "http://deb.debian.org/debian";

/// Packages baked into the build root when the recipe selects the rustup
/// toolchain provider, so the pinned toolchain can be fetched into it.
///
/// `curl` (with `ca-certificates`, already in [`TOOLCHAIN`]) downloads the
/// upstream `rustup-init` in [`install_toolchain`], which runs over the root
/// once it is provisioned. The archive's own
/// `rustc`/`cargo` stay installed to satisfy the build's declared build-deps —
/// which rules out Debian's `rustup` package, since it `Conflicts` with them;
/// the upstream installer lands rustup in `~/.cargo/bin` instead, shadowing
/// them on `PATH` without a packaging conflict.
const RUSTUP_ENABLERS: [&str; 1] = ["curl"];

/// The generic Debian build toolchain baked into every build root, on top of
/// which a component's own build-dependencies are installed.
///
/// `ca-certificates` and `git` are here for the vendor pass, not the compile:
/// the pass-1 vendor cage runs the component's own vendoring idiom (for a
/// cargo/`just` project, `cargo vendor`), which fetches crates and git
/// dependencies over HTTPS. `debian/control` build-deps don't include them —
/// upstream assumes vendoring runs on a developer's machine, outside the build
/// chroot, where they already exist — but src2deb runs it inside the cage, so
/// without `ca-certificates` cargo's TLS cannot verify the archive or GitHub and
/// vendoring fails (silently, for a Makefile that pipes `cargo vendor` output).
pub const TOOLCHAIN: [&str; 5] = [
    "build-essential",
    "dpkg-dev",
    "fakeroot",
    "ca-certificates",
    "git",
];

/// The directory within the work directory holding every shared base.
pub const BASE_DIR: &str = "base";
/// The directory within the work directory holding every fully-reprovisioned
/// root.
pub const ROOTS_DIR: &str = "roots";
/// The directory within the work directory holding every overlay upper.
pub const UPPERS_DIR: &str = "uppers";

/// The path of the shared base for `suite` and `architecture` under `work_dir`:
/// `base/<suite>/<architecture>/`.
///
/// Keyed by the target for the same reason the plan key carries one: a root is a
/// bootstrap of one suite for one architecture, so a base provisioned for
/// another target never matches the key and is rebuilt from clean. Sharing one
/// path across targets is therefore not sharing at all — it is each target
/// discarding the last one's bootstrap. Keying it instead lets a work directory
/// hold a warm base per target, which is what makes a multi-architecture run
/// pay for one bootstrap each rather than one per run.
///
/// The cost is disk: a base per suite and architecture rather than one. See the
/// work-directory chapter of the guide.
pub fn base_dir(work_dir: &Path, suite: &str, architecture: &str) -> PathBuf {
    work_dir.join(BASE_DIR).join(suite).join(architecture)
}

/// The path of the fully-reprovisioned roots for `suite` and `architecture`
/// under `work_dir`: `roots/<suite>/<architecture>/`, holding one root per
/// component.
///
/// Keyed like [`base_dir`], and for the same reason.
pub fn roots_dir(work_dir: &Path, suite: &str, architecture: &str) -> PathBuf {
    work_dir.join(ROOTS_DIR).join(suite).join(architecture)
}

/// The path of the overlay uppers for `suite` and `architecture` under
/// `work_dir`: `uppers/<suite>/<architecture>/`, holding one upper per
/// component while it builds.
///
/// An upper is disposable, so nothing here is cached and nothing is reused — it
/// is keyed to stay symmetric with the base it layers over, so a work directory
/// holding two targets' roots holds two targets' uppers beside them rather than
/// one shared directory whose contents belong to whichever target ran last.
pub fn uppers_dir(work_dir: &Path, suite: &str, architecture: &str) -> PathBuf {
    work_dir.join(UPPERS_DIR).join(suite).join(architecture)
}

/// A stable, archive-anchored cache key for a resolved [`Plan`]: the suite,
/// architecture, the pinned rustup toolchain if the recipe names one, and every
/// package's name, version, and archive SHA-256 in the plan's own name order.
///
/// Two resolves of an unchanged archive state produce the same key; any change
/// to the resolved set — a new upstream version, an added or removed
/// build-dependency, a different package fed in from the local pool — changes
/// it. Each digest is archive-verified (it chains back to the release
/// signature), so the key reflects exactly what a bootstrap would install, not a
/// mirror's unverified word. Written beside a build root so a later run can tell
/// whether the root still matches the plan it was provisioned for.
///
/// The toolchain belongs in the key because it is installed into the root as
/// part of provisioning it (see [`install_toolchain`]). Without it the key would
/// describe less than the root holds, and a recipe that repinned its toolchain
/// would reuse a root carrying the version it replaced.
fn plan_key(plan: &Plan, rustup_version: Option<&str>) -> String {
    let rows = plan.packages.iter().map(|package| {
        (
            package.name.as_str(),
            package.version.as_str(),
            package.sha256.as_str(),
        )
    });
    format_plan_key(&plan.suite, &plan.architecture, rustup_version, rows)
}

/// Formats a plan key from its parts. Split from [`plan_key`] so the key's
/// contract can be exercised without constructing a ferroday-cage [`Plan`].
fn format_plan_key<'a>(
    suite: &str,
    architecture: &str,
    rustup_version: Option<&str>,
    packages: impl Iterator<Item = (&'a str, &'a str, &'a str)>,
) -> String {
    let mut key = format!("{suite} {architecture}\n");
    // A line of its own, so a root provisioned without a pinned toolchain and
    // one provisioned with any given version can never share a key.
    key.push_str("rustup ");
    key.push_str(rustup_version.unwrap_or("none"));
    key.push('\n');
    for (name, version, sha256) in packages {
        key.push_str(name);
        key.push(' ');
        key.push_str(version);
        key.push(' ');
        key.push_str(sha256);
        key.push('\n');
    }
    key
}

/// Installs the pinned rustup `version` into the build root at `dir`, by running
/// one cage over the root with the host network. `component` names the root for
/// the [`Progress::InstallingToolchain`] it reports, or is `None` for the shared
/// base.
///
/// Called once per root, immediately after the root is provisioned and before
/// its plan key is recorded, so an interrupted install leaves no key claiming
/// the root is current and the next run provisions from clean.
///
/// This is provisioning work rather than build work because of where a build's
/// writes go. Under the layered strategy a build pass runs on an overlay of the
/// shared base and a per-component upper, and the upper is disposed when the
/// component finishes — so a toolchain installed from a pass would be fetched
/// again for every component of every run, and the run's success would depend on
/// the rustup servers being reachable that many times. Installed into the root,
/// it is fetched once for the shared base and reused by every component.
///
/// The installer's output is captured rather than streamed, so a parallel run's
/// stderr is not interleaved with rustup's own progress rendering; a failure
/// carries what it wrote. Like a layered increment, an install already under way
/// cannot be stopped, so a cancelled run declines to start one instead — the
/// cage is tied to this process's lifetime, so the escape hatch of a second
/// Ctrl-C still leaves no installer running.
fn install_toolchain(
    dir: &Path,
    component: Option<&str>,
    version: &str,
    cancel: &Cancel,
    reporter: &mut dyn FnMut(Progress),
) -> Result<()> {
    if cancel.requested() {
        return Err(Error::Cancelled);
    }
    reporter(Progress::InstallingToolchain { component, version });
    let script = crate::toolchain::install_script(version);
    let output = Cage::builder()
        .rootfs(dir)
        .network(ferroday_cage::Network::Host)
        .stop_with_caller(true)
        .command("/bin/sh")
        .args(["-e", "-u", "-c", script.as_str()])
        .build()
        .map_err(Error::Cage)?
        .output()
        .map_err(Error::Cage)?;
    if !output.status.success() {
        return Err(Error::Toolchain {
            version: version.to_string(),
            reason: format!(
                "{}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(())
}

/// The sidecar file that records the [`plan_key`] a build root was provisioned
/// for. It sits beside the root rather than inside it, so removing the root and
/// reading its key are independent.
fn plan_sidecar(dir: &Path) -> PathBuf {
    let mut sidecar = dir.as_os_str().to_os_string();
    sidecar.push(".plan");
    PathBuf::from(sidecar)
}

/// The overlay work directory ferroday-cage stages beside an `upper`, named
/// `.<upper>.work` in the same parent.
///
/// `stage_layer` creates this alongside the upper, and a `BuildLayer` disposes
/// both on drop; reproducing the name here lets [`LayeredProvision`] clean up a
/// work directory an interrupted run leaked, symmetrically with the upper.
/// Returns `None` only for a path with no parent or file name, which an upper
/// under the uppers directory always has.
fn overlay_work_dir(upper: &Path) -> Option<PathBuf> {
    let (parent, name) = (upper.parent()?, upper.file_name()?);
    let mut work_name = std::ffi::OsString::from(".");
    work_name.push(name);
    work_name.push(".work");
    Some(parent.join(work_name))
}

/// Removes a tree src2deb had provisioned or built into, recovering from a
/// directory that cannot be walked.
///
/// [`provision::remove`] is the removal proper — it is the inverse of `ensure`,
/// it takes the `<dir>.lock` file left beside a root, and it is idempotent on an
/// absent path. What it cannot do is descend a directory with no owner traversal,
/// and neither can anything else that only walks the tree. The kernel leaves
/// exactly that inside an overlay work tree, so a run that never unwound — a
/// `SIGKILL`, a power loss, or the second Ctrl-C that exits outright so a stuck
/// graceful stop stays escapable — leaks one that no later run could clear. The
/// component would then fail at the same point on every run.
///
/// So a failure is retried once with traversal restored, which mirrors how
/// ferroday-cage disposes of a layer staged under the single-identity map
/// src2deb provisions with: restore permissions while descending, rather than
/// escalating through a map whose entries are the caller's own anyway. The retry
/// is only on failure, so the ordinary removal is unchanged and pays nothing.
fn remove_provisioned(path: &Path) -> Result<()> {
    if provision::remove(path).is_ok() {
        return Ok(());
    }
    restore_traversal(path);
    // The retry's error is the one reported: it describes the tree as it stands
    // after everything that could be done to it, which is what a caller has to
    // act on. The first error only described a state that no longer holds.
    provision::remove(path).map_err(Error::from)
}

/// Restores owner traversal on every directory at or under `path`, descending as
/// it goes.
///
/// Best-effort throughout: this exists only so the removal that follows can walk
/// the tree, and that removal reports the real failure if anything here did not
/// take. A path that is absent, is not a directory, or cannot be read is simply
/// left alone — the removal handles the first two and fails informatively on the
/// third.
fn restore_traversal(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    // Only directories block a walk, and only their own permissions do; a
    // symlink's target is never followed here.
    if !metadata.is_dir() {
        return;
    }
    let mode = metadata.permissions().mode();
    if mode & 0o700 != 0o700 {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o700));
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        restore_traversal(&entry.path());
    }
}

/// Bridges one build root's ferroday-cage provisioning events onto src2deb's
/// [`Progress`] stream.
///
/// It carries the little state the mapping needs: the resolved plan's package
/// count, which the unpack events do not carry themselves, so unpacking reports
/// the same `n/total` shape downloading does. One of these serves one root, so
/// a parallel run's concurrent provisions cannot cross counts.
struct RootProgress<'a> {
    /// The component whose root this is, or `None` for the shared base.
    component: Option<&'a str>,
    /// How many packages the resolved plan installs, from the plan event.
    packages: usize,
    /// How many of them have been unpacked so far.
    extracted: usize,
}

impl<'a> RootProgress<'a> {
    /// Creates a mapper for the root `component` names, or for the shared base.
    fn new(component: Option<&'a str>) -> RootProgress<'a> {
        RootProgress {
            component,
            packages: 0,
            extracted: 0,
        }
    }

    /// Maps one provisioning event onto `reporter`.
    fn report(&mut self, event: &DebianEvent<'_>, reporter: &mut dyn FnMut(Progress)) {
        let component = self.component;
        match event {
            DebianEvent::Fetching { url, .. } => reporter(Progress::Fetching { component, url }),
            DebianEvent::Resolved { plan, .. } => {
                self.packages = plan.packages.len();
                self.extracted = 0;
                reporter(Progress::PackagesResolved {
                    component,
                    packages: self.packages,
                });
            }
            DebianEvent::Downloading {
                package,
                index,
                total,
                ..
            } => reporter(Progress::Downloading {
                component,
                package,
                index: *index,
                total: *total,
            }),
            DebianEvent::Extracting { package, .. } => {
                self.extracted += 1;
                reporter(Progress::Extracting {
                    component,
                    package,
                    index: self.extracted,
                    // The plan always precedes the unpacking, so the total is
                    // known; the floor keeps the counter coherent rather than
                    // reading past its total if it ever were not.
                    total: self.packages.max(self.extracted),
                });
            }
            // `Resolving` marks a phase `Provisioning` has already announced,
            // and `CommandOutput` is dpkg's raw byte stream rather than
            // progress. Both are dropped, as is anything added later.
            _ => {}
        }
    }
}

/// The observer a bootstrap runs under: it forwards progress to src2deb's
/// [`Progress`] stream and answers the run's cancellation signal.
///
/// This is the seam that carries cancellation. A layered run's per-component
/// increment is staged through [`Debian::observe`] instead, which reports but
/// cannot be stopped, so an increment already under way runs to completion.
struct Bootstrap<'a, 'r> {
    root: RootProgress<'a>,
    cancel: &'a Cancel,
    reporter: &'r mut dyn FnMut(Progress),
}

impl<'a, 'r> Bootstrap<'a, 'r> {
    /// Creates the observer for `component`'s root, or for the shared base.
    fn new(
        component: Option<&'a str>,
        cancel: &'a Cancel,
        reporter: &'r mut dyn FnMut(Progress),
    ) -> Bootstrap<'a, 'r> {
        Bootstrap {
            root: RootProgress::new(component),
            cancel,
            reporter,
        }
    }
}

impl ProvisionObserver for Bootstrap<'_, '_> {
    fn progress(&mut self, event: ProvisionEvent<'_>) {
        // Only the Debian provisioner runs under this observer, so the tarball
        // events a `ProvisionEvent` can also carry never arrive.
        if let ProvisionEvent::Debian(event) = event {
            self.root.report(event, self.reporter);
        }
    }

    fn cancelled(&mut self) -> bool {
        self.cancel.requested()
    }
}

/// Resolves a root's install plan, reporting the archive work it does.
///
/// The plan is what the build-root cache is keyed on, so it is resolved before
/// any decision to provision — on every run, including one that goes on to
/// reuse the root untouched. Observing it keeps that leading release-and-index
/// fetch from being the run's first silent stretch.
///
/// Only the fetches are reported, not the resolved set: this resolve exists to
/// decide whether the root needs provisioning at all, so announcing a package
/// set that a reused root never installs would misreport the run. The provision
/// that does install a set reports it.
fn resolve_plan(
    debian: &mut Debian<'_>,
    component: Option<&str>,
    reporter: &mut dyn FnMut(Progress),
) -> Result<Plan> {
    let mut sink = |event: DebianEvent<'_>| {
        if let DebianEvent::Fetching { url, .. } = event {
            reporter(Progress::Fetching { component, url });
        }
    };
    debian.observe(&mut sink).resolve().map_err(Error::Debian)
}

/// A provisioner that installs `plan` verbatim, over the archives `pool_repo`
/// and `config` name.
///
/// The second half of resolving a bootstrap once. [`resolve_plan`] produced the
/// plan a moment earlier from these same archives, and handing it back closes
/// two things at once:
///
/// - **The divergence window.** Resolving again would let the archive publish
///   between the two, provisioning the root with a package set that is not the
///   one the recorded [`plan_key`] describes — so the key would claim a root is
///   current for a plan it does not hold.
/// - **The second index.** A bootstrap that installs a plan fetches no release
///   and no index, which is around 9 MB for a Debian suite.
///
/// The builder carries no `include`, `exclude`, or `base_priority`: a plan
/// already names every package to install, so the provisioner refuses those
/// alongside one. Everything that shapes *how* rather than *what* still applies
/// — the cache directory, the identity map, and the repositories, which the plan
/// names its packages' sources by index into.
///
/// **The trust model narrows, and only here.** Installing from a plan skips the
/// release and the index, so the package digests no longer chain to an archive
/// signature at install time. This plan was archive-verified seconds earlier, in
/// this run, by this process, and each `.deb` is still verified against the
/// digest it records. That is the narrow case where the trade is sound; it is
/// not an argument for replaying a plan kept from an earlier run.
fn pinned_provisioner(
    config: &ProvisionConfig,
    pool_repo: Option<Repository>,
    plan: Plan,
) -> Result<Debian<'static>> {
    let mut builder = config.debian_builder()?;
    if let Some(repository) = pool_repo {
        builder = builder.repository(repository);
    }
    builder.plan(plan).build().map_err(Error::Debian)
}

/// Provisions `dir` for `key` unless it already holds a root provisioned for the
/// same key, rebuilding from clean when a prior run left it provisioned for a
/// different plan.
///
/// This is the provision half of the build-root cache: the archive-verified plan
/// is the cache key, so a component whose resolved plan is unchanged reuses its
/// root, while a changed dependency set forces a fresh provision rather than
/// silently reusing a root built for the old set — the staleness `ensure` alone,
/// which keys only on the directory existing, would miss. Returns whether it
/// (re)provisioned.
///
/// It clears the sidecar but never writes it: recording a root as clean-for-`key`
/// is [`record_plan_key`]'s job, which the caller invokes once the root is
/// genuinely reusable — immediately for a base an overlay keeps pristine, or only
/// after a successful build for a root built in place (see [`BuildRoot::commit`]).
/// An interrupted rebuild therefore leaves no key claiming a partial root is
/// current.
///
/// `observer` receives the bootstrap's progress and is asked whether to stop;
/// a cancelled bootstrap fails with [`Error::Cancelled`] and leaves nothing
/// behind, so the next run provisions from clean.
fn ensure_for_plan(
    dir: &Path,
    debian: &mut Debian<'_>,
    key: &str,
    observer: &mut dyn ProvisionObserver,
) -> Result<bool> {
    let sidecar = plan_sidecar(dir);
    if dir.exists() && read_plan_key(&sidecar)?.as_deref() == Some(key) {
        return Ok(false);
    }
    // Absent, or provisioned for a different plan: rebuild from clean so no
    // package from the prior plan lingers. The removal is idempotent, so it needs
    // no existence check: it takes the `<dir>.lock` file `ensure` leaves beside
    // the root along with the root, deletes a tree whose ownership an identity
    // map wrote, which a plain `remove_dir_all` cannot, and recovers a directory
    // a build left unwalkable — which for a full-reprovision root, written in
    // place by its build, is a tree the build itself chose the modes in.
    remove_provisioned(dir)?;
    clear_plan_key(dir);
    // `Provision` is the configurable form of `ensure`, and the only one that
    // carries an observer.
    Provision::new(dir)
        .observe(observer)
        .run(debian)
        .map_err(Error::from)?;
    Ok(true)
}

/// Records that the root at `dir` is provisioned for `key`, by writing the plan
/// sidecar beside it. A later [`ensure_for_plan`] reuses the root while the key
/// still matches.
fn record_plan_key(dir: &Path, key: &str) -> Result<()> {
    let sidecar = plan_sidecar(dir);
    std::fs::write(&sidecar, key).map_err(|err| io_error("writing the plan cache", &sidecar, err))
}

/// Clears the plan sidecar beside the root at `dir`, so the root is not treated
/// as clean-for-any-plan until it is recorded again. A missing sidecar is not an
/// error.
fn clear_plan_key(dir: &Path) {
    let _ = std::fs::remove_file(plan_sidecar(dir));
}

/// Reads a plan-cache sidecar, returning `None` when it is absent.
fn read_plan_key(sidecar: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(sidecar) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_error("reading the plan cache", sidecar, err)),
    }
}

/// Adds each recipe [`Repository`](RecipeRepository) to a Debian builder as an
/// additional archive, resolving its suite and mirror against the recipe's
/// primary when it leaves them unset.
///
/// Shared with [`crate::check`], so a runtime dependency is judged available
/// against exactly the archives a build root is provisioned from rather than
/// against a second reading of the same recipe fields.
pub(crate) fn add_repositories<'a>(
    mut builder: DebianBuilder<'a>,
    repositories: &[RecipeRepository],
    default_suite: &str,
    default_mirror: Option<&str>,
) -> Result<DebianBuilder<'a>> {
    for repository in repositories {
        builder = builder.repository(archive_repository(
            repository,
            default_suite,
            default_mirror,
        )?);
    }
    Ok(builder)
}

/// The names an archive offers, and what provides each virtual one.
///
/// Answered rather than handed over: a merged Debian suite is tens of thousands
/// of names, and the provisioner's own projection of them is the thing to
/// interrogate rather than copy. A trait rather than the projection itself so
/// the readings built on it — [`crate::check`]'s over a pool, and
/// [`crate::plan`]'s over a recipe's packaging — can be exercised over a known
/// set of names.
pub(crate) trait Names {
    /// Whether `name` is there, as a real package or as one something provides.
    fn contains(&self, name: &str) -> bool;

    /// The real packages that provide the virtual package `name`, in a stable
    /// order, and empty for an ordinary real package — nothing provides itself.
    fn providers(&self, name: &str) -> Vec<String>;
}

impl Names for Available {
    fn contains(&self, name: &str) -> bool {
        Available::contains(self, name)
    }

    fn providers(&self, name: &str) -> Vec<String> {
        Available::providers(self, name)
            .map(str::to_string)
            .collect()
    }
}

/// Every name the archives a build root is provisioned from offer, for
/// `architecture`: the recipe's suite, its additional repositories, and `pool`
/// when the run has one to read.
///
/// Reading them downloads no package, unpacks nothing, and resolves nothing — it
/// fetches each archive's release and index and projects them to their names —
/// so a foreign architecture is read as readily as the host's, and any number of
/// names costs one pass.
///
/// Shared for the same reason [`add_repositories`] is: a dependency is judged
/// available against exactly the archives a build root resolves from, rather
/// than against a second reading of the same recipe fields.
pub(crate) fn available_names(
    recipe: &crate::recipe::Recipe,
    architecture: &str,
    pool: Option<Repository>,
) -> Result<Box<dyn Names>> {
    let mirror = recipe.mirror.as_deref();
    let builder: DebianBuilder<'static> =
        Debian::builder(recipe.suite.clone()).architecture(architecture.to_string());
    let builder = match mirror {
        Some(mirror) => builder.mirror(mirror.to_string()),
        None => builder,
    };
    let mut builder = add_repositories(builder, &recipe.repositories, &recipe.suite, mirror)?;
    if let Some(pool) = pool {
        builder = builder.repository(pool);
    }
    let names = builder
        .build()
        .map_err(Error::Debian)?
        .available()
        .map_err(Error::Debian)?;
    Ok(Box::new(names))
}

/// Builds a ferroday-cage [`Repository`] from a recipe [`Repository`](RecipeRepository).
fn archive_repository(
    repository: &RecipeRepository,
    default_suite: &str,
    default_mirror: Option<&str>,
) -> Result<Repository> {
    let suite = repository.suite.as_deref().unwrap_or(default_suite);
    let mirror = repository
        .mirror
        .as_deref()
        .or(default_mirror)
        .unwrap_or(DEFAULT_MIRROR);
    let mut builder = Repository::builder(suite)
        .name(repository.name.as_str())
        .mirror(mirror)
        .components(repository.components.iter().map(String::as_str))
        .trust_unsigned(repository.trust_unsigned);
    if let Some(keyring) = &repository.keyring {
        builder = builder.keyring(keyring);
    }
    builder.build().map_err(Error::Debian)
}

/// The distinct archive states a run's resolves observed.
///
/// A run resolves many times — once for the shared base, once for each
/// component's root — and every one of those resolves reports the state of every
/// archive it read: the mirror that actually served, the digest of the release
/// body that was verified, that release's own `Date`, and the key that verified
/// it. Those states should be identical across a run, and a run where they are
/// not is the interesting case: the archive published while the run was building
/// against it.
///
/// So the states are collected rather than assumed, and only the distinct ones
/// are kept. One entry per configured repository is the ordinary result; two for
/// one mirror is a run that saw the archive move, which the record shows rather
/// than flattens.
///
/// Behind a lock because a parallel run's workers resolve concurrently. The lock
/// is held only to compare and push, never across a fetch.
#[derive(Debug, Default)]
struct ArchiveLog {
    seen: Mutex<Vec<ResolvedArchive>>,
}

impl ArchiveLog {
    /// Records any of `archives` not already held.
    ///
    /// A poisoned lock is passed over. This is a provenance record rather than
    /// build state, and losing an entry to a panicking worker is not a reason to
    /// fail a run that is otherwise fine — the worker's own failure is reported
    /// on its own terms.
    fn observe(&self, archives: &[ResolvedArchive]) {
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        for archive in archives {
            if !seen.contains(archive) {
                seen.push(archive.clone());
            }
        }
    }

    /// Every state observed, in an order no scheduling can vary: by mirror, then
    /// by suite, then by the digest of the release that was verified.
    ///
    /// Sorted rather than kept in observation order, because a parallel run
    /// observes them in whichever order its workers happen to finish and a
    /// provenance record must not vary with that. Where one mirror and suite
    /// yield two entries, the release `Date` each carries is what orders them in
    /// time, and it is the archive's own fact rather than an artefact of how the
    /// run was scheduled.
    fn resolved(&self) -> Vec<ResolvedArchive> {
        let Ok(seen) = self.seen.lock() else {
            return Vec::new();
        };
        let mut archives = seen.clone();
        archives.sort_by(|a, b| {
            (&a.mirror, &a.suite, &a.release_sha256).cmp(&(&b.mirror, &b.suite, &b.release_sha256))
        });
        archives
    }
}

/// A provisioned build root, ready to root a build cage.
pub trait BuildRoot {
    /// Returns a [`CageBuilder`] already rooted at this build environment: a
    /// plain rootfs for a fully-reprovisioned root, or an overlay for a layered
    /// one.
    fn cage_builder(&self) -> CageBuilder;

    /// Marks the root reusable after a successful build.
    ///
    /// A root built in place (full reprovisioning) clears its plan marker for the
    /// build and re-records it here, so a build that fails partway leaves no
    /// marker and forces a clean rebuild next run. An overlay-backed root, whose
    /// base is never mutated by a build, needs nothing and uses this default
    /// no-op.
    fn commit(&self) -> Result<()> {
        Ok(())
    }
}

/// Provisions build roots for components.
///
/// [`prepare`](Self::prepare) mutates shared state (bootstrapping the layered
/// base) and runs once before any build. [`build_root`](Self::build_root) takes
/// `&self` and touches only per-component state, so a parallel build can share
/// one provider across threads and call it from several at once: the shared
/// package cache stages each entry under a name unique to its writer, so two
/// provisions downloading the same package both succeed.
///
/// Both methods take the run's reporter, because provisioning is the longest
/// stretch of a run and has the most to say: a cold bootstrap fetches and
/// unpacks several hundred packages. Every event it reports is labeled with the
/// root it belongs to, so a parallel run's interleaved output stays
/// attributable.
pub trait BuildRootProvider {
    /// Prepares any shared state (for [`FullReprovision`], nothing).
    fn prepare(&mut self, reporter: &mut dyn FnMut(Progress)) -> Result<()>;

    /// Provisions a build root whose baked packages are the toolchain plus
    /// `build_deps`, resolving against the archive and `pool_repo` — the local
    /// pool as a repository, or `None` when it holds nothing yet.
    ///
    /// The caller resolves `pool_repo` from the pool, rather than this taking
    /// the pool itself, so provisioning depends on nothing but the repository
    /// declaration it was handed.
    fn build_root(
        &self,
        component: &Component,
        build_deps: &[String],
        pool_repo: Option<Repository>,
        reporter: &mut dyn FnMut(Progress),
    ) -> Result<Box<dyn BuildRoot>>;

    /// Every distinct archive state this provider's resolves observed, in a
    /// stable order.
    ///
    /// Only the distinct ones: every root a run provisions resolves against the
    /// same archives, so a 26-component run observes each of them 27 times and
    /// the record should hold one entry. Two entries for one mirror and suite is
    /// a run that saw the archive publish while it was building against it.
    ///
    /// Read once the run's roots are all provisioned, so it accounts for every
    /// resolve the run made rather than for whichever came first.
    fn archives(&self) -> Vec<ResolvedArchive>;
}

/// The provisioning settings a [`FullReprovision`] or [`LayeredProvision`]
/// shares: where packages come from and what generic tooling is baked in,
/// independent of the disposable-root strategy layered on top.
pub struct ProvisionConfig {
    suite: String,
    architecture: String,
    mirror: Option<String>,
    cache_dir: Option<PathBuf>,
    repositories: Vec<RecipeRepository>,
    /// The pinned rustup toolchain to install into each root, or `None` to build
    /// with the archive's own Rust.
    rustup_version: Option<String>,
}

impl ProvisionConfig {
    /// Creates the shared configuration. `cache_dir` is the content-addressed
    /// package cache; `repositories` are the recipe's extra archives; and
    /// `rustup_version` pins a rustup toolchain to install into each root over
    /// the archive's own Rust, or `None` to build with the archive's.
    pub fn new(
        suite: impl Into<String>,
        architecture: impl Into<String>,
        mirror: Option<String>,
        cache_dir: Option<PathBuf>,
        repositories: Vec<RecipeRepository>,
        rustup_version: Option<String>,
    ) -> ProvisionConfig {
        ProvisionConfig {
            suite: suite.into(),
            architecture: architecture.into(),
            mirror,
            cache_dir,
            repositories,
            rustup_version,
        }
    }

    /// The pinned rustup toolchain each root carries, or `None` for the
    /// archive's own Rust.
    fn rustup_version(&self) -> Option<&str> {
        self.rustup_version.as_deref()
    }

    /// Starts a Debian builder carrying the suite, architecture, mirror, cache,
    /// and the recipe's additional repositories — everything shared between the
    /// base bootstrap, a full root, and a layer's delta resolution.
    fn debian_builder(&self) -> Result<DebianBuilder<'static>> {
        let mut builder =
            Debian::builder(self.suite.as_str()).architecture(self.architecture.as_str());
        if let Some(mirror) = &self.mirror {
            builder = builder.mirror(mirror.as_str());
        }
        if let Some(cache) = &self.cache_dir {
            builder = builder.cache_dir(cache);
        }
        add_repositories(
            builder,
            &self.repositories,
            &self.suite,
            self.mirror.as_deref(),
        )
    }

    /// Adds the generic build toolchain to a builder: the [`TOOLCHAIN`] packages,
    /// plus the [`RUSTUP_ENABLERS`] when the recipe installs a pinned toolchain
    /// over the archive's Rust.
    fn include_toolchain<'a>(&self, builder: DebianBuilder<'a>) -> DebianBuilder<'a> {
        let builder = builder.include(TOOLCHAIN.iter().copied());
        if self.rustup_version.is_some() {
            builder.include(RUSTUP_ENABLERS.iter().copied())
        } else {
            builder
        }
    }
}

/// Bakes a fully-configured rootfs per component with the Debian provisioner.
/// The fallback strategy for a host without overlay support.
///
/// Each root is plan-keyed (see `ensure_for_plan`), so a re-run reuses a
/// component's root when its dependency plan is unchanged and rebuilds it when
/// the plan changed. Reuse is not hermetic — with no overlay to isolate them,
/// the build's writes land in the root itself — so a reused root carries the
/// prior build's mutations; this trades hermeticity for avoiding a full
/// re-bootstrap, which is why [`LayeredProvision`] is preferred where an overlay
/// is available.
pub struct FullReprovision {
    config: ProvisionConfig,
    roots_dir: PathBuf,
    cancel: Cancel,
    archives: ArchiveLog,
}

impl FullReprovision {
    /// Creates the strategy. `roots_dir` holds one provisioned rootfs per
    /// component; `config` supplies the shared provisioning settings; `cancel`
    /// stops a bootstrap in flight.
    pub fn new(
        config: ProvisionConfig,
        roots_dir: impl Into<PathBuf>,
        cancel: Cancel,
    ) -> FullReprovision {
        FullReprovision {
            config,
            roots_dir: roots_dir.into(),
            cancel,
            archives: ArchiveLog::default(),
        }
    }
}

impl BuildRootProvider for FullReprovision {
    fn prepare(&mut self, _reporter: &mut dyn FnMut(Progress)) -> Result<()> {
        Ok(())
    }

    fn archives(&self) -> Vec<ResolvedArchive> {
        self.archives.resolved()
    }

    fn build_root(
        &self,
        component: &Component,
        build_deps: &[String],
        pool_repo: Option<Repository>,
        reporter: &mut dyn FnMut(Progress),
    ) -> Result<Box<dyn BuildRoot>> {
        let name = component.name.as_str();
        reporter(Progress::Provisioning {
            component: Some(name),
        });
        let root_dir = self.roots_dir.join(name);

        let mut builder = self
            .config
            .include_toolchain(self.config.debian_builder()?)
            .include(build_deps.iter().cloned());
        if let Some(repository) = pool_repo.clone() {
            builder = builder.repository(repository);
        }

        // Resolve once. The plan is the cache key and the install set both:
        // provision the root only when no root already matches this component's
        // archive-verified plan, and provision it from that exact plan rather
        // than resolving a second time. See `pinned_provisioner`.
        let mut resolver = builder.build().map_err(Error::Debian)?;
        let rustup_version = self.config.rustup_version();
        let plan = resolve_plan(&mut resolver, Some(name), reporter)?;
        self.archives.observe(&plan.archives);
        let key = plan_key(&plan, rustup_version);
        let mut debian = pinned_provisioner(&self.config, pool_repo, plan)?;
        let mut observer = Bootstrap::new(Some(name), &self.cancel, reporter);
        let provisioned = ensure_for_plan(&root_dir, &mut debian, &key, &mut observer)?;
        // Only a freshly-provisioned root needs the toolchain: a reused one was
        // provisioned for this same key, which names the pinned version, so it
        // already carries it.
        if provisioned && let Some(version) = rustup_version {
            install_toolchain(&root_dir, Some(name), version, &self.cancel, reporter)?;
        }

        // This root has no overlay: the build writes into it directly. Clear its
        // plan marker for the build's duration so a build that fails partway
        // leaves a root with no marker, forcing a clean rebuild next run rather
        // than reusing the failed root. `commit` re-records the key on success.
        clear_plan_key(&root_dir);

        Ok(Box::new(PlainRoot {
            rootfs: root_dir,
            key,
        }))
    }
}

/// A build root that is a plain provisioned rootfs directory.
struct PlainRoot {
    rootfs: PathBuf,
    /// The archive-verified plan key the root was provisioned for, re-recorded
    /// by [`commit`](BuildRoot::commit) once the build succeeds.
    key: String,
}

impl BuildRoot for PlainRoot {
    fn cage_builder(&self) -> CageBuilder {
        Cage::builder().rootfs(&self.rootfs)
    }

    fn commit(&self) -> Result<()> {
        // The build finished; re-record the root as clean-for-key so the next run
        // may reuse it, accepting this build's in-place mutations — the
        // documented full-reprovision trade-off.
        record_plan_key(&self.rootfs, &self.key)
    }
}

/// Bootstraps a shared base once, then stages each component's build-dependency
/// delta into a disposable overlay upper. The layered strategy, used when the
/// host supports an unprivileged overlay.
///
/// The base carries the [`TOOLCHAIN`], and the recipe's pinned rustup toolchain
/// when it names one, and is bootstrapped in [`prepare`] once;
/// every component's [`build_root`] resolves only the packages the base lacks,
/// installs that increment into a throwaway upper, and roots the build cage on
/// an overlay of `base + increment`. The upper is disposed when its
/// `LayeredRoot` drops, leaving the base pristine for the next component.
///
/// [`prepare`]: BuildRootProvider::prepare
/// [`build_root`]: BuildRootProvider::build_root
pub struct LayeredProvision {
    config: ProvisionConfig,
    /// The shared base, bootstrapped once with the toolchain baked in.
    base_dir: PathBuf,
    /// Holds one disposable overlay upper per component.
    uppers_dir: PathBuf,
    cancel: Cancel,
    archives: ArchiveLog,
}

impl LayeredProvision {
    /// Creates the strategy. `base_dir` holds the shared base and `uppers_dir`
    /// holds one disposable upper per component; `config` supplies the shared
    /// provisioning settings; `cancel` stops the base bootstrap in flight.
    pub fn new(
        config: ProvisionConfig,
        base_dir: impl Into<PathBuf>,
        uppers_dir: impl Into<PathBuf>,
        cancel: Cancel,
    ) -> LayeredProvision {
        LayeredProvision {
            config,
            base_dir: base_dir.into(),
            uppers_dir: uppers_dir.into(),
            cancel,
            archives: ArchiveLog::default(),
        }
    }
}

impl BuildRootProvider for LayeredProvision {
    fn archives(&self) -> Vec<ResolvedArchive> {
        self.archives.resolved()
    }

    fn prepare(&mut self, reporter: &mut dyn FnMut(Progress)) -> Result<()> {
        reporter(Progress::Provisioning { component: None });
        // Bootstrap the shared base once, toolchain baked in. Every component's
        // delta layers over this; the base is the overlay's read-only lower and
        // is never written by a build, so keying it on its plan is safe — a
        // stale base (a changed toolchain resolution) rebuilds, an unchanged one
        // is reused across runs.
        //
        // Resolved once and installed from that plan, as a full root is: the
        // base is the one bootstrap a layered run performs, so it is where the
        // second resolve was. See `pinned_provisioner`.
        let mut resolver = self
            .config
            .include_toolchain(self.config.debian_builder()?)
            .build()
            .map_err(Error::Debian)?;
        let rustup_version = self.config.rustup_version();
        let plan = resolve_plan(&mut resolver, None, reporter)?;
        self.archives.observe(&plan.archives);
        let key = plan_key(&plan, rustup_version);
        // The base resolves against the archive alone: it is bootstrapped before
        // any component builds, so the pool holds nothing to feed into it.
        let mut debian = pinned_provisioner(&self.config, None, plan)?;
        let mut observer = Bootstrap::new(None, &self.cancel, reporter);
        let provisioned = ensure_for_plan(&self.base_dir, &mut debian, &key, &mut observer)?;
        // Install the pinned toolchain into the base while it is still being
        // provisioned, so every component's overlay inherits it and no build
        // pass has to fetch it. Only a freshly-provisioned base needs it: a
        // reused one matched a key that names the pinned version.
        if provisioned && let Some(version) = rustup_version {
            install_toolchain(&self.base_dir, None, version, &self.cancel, reporter)?;
        }
        // The base is the overlay's read-only lower and is never written by a
        // build, so it is clean-for-key the moment it is provisioned and its
        // toolchain is in place.
        record_plan_key(&self.base_dir, &key)?;
        Ok(())
    }

    fn build_root(
        &self,
        component: &Component,
        build_deps: &[String],
        pool_repo: Option<Repository>,
        reporter: &mut dyn FnMut(Progress),
    ) -> Result<Box<dyn BuildRoot>> {
        // Staging an increment cannot be stopped once it starts, so a cancelled
        // run declines to start one rather than adding a layer it is about to
        // throw away.
        if self.cancel.requested() {
            return Err(Error::Cancelled);
        }
        let name = component.name.as_str();
        reporter(Progress::Provisioning {
            component: Some(name),
        });
        let upper = self.uppers_dir.join(name);
        // The upper is re-staged fresh every run rather than plan-cached like the
        // base: the build cage roots on `overlay_rootfs(base, upper)`, so the
        // build writes into this same upper, and reusing it would carry a prior
        // build's mutations into the next. Staging the clean delta each time
        // keeps every build hermetic; the delta download is content-addressed
        // cached, so re-staging re-extracts and re-configures without re-fetching.
        //
        // Clear any upper and overlay work directory a prior run left behind, so
        // neither merges into this overlay. `stage_layer` creates both the upper
        // and a sibling work directory, so cleanup removes both to stay
        // symmetric with what a run that never unwound leaves; a normal run's
        // `BuildLayer` already disposes both on drop, and removal is idempotent
        // on the absent paths.
        remove_provisioned(&upper)?;
        if let Some(work) = overlay_work_dir(&upper) {
            remove_provisioned(&work)?;
        }

        // The base already carries the toolchain, so the layer resolves only the
        // component's own build-deps as the delta; the extra repositories and the
        // pool feed earlier components' and backported `.debs` into that
        // resolution.
        //
        // No plan is pinned here, and none can be. `stage_layer` resolves its
        // own delta against the base's installed set and never consults a plan
        // the builder carries — a plan describes a full bootstrap, and the
        // combination is accepted at build time and then ignored, so setting one
        // would read as pinning while pinning nothing. The layer needs none
        // regardless: it resolves once, where a bootstrap resolved twice.
        let mut builder = self
            .config
            .debian_builder()?
            .base_layer(&self.base_dir)
            .include(build_deps.iter().cloned());
        if let Some(repository) = pool_repo {
            builder = builder.repository(repository);
        }

        // `stage_layer` is not an `ensure`, so it observes through the
        // Debian-specific sink rather than a `Provision` observer: the sink is
        // the only way to see an increment being resolved, downloaded, and
        // unpacked, which on the preferred strategy is every per-component root
        // there is.
        //
        // The sink is also where the layer's archive state comes from. A layer
        // resolves inside `stage_layer` and hands back no plan, so the resolved
        // event carrying one is the only view of it — and it is the view that
        // matters, since the pool is a repository the base never saw.
        let mut debian = builder.build().map_err(Error::Debian)?;
        let mut root = RootProgress::new(Some(name));
        let mut sink = |event: DebianEvent<'_>| {
            if let DebianEvent::Resolved { plan, .. } = &event {
                self.archives.observe(&plan.archives);
            }
            root.report(&event, reporter);
        };
        let layer = debian
            .observe(&mut sink)
            .stage_layer(&upper)
            .map_err(Error::from)?;

        Ok(Box::new(LayeredRoot {
            base: self.base_dir.clone(),
            layer,
        }))
    }
}

/// A build root that is an overlay of a shared read-only base and a disposable
/// per-component upper. Dropping it disposes the upper via the owned
/// [`BuildLayer`], reverting the increment and leaving the base pristine.
struct LayeredRoot {
    base: PathBuf,
    layer: BuildLayer,
}

impl BuildRoot for LayeredRoot {
    fn cage_builder(&self) -> CageBuilder {
        Cage::builder().overlay_rootfs(&self.base, self.layer.path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::Source;

    fn key(suite: &str, arch: &str, packages: &[(&str, &str, &str)]) -> String {
        format_plan_key(suite, arch, None, packages.iter().copied())
    }

    /// A component with no build-deps, for exercising a provider.
    fn component(name: &str) -> Component {
        Component {
            name: name.to_string(),
            source: Source {
                git: Some("https://example.invalid/repo.git".to_string()),
                ..Source::default()
            },
            ..Component::default()
        }
    }

    /// A configuration whose first archive is unusable: a signed repository
    /// whose keyring is not there. Building the provisioner fails on it before
    /// anything is fetched, which is what lets a provider's reporting be
    /// exercised without a network or a sandbox.
    fn unusable_config() -> ProvisionConfig {
        ProvisionConfig::new(
            "trixie",
            "amd64",
            None,
            None,
            vec![RecipeRepository {
                name: "backports".to_string(),
                suite: None,
                mirror: None,
                components: vec!["main".to_string()],
                trust_unsigned: false,
                keyring: Some(PathBuf::from("/nonexistent/keyring.gpg")),
            }],
            None,
        )
    }

    /// The events a provider reports, rendered enough to assert their order.
    fn record(events: &mut Vec<String>) -> impl FnMut(Progress) + '_ {
        move |event| match event {
            Progress::Provisioning { component } => {
                events.push(format!("provisioning {}", component.unwrap_or("<base>")));
            }
            _ => events.push("other".to_string()),
        }
    }

    #[test]
    fn a_leaked_overlay_work_tree_is_removable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("src2deb-leftovers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // What a run that never unwound leaves beside an upper: the overlay work
        // directory, whose `work` subdirectory the kernel leaves mode 0. Nothing
        // that merely walks the tree can descend it, so a removal that does not
        // restore traversal first fails and the component can never build again.
        let upper = dir.join("cosmic-comp");
        let work = overlay_work_dir(&upper).expect("an upper under a parent has a work sibling");
        std::fs::create_dir_all(upper.join("usr/bin")).unwrap();
        std::fs::write(upper.join("usr/bin/thing"), b"x").unwrap();
        std::fs::create_dir_all(work.join("work/incompat")).unwrap();
        std::fs::write(work.join("work/incompat/leftover"), b"x").unwrap();
        std::fs::write(work.join("index"), b"i").unwrap();
        for blocked in [work.join("work/incompat"), work.join("work")] {
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        }

        remove_provisioned(&upper).expect("the upper is removable");
        remove_provisioned(&work).expect("the mode-0 work tree is removable");
        assert!(!upper.exists());
        assert!(!work.exists());

        // Idempotent: the ordinary case is that neither is there at all.
        remove_provisioned(&upper).expect("removing an absent upper is fine");
        remove_provisioned(&work).expect("removing an absent work tree is fine");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restoring_traversal_leaves_files_and_symlinks_alone() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("src2deb-traversal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/file"), b"x").unwrap();
        std::fs::set_permissions(dir.join("sub/file"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        std::os::unix::fs::symlink("/nonexistent", dir.join("dangling")).unwrap();
        std::fs::set_permissions(dir.join("sub"), std::fs::Permissions::from_mode(0o000)).unwrap();

        restore_traversal(&dir);

        // The directory is traversable again...
        let mode = |path: &Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&dir.join("sub")) & 0o700, 0o700);
        // ...while the file's own mode is untouched, and a dangling symlink is
        // walked over rather than followed.
        assert_eq!(mode(&dir.join("sub/file")), 0o000);
        assert!(dir.join("dangling").symlink_metadata().is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_layered_provider_announces_a_root_before_it_provisions_it() {
        let mut events = Vec::new();
        let provider = LayeredProvision::new(
            unusable_config(),
            "/nonexistent/base",
            "/nonexistent/uppers",
            Cancel::new(),
        );
        // The provision fails, but only after the root has been announced: the
        // announcement has to lead, or a long provision reports nothing until
        // it is over.
        let outcome = provider.build_root(
            &component("cosmic-comp"),
            &[],
            None,
            &mut record(&mut events),
        );
        assert!(outcome.is_err());
        assert_eq!(events, ["provisioning cosmic-comp"]);
    }

    #[test]
    fn a_full_reprovision_provider_announces_a_root_before_it_provisions_it() {
        let mut events = Vec::new();
        let provider = FullReprovision::new(unusable_config(), "/nonexistent/roots", Cancel::new());
        let outcome = provider.build_root(
            &component("cosmic-comp"),
            &[],
            None,
            &mut record(&mut events),
        );
        assert!(outcome.is_err());
        assert_eq!(events, ["provisioning cosmic-comp"]);
    }

    #[test]
    fn a_layered_provider_declines_to_stage_a_layer_once_cancelled() {
        let cancel = Cancel::new();
        cancel.request();
        let provider = LayeredProvision::new(
            unusable_config(),
            "/nonexistent/base",
            "/nonexistent/uppers",
            cancel,
        );
        // Staging cannot be stopped once it starts, so a cancelled run does not
        // start one — and says so as a cancellation, not a provisioning
        // failure, even though the configuration would also have failed.
        let mut events = Vec::new();
        let outcome = provider.build_root(
            &component("cosmic-comp"),
            &[],
            None,
            &mut record(&mut events),
        );
        assert!(matches!(outcome, Err(Error::Cancelled)));
        assert!(events.is_empty());
    }

    #[test]
    fn the_bootstrap_observer_answers_the_runs_cancellation_signal() {
        let cancel = Cancel::new();
        let mut reporter = |_: Progress| {};
        let mut observer = Bootstrap::new(Some("cosmic-comp"), &cancel, &mut reporter);
        assert!(!observer.cancelled());
        cancel.request();
        // The provisioner consults this at each package boundary, so a run
        // cancelled mid-bootstrap stops within one package.
        assert!(observer.cancelled());
    }

    #[test]
    fn a_cancelled_provision_is_not_a_provisioning_failure() {
        // The provisioner reports a stopped run as `Cancelled`; src2deb has to
        // keep it distinct, or a cancelled run reads as a broken one.
        let cancelled: Error = ferroday_cage::provision::ProvisionError::Cancelled.into();
        assert!(matches!(cancelled, Error::Cancelled));
        let failed: Error = ferroday_cage::provision::ProvisionError::io(
            "provisioning",
            "/root",
            std::io::Error::other("boom"),
        )
        .into();
        assert!(matches!(failed, Error::Provision(_)));
    }

    #[test]
    fn plan_key_is_stable_and_sensitive_to_each_field() {
        let base = key(
            "trixie",
            "amd64",
            &[("libc6", "2.41-1", "aa"), ("zlib1g", "1.3-1", "bb")],
        );
        // The same resolved set yields the same key: a reused root is recognized.
        assert_eq!(
            base,
            key(
                "trixie",
                "amd64",
                &[("libc6", "2.41-1", "aa"), ("zlib1g", "1.3-1", "bb")]
            )
        );
        // A new version of a package changes the key, so a bumped build-dep
        // rebuilds rather than reusing a stale root.
        assert_ne!(
            base,
            key(
                "trixie",
                "amd64",
                &[("libc6", "2.42-1", "aa"), ("zlib1g", "1.3-1", "bb")]
            )
        );
        // A different archive digest at the same version changes the key.
        assert_ne!(
            base,
            key(
                "trixie",
                "amd64",
                &[("libc6", "2.41-1", "cc"), ("zlib1g", "1.3-1", "bb")]
            )
        );
        // An added or removed package changes the key.
        assert_ne!(base, key("trixie", "amd64", &[("libc6", "2.41-1", "aa")]));
        // The suite and architecture are part of the key.
        assert_ne!(
            base,
            key(
                "forky",
                "amd64",
                &[("libc6", "2.41-1", "aa"), ("zlib1g", "1.3-1", "bb")]
            )
        );
        assert_ne!(
            base,
            key(
                "trixie",
                "arm64",
                &[("libc6", "2.41-1", "aa"), ("zlib1g", "1.3-1", "bb")]
            )
        );
    }

    #[test]
    fn the_pinned_toolchain_is_part_of_the_plan_key() {
        // The toolchain is installed into the root as part of provisioning it,
        // so the key has to describe it too. Otherwise a recipe that repinned
        // its toolchain would reuse a root carrying the version it replaced —
        // and under the layered strategy that root is the shared base, so every
        // component would build against the wrong compiler.
        let packages = [("libc6", "2.41-1", "aa")];
        let archive = format_plan_key("trixie", "amd64", None, packages.iter().copied());
        let pinned = format_plan_key("trixie", "amd64", Some("1.97.0"), packages.iter().copied());
        let repinned = format_plan_key("trixie", "amd64", Some("1.98.0"), packages.iter().copied());

        assert_ne!(archive, pinned);
        assert_ne!(pinned, repinned);
        // The same pin over the same archive state still reuses the root.
        assert_eq!(
            pinned,
            format_plan_key("trixie", "amd64", Some("1.97.0"), packages.iter().copied()),
        );
    }

    /// An archive state as a resolve reports one, varied by `digest`.
    ///
    /// Built through the plan document rather than by construction:
    /// `ResolvedArchive` is `#[non_exhaustive]`, so a consumer cannot make one,
    /// and parsing is how a caller outside the crate comes by a value at all.
    fn archive(mirror: &str, digest: &str) -> ResolvedArchive {
        let document = format!(
            "Format: ferroday-cage-plan 1\nSuite: trixie\nArchitecture: amd64\n\n\
             Archive: 0\nMirror: {mirror}\nSuite: trixie\nComponents: main\n\
             Release-SHA256: {digest}\nSigned-By: \n"
        );
        Plan::parse_document(&document)
            .expect("a well-formed plan document")
            .archives
            .remove(0)
    }

    #[test]
    fn an_archive_state_is_recorded_once_however_many_roots_observed_it() {
        // Every root a run provisions resolves against the same archives, so a
        // 26-component run observes each of them 27 times. The record holds one
        // entry, not 27.
        let log = ArchiveLog::default();
        let debian = archive("http://deb.debian.org/debian", "aaaa");
        for _ in 0..27 {
            log.observe(std::slice::from_ref(&debian));
        }
        assert_eq!(log.resolved(), [debian]);
    }

    #[test]
    fn an_archive_that_published_mid_run_is_recorded_as_the_two_states_it_was() {
        // The interesting case, and the reason the states are compared rather
        // than assumed identical: the archive moved while the run was building
        // against it, so some roots hold packages selected from one state and
        // some from another.
        let log = ArchiveLog::default();
        let before = archive("http://deb.debian.org/debian", "aaaa");
        let after = archive("http://deb.debian.org/debian", "bbbb");
        log.observe(std::slice::from_ref(&before));
        log.observe(std::slice::from_ref(&after));
        assert_eq!(log.resolved(), [before, after]);
    }

    #[test]
    fn the_recorded_order_does_not_depend_on_which_root_resolved_first() {
        // A parallel run's workers observe archives in whichever order they
        // finish. A provenance record must not vary with that, so the states are
        // ordered by what they are rather than by when they were seen.
        let pool = archive("file:///work/pool/trixie/amd64", "cccc");
        let debian = archive("http://deb.debian.org/debian", "aaaa");

        let one_way = ArchiveLog::default();
        one_way.observe(&[pool.clone(), debian.clone()]);
        let other_way = ArchiveLog::default();
        other_way.observe(&[debian.clone(), pool.clone()]);

        assert_eq!(one_way.resolved(), other_way.resolved());
        // `file://` before `http://`, which is the mirror ordering.
        assert_eq!(one_way.resolved(), [pool, debian]);
    }

    #[test]
    fn the_plan_sidecar_sits_beside_the_root() {
        assert_eq!(
            plan_sidecar(Path::new("/work/roots/cosmic-comp")),
            Path::new("/work/roots/cosmic-comp.plan"),
        );
    }

    #[test]
    fn the_overlay_work_dir_is_a_hidden_sibling_of_the_upper() {
        // Matches ferroday-cage's `.<upper>.work` layout, so an interrupted run's
        // leaked work directory is cleaned up alongside the upper.
        assert_eq!(
            overlay_work_dir(Path::new("/work/uppers/cosmic-comp")),
            Some(PathBuf::from("/work/uppers/.cosmic-comp.work")),
        );
    }
}
