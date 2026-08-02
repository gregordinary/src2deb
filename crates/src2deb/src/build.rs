//! Vendoring and building a component in two cage passes.
//!
//! Debian packages that vendor Rust crates (as COSMIC's do) vendor *outside* a
//! build chroot and build *offline* inside it. src2deb mirrors that in two
//! passes against the same build root:
//!
//! 1. [`vendor`](Builder::vendor) runs `debian/rules clean` in a cage **with**
//!    the host network, binding the source tree read-write. That triggers the
//!    component's own vendoring idiom (which Pop's rules gate behind
//!    `ischroot ||`), leaving a `vendor.tar` in the tree.
//! 2. [`build`](Builder::build) copies the now-vendored tree to a writable
//!    directory inside a cage with an **isolated** network, stamps the build's
//!    version into that copy's `debian/changelog` (see [`crate::version`]), and
//!    runs `dpkg-buildpackage -nc` — the `-nc` skips the pre-build clean, so
//!    vendoring is not re-triggered offline — then collects the artifacts to
//!    the read-write `/out` bind.
//!
//! The host's checkout gains a `vendor.tar` in pass 1 but is otherwise the
//! working copy src2deb owns; only the finished packages leave the cage. The
//! version stamp lands on the cage's copy rather than the checkout, so the
//! resolved tree keeps upstream's changelog and a rebuild starts from the same
//! base version rather than compounding suffixes.
//!
//! Either pass can be cancelled while it runs: the wait polls the run's
//! [`Cancel`] signal, and a cancelled pass is asked to stop, then killed if it
//! does not.
//!
//! # Trust boundary
//!
//! Pass 1 executes arbitrary upstream code (`debian/rules clean`) with
//! [`Network::Host`], so the vendoring step can fetch its crates. Its filesystem
//! is sandboxed to the source tree, but its network is the host's: the vendor
//! pass is where the build trusts upstream. Pass 2, which produces the packages,
//! runs with [`Network::Isolated`]. The build is hermetic; acquiring the
//! dependencies to build it is not.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferroday_cage::{Cage, CageBuilder, ExitStatus, Network, ResolvedInputs, Running};

use crate::cancel::Cancel;
use crate::engine::Progress;
use crate::error::{Error, Result, io_error};
use crate::observer::LineObserver;
use crate::provision::BuildRoot;

/// Where the source tree is bound inside the cage.
const SOURCE_DEST: &str = "/src";
/// Where the read-write output directory is bound inside the cage.
const OUTPUT_DEST: &str = "/out";

/// The directory within the work directory that holds every output tree.
pub const OUT_DIR: &str = "out";

/// The path of the output tree for `suite` and `architecture` under `work_dir`:
/// `out/<suite>/<architecture>/`, under which each component gets a directory of
/// its own.
///
/// Scoped to the same identity as the pool ([`pool_dir`](crate::pool::pool_dir))
/// and the manifest ([`manifest_path`](crate::manifest::manifest_path)), and for
/// the same reason: an `Architecture: all` package's file name carries no
/// architecture and its stamped version is identical across architectures, so
/// the same component built for two of them into one work directory would
/// otherwise write one file name twice and leave only the last build's artifacts
/// behind. The suite joins it so an output tree and the pool it feeds are
/// reachable by one identity.
pub fn output_dir(work_dir: &Path, suite: &str, architecture: &str) -> PathBuf {
    work_dir.join(OUT_DIR).join(suite).join(architecture)
}

/// The environment variable naming the upstream commit a component is built
/// from.
///
/// Pop's packaging expects this from whatever drives the build: its vendor
/// recipes stamp the commit into `.cargo/config.toml` so the binary can report
/// the revision it came from. `just` runs recipe lines under `sh -cu`, so a
/// component that reads it unguarded fails outright when it is unset rather
/// than merely building without the stamp.
///
/// It is a commit hash and nothing else, so it is set only for a source that
/// has one. Packaging that reads it is packaging written against a git
/// checkout, and handing it a value of some other shape would be worse than
/// handing it nothing.
const SOURCE_GIT_HASH: &str = "SOURCE_GIT_HASH";

/// How often a running pass checks whether the run has been cancelled.
///
/// The wait pumps the cage's output while it polls, so this is the resolution
/// of a cancel, not of the output stream.
const CANCEL_POLL: Duration = Duration::from_millis(200);

