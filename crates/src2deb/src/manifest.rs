//! The per-run provenance manifest: what each component resolved to and what it
//! produced.
//!
//! A run writes a manifest under the work directory, mapping every component to
//! what its source resolved to and, for a component that built, the exact
//! package versions it produced. It ties a run's inputs (the source
//! fingerprints, and the sandbox the builds ran in) to its outputs (versioned
//! `.debs`), which is the basis of a reproducibility story: the same manifest
//! names the revisions to check out and the conditions they were built under.
//!
//! Each component's inputs are recorded by kind and value, and each says whether
//! it is pinned, so a build from a source that cannot be reproduced is not
//! mistaken for one that can. See [`crate::fingerprint`].
//!
//! The manifest also carries a run's state forward. A component built this run is
//! recorded fresh; one skipped because it was already built keeps its prior
//! `built` record, so a `--skip-published` run can tell a component whose source
//! is unchanged from one that needs rebuilding, and the manifest stays complete
//! across selective runs.
//!
//! A manifest belongs to one recipe built for one suite and architecture, and
//! [`manifest_path`] gives it a path of its own under the work directory. A work
//! directory is shared deliberately — that is how separate recipes publish into
//! one pool, and the pool composes their packages — so a single manifest file
//! would make each run destroy the last one's provenance and offer the wrong
//! records to the next `--skip-published` run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ferroday_cage::provision::debian::ResolvedArchive;
use ferroday_cage::{
    IdRange, Limit, Network, ResolvedHardening, ResolvedIdentity, ResolvedInputs, ResolvedMount,
    ResolvedRlimit, ResolvedRoot, Resource,
};
use serde::{Deserialize, Serialize};

use crate::error::{Result, io_error};
use crate::fingerprint::Fingerprint;
use crate::version::VersionStamp;

/// The directory within the work directory that holds every manifest.
pub const MANIFEST_DIR: &str = "manifests";

/// The path of the manifest for `recipe` built for `suite` and `architecture`
/// under `work_dir`: `manifests/<recipe>/<suite>/<architecture>.toml`.
///
/// The three fields are nested rather than joined into one file name so no two
/// identities can collide on a path — a flat `<recipe>-<suite>-<arch>` would let
/// a recipe whose name ends in a suite's name write over another's.
///
/// Each field is validated as a single benign path segment when the recipe
/// loads ([`Recipe::load`](crate::Recipe::load)), so none of them can climb out
/// of the work directory here.
pub fn manifest_path(work_dir: &Path, recipe: &str, suite: &str, architecture: &str) -> PathBuf {
    manifest_dir(work_dir, recipe, suite).join(format!("{architecture}.toml"))
}

/// The directory holding every architecture's manifest for `recipe` at `suite`:
/// `manifests/<recipe>/<suite>/`.
///
/// One file per architecture, so the directory listing is the record of which
/// architectures a work directory holds a build for. That is what an
/// [export](crate::export) enumerates, since a run builds for one architecture
/// at a time and an archive merges them.
pub fn manifest_dir(work_dir: &Path, recipe: &str, suite: &str) -> PathBuf {
    work_dir.join(MANIFEST_DIR).join(recipe).join(suite)
}

/// The path a manifest is staged at before being renamed over `path`.
///
/// A sibling of the manifest, so the rename never crosses a filesystem, and
/// named for the writing process, so two runs staging sibling manifests under
/// one work directory cannot overwrite each other's staged file.
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

/// The status of a component that built successfully.
pub const STATUS_BUILT: &str = "built";
/// The status of a component whose build failed.
pub const STATUS_FAILED: &str = "failed";
/// The status of a component not built this run (already published, or not
/// selected).
pub const STATUS_SKIPPED: &str = "skipped";

/// A run's provenance: the recipe it built, the sandbox its builds ran in, and
/// the state of each component.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// The recipe name.
    pub recipe: String,
    /// The Debian suite built for.
    pub suite: String,
    /// The architecture built for.
    pub architecture: String,
    /// The date the run's versions were stamped with, as `YYYY-MM-DD`, absent
    /// when the run built nothing.
    ///
    /// Carried forward like the sandbox record, and for the same reason: a run
    /// that builds nothing keeps the date of the one that produced the packages
    /// this manifest still calls built. Overwriting it with the date of a run
    /// that produced nothing would make a later reproduction rebuild against the
    /// wrong clock.
    #[serde(
        rename = "build-date",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub build_date: Option<String>,
    /// The sandbox inputs the run's builds ran under, absent when the run built
    /// nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxRecord>,
    /// The archive states the run's build roots resolved against.
    ///
    /// Carried forward like the sandbox record and for the same reason: a run
    /// that provisions nothing resolves nothing, and the archives the packages
    /// this manifest still calls built came from are the ones already recorded.
    #[serde(rename = "archive", default, skip_serializing_if = "Vec::is_empty")]
    pub archives: Vec<ArchiveRecord>,
    /// The `qemu-user` interpreter a foreign build ran through.
    ///
    /// Absent for a native build, which is the honest answer rather than a
    /// failure to look: nothing interpreted anything. Carried forward like the
    /// sandbox record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<InterpreterRecord>,
    /// Each component's record, in build order.
    #[serde(rename = "component", default)]
    pub components: Vec<ComponentRecord>,
}

/// One archive state a run's build roots resolved against.
///
/// The plan key a component records is a digest over names, versions, and
/// package digests. It says which packages a root holds and nothing about what
/// they were selected *from* — and the same suite resolves to different versions
/// a week apart, so a record naming only the selection cannot say what the
/// selection was made from. This is that: the mirror that actually served, the
/// digest of the release body that was verified, that release's own dates, and
/// the key that verified it.
///
/// # One entry per archive, usually
///
/// A run resolves once for the shared base and once per component's root, so it
/// observes each configured archive several times. The states are compared rather
/// than assumed identical, and only the distinct ones are recorded: one entry per
/// repository is the ordinary result, and two for one mirror and suite is a run
/// that saw the archive publish while it was building against it. Each carries
/// the release's own `Date`, which is what orders the two in time.
///
/// A projection of ferroday-cage's `ResolvedArchive` into the manifest's own
/// vocabulary, as the mount records are, so the manifest's schema stays
/// src2deb's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRecord {
    /// The mirror URL that served the release, which is where the packages were
    /// fetched from.
    ///
    /// The one that answered rather than the configured list: a repository with
    /// a fallback resolves against whichever mirror served, and recording the
    /// list would describe a choice rather than the choice made.
    pub mirror: String,
    /// The suite as the repository requested it, which for an additional
    /// repository need not be the run's own.
    pub suite: String,
    /// The components resolved from this archive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<String>,
    /// The SHA-256 of the release body that was verified, lowercase hex.
    ///
    /// For a signed archive this is the digest of the cleartext the signature
    /// covers, so it names the exact archive state that signature vouched for.
    #[serde(rename = "release-sha256")]
    pub release_sha256: String,
    /// The release's `Date` field, as written, when it carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// The release's `Valid-Until` field, as written, when it carried one.
    #[serde(
        rename = "valid-until",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub valid_until: Option<String>,
    /// The fingerprints of the key that verified the release, uppercase hex.
    ///
    /// Written empty rather than omitted for an archive trusted unsigned, where
    /// nothing was verified — the local pool is one. An empty list is a fact
    /// about the archive; an absent key could not be told from a record written
    /// before the field existed.
    #[serde(rename = "signed-by")]
    pub signed_by: Vec<String>,
}

/// The `qemu-user` interpreter a foreign build ran every target binary through.
///
/// A foreign build compiles nothing on the CPU directly: `rustc`, `cc`, `ld`,
/// and every configure probe execute under an emulator, and a changed emulator
/// silently changes compiled output. src2deb already records the architecture
/// and whether the build was foreign; this records what executed it.
///
/// The values come from the kernel's own `binfmt_misc` registration rather than
/// from a `PATH` lookup. That is both more faithful — it is the path binaries
/// actually execute through — and the only thing available, since the build
/// environment strips `PATH`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterpreterRecord {
    /// The qemu target name the handler is registered under: `aarch64` for the
    /// Debian architecture `arm64`, and so on.
    pub name: String,
    /// The interpreter exactly as the kernel recorded it.
    ///
    /// On the common Debian layout this is a wrapper —
    /// `/usr/libexec/qemu-binfmt/aarch64-binfmt-P` — that symlinks to the real
    /// binary. It is the provenance-faithful value and the one hashed; it is not
    /// a path that can be executed, since qemu refuses to run under its binfmt
    /// wrapper name.
    pub path: String,
    /// [`path`](Self::path) canonicalized, absent when it does not resolve.
    ///
    /// Recorded beside the registered path because the two are different facts:
    /// repointing the symlink changes the interpreter without changing the
    /// registration, which one path alone could not show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
    /// The SHA-256 of the interpreter's bytes, absent when it could not be read.
    ///
    /// Taken of [`path`](Self::path), which `open` follows the symlink along, so
    /// this is the real binary's digest and needs no canonicalization.
    ///
    /// **It carries a caveat.** The `F` flag means the kernel opened and holds
    /// the interpreter at *registration* time, so a digest taken during a build
    /// may be of a file that replaced the one actually running. A digest with a
    /// stated caveat is still worth more to a rebuild comparison than none —
    /// two runs whose digests differ definitely ran different interpreters —
    /// but two that agree agree only about the file on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Whether the handler was enabled.
    ///
    /// A registered handler that is switched off is recorded as present and not
    /// enabled rather than as absent: the registration exists, and the
    /// distinction is "turn it on" against "install it".
    pub enabled: bool,
    /// The registration's flag letters, as written — `POF` on a Debian host.
    ///
    /// A foreign bootstrap needs `F`, which is what keeps the interpreter
    /// working after the cage pivots into a rootfs where its own path no longer
    /// resolves.
    pub flags: String,
}