/// How long a cancelled pass is given to exit on `SIGTERM` before it is killed.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// The vendor script body (pass 1): trigger the component's own vendoring.
///
/// `debian/rules clean` runs each component's `cargo vendor` / `just vendor` /
/// `make vendor` idiom, which Pop's rules gate behind `ischroot ||` so it fires
/// outside a build chroot. In a network cage this leaves a `vendor.tar` (and a
/// `.cargo/config`) in the source tree for the offline build.
const VENDOR_BODY: &str = r#"cd /src
chmod +x debian/rules 2>/dev/null || true
./debian/rules clean
"#;

/// The build script head (pass 2): copy the vendored source to a writable tree.
///
/// The copy is what makes stamping the changelog safe — the source bind is
/// read-only, and the tree edited here is the cage's own.
const BUILD_HEAD: &str = r#"rm -rf /build
mkdir -p /build
cp -a /src /build/tree
cd /build/tree
"#;

/// The build script tail (pass 2): build binary packages offline and copy the
/// artifacts to the output bind.
///
/// `-nc` skips the pre-build clean, so the vendoring step is not re-triggered
/// inside the isolated cage; the build consumes the `vendor.tar` from pass 1.
/// `binaries` selects which binary packages are built; see [`Binaries`].
///
/// The `.changes` and `.buildinfo` travel with the packages: the first is the
/// authoritative list of what the build produced, and the second is what it was
/// built against. See [`BuildInfo`].
fn build_tail(binaries: Binaries) -> String {
    format!(
        "dpkg-buildpackage -us -uc {} -nc\n\
         find /build -maxdepth 1 -type f \\( -name '*.deb' -o -name '*.ddeb' -o \
         -name '*.changes' -o -name '*.buildinfo' \\) -exec cp -p {{}} /out/ \\;\n",
        binaries.flag(),
    )
}

/// Which of a component's binary packages a build produces.
///
/// An `Architecture: all` package's file name carries no architecture and its
/// stamped version does not vary with one, so a recipe built for two
/// architectures produces it twice — the same name and version over different
/// bytes. Which architecture makes it is the recipe's to settle; see
/// [`Recipe::owns_arch_indep`](crate::Recipe::owns_arch_indep).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binaries {
    /// Every binary package the component declares, architecture-dependent and
    /// `Architecture: all` alike.
    All,
    /// Only the architecture-dependent packages, leaving `Architecture: all`
    /// output to the architecture that owns it.
    ArchitectureDependent,
}

impl Binaries {
    /// The `dpkg-buildpackage` build-type flag that selects this set.
    fn flag(self) -> &'static str {
        match self {
            Binaries::All => "-b",
            Binaries::ArchitectureDependent => "-B",
        }
    }
}

/// The heredoc delimiter the stamp step writes the changelog entry with.
///
/// Quoted at the point of use so the shell performs no expansion on the entry:
/// it is upstream text (a source package name and a maintainer identity) and
/// must reach the file exactly as [`crate::version`] rendered it.
const ENTRY_DELIMITER: &str = "SRC2DEB_CHANGELOG_ENTRY";

/// The script step that prepends `entry` to the build tree's
/// `debian/changelog`, so `dpkg-buildpackage` reads the stamped version.
///
/// Writing the entry first and appending the original avoids needing an
/// in-place editor in the build root: the step uses only the shell and `cat`.
///
/// An entry that does not already end in a newline is given one. A heredoc
/// terminator must stand alone on its line, so without it the delimiter would
/// land on the entry's last line and the heredoc would run to the end of the
/// script: the append and the move would be consumed as content, the changelog
/// would be left exactly as upstream wrote it, and the script would still exit
/// zero. The build would then publish an unstamped package with nothing
/// anywhere reporting it — the one failure the version stamp exists to prevent,
/// arrived at silently. The newline is supplied here rather than demanded of
/// the caller because [`Target::changelog_entry`] is public.
fn stamp_step(entry: &str) -> String {
    let terminate = if entry.ends_with('\n') { "" } else { "\n" };
    format!(
        "cat > debian/changelog.src2deb <<'{ENTRY_DELIMITER}'\n\
         {entry}{terminate}{ENTRY_DELIMITER}\n\
         cat debian/changelog >> debian/changelog.src2deb\n\
         mv debian/changelog.src2deb debian/changelog\n"
    )
}

/// The component a pass operates on.
///
/// The values travel together because a pass needs them together, and two are
/// not merely labels: the commit reaches the build as `SOURCE_GIT_HASH`, so the
/// packages record the revision they came from, and the changelog entry sets
/// the version they are built as.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    /// The component's name, used to label the pass's output.
    pub component: &'a str,
    /// The host path of the resolved source tree.
    pub tree: &'a Path,
    /// The commit the tree is checked out at, for a source that is a git
    /// repository, or `None` for a source that has no commit.
    ///
    /// Passed to both passes as `SOURCE_GIT_HASH`. A target with none sets no
    /// such variable, rather than one holding a value that is not a commit.
    pub commit: Option<&'a str>,
    /// The `debian/changelog` entry that stamps this build's version, prepended
    /// to the build's own copy of the tree, or `None` to build the version
    /// upstream's changelog already declares.
    ///
    /// Rendered by [`crate::version`] on the host, where the run's date and the
    /// component's existing changelog are both available, so the cage is handed
    /// finished text rather than the means to compose it. Any well-formed entry
    /// does; a missing trailing newline is supplied when the entry is written.
    pub changelog_entry: Option<&'a str>,
    /// Which of the component's binary packages the build produces. Only the
    /// build pass reads this; the vendor pass builds nothing.
    pub binaries: Binaries,
}

/// What a build pass produced: its artifacts, the `.buildinfo` recording what
/// they were built from, and the inputs the sandbox applied while producing
/// them.
///
/// The inputs come from the cage that actually ran, not from a second
/// description of it, so they cannot drift from what the build saw — which is
/// the point of recording them at all.
#[derive(Debug)]
pub struct BuildOutcome {
    /// The artifacts written to the output directory.
    pub artifacts: Vec<Artifact>,
    /// The `.buildinfo` the build wrote, when it wrote one.
    pub buildinfo: Option<BuildInfo>,
    /// The environment and mounts the build cage applied.
    pub inputs: ResolvedInputs,
}

/// The `.buildinfo` a build wrote, and its checksum.
///
/// `dpkg-buildpackage` writes one per build, recording the exact package set
/// installed in the build root, the build environment, and the checksums of the
/// binaries produced. It is Debian's own artefact for the question the
/// provenance manifest otherwise answers in src2deb's own vocabulary — what a
/// package was built against — and it is the file a rebuild is compared with.
///
/// src2deb records it and carries it alongside the packages. It does not read
/// it: what it holds is dpkg's to define.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    /// The path to the `.buildinfo` on the host, under the output directory.
    pub path: PathBuf,
    /// Its SHA-256, in lowercase hexadecimal, measured from the file as the
    /// build left it.
    pub sha256: String,
}

/// A built package artifact.
#[derive(Debug, Clone)]
pub struct Artifact {
    /// The binary package name.
    pub package: String,
    /// The package version, taken from the file name between the first two
    /// underscores (`name_version_arch.deb`), or empty when the name does not
    /// follow that convention.
    pub version: String,
    /// The path to the `.deb` on the host, under the output directory.
    pub path: PathBuf,
}

/// Runs component builds in cages.
#[derive(Debug, Clone)]
pub struct Builder {
    /// The pinned rustup toolchain to install and prefer on `PATH`, or `None`
    /// to build with the archive's own Rust.
    rustup_version: Option<String>,
    /// The run's cancellation signal, consulted while a pass runs.
    cancel: Cancel,
}

impl Builder {
    /// Creates a builder. `rustup_version` installs and prefers a pinned rustup
    /// toolchain over the archive's Rust; `None` uses the archive's own.
    /// `cancel` stops a pass in flight; [`Cancel::default`] never does.
    pub fn new(rustup_version: Option<String>, cancel: Cancel) -> Builder {
        Builder {
            rustup_version,
            cancel,
        }
    }

    /// The shell prelude that prefers the configured toolchain over the
    /// archive's Rust, for both passes. Empty for the archive toolchain.
    ///
    /// Naming only, never installing: the toolchain is put into the build root
    /// while the root is provisioned (see [`crate::toolchain`]), so both passes
    /// find it already there. Installing it from a pass instead would mean
    /// fetching it once per component per run under the layered strategy, where
    /// a pass writes into a per-component overlay upper that is discarded when
    /// the component finishes.
    fn toolchain_prelude(&self) -> String {
        match &self.rustup_version {
            Some(_) => crate::toolchain::prelude(),
            None => String::new(),
        }
    }