impl InterpreterRecord {
    /// Records the interpreter a build for `architecture` runs through, or
    /// `None` for a build that runs natively or a foreign one with no handler
    /// registered.
    ///
    /// A missing handler is not distinguished from a native build here because a
    /// build needing one and not having it never gets this far: ferroday-cage
    /// refuses the bootstrap before any download. See [`crate::arch`].
    pub fn of(architecture: &str) -> Option<InterpreterRecord> {
        let interpreter = ferroday_cage::provision::debian::foreign_interpreter(architecture)?;
        Some(InterpreterRecord {
            sha256: digest_of(&interpreter.path),
            name: interpreter.name,
            path: path(&interpreter.path),
            resolved: interpreter.resolved.as_deref().map(path),
            enabled: interpreter.enabled,
            flags: interpreter.flags,
        })
    }
}

/// The SHA-256 of the file at `path`, or `None` when it cannot be read.
///
/// An unreadable interpreter is a fact rather than an error: everything else
/// about the registration is still worth recording, and a run whose build
/// succeeded plainly did execute something.
fn digest_of(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).ok()?;
    Some(crate::fingerprint::hex(&hasher.finalize()))
}

impl ArchiveRecord {
    /// Records an archive as a resolve found it.
    pub fn of(archive: &ResolvedArchive) -> ArchiveRecord {
        ArchiveRecord {
            mirror: archive.mirror.clone(),
            suite: archive.suite.clone(),
            components: archive.components.clone(),
            release_sha256: archive.release_sha256.clone(),
            date: archive.date.clone(),
            valid_until: archive.valid_until.clone(),
            signed_by: archive.signed_by.clone(),
        }
    }
}

/// The sandbox inputs a run's builds ran under: what the build command was
/// rooted on, the identity it held, the network and limits it ran under, the
/// hardening applied to it, the environment it carried, and every mount its
/// sandbox established.
///
/// What a build produces depends on all of these, and none of them follows from
/// the source revisions alone. ferroday-cage's base environment and managed
/// mount profile are both outside its compatibility promise and may change
/// between releases, so a library version does not state them either. Recording
/// them says what a build actually ran under rather than leaving it to be
/// inferred.
///
/// # Why the record is run-level
///
/// Every component's build pass applies the same environment, the same mount
/// sequence, and the same posture, differing only in host paths: the source and
/// output binds name the component's own directories, and under the layered
/// strategy so does the overlay root's upper. So one component's record stands
/// for the run, and [`component`](Self::component) names which one it was taken
/// from — the earliest in build order the run built, so a `--jobs N` run records
/// what a sequential run would.
///
/// The root is the field that most tests that reasoning, and it survives it for
/// the same reason the binds do. What an overlay root says about a build is its
/// *shape* — that the command saw a merge of this read-only lower stack with a
/// writable layer — and the lower stack is the shared base every component
/// builds over. The upper is that component's scratch, named here as the source
/// bind is: one component's path, standing for a run in which each has its own.
///
/// Like a component's record, it is carried forward: a run that builds nothing
/// keeps the one already in the manifest, because the packages that manifest
/// still calls built were built under it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRecord {
    /// The component whose build pass the record was taken from — the earliest
    /// in build order that the run built, so a parallel run records what a
    /// sequential run would rather than whichever worker finished first.
    pub component: String,
    /// What the build command's root filesystem was.
    ///
    /// Its own field rather than a mount, as the sandbox library has it: the
    /// root is not something the profile lays over the sandbox, it is what the
    /// profile is laid over. Before it was recorded, a plain root and an overlay
    /// over the same base produced byte-identical records while describing two
    /// different builds.
    pub root: RootRecord,
    /// The identity the build command held inside the sandbox.
    ///
    /// Whether a build sees uid 0 changes what it produces: `Rules-Requires-Root`
    /// handling turns on exactly this, and a file's recorded ownership follows
    /// from it.
    pub identity: IdentityRecord,
    /// The network the build command could reach.
    ///
    /// The first thing a reproducibility claim is challenged on. src2deb's build
    /// pass runs isolated; its vendor pass, which this does not record, does not.
    pub network: String,
    /// The resource limits the build command ran under, in the order applied.
    ///
    /// Empty for a build that set none, which is every build src2deb runs today.
    /// A build that adapts its parallelism to `RLIMIT_NOFILE`, or that fails a
    /// link under `RLIMIT_AS`, produces different output.
    #[serde(rename = "rlimit", default, skip_serializing_if = "Vec::is_empty")]
    pub rlimits: Vec<RlimitRecord>,
    /// The hardening controls the build command ran under.
    ///
    /// Recorded as [`Unavailable`](HardeningRecord::Unavailable) when the
    /// sandbox library was built without the layer, rather than omitted: a
    /// record that left the key out could not be told from one written before
    /// the key existed.
    pub hardening: HardeningRecord,
    /// The build command's complete environment, by variable name.
    pub env: BTreeMap<String, String>,
    /// Every mount the sandbox established, in the order it established them:
    /// the managed profile first, then src2deb's own binds.
    #[serde(rename = "mount", default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountRecord>,
}

/// What a build's sandbox was rooted on.
///
/// A projection of ferroday-cage's [`ResolvedRoot`] into the manifest's own
/// vocabulary, as [`MountRecord`] is of its mounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RootRecord {
    /// No root swap: the command ran against the host's own filesystem.
    Host,
    /// A plain rootfs, pivoted into. This is full reprovisioning's per-component
    /// root, which its build writes into directly.
    Plain {
        /// The rootfs as an absolute, canonicalized host path.
        path: String,
    },
    /// An overlay assembled over a rootfs and pivoted into. This is the layered
    /// strategy: the shared base as the read-only lower, and the component's own
    /// build-dependency increment as the writable upper.
    Overlay {
        /// The lower layers, base first.
        lower: Vec<String>,
        /// The upper layer, where the sandbox's writes land. This component's
        /// own, as the source and output binds are.
        upper: String,
        /// The work directory the overlay requires beside the upper.
        work: String,
    },
    /// A root of a kind this version of src2deb does not recognize.
    ///
    /// [`ResolvedRoot`] is `#[non_exhaustive]`, so a later ferroday-cage may
    /// root a cage in terms src2deb has no field for. Recorded rather than
    /// passed over, for the reason [`MountRecord::Unknown`] is: the record says
    /// the build was rooted on something it cannot describe, which is a
    /// different statement from saying nothing.
    Unknown,
}

/// The identity a build's command held inside its sandbox.
///
/// A projection of ferroday-cage's [`ResolvedIdentity`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum IdentityRecord {
    /// No user namespace: the command ran as the calling user.
    Caller,
    /// The single-identity map: the calling user is root inside the sandbox and
    /// no other id is mapped. This is what src2deb provisions and builds under.
    Single,
    /// A range map, written from outside the namespace by a delegate.
    Ranged {
        /// The uid extents, in the order they were written.
        uid: Vec<IdRangeRecord>,
        /// The gid extents, in the order they were written.
        gid: Vec<IdRangeRecord>,
    },
    /// An identity of a kind this version of src2deb does not recognize; see
    /// [`RootRecord::Unknown`].
    Unknown,
}

/// One extent of a range identity map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdRangeRecord {
    /// The first id inside the sandbox.
    pub inside: u32,
    /// The first id on the host.
    pub outside: u32,
    /// How many consecutive ids the extent covers.
    pub count: u32,
}

/// One resource limit a build's sandbox applied.
///
/// Each limit is the finite amount, or `"unlimited"` for the kernel's
/// `RLIM_INFINITY` — the spelling ferroday-cage's own profile format uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RlimitRecord {
    /// The kernel resource the limit governs, in the kebab-case spelling
    /// ferroday-cage names it by: `address-space`, `open-files`, and so on.
    pub resource: String,
    /// The soft limit: what the kernel enforced.
    pub soft: String,
    /// The hard limit: the ceiling the command could raise its soft limit to.
    pub hard: String,
}