    /// Pass 1: vendor `target`'s crates in a cage rooted at `root`, with the
    /// host network, so the offline build in [`build`](Self::build) needs none.
    ///
    /// Binds the source tree read-write and runs the vendor script; the
    /// resulting `vendor.tar` persists in the host's source tree. Each line of
    /// the cage's output is streamed to `reporter` as [`Progress::Output`].
    pub fn vendor(
        &self,
        root: &dyn BuildRoot,
        target: Target<'_>,
        reporter: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        let script = format!("{}{VENDOR_BODY}", self.toolchain_prelude());
        let cage = build_cage(root)
            .bind(target.tree, SOURCE_DEST)
            // An `Option` iterates once or not at all, so a source with no
            // commit sets no variable rather than an empty one.
            .envs(target.commit.map(|commit| (SOURCE_GIT_HASH, commit)))
            .network(Network::Host)
            .command("/bin/sh")
            .args(["-e", "-u", "-c", script.as_str()])
            .build()
            .map_err(Error::Cage)?;

        let status = run_streaming(&cage, target.component, &self.cancel, reporter)?;
        if !status.success() {
            return Err(Error::Vendor {
                component: target.component.to_string(),
                status,
            });
        }
        Ok(())
    }

    /// Pass 2: build `target`'s vendored source offline in a cage rooted at
    /// `root`, and return the artifacts written to `out_dir` along with the
    /// inputs the cage applied. Each line of the cage's output is streamed to
    /// `reporter` as [`Progress::Output`].
    ///
    /// This is the pass whose inputs a run records: it is the one that produces
    /// the packages, and the hermetic one. The vendor pass only fetches sources
    /// into the tree, and does so with the host network, so what it ran under
    /// says nothing about what the packages were built from.
    pub fn build(
        &self,
        root: &dyn BuildRoot,
        target: Target<'_>,
        out_dir: &Path,
        reporter: &mut dyn FnMut(Progress),
    ) -> Result<BuildOutcome> {
        // Start from an empty output directory. The work directory is persistent,
        // so a prior run's artifacts would otherwise accumulate here, and
        // `collect_artifacts` could then pick a stale `.changes` — publishing an
        // old version's `.debs`, or naming files a prune has since removed.
        if out_dir.exists() {
            std::fs::remove_dir_all(out_dir).map_err(|err| io_error("clearing", out_dir, err))?;
        }
        std::fs::create_dir_all(out_dir).map_err(|err| io_error("creating", out_dir, err))?;

        let stamp = target.changelog_entry.map(stamp_step).unwrap_or_default();
        let script = format!(
            "{}{BUILD_HEAD}{stamp}{}",
            self.toolchain_prelude(),
            build_tail(target.binaries),
        );
        let cage = build_cage(root)
            .bind_ro(target.tree, SOURCE_DEST)
            .bind(out_dir, OUTPUT_DEST)
            .envs(target.commit.map(|commit| (SOURCE_GIT_HASH, commit)))
            .network(Network::Isolated)
            .command("/bin/sh")
            .args(["-e", "-u", "-c", script.as_str()])
            .build()
            .map_err(Error::Cage)?;

        // Taken from the cage that runs, before it does, so the record is of
        // this build and not of a description of it.
        let inputs = cage.resolved_inputs();

        let status = run_streaming(&cage, target.component, &self.cancel, reporter)?;
        if !status.success() {
            return Err(Error::Build {
                component: target.component.to_string(),
                status,
            });
        }

        Ok(BuildOutcome {
            artifacts: collect_artifacts(out_dir)?,
            buildinfo: collect_buildinfo(out_dir)?,
            inputs,
        })
    }
}

/// A [`CageBuilder`] rooted at a provisioned build root, carrying the policy
/// both passes share.
///
/// The sandbox's lifetime is tied to this process, so a src2deb that is killed
/// outright — where nothing of src2deb's gets to run — does not leave an
/// in-cage `dpkg-buildpackage` behind. The tie is a pipe the launch handle
/// holds open, not a signal arrangement, so it costs nothing while the build
/// runs normally.
fn build_cage(root: &dyn BuildRoot) -> CageBuilder {
    root.cage_builder().stop_with_caller(true)
}

/// Runs a cage, streaming its output a line at a time to `reporter` as
/// [`Progress::Output`] events, and returns the command's exit status.
///
/// The cage is spawned and waited on in slices rather than run to completion in
/// one blocking call, so `cancel` is consulted while the build runs. The wait
/// pumps captured output between slices, so polling costs the output stream
/// nothing. A cancelled pass is stopped by [`stop`] and reported as
/// [`Error::Cancelled`], never as a failed build.
fn run_streaming(
    cage: &Cage,
    component: &str,
    cancel: &Cancel,
    reporter: &mut dyn FnMut(Progress),
) -> Result<ExitStatus> {
    if cancel.requested() {
        return Err(Error::Cancelled);
    }
    let mut observer = LineObserver::new(|stream, line| {
        reporter(Progress::Output {
            component,
            stream,
            line,
        });
    });
    // Scoped so the running sandbox — which borrows the observer for its whole
    // life — is done with it before the trailing partial lines are flushed.
    let status = {
        let mut running = cage.spawn_with(&mut observer).map_err(Error::Cage)?;
        wait_cancellable(&mut running, cancel)
    };
    observer.finish();
    status
}

/// Waits for a running pass, stopping it if the run is cancelled.
fn wait_cancellable(running: &mut Running<'_>, cancel: &Cancel) -> Result<ExitStatus> {
    loop {
        if cancel.requested() {
            stop(running)?;
            return Err(Error::Cancelled);
        }
        if let Some(status) = running.wait_timeout(CANCEL_POLL).map_err(Error::Cage)? {
            return Ok(status);
        }
    }
}

/// Stops a running pass: `SIGTERM`, a grace period, then `SIGKILL`.
///
/// The graceful signal first gives `dpkg-buildpackage` the chance to finish the
/// file it is writing; the kill bounds how long a cancel takes when it does
/// not. The sandbox holds a PID namespace, so killing its init takes every
/// process inside with it.
fn stop(running: &mut Running<'_>) -> Result<()> {
    running.terminate().map_err(Error::Cage)?;
    let deadline = Instant::now() + CANCEL_GRACE;
    if running
        .wait_deadline(deadline)
        .map_err(Error::Cage)?
        .is_none()
    {
        running.kill().map_err(Error::Cage)?;
        // Collect the outcome, so the sandbox is reaped here rather than left
        // for the process to clean up at exit.
        running.wait().map_err(Error::Cage)?;
    }
    Ok(())
}

/// Collects the built `.deb`s from the output directory.
///
/// Prefers the `.changes` file `dpkg-buildpackage` writes — the authoritative
/// list of this build's artifacts — and falls back to globbing `.deb`s when no
/// `.changes` is present.
fn collect_artifacts(out_dir: &Path) -> Result<Vec<Artifact>> {
    if let Some(changes) = find_changes(out_dir)? {
        let text =
            std::fs::read_to_string(&changes).map_err(|err| io_error("reading", &changes, err))?;
        let debs = changes_debs(&text);
        if !debs.is_empty() {
            return Ok(debs
                .into_iter()
                .map(|name| artifact(out_dir, &name))
                .collect());
        }
    }
    // Fallback: every .deb/.ddeb in the output directory.
    let mut artifacts = Vec::new();
    for entry in std::fs::read_dir(out_dir).map_err(|err| io_error("reading", out_dir, err))? {
        let entry = entry.map_err(|err| io_error("reading", out_dir, err))?;
        let path = entry.path();
        let is_deb = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == "deb" || ext == "ddeb");
        if is_deb && let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            artifacts.push(artifact(out_dir, name));
        }
    }
    Ok(artifacts)
}

/// Finds the first `.changes` file in the output directory.
fn find_changes(out_dir: &Path) -> Result<Option<PathBuf>> {
    first_with_extension(out_dir, "changes")
}

/// Collects the `.buildinfo` from the output directory, checksummed.
///
/// A build that wrote none is reported as `None` rather than as a failure: the
/// packages are what the build was for, and the record says what there was to
/// record. Every `dpkg-buildpackage` in a suite src2deb targets writes one, so
/// in practice this is present.
fn collect_buildinfo(out_dir: &Path) -> Result<Option<BuildInfo>> {
    let Some(path) = first_with_extension(out_dir, "buildinfo")? else {
        return Ok(None);
    };
    let sha256 = sha256_file(&path)?;
    Ok(Some(BuildInfo { path, sha256 }))
}