/// The hardening controls a build's sandbox applied before `execve`.
///
/// A projection of ferroday-cage's [`ResolvedHardening`]. The subtlest posture
/// to record and the one most worth recording: a seccomp policy changes which
/// syscalls succeed, and a configure test that probes one reads the refusal as
/// an absent feature, so two builds under two policies can differ with nothing
/// else to show for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum HardeningRecord {
    /// The hardening layer was not compiled into the sandbox library, so no
    /// Landlock ruleset, seccomp filter, or capability drop could apply. This is
    /// what src2deb builds under.
    ///
    /// Distinct from an [`Applied`](Self::Applied) posture whose controls are
    /// all empty, which is a build that could have hardened and did not.
    Unavailable,
    /// The hardening layer was compiled in; these are the controls in force.
    Applied {
        /// The Landlock filesystem grants, in the order declared: each a path
        /// and the `LANDLOCK_ACCESS_FS_*` rights allowed beneath it.
        #[serde(rename = "landlock-fs", default, skip_serializing_if = "Vec::is_empty")]
        landlock_fs: Vec<LandlockFsRecord>,
        /// The Landlock network grants, in the order declared.
        #[serde(
            rename = "landlock-net",
            default,
            skip_serializing_if = "Vec::is_empty"
        )]
        landlock_net: Vec<LandlockNetRecord>,
        /// The installed seccomp filter's length in BPF instructions, absent
        /// when no filter was installed.
        ///
        /// The program itself is not recorded: it is thousands of instructions
        /// and means nothing to a reader. The length distinguishes one policy
        /// from another at a glance.
        #[serde(
            rename = "seccomp-instructions",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        seccomp_instructions: Option<usize>,
        /// The capability bits retained across the drop, absent when the
        /// namespaced capability set was left untouched.
        #[serde(
            rename = "keep-capabilities",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        keep_capabilities: Option<u64>,
    },
    /// A hardening posture this version of src2deb does not recognize; see
    /// [`RootRecord::Unknown`].
    Unknown,
}

/// One Landlock filesystem grant a build's sandbox enrolled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandlockFsRecord {
    /// The granted path, as the command saw it after the root swap.
    pub path: String,
    /// The `LANDLOCK_ACCESS_FS_*` rights allowed beneath it, as the kernel
    /// spells them and before the command stage narrows them to its ABI.
    pub access: u64,
}

/// One Landlock network grant a build's sandbox enrolled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandlockNetRecord {
    /// The TCP port the grant governs.
    pub port: u16,
    /// The `LANDLOCK_ACCESS_NET_*` rights allowed on it.
    pub access: u64,
}

/// One mount a build's sandbox established.
///
/// A projection of ferroday-cage's [`ResolvedMount`] into the manifest's own
/// vocabulary, so the manifest's schema stays src2deb's to keep rather than
/// inheriting the sandbox library's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MountRecord {
    /// A tmpfs.
    Tmpfs {
        /// The mount point inside the sandbox.
        target: String,
        /// The kernel's raw `MS_*` flags.
        flags: u64,
        /// The filesystem data string.
        data: String,
    },
    /// A procfs instance.
    Procfs {
        /// The mount point inside the sandbox.
        target: String,
    },
    /// A devpts instance.
    Devpts {
        /// The mount point inside the sandbox.
        target: String,
        /// The filesystem data string.
        data: String,
    },
    /// A bind of a host path.
    Bind {
        /// The host path bound in.
        source: String,
        /// The mount point inside the sandbox.
        target: String,
        /// Whether the bind is remounted read-only.
        #[serde(rename = "read-only")]
        read_only: bool,
    },
    /// A mount whose parameters go to the kernel verbatim.
    Raw {
        /// The mount source, when one was given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        /// The mount point inside the sandbox.
        target: String,
        /// The filesystem type, when one was given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fstype: Option<String>,
        /// The kernel's raw `MS_*` flags.
        flags: u64,
        /// The filesystem data string, when one was given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
    /// A symlink established inside the rootfs, part of what the managed
    /// profile lays down.
    Symlink {
        /// The link's own path inside the sandbox.
        path: String,
        /// The path the link points at, as written.
        target: String,
    },
    /// A mount of a kind this version of src2deb does not recognize, recorded
    /// by where it was established.
    ///
    /// [`ResolvedMount`] is `#[non_exhaustive]`, so a later ferroday-cage may
    /// establish a mount described in terms src2deb has no field for. Recording
    /// the position keeps the sequence complete and honest — the record says
    /// something was mounted here rather than passing over it.
    Unknown {
        /// The mount point inside the sandbox.
        target: String,
    },
}

impl SandboxRecord {
    /// Records the inputs `component`'s build cage resolved to.
    pub fn of(component: impl Into<String>, inputs: &ResolvedInputs) -> SandboxRecord {
        SandboxRecord {
            component: component.into(),
            root: root_record(&inputs.root),
            identity: identity_record(&inputs.identity),
            network: network_name(inputs.network).to_string(),
            rlimits: inputs.rlimits.iter().map(rlimit_record).collect(),
            hardening: hardening_record(&inputs.hardening),
            env: inputs
                .env
                .iter()
                .map(|(name, value)| (text(name), text(value)))
                .collect(),
            mounts: inputs.mounts.iter().map(mount_record).collect(),
        }
    }
}

/// The manifest record for a resolved root.
fn root_record(root: &ResolvedRoot) -> RootRecord {
    match root {
        ResolvedRoot::Host => RootRecord::Host,
        ResolvedRoot::Plain { path: rootfs } => RootRecord::Plain { path: path(rootfs) },
        ResolvedRoot::Overlay { lower, upper, work } => RootRecord::Overlay {
            lower: lower.iter().map(|layer| path(layer)).collect(),
            upper: path(upper),
            work: path(work),
        },
        _ => RootRecord::Unknown,
    }
}

/// The manifest record for a resolved identity.
fn identity_record(identity: &ResolvedIdentity) -> IdentityRecord {
    match identity {
        ResolvedIdentity::Caller => IdentityRecord::Caller,
        ResolvedIdentity::Single => IdentityRecord::Single,
        ResolvedIdentity::Ranged { uid, gid } => IdentityRecord::Ranged {
            uid: uid.iter().map(id_range_record).collect(),
            gid: gid.iter().map(id_range_record).collect(),
        },
        _ => IdentityRecord::Unknown,
    }
}

/// The manifest record for one extent of a range identity map.
fn id_range_record(range: &IdRange) -> IdRangeRecord {
    IdRangeRecord {
        inside: range.inside,
        outside: range.outside,
        count: range.count,
    }
}

/// The manifest's name for a network posture.
///
/// `unknown` for a posture added to ferroday-cage after this was written, for
/// the reason [`RootRecord::Unknown`] exists: a record that named nothing could
/// not be told from one written before the field existed.
fn network_name(network: Network) -> &'static str {
    match network {
        Network::Isolated => "isolated",
        Network::Host => "host",
        Network::None => "none",
        _ => "unknown",
    }
}

/// The manifest record for one resource limit.
fn rlimit_record(rlimit: &ResolvedRlimit) -> RlimitRecord {
    RlimitRecord {
        resource: resource_name(rlimit.resource).to_string(),
        soft: limit_amount(rlimit.soft),
        hard: limit_amount(rlimit.hard),
    }
}

/// The manifest's name for a kernel resource, in the kebab-case spelling
/// ferroday-cage's own profile format and command line use.
fn resource_name(resource: Resource) -> &'static str {
    match resource {
        Resource::AddressSpace => "address-space",
        Resource::CoreDump => "core-dump",
        Resource::CpuTime => "cpu-time",
        Resource::Data => "data",
        Resource::FileSize => "file-size",
        Resource::LockedMemory => "locked-memory",
        Resource::OpenFiles => "open-files",
        Resource::PendingSignals => "pending-signals",
        Resource::Processes => "processes",
        Resource::Stack => "stack",
        _ => "unknown",
    }
}

/// A limit as the manifest writes it: the finite amount, or `unlimited` for the
/// kernel's `RLIM_INFINITY`, which is the spelling ferroday-cage uses.
fn limit_amount(limit: Limit) -> String {
    match limit.amount() {
        Some(amount) => amount.to_string(),
        None => "unlimited".to_string(),
    }
}

/// The manifest record for a resolved hardening posture.
fn hardening_record(hardening: &ResolvedHardening) -> HardeningRecord {
    match hardening {
        ResolvedHardening::Unavailable => HardeningRecord::Unavailable,
        ResolvedHardening::Applied {
            landlock_fs,
            landlock_net,
            seccomp_instructions,
            keep_capabilities,
        } => HardeningRecord::Applied {
            landlock_fs: landlock_fs
                .iter()
                .map(|grant| LandlockFsRecord {
                    path: path(&grant.path),
                    access: grant.access,
                })
                .collect(),
            landlock_net: landlock_net
                .iter()
                .map(|grant| LandlockNetRecord {
                    port: grant.port,
                    access: grant.access,
                })
                .collect(),
            seccomp_instructions: *seccomp_instructions,
            keep_capabilities: *keep_capabilities,
        },
        _ => HardeningRecord::Unknown,
    }
}

/// The manifest record for one resolved mount.
fn mount_record(mount: &ResolvedMount) -> MountRecord {
    match mount {
        ResolvedMount::Tmpfs {
            target,
            flags,
            data,
        } => MountRecord::Tmpfs {
            target: path(target),
            flags: *flags,
            data: data.clone(),
        },
        ResolvedMount::Procfs { target } => MountRecord::Procfs {
            target: path(target),
        },
        ResolvedMount::Devpts { target, data } => MountRecord::Devpts {
            target: path(target),
            data: data.clone(),
        },
        ResolvedMount::Bind {
            source,
            target,
            read_only,
        } => MountRecord::Bind {
            source: path(source),
            target: path(target),
            read_only: *read_only,
        },
        ResolvedMount::Raw {
            source,
            target,
            fstype,
            flags,
            data,
        } => MountRecord::Raw {
            source: source.as_deref().map(path),
            target: path(target),
            fstype: fstype.clone(),
            flags: *flags,
            data: data.clone(),
        },
        ResolvedMount::Symlink { path: link, target } => MountRecord::Symlink {
            path: path(link),
            target: path(target),
        },
        // A mount kind added to ferroday-cage after this was written. Its
        // position is still worth recording; see `MountRecord::Unknown`.
        other => MountRecord::Unknown {
            target: path(other.get_target()),
        },
    }
}

/// A path as manifest text. TOML is UTF-8, so a path that is not is recorded
/// lossily rather than dropped — every path src2deb binds is one it composed
/// from the work directory.
fn path(value: &Path) -> String {
    value.to_string_lossy().into_owned()
}

/// An environment name or value as manifest text, lossily as [`path`].
fn text(value: &std::ffi::OsStr) -> String {
    value.to_string_lossy().into_owned()
}

/// One component's record: what its source resolved to, its status, and either
/// the packages it produced or the reason it failed.
///
/// Field order is the serialized order, and TOML admits no scalar after a table:
/// every plain field is declared before [`source`](Self::source) and
/// [`packages`](Self::packages), which are written as arrays of tables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentRecord {
    /// The component name.
    pub name: String,
    /// The status: [`STATUS_BUILT`], [`STATUS_FAILED`], or [`STATUS_SKIPPED`].
    pub status: String,
    /// The failure reason, present only when the component failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The upstream version the recipe declared for the component, present only
    /// when it declares one — that is, when its packaging carries no
    /// `debian/changelog` of its own and src2deb wrote one.
    ///
    /// Recorded because it is the one thing a run can change that produces
    /// different packages while leaving every input the fingerprint names
    /// exactly where it was. Without it, editing `version` and re-running with
    /// `--skip-published` would skip the component and never publish the version
    /// that was asked for. See [`is_built_at`](Self::is_built_at).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// How the component's version was stamped, recorded only when it was not
    /// the default.
    ///
    /// The second thing a run can change that produces different packages while
    /// leaving every input the fingerprint names exactly where it was, and it is
    /// recorded for the same reason the declared
    /// [`version`](Self::version) is. Omitted when it is
    /// [`VersionStamp::Supersede`], so a record written before the setting
    /// existed reads back as what it in fact was and does not provoke a rebuild.
    /// See [`is_built_at`](Self::is_built_at).
    #[serde(default, skip_serializing_if = "is_default_version_stamp")]
    pub version_stamp: VersionStamp,
    /// The `.buildinfo` the component's build wrote, present only when it built
    /// and wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buildinfo: Option<BuildInfoRecord>,
    /// What the component's source resolved to: one entry per input, each
    /// naming its kind, its value, and whether it is pinned.
    ///
    /// Empty for a component that failed before it resolved anything, which is
    /// the record saying it never got that far rather than naming an input it
    /// never reached.
    #[serde(
        rename = "source",
        default,
        skip_serializing_if = "Fingerprint::is_empty"
    )]
    pub source: Fingerprint,
    /// The packages the component produced, present only when it built.
    #[serde(rename = "package", default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<PackageRecord>,
}

/// One produced package: its name and version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageRecord {
    /// The binary package name.
    pub name: String,
    /// The package version.
    pub version: String,
}

/// A reference to the `.buildinfo` a component's build wrote.
///
/// The manifest names the file and its checksum rather than restating what it
/// holds. `.buildinfo` is Debian's own record of what a package was built
/// against — the installed package set of the build root above all, which the
/// `[sandbox]` section does not carry — and it is what a rebuild is compared
/// with. Naming it keeps one authority for that rather than two that can
/// disagree, and the checksum makes it possible to tell the recorded file from
/// one that has since changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfoRecord {
    /// The path to the `.buildinfo`, relative to the work directory, or the
    /// full path when it lies outside one.
    pub path: String,
    /// Its SHA-256, in lowercase hexadecimal.
    pub sha256: String,
}

impl BuildInfoRecord {
    /// Records `buildinfo`, naming its path relative to `work_dir`.
    ///
    /// Relative because the manifest lives under the same work directory, so a
    /// work directory that is moved or copied keeps a manifest whose references
    /// still resolve. A path outside the work directory — which the output tree
    /// never is — is recorded whole rather than made into a relative path that
    /// climbs out of it.
    pub fn of(buildinfo: &crate::build::BuildInfo, work_dir: &Path) -> BuildInfoRecord {
        let path = buildinfo
            .path
            .strip_prefix(work_dir)
            .unwrap_or(&buildinfo.path);
        BuildInfoRecord {
            path: path.to_string_lossy().into_owned(),
            sha256: buildinfo.sha256.clone(),
        }
    }
}

/// What a run would build a component as: everything that decides the version
/// its packages carry, short of the contents of the source tree itself.
///
/// The [`Fingerprint`] alone answered this until a recipe could change a
/// package's version without changing a byte of what it resolved. Two settings
/// now can — a declared [`version`](crate::Component::version), and the
/// [`version_stamp`](crate::Component::version_stamp) that decides how it is
/// stamped — so each is compared beside the fingerprint rather than folded into
/// it. They are grouped here because they are always used together: a
/// `--skip-published` run asks one question about all of them, and a manifest
/// records all of them so the next run can ask it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildIdentity<'a> {
    /// What the component's source resolved to.
    pub source: &'a Fingerprint,
    /// The upstream version the recipe declared, or `None` for a component that
    /// takes one from its own `debian/changelog`.
    pub version: Option<&'a str>,
    /// How that version is stamped.
    pub version_stamp: VersionStamp,
}

impl ComponentRecord {
    /// Whether this record marks the component as built at `identity`, so a
    /// `--skip-published` run may skip it. A failed or skipped record is not a
    /// reason to skip.
    ///
    /// An unpinned source is never skippable, however exactly it matches. Its
    /// value names where the tree was read from and not what the tree held, so
    /// two runs agreeing on it establishes nothing about whether the source
    /// moved — and skipping on that basis would quietly publish yesterday's
    /// package as today's build.
    ///
    /// The two settings beside the source are compared for the reason
    /// [`BuildIdentity`] groups them: a recipe that declares `1.2.4` where it
    /// declared `1.2.3` resolves byte-identical trees and must still build, and
    /// so does one that moves a component to [`VersionStamp::Backport`].
    pub fn is_built_at(&self, identity: &BuildIdentity) -> bool {
        self.status == STATUS_BUILT
            && identity.source.is_pinned()
            && &self.source == identity.source
            && self.version.as_deref() == identity.version
            && self.version_stamp == identity.version_stamp
    }
}

/// Whether a version stamp is the one a record leaves out.
///
/// Serde needs a predicate rather than a comparison, and the default is what a
/// record written before the setting existed reads back as.
fn is_default_version_stamp(version_stamp: &VersionStamp) -> bool {
    *version_stamp == VersionStamp::default()
}

impl Manifest {
    /// Assembles a manifest of `records` for the recipe's identity.
    pub fn new(
        recipe: impl Into<String>,
        suite: impl Into<String>,
        architecture: impl Into<String>,
        records: Vec<ComponentRecord>,
    ) -> Manifest {
        Manifest {
            recipe: recipe.into(),
            suite: suite.into(),
            architecture: architecture.into(),
            build_date: None,
            sandbox: None,
            archives: Vec::new(),
            interpreter: None,
            components: records,
        }
    }

    /// Records the sandbox inputs the run's builds ran under. A run that built
    /// nothing has none to record.
    pub fn with_sandbox(mut self, sandbox: Option<SandboxRecord>) -> Manifest {
        self.sandbox = sandbox;
        self
    }

    /// Records the archive states the run's build roots resolved against. A run
    /// that provisioned nothing resolved nothing and keeps what is already
    /// there; see [`Manifest::archives`].
    pub fn with_archives(mut self, archives: Vec<ArchiveRecord>) -> Manifest {
        self.archives = archives;
        self
    }