/// Finds the first file in `dir` with the given extension.
///
/// One build writes one `.changes` and one `.buildinfo`, and the output
/// directory is emptied before the build, so "first" is "the one".
fn first_with_extension(dir: &Path, extension: &str) -> Result<Option<PathBuf>> {
    for entry in std::fs::read_dir(dir).map_err(|err| io_error("reading", dir, err))? {
        let entry = entry.map_err(|err| io_error("reading", dir, err))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some(extension) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// The SHA-256 of a file, in lowercase hexadecimal.
///
/// Read in chunks rather than into one buffer, so the cost does not follow the
/// size of the file.
fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path).map_err(|err| io_error("opening", path, err))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| io_error("reading", path, err))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(crate::fingerprint::hex(&hasher.finalize()))
}

/// Extracts the `.deb`/`.ddeb` file names from a `.changes` file's `Files:`
/// section (the last field of each indented entry is the file name).
fn changes_debs(text: &str) -> Vec<String> {
    let mut debs = Vec::new();
    let mut in_files = false;
    for line in text.lines() {
        if line.starts_with("Files:") {
            in_files = true;
            continue;
        }
        if in_files {
            if line.starts_with([' ', '\t']) {
                if let Some(name) = line.split_whitespace().last()
                    && (name.ends_with(".deb") || name.ends_with(".ddeb"))
                {
                    debs.push(name.to_string());
                }
            } else {
                // A non-indented line ends the section.
                break;
            }
        }
    }
    debs
}