    /// Records the interpreter a foreign build ran through. A native build ran
    /// through none and records none; see [`Manifest::interpreter`].
    pub fn with_interpreter(mut self, interpreter: Option<InterpreterRecord>) -> Manifest {
        self.interpreter = interpreter;
        self
    }

    /// Records the date the run's versions were stamped with. A run that built
    /// nothing has none of its own to record.
    pub fn with_build_date(mut self, build_date: Option<String>) -> Manifest {
        self.build_date = build_date;
        self
    }

    /// Loads the manifest a prior run wrote at `path` (see [`manifest_path`]),
    /// or `None` when none is present. A manifest that cannot be parsed is an
    /// error, so a resumed run does not silently proceed on a corrupt record.
    ///
    /// A manifest written before a field this version requires is refused along
    /// with a corrupt one, and deliberately: the alternative is defaulting the
    /// field, which would have a provenance record state a posture no build ever
    /// observed. The remedy is to delete the manifest, which costs the next run
    /// a rebuild of what `--skip-published` would have skipped and costs the
    /// record nothing it could honestly have kept.
    pub fn load(path: &Path) -> Result<Option<Manifest>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_error("reading the manifest", path, err)),
        };
        let manifest = toml::from_str(&text).map_err(|err| {
            io_error(
                "parsing the manifest",
                path,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{err}\nA manifest an older src2deb wrote may not carry a field this \
                         one requires. Delete it and rebuild; the next run records the whole \
                         recipe again."
                    ),
                ),
            )
        })?;
        Ok(Some(manifest))
    }

    /// The records indexed by component name, for looking up a prior run's state.
    pub fn records_by_name(&self) -> BTreeMap<&str, &ComponentRecord> {
        self.components
            .iter()
            .map(|record| (record.name.as_str(), record))
            .collect()
    }

    /// Renders the manifest as TOML.
    pub fn to_toml(&self) -> String {
        // The manifest is composed entirely of strings and string tables, none of
        // which TOML serialization can reject, so this does not fail.
        toml::to_string(self).expect("a manifest of strings serializes to TOML")
    }

    /// Writes the manifest to `path` (see [`manifest_path`]), creating the
    /// directories leading to it.
    ///
    /// The write is atomic: the TOML lands in a temporary file beside the
    /// manifest and is renamed over it, so a reader sees either the whole prior
    /// manifest or the whole new one. That matters because [`load`](Self::load)
    /// treats an unparseable manifest as a hard error, and the manifest is
    /// written at the very end of a run — exactly where a second Ctrl-C, a
    /// `SIGKILL`, or a power loss lands. A partial write would otherwise leave
    /// every later run against that work directory failing on a truncated file
    /// with nothing to suggest deleting it.
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| io_error("creating the manifest directory", parent, err))?;
        }
        // Beside the manifest rather than in a temporary directory, so the
        // rename stays within one filesystem and is therefore atomic. The name
        // carries this process's id, so two runs writing sibling manifests
        // cannot stage over each other.
        let staging = staging_path(path);
        std::fs::write(&staging, self.to_toml())
            .map_err(|err| io_error("writing the manifest", &staging, err))?;
        std::fs::rename(&staging, path).map_err(|err| {
            // The staged file is of no use to anyone once the rename has failed,
            // and leaving it behind would accumulate one per failed run.
            let _ = std::fs::remove_file(&staging);
            io_error("writing the manifest", path, err)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::fingerprint::{SourceInput, SourceRole};

    /// A git source at `commit`, the shape the resolver produces.
    /// The identity a run would build a component at, so a test naming a
    /// source and a version is not also restating the default stamp.
    fn identity<'a>(source: &'a Fingerprint, version: Option<&'a str>) -> BuildIdentity<'a> {
        BuildIdentity {
            source,
            version,
            version_stamp: VersionStamp::default(),
        }
    }

    fn git(commit: &str) -> Fingerprint {
        Fingerprint::of(SourceInput::git(SourceRole::Source, commit))
    }

    fn built(name: &str, commit: &str, version: &str) -> ComponentRecord {
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
                version: version.to_string(),
            }],
        }
    }

    #[test]
    fn a_manifest_round_trips_through_toml() {
        let manifest = Manifest::new(
            "cosmic-epoch",
            "trixie",
            "amd64",
            vec![
                built("cosmic-randr", "abc123", "1.0-1"),
                ComponentRecord {
                    name: "cosmic-osd".to_string(),
                    status: STATUS_FAILED.to_string(),
                    error: Some("boom".to_string()),
                    version: None,
                    version_stamp: VersionStamp::default(),
                    buildinfo: None,
                    source: git("def456"),
                    packages: Vec::new(),
                },
            ],
        );
        let toml = manifest.to_toml();
        let parsed = Manifest::load_from_str(&toml);
        assert_eq!(parsed.recipe, "cosmic-epoch");
        assert_eq!(parsed.components.len(), 2);
        assert!(parsed.components[0].is_built_at(&identity(&git("abc123"), None)));
        assert!(!parsed.components[0].is_built_at(&identity(&git("other"), None)));
        assert_eq!(parsed.components[0].packages[0].version, "1.0-1");
        assert_eq!(parsed.components[1].status, STATUS_FAILED);
        assert_eq!(parsed.components[1].error.as_deref(), Some("boom"));
    }

    #[test]
    fn a_component_records_its_source_by_kind_value_and_pinned_ness() {
        // What a reader has to be able to tell apart without knowing src2deb's
        // table of kinds: an input that names exactly what was built, and one
        // that only names where it was read from.
        let manifest = Manifest::new(
            "r",
            "trixie",
            "amd64",
            vec![ComponentRecord {
                name: "c".to_string(),
                status: STATUS_BUILT.to_string(),
                error: None,
                version: None,
                version_stamp: VersionStamp::default(),
                buildinfo: None,
                source: Fingerprint::over(vec![
                    SourceInput::git(SourceRole::Source, "abc123"),
                    SourceInput::path(SourceRole::Packaging, "/home/someone/packaging"),
                ]),
                packages: Vec::new(),
            }],
        );
        let toml = manifest.to_toml();
        assert!(toml.contains("[[component.source]]"), "{toml}");
        assert!(toml.contains("kind = \"git\""), "{toml}");
        assert!(toml.contains("value = \"abc123\""), "{toml}");
        assert!(toml.contains("pinned = true"), "{toml}");
        assert!(toml.contains("kind = \"path\""), "{toml}");
        assert!(toml.contains("pinned = false"), "{toml}");
        assert_eq!(
            Manifest::load_from_str(&toml).components[0].source,
            manifest.components[0].source,
        );
    }

    #[test]
    fn a_buildinfo_is_recorded_relative_to_the_work_directory() {
        // The manifest lives under the same work directory, so a relative
        // reference survives that directory being moved or copied.
        let buildinfo = crate::build::BuildInfo {
            path: PathBuf::from("/work/out/trixie/amd64/cosmic-randr/r_1.0_amd64.buildinfo"),
            sha256: "abc123".to_string(),
        };
        let record = BuildInfoRecord::of(&buildinfo, Path::new("/work"));
        assert_eq!(
            record.path,
            "out/trixie/amd64/cosmic-randr/r_1.0_amd64.buildinfo"
        );
        assert_eq!(record.sha256, "abc123");

        // A path outside the work directory is recorded whole, rather than made
        // into a relative path that climbs out of it.
        let outside = BuildInfoRecord::of(&buildinfo, Path::new("/elsewhere"));
        assert_eq!(
            outside.path,
            "/work/out/trixie/amd64/cosmic-randr/r_1.0_amd64.buildinfo"
        );
    }

    #[test]
    fn a_manifest_carries_a_components_buildinfo_reference_through_toml() {
        let manifest = Manifest::new(
            "r",
            "trixie",
            "amd64",
            vec![ComponentRecord {
                buildinfo: Some(BuildInfoRecord {
                    path: "out/trixie/amd64/c/c_1.0_amd64.buildinfo".to_string(),
                    sha256: "abc123".to_string(),
                }),
                ..built("c", "abc", "1.0-1")
            }],
        );
        let toml = manifest.to_toml();
        assert!(toml.contains("[component.buildinfo]"), "{toml}");
        let parsed = Manifest::load_from_str(&toml);
        assert_eq!(
            parsed.components[0].buildinfo,
            manifest.components[0].buildinfo
        );
        // A component with none omits the section rather than writing an empty
        // one, so the manifest never names a file that is not there.
        let none = Manifest::new("r", "trixie", "amd64", vec![built("c", "abc", "1.0-1")]);
        assert!(!none.to_toml().contains("buildinfo"), "{}", none.to_toml());
    }

    #[test]
    fn a_component_that_never_resolved_records_no_source_at_all() {
        // The record says it never got that far, rather than naming an input it
        // never reached.
        let manifest = Manifest::new(
            "r",
            "trixie",
            "amd64",
            vec![ComponentRecord {
                name: "c".to_string(),
                status: STATUS_FAILED.to_string(),
                error: Some("no such repository".to_string()),
                version: None,
                version_stamp: VersionStamp::default(),
                buildinfo: None,
                source: Fingerprint::none(),
                packages: Vec::new(),
            }],
        );
        let toml = manifest.to_toml();
        assert!(!toml.contains("component.source"), "{toml}");
        let parsed = Manifest::load_from_str(&toml);
        assert!(parsed.components[0].source.is_empty());
    }

    /// A cage built the way a build pass builds one, so the sandbox record is
    /// exercised against a real resolved profile rather than a hand-written
    /// stand-in. Building a cage validates configuration and canonicalizes the
    /// rootfs; it launches nothing and needs no privilege.
    fn resolved_inputs(dir: &Path) -> ferroday_cage::ResolvedInputs {
        let (rootfs, source, out) = (dir.join("root"), dir.join("src"), dir.join("out"));
        for path in [&rootfs, &source, &out] {
            std::fs::create_dir_all(path).unwrap();
        }
        ferroday_cage::Cage::builder()
            .rootfs(&rootfs)
            .stop_with_caller(true)
            .bind_ro(&source, "/src")
            .bind(&out, "/out")
            .network(ferroday_cage::Network::Isolated)
            .command("/bin/sh")
            .args(["-e", "-u", "-c", "true"])
            .build()
            .expect("a cage over a scratch rootfs builds")
            .resolved_inputs()
    }

    /// A record with nothing in the fields a test is not about, for filling in
    /// around the one field it is. `SandboxRecord` has no `Default` on purpose —
    /// a record has to state what it recorded — so this is a test's own stand-in
    /// rather than a shape the library offers.
    fn bare_record() -> SandboxRecord {
        SandboxRecord {
            component: "c".to_string(),
            root: RootRecord::Plain {
                path: "/work/roots/c".to_string(),
            },
            identity: IdentityRecord::Single,
            network: "isolated".to_string(),
            rlimits: Vec::new(),
            hardening: HardeningRecord::Unavailable,
            env: BTreeMap::new(),
            mounts: Vec::new(),
        }
    }

    fn scratch(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "src2deb-manifest-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writing_a_manifest_leaves_no_partial_file_and_replaces_the_prior_one() {
        let dir = scratch("atomic");
        let path = manifest_path(&dir, "cosmic-epoch", "trixie", "amd64");

        Manifest::new(
            "cosmic-epoch",
            "trixie",
            "amd64",
            vec![built("a", "abc", "1")],
        )
        .write(&path)
        .unwrap();
        Manifest::new(
            "cosmic-epoch",
            "trixie",
            "amd64",
            vec![built("a", "def", "2")],
        )
        .write(&path)
        .unwrap();

        // The second write replaced the first outright, and the staging file it
        // renamed through is gone — a leftover would accumulate one per run, and
        // a manifest left half-written would fail every later `load`.
        let loaded = Manifest::load(&path).unwrap().expect("a manifest is there");
        assert!(loaded.components[0].is_built_at(&identity(&git("def"), None)));
        let siblings: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(siblings, ["amd64.toml"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sandbox_record_captures_the_environment_and_every_mount_in_order() {
        let dir = scratch("inputs");
        let inputs = resolved_inputs(&dir);
        let record = SandboxRecord::of("cosmic-randr", &inputs);

        assert_eq!(record.component, "cosmic-randr");
        // The environment the build command carries, base composed in.
        assert_eq!(record.env["HOME"], "/root");
        assert!(record.env["PATH"].contains("/usr/bin"));
        // Every mount, in the order the sandbox establishes them, so the record
        // is one entry per resolved mount and not a summary of them.
        assert_eq!(record.mounts.len(), inputs.mounts.len());
        // src2deb's own binds come last, after the managed profile, and carry
        // the read-only posture each pass declared.
        let binds: Vec<&MountRecord> = record
            .mounts
            .iter()
            .filter(|mount| {
                matches!(mount, MountRecord::Bind { target, .. } if target == "/src" || target == "/out")
            })
            .collect();
        assert_eq!(
            binds,
            [
                &MountRecord::Bind {
                    source: dir.join("src").to_string_lossy().into_owned(),
                    target: "/src".to_string(),
                    read_only: true,
                },
                &MountRecord::Bind {
                    source: dir.join("out").to_string_lossy().into_owned(),
                    target: "/out".to_string(),
                    read_only: false,
                },
            ],
        );
        // The managed profile is in there too: the record names mounts no
        // builder accessor could report.
        assert!(
            record
                .mounts
                .iter()
                .any(|mount| matches!(mount, MountRecord::Procfs { target } if target == "/proc"))
        );
        assert!(record.mounts.iter().any(
            |mount| matches!(mount, MountRecord::Symlink { path, .. } if path == "/dev/stdin")
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same scratch cage as [`resolved_inputs`], rooted on an overlay of the
    /// rootfs and an upper rather than on the rootfs directly — the two shapes
    /// src2deb's two provisioning strategies produce.
    fn overlay_inputs(dir: &Path) -> ferroday_cage::ResolvedInputs {
        let (rootfs, upper) = (dir.join("root"), dir.join("upper"));
        for path in [&rootfs, &upper] {
            std::fs::create_dir_all(path).unwrap();
        }
        ferroday_cage::Cage::builder()
            .overlay_rootfs(&rootfs, &upper)
            .stop_with_caller(true)
            .command("/bin/sh")
            .args(["-e", "-u", "-c", "true"])
            .build()
            .expect("a cage over a scratch overlay builds")
            .resolved_inputs()
    }

    #[test]
    fn an_overlay_root_is_told_apart_from_a_plain_one_over_the_same_base() {
        // The gap the root closes. Before it was recorded, these two produced
        // byte-identical records: the same environment, the same managed mount
        // profile, and no mention of what any of it was laid over. They are
        // different builds — one writes into the rootfs, the other into a layer
        // over it — and now they read as different records.
        let dir = scratch("roots");
        let plain = SandboxRecord::of("cosmic-randr", &resolved_inputs(&dir));
        let overlaid = SandboxRecord::of("cosmic-randr", &overlay_inputs(&dir));

        let rootfs = dir.join("root").canonicalize().unwrap();
        assert_eq!(
            plain.root,
            RootRecord::Plain {
                path: rootfs.to_string_lossy().into_owned(),
            },
        );
        let RootRecord::Overlay { lower, upper, work } = &overlaid.root else {
            panic!("an overlay-rooted cage records an overlay root: {overlaid:?}");
        };
        // The lower stack is the shared base every component builds over, which
        // is the part of an overlay root that says what the build saw.
        assert_eq!(lower, &[rootfs.to_string_lossy().into_owned()]);
        // The upper and its work directory are this component's own, named as
        // the source and output binds are.
        assert!(upper.ends_with("upper"), "{upper}");
        assert!(!work.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_postures_a_build_ran_under_are_recorded_whether_or_not_they_were_set() {
        let dir = scratch("posture");
        let record = SandboxRecord::of("cosmic-randr", &resolved_inputs(&dir));

        // src2deb builds under the single-identity map, so the build sees uid 0
        // — which is what `Rules-Requires-Root` handling turns on.
        assert_eq!(record.identity, IdentityRecord::Single);
        // The build pass has no network. This is the first thing a
        // reproducibility claim is challenged on.
        assert_eq!(record.network, "isolated");
        // Nothing sets a resource limit, and an empty list says so.
        assert!(record.rlimits.is_empty());
        // The hardening layer is not compiled in, which is recorded as such
        // rather than omitted: a record that left the key out could not be told
        // from one written before the key existed.
        assert_eq!(record.hardening, HardeningRecord::Unavailable);

        // And every one of them survives the round trip a `--skip-published`
        // run reads the manifest back through.
        let toml = Manifest::new("r", "trixie", "amd64", Vec::new())
            .with_sandbox(Some(record.clone()))
            .to_toml();
        let parsed = Manifest::load_from_str(&toml)
            .sandbox
            .expect("the record survives");
        assert_eq!(parsed.root, record.root);
        assert_eq!(parsed.identity, record.identity);
        assert_eq!(parsed.network, record.network);
        assert_eq!(parsed.hardening, record.hardening);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_applied_hardening_posture_is_not_an_unavailable_one() {
        // A build that could have hardened and did not is a different fact from
        // one whose sandbox could not harden at all, and the record has to keep
        // them apart. src2deb produces the second today; the first is what a
        // build under a sandbox with the layer compiled in would record.
        let empty = HardeningRecord::Applied {
            landlock_fs: Vec::new(),
            landlock_net: Vec::new(),
            seccomp_instructions: None,
            keep_capabilities: None,
        };
        assert_ne!(empty, HardeningRecord::Unavailable);

        let toml = Manifest::new("r", "trixie", "amd64", Vec::new())
            .with_sandbox(Some(SandboxRecord {
                hardening: empty.clone(),
                ..bare_record()
            }))
            .to_toml();
        assert!(toml.contains("kind = \"applied\""), "{toml}");
        assert_eq!(
            Manifest::load_from_str(&toml).sandbox.unwrap().hardening,
            empty,
        );
    }

    #[test]
    fn a_root_kind_added_upstream_is_recorded_as_one_this_version_cannot_name() {
        // `ResolvedRoot` is non-exhaustive, as `ResolvedMount` is, and the
        // fallback says the same thing: the build was rooted on something this
        // version has no field for, which is not the same as saying nothing.
        let toml = Manifest::new("r", "trixie", "amd64", Vec::new())
            .with_sandbox(Some(SandboxRecord {
                root: RootRecord::Unknown,
                ..bare_record()
            }))
            .to_toml();
        assert!(toml.contains("kind = \"unknown\""), "{toml}");
        assert_eq!(
            Manifest::load_from_str(&toml).sandbox.unwrap().root,
            RootRecord::Unknown,
        );
    }

    #[test]
    fn a_native_build_records_no_interpreter() {
        // Nothing interpreted anything, which is a different statement from
        // having failed to look — and the honest one.
        let native = crate::arch::host_architecture();
        assert_eq!(InterpreterRecord::of(&native), None);

        let toml = Manifest::new("r", "trixie", &native, Vec::new()).to_toml();
        assert!(!toml.contains("interpreter"), "{toml}");
        assert!(Manifest::load_from_str(&toml).interpreter.is_none());
    }

    #[test]
    fn a_foreign_build_records_the_interpreter_it_ran_through() {
        // Reads the running kernel's own binfmt registration, so it only says
        // anything on a host that has one. A developer host without qemu-user
        // installed cannot build foreign either, so there is nothing to assert
        // about it beyond the native case above.
        let foreign = ["arm64", "amd64", "riscv64", "ppc64el"]
            .into_iter()
            .find(|target| crate::arch::is_foreign(&crate::arch::host_architecture(), target))
            .expect("some architecture is foreign to any host");
        let Some(record) = InterpreterRecord::of(foreign) else {
            return;
        };

        // The qemu target name rather than the Debian architecture: `aarch64`
        // for `arm64`.
        assert!(!record.name.is_empty());
        // The registered path, which is what binaries execute through — not
        // whatever a `PATH` lookup would find, and the build environment has no
        // `PATH` to look in anyway.
        assert!(record.path.starts_with('/'), "{}", record.path);
        // The digest is of the registered path, which `open` follows the symlink
        // along, so it is the real binary's bytes.
        let sha256 = record
            .sha256
            .clone()
            .expect("a registered interpreter reads");
        assert_eq!(sha256.len(), 64, "{sha256}");
        if let Some(resolved) = &record.resolved {
            assert_eq!(digest_of(Path::new(resolved)), Some(sha256));
        }

        let toml = Manifest::new("r", "trixie", foreign, Vec::new())
            .with_interpreter(Some(record.clone()))
            .to_toml();
        assert_eq!(Manifest::load_from_str(&toml).interpreter, Some(record));
    }

    #[test]
    fn a_registered_but_disabled_handler_is_not_an_absent_one() {
        // The registration exists either way, and the distinction is "turn it
        // on" against "install it". A handler that is off is recorded as
        // present and not enabled rather than being dropped.
        let off = InterpreterRecord {
            name: "aarch64".to_string(),
            path: "/usr/libexec/qemu-binfmt/aarch64-binfmt-P".to_string(),
            resolved: Some("/usr/bin/qemu-aarch64".to_string()),
            sha256: Some("ab".repeat(32)),
            enabled: false,
            flags: "POF".to_string(),
        };
        let toml = Manifest::new("r", "trixie", "arm64", Vec::new())
            .with_interpreter(Some(off.clone()))
            .to_toml();
        assert!(toml.contains("enabled = false"), "{toml}");
        assert_eq!(Manifest::load_from_str(&toml).interpreter, Some(off));
    }

    #[test]
    fn a_manifest_carrying_a_sandbox_record_round_trips_through_toml() {
        let dir = scratch("round-trip");
        let record = SandboxRecord::of("cosmic-randr", &resolved_inputs(&dir));
        let manifest = Manifest::new(
            "cosmic-epoch",
            "trixie",
            "amd64",
            vec![built("cosmic-randr", "abc123", "1.0-1")],
        )
        .with_sandbox(Some(record.clone()));

        // The manifest is read back by `--skip-published`, so every mount kind
        // has to survive TOML in both directions, tag and all.
        let parsed = Manifest::load_from_str(&manifest.to_toml());
        let parsed = parsed.sandbox.expect("the sandbox record survives");
        assert_eq!(parsed.component, record.component);
        assert_eq!(parsed.env, record.env);
        assert_eq!(parsed.mounts, record.mounts);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manifest_without_a_sandbox_record_omits_the_section() {
        // A first run that built nothing has no sandbox to record, and says so
        // by leaving the section out rather than writing an empty one.
        let manifest = Manifest::new("r", "trixie", "amd64", Vec::new());
        let toml = manifest.to_toml();
        assert!(!toml.contains("sandbox"), "{toml}");
        assert!(Manifest::load_from_str(&toml).sandbox.is_none());
    }

    /// An archive record as a run resolving against the Debian CDN produces one.
    fn signed_archive() -> ArchiveRecord {
        ArchiveRecord {
            mirror: "http://deb.debian.org/debian".to_string(),
            suite: "trixie".to_string(),
            components: vec!["main".to_string()],
            release_sha256: "74122baf".to_string(),
            date: Some("Sat, 11 Jul 2026 09:02:23 UTC".to_string()),
            valid_until: Some("Sat, 18 Jul 2026 09:02:23 UTC".to_string()),
            signed_by: vec!["4CB50190207B4758A3F73A796ED0E7B82643E131".to_string()],
        }
    }

    /// One as the run's own `file://` pool produces: trusted unsigned, so
    /// nothing verified it.
    fn unsigned_pool() -> ArchiveRecord {
        ArchiveRecord {
            mirror: "file:///work/pool/trixie/amd64".to_string(),
            suite: "trixie".to_string(),
            components: vec!["main".to_string()],
            release_sha256: "c50692c3".to_string(),
            date: Some("Mon, 03 Aug 2026 06:41:35 UTC".to_string()),
            valid_until: None,
            signed_by: Vec::new(),
        }
    }

    #[test]
    fn a_manifest_records_the_archive_state_each_root_resolved_against() {
        let manifest = Manifest::new("r", "trixie", "amd64", Vec::new())
            .with_archives(vec![unsigned_pool(), signed_archive()]);
        let toml = manifest.to_toml();

        // The signing key the release was verified against, which is what the
        // plan key alone could never say: it digests the selection and not what
        // the selection was made from.
        assert!(
            toml.contains("signed-by = [\"4CB50190207B4758A3F73A796ED0E7B82643E131\"]"),
            "{toml}",
        );
        // The run's own pool verifies nothing, and records that as an empty list
        // rather than by leaving the key out — an absent key could not be told
        // from a record written before the field existed.
        assert!(toml.contains("signed-by = []"), "{toml}");

        let parsed = Manifest::load_from_str(&toml);
        assert_eq!(parsed.archives, [unsigned_pool(), signed_archive()]);
    }

    #[test]
    fn a_manifest_that_resolved_nothing_omits_the_archives() {
        // A run that provisions nothing resolves nothing. It says so by leaving
        // the section out, as it does for the sandbox record.
        let toml = Manifest::new("r", "trixie", "amd64", Vec::new()).to_toml();
        assert!(!toml.contains("archive"), "{toml}");
        assert!(Manifest::load_from_str(&toml).archives.is_empty());
    }

    #[test]
    fn a_valid_until_the_release_did_not_carry_is_left_out() {
        // Debian's own releases carry one and a locally written pool does not,
        // and the two are different facts about an archive rather than one
        // rendered two ways.
        let toml =
            Manifest::new("r", "trixie", "amd64", Vec::new()).with_archives(vec![unsigned_pool()]);
        let toml = toml.to_toml();
        assert!(!toml.contains("valid-until"), "{toml}");
        assert_eq!(Manifest::load_from_str(&toml).archives[0].valid_until, None);
    }

    #[test]
    fn a_mount_kind_added_upstream_is_recorded_by_its_position() {
        // `ResolvedMount` is non-exhaustive: a kind added to ferroday-cage
        // after this was written still has to appear in the sequence, or the
        // record would quietly claim a build saw less than it did.
        let record = mount_record(&ResolvedMount::Procfs {
            target: std::path::PathBuf::from("/proc"),
        });
        assert_eq!(
            record,
            MountRecord::Procfs {
                target: "/proc".to_string()
            }
        );
        // The fallback keeps the position even with nothing else to say.
        let unknown = MountRecord::Unknown {
            target: "/somewhere".to_string(),
        };
        let toml = toml::to_string(&SandboxRecord {
            mounts: vec![unknown.clone()],
            ..bare_record()
        })
        .unwrap();
        assert!(toml.contains("kind = \"unknown\""), "{toml}");
        let parsed: SandboxRecord = toml::from_str(&toml).unwrap();
        assert_eq!(parsed.mounts, [unknown]);
    }

    #[test]
    fn each_recipe_suite_and_architecture_gets_its_own_manifest_path() {
        let work = Path::new("/w");
        let base = manifest_path(work, "cosmic-epoch", "forky", "arm64");
        assert_eq!(
            base,
            Path::new("/w/manifests/cosmic-epoch/forky/arm64.toml")
        );
        // Varying any one field moves the manifest.
        for other in [
            manifest_path(work, "adw-gtk3", "forky", "arm64"),
            manifest_path(work, "cosmic-epoch", "trixie", "arm64"),
            manifest_path(work, "cosmic-epoch", "forky", "amd64"),
        ] {
            assert_ne!(base, other);
        }
        // Nesting keeps identities that would collide if the fields were joined
        // into one name apart: "a" / "b-c" is not "a-b" / "c".
        assert_ne!(
            manifest_path(work, "a", "b-c", "arm64"),
            manifest_path(work, "a-b", "c", "arm64"),
        );
    }

    #[test]
    fn two_recipes_sharing_a_work_directory_keep_their_own_provenance() {
        // A work directory is shared deliberately: that is how separate recipes
        // publish into one pool. Writing one recipe's manifest must not destroy
        // another's, nor offer its records to the next `--skip-published` run.
        let work = scratch("shared");
        let cosmic = manifest_path(&work, "cosmic-epoch", "forky", "arm64");
        let theme = manifest_path(&work, "adw-gtk3", "forky", "arm64");

        Manifest::new(
            "cosmic-epoch",
            "forky",
            "arm64",
            vec![built("cosmic-randr", "abc123", "1.0-1")],
        )
        .write(&cosmic)
        .expect("the first manifest writes");
        Manifest::new(
            "adw-gtk3",
            "forky",
            "arm64",
            vec![built("adw-gtk3", "213b5eda", "5.3.0-0pop1")],
        )
        .write(&theme)
        .expect("the second manifest writes");

        let first = Manifest::load(&cosmic)
            .unwrap()
            .expect("the first survives");
        assert_eq!(first.recipe, "cosmic-epoch");
        assert_eq!(first.components.len(), 1);
        assert!(first.components[0].is_built_at(&identity(&git("abc123"), None)));

        let second = Manifest::load(&theme)
            .unwrap()
            .expect("the second is there");
        assert_eq!(second.recipe, "adw-gtk3");
        assert_eq!(second.components[0].name, "adw-gtk3");

        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn the_same_recipe_built_for_another_suite_starts_from_no_records() {
        // The pool is suite- and architecture-scoped, so a forky run has none of
        // a trixie run's packages. Reading the trixie manifest would let
        // `--skip-published` skip a component that was never built for forky.
        let work = scratch("suites");
        Manifest::new(
            "cosmic-epoch",
            "trixie",
            "arm64",
            vec![built("cosmic-randr", "abc123", "1.0-1")],
        )
        .write(&manifest_path(&work, "cosmic-epoch", "trixie", "arm64"))
        .expect("the trixie manifest writes");

        let forky =
            Manifest::load(&manifest_path(&work, "cosmic-epoch", "forky", "arm64")).unwrap();
        assert!(forky.is_none(), "forky must not inherit trixie's records");

        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn writing_a_manifest_creates_the_directories_leading_to_it() {
        let work = scratch("mkdir");
        let path = manifest_path(&work, "r", "trixie", "amd64");
        assert!(!path.parent().unwrap().exists());
        Manifest::new("r", "trixie", "amd64", Vec::new())
            .write(&path)
            .expect("write creates its own directories");
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn a_missing_manifest_loads_as_none_but_a_corrupt_one_is_an_error() {
        let work = scratch("corrupt");
        let path = manifest_path(&work, "r", "trixie", "amd64");
        assert!(Manifest::load(&path).unwrap().is_none());

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "recipe = ").unwrap();
        let err = Manifest::load(&path).unwrap_err();
        assert!(format!("{err}").contains("parsing the manifest"), "{err}");

        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn only_a_built_record_of_the_same_source_is_skippable() {
        let record = built("c", "abc", "1.0");
        assert!(record.is_built_at(&identity(&git("abc"), None)));
        // A different commit, or a non-built status, is not skippable.
        assert!(!record.is_built_at(&identity(&git("xyz"), None)));
        let mut failed = record.clone();
        failed.status = STATUS_FAILED.to_string();
        assert!(!failed.is_built_at(&identity(&git("abc"), None)));
        let mut skipped = record;
        skipped.status = STATUS_SKIPPED.to_string();
        assert!(!skipped.is_built_at(&identity(&git("abc"), None)));
    }

    #[test]
    fn an_unpinned_source_is_never_skippable_however_well_it_matches() {
        // A path names where a tree was read from, not what it held, so a run
        // agreeing with the record establishes nothing about whether the source
        // moved. Skipping here would publish a stale package as a fresh build.
        let working_tree = Fingerprint::of(SourceInput::path(
            SourceRole::Source,
            "/home/someone/cosmic-comp",
        ));
        let record = ComponentRecord {
            name: "c".to_string(),
            status: STATUS_BUILT.to_string(),
            error: None,
            version: None,
            version_stamp: VersionStamp::default(),
            buildinfo: None,
            source: working_tree.clone(),
            packages: Vec::new(),
        };
        assert_eq!(record.source, working_tree);
        assert!(!record.is_built_at(&identity(&working_tree, None)));

        // One unpinned input among pinned ones is enough: whatever else the
        // build consumed, part of it cannot be compared.
        let overlaid = Fingerprint::over(vec![
            SourceInput::git(SourceRole::Source, "abc"),
            SourceInput::path(SourceRole::Packaging, "/home/someone/packaging"),
        ]);
        let record = ComponentRecord {
            source: overlaid.clone(),
            ..record
        };
        assert!(!record.is_built_at(&identity(&overlaid, None)));
    }

    #[test]
    fn a_source_that_gained_an_input_is_not_the_source_that_was_built() {
        // Adding a patch series or a packaging overlay to a component makes it
        // a different source, so a prior run's record does not excuse a build.
        let record = built("c", "abc", "1.0");
        let gained = Fingerprint::over(vec![
            SourceInput::git(SourceRole::Source, "abc"),
            SourceInput::sha256(SourceRole::Packaging, "9f8e7d6"),
        ]);
        assert!(!record.is_built_at(&identity(&gained, None)));
    }

    #[test]
    fn a_declared_version_that_moved_is_not_the_build_that_was_recorded() {
        // The one edit a recipe can make that produces different packages while
        // every input the fingerprint names stays exactly where it was. Without
        // this, `--skip-published` would skip the component and the version that
        // was asked for would never be published.
        let source = git("abc");
        let record = ComponentRecord {
            version: Some("1.2.3".to_string()),
            ..built("c", "abc", "1.2.3+deb13.20260731.abc")
        };
        assert!(record.is_built_at(&identity(&source, Some("1.2.3"))));
        assert!(!record.is_built_at(&identity(&source, Some("1.2.4"))));
        // Dropping the declaration is a move too: the version now comes from the
        // component's own changelog, which is a different answer.
        assert!(!record.is_built_at(&identity(&source, None)));

        // ...and a component that never declared one is unaffected, which is
        // every component whose packaging ships a changelog.
        let undeclared = built("c", "abc", "1.0-1");
        assert!(undeclared.is_built_at(&identity(&source, None)));
        assert!(!undeclared.is_built_at(&identity(&source, Some("1.2.3"))));
    }

    #[test]
    fn a_declared_version_is_recorded_and_omitted_when_there_is_none() {
        let manifest = Manifest::new(
            "r",
            "trixie",
            "amd64",
            vec![ComponentRecord {
                version: Some("1.2.3".to_string()),
                ..built("c", "abc", "1.2.3+deb13.20260731.abc")
            }],
        );
        let toml = manifest.to_toml();
        assert!(toml.contains("version = \"1.2.3\""), "{toml}");
        assert_eq!(
            Manifest::load_from_str(&toml).components[0]
                .version
                .as_deref(),
            Some("1.2.3"),
        );
        // A component that declared none writes no field, so the manifest never
        // claims a declaration the recipe did not make.
        let none = Manifest::new(
            "r",
            "trixie",
            "amd64",
            vec![ComponentRecord {
                packages: Vec::new(),
                ..built("c", "abc", "1.0-1")
            }],
        );
        assert!(!none.to_toml().contains("version"), "{}", none.to_toml());
    }

    impl Manifest {
        /// Parses a manifest from TOML text, for tests.
        fn load_from_str(text: &str) -> Manifest {
            toml::from_str(text).expect("valid manifest toml")
        }
    }
}