/// Builds an [`Artifact`] from an output-directory file name.
///
/// A Debian binary file name is `name_version_arch.deb`, so the package name is
/// the portion before the first `_` and the version is the portion between the
/// first two. A name that does not follow the convention keeps the whole string
/// as the package and leaves the version empty.
fn artifact(out_dir: &Path, file_name: &str) -> Artifact {
    let mut fields = file_name.splitn(3, '_');
    let package = fields.next().unwrap_or(file_name).to_string();
    let version = fields.next().unwrap_or_default().to_string();
    Artifact {
        package,
        version,
        path: out_dir.join(file_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A unique empty scratch directory for a filesystem test.
    fn scratch(label: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("src2deb-build-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const CHANGES: &str = "\
Format: 1.8
Source: foo
Files:
 d41d8cd 12 admin optional foo_1.0_amd64.deb
 e99a18c 34 admin optional foo-dbgsym_1.0_amd64.ddeb
 c3fcd3d 56 admin optional foo_1.0_amd64.buildinfo
Checksums-Sha256:
 abc 12 foo_1.0_amd64.deb
";

    const OLD_CHANGELOG: &str = "\
pkg (1.0-1) trixie; urgency=low

  * The entry upstream ships.

 -- Upstream <up@example.invalid>  Mon, 14 Jul 2026 09:00:00 +0000
";

    /// Runs a shell script fragment in `dir` and asserts it succeeded.
    fn run_in(dir: &Path, body: &str) {
        let script = format!("cd {}\n{body}", dir.display());
        let status = std::process::Command::new("/bin/sh")
            .args(["-e", "-u", "-c", script.as_str()])
            .status()
            .expect("running /bin/sh");
        assert!(status.success(), "script failed: {script}");
    }

    #[test]
    fn the_stamp_step_prepends_its_entry_above_the_existing_changelog() {
        let dir = scratch("stamp");
        std::fs::create_dir_all(dir.join("debian")).unwrap();
        std::fs::write(dir.join("debian/changelog"), OLD_CHANGELOG).unwrap();

        let entry = "\
pkg (1.0-1+deb13.20260731.abc1234) trixie; urgency=medium

  * Automated build from source: abc1234.

 -- Upstream <up@example.invalid>  Fri, 31 Jul 2026 00:00:00 +0000

";
        run_in(&dir, &stamp_step(entry));

        let text = std::fs::read_to_string(dir.join("debian/changelog")).unwrap();
        // The stamped entry leads, so dpkg-buildpackage reads the new version,
        // and upstream's history is kept below it rather than replaced.
        assert!(
            text.starts_with(entry),
            "stamped entry is not first: {text}"
        );
        assert!(text.ends_with(OLD_CHANGELOG), "history was lost: {text}");
        // The scratch file the step writes through does not survive it.
        assert!(!dir.join("debian/changelog.src2deb").exists());
    }

    #[test]
    fn the_stamp_step_does_not_expand_shell_syntax_in_the_entry() {
        // The entry carries upstream text — a source package name and a
        // maintainer identity — so it must reach the file verbatim. The
        // heredoc is quoted for exactly this reason; unquoted, the shell would
        // substitute here and write a changelog naming something else.
        let dir = scratch("stamp-metachars");
        std::fs::create_dir_all(dir.join("debian")).unwrap();
        std::fs::write(dir.join("debian/changelog"), OLD_CHANGELOG).unwrap();

        let entry = "pkg (1.0-1) trixie; urgency=medium\n\n \
                     * Built by $USER `id -un` ${HOME} \\$(hostname).\n\n \
                     -- A $Maintainer <m@example.invalid>  Fri, 31 Jul 2026 00:00:00 +0000\n\n";
        run_in(&dir, &stamp_step(entry));

        let text = std::fs::read_to_string(dir.join("debian/changelog")).unwrap();
        assert!(text.starts_with(entry), "the entry was rewritten: {text}");
    }

    #[test]
    fn an_entry_without_a_trailing_newline_still_stamps_the_changelog() {
        // Without the newline the heredoc terminator would share a line with the
        // entry, so the heredoc would swallow the rest of the script: the
        // changelog would keep upstream's version, the script would exit 0, and
        // an unstamped package would be published with nothing reporting it.
        let dir = scratch("stamp-no-newline");
        std::fs::create_dir_all(dir.join("debian")).unwrap();
        std::fs::write(dir.join("debian/changelog"), OLD_CHANGELOG).unwrap();

        let entry = "\
pkg (1.0-1+deb13.20260731.abc1234) trixie; urgency=medium

  * Automated build from source: abc1234.

 -- Upstream <up@example.invalid>  Fri, 31 Jul 2026 00:00:00 +0000";
        run_in(&dir, &stamp_step(entry));

        let text = std::fs::read_to_string(dir.join("debian/changelog")).unwrap();
        assert!(text.starts_with(entry), "the entry was not written: {text}");
        assert!(text.ends_with(OLD_CHANGELOG), "history was lost: {text}");
        assert!(!dir.join("debian/changelog.src2deb").exists());
    }

    #[test]
    fn a_target_without_a_stamp_leaves_the_changelog_alone() {
        // `stamp_step` is only reached for a target that carries an entry; the
        // empty string it defaults to must contribute nothing to the script.
        let target = Target {
            component: "pkg",
            tree: Path::new("/src"),
            commit: Some("abc1234"),
            changelog_entry: None,
            binaries: Binaries::All,
        };
        let stamp = target.changelog_entry.map(stamp_step).unwrap_or_default();
        assert!(stamp.is_empty());
        let script = format!("{BUILD_HEAD}{stamp}{}", build_tail(target.binaries));
        assert!(!script.contains("debian/changelog"));
        assert!(script.contains("dpkg-buildpackage"));
    }

    #[test]
    fn each_suite_and_architecture_gets_its_own_output_tree() {
        let work = Path::new("/w");
        let base = output_dir(work, "forky", "arm64");
        assert_eq!(base, Path::new("/w/out/forky/arm64"));
        // A component's artifacts hang off the run's tree, so two runs keep both
        // sets rather than overwriting one — which for two architectures is a
        // requirement, since an `Architecture: all` package's file name carries
        // no architecture and its stamped version does not vary with one.
        assert_ne!(
            base.join("cosmic-icons"),
            output_dir(work, "trixie", "arm64").join("cosmic-icons"),
        );
        assert_ne!(output_dir(work, "a", "b-c"), output_dir(work, "a-b", "c"));
    }

    #[test]
    fn changes_debs_takes_deb_names_from_the_files_section() {
        // The `.deb` and `.ddeb` are taken; the `.buildinfo` is not, and the
        // section ends at the non-indented `Checksums-Sha256:` line, so its
        // entries are never read.
        assert_eq!(
            changes_debs(CHANGES),
            ["foo_1.0_amd64.deb", "foo-dbgsym_1.0_amd64.ddeb"],
        );
    }

    #[test]
    fn changes_debs_is_empty_without_a_files_section() {
        assert!(changes_debs("Format: 1.8\nSource: foo\n").is_empty());
    }

    #[test]
    fn artifact_splits_the_package_name_and_version_from_the_file_name() {
        let dir = Path::new("/out");
        let a = artifact(dir, "libcosmic-dev_2.0-1_amd64.deb");
        assert_eq!(a.package, "libcosmic-dev");
        assert_eq!(a.version, "2.0-1");
        assert_eq!(
            artifact(dir, "foo_1.0_amd64.deb").path,
            Path::new("/out/foo_1.0_amd64.deb"),
        );
        // A name with no underscore is taken whole, with an empty version.
        let odd = artifact(dir, "oddname");
        assert_eq!(odd.package, "oddname");
        assert_eq!(odd.version, "");
    }

    #[test]
    fn collect_artifacts_prefers_the_changes_manifest() {
        let dir = scratch("changes");
        std::fs::write(dir.join("foo_1.0_amd64.changes"), CHANGES).unwrap();
        // Stray files the manifest does not list are ignored in favor of it.
        std::fs::write(dir.join("stale_0.9_amd64.deb"), b"").unwrap();

        let mut names: Vec<String> = collect_artifacts(&dir)
            .unwrap()
            .into_iter()
            .map(|artifact| artifact.package)
            .collect();
        names.sort();
        assert_eq!(names, ["foo", "foo-dbgsym"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_buildinfo_is_collected_with_its_checksum() {
        let dir = scratch("buildinfo");
        std::fs::write(dir.join("foo_1.0_amd64.changes"), CHANGES).unwrap();
        std::fs::write(dir.join("foo_1.0_amd64.deb"), b"").unwrap();
        std::fs::write(dir.join("foo_1.0_amd64.buildinfo"), b"abc").unwrap();

        let buildinfo = collect_buildinfo(&dir)
            .unwrap()
            .expect("the build wrote one");
        assert_eq!(buildinfo.path, dir.join("foo_1.0_amd64.buildinfo"));
        // Measured from the file rather than taken from the `.changes` that
        // also names it, so the record describes the bytes on disk.
        assert_eq!(
            buildinfo.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_build_that_wrote_no_buildinfo_records_none_rather_than_failing() {
        // The packages are what the build was for; the record says what there
        // was to record.
        let dir = scratch("no-buildinfo");
        std::fs::write(dir.join("foo_1.0_amd64.deb"), b"").unwrap();
        assert!(collect_buildinfo(&dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_larger_than_one_read_hashes_the_same_as_its_contents() {
        // The hash is taken in chunks, so a file past the buffer boundary must
        // still agree with the digest of the whole.
        let dir = scratch("chunked");
        let path = dir.join("big.buildinfo");
        std::fs::write(&path, vec![b'x'; 200_000]).unwrap();
        let digest = sha256_file(&path).unwrap();
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));

        // The same bytes written in one go hash identically, which is the
        // property chunking must not disturb.
        let same = dir.join("same.buildinfo");
        std::fs::write(&same, vec![b'x'; 200_000]).unwrap();
        assert_eq!(sha256_file(&same).unwrap(), digest);
        // ...and one byte more is a different file.
        let longer = dir.join("longer.buildinfo");
        std::fs::write(&longer, vec![b'x'; 200_001]).unwrap();
        assert_ne!(sha256_file(&longer).unwrap(), digest);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_build_type_flag_follows_which_binaries_the_run_produces() {
        // `-b` builds every binary package, `-B` only the architecture-dependent
        // ones. The whole of arch-indep ownership comes down to this one flag,
        // so the mapping is asserted rather than assumed.
        assert!(build_tail(Binaries::All).contains("dpkg-buildpackage -us -uc -b -nc"));
        assert!(
            build_tail(Binaries::ArchitectureDependent)
                .contains("dpkg-buildpackage -us -uc -B -nc")
        );
    }

    #[test]
    fn the_build_script_copies_the_buildinfo_out_alongside_the_packages() {
        // The manifest names a file the run has to actually keep, so the copy
        // step is what makes the reference resolve. `.changes` travels with it,
        // since it is what names the set.
        let tail = build_tail(Binaries::All);
        for suffix in ["*.deb", "*.ddeb", "*.changes", "*.buildinfo"] {
            assert!(
                tail.contains(&format!("-name '{suffix}'")),
                "the build tail must collect {suffix}: {tail}",
            );
        }
    }

    #[test]
    fn collect_artifacts_globs_debs_when_no_changes_is_present() {
        let dir = scratch("glob");
        std::fs::write(dir.join("foo_1.0_amd64.deb"), b"").unwrap();
        std::fs::write(dir.join("foo-dbgsym_1.0_amd64.ddeb"), b"").unwrap();
        std::fs::write(dir.join("notes.txt"), b"").unwrap();

        let mut names: Vec<String> = collect_artifacts(&dir)
            .unwrap()
            .into_iter()
            .map(|artifact| artifact.package)
            .collect();
        names.sort();
        assert_eq!(names, ["foo", "foo-dbgsym"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
