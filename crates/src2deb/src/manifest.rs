//! The per-run provenance manifest: what each component resolved to and what it
//! produced.
//!
//! A run writes a manifest under the work directory, mapping every component to
//! the commit its source resolved to and, for a component that built, the exact
//! package versions it produced. It ties a run's inputs (pinned source
//! revisions, and the sandbox the builds ran in) to its outputs (versioned
//! `.debs`), which is the basis of a reproducibility story: the same manifest
//! names the revisions to check out and the conditions they were built under.
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

use ferroday_cage::{ResolvedInputs, ResolvedMount};
use serde::{Deserialize, Serialize};

use crate::error::{Result, io_error};

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
    work_dir
        .join(MANIFEST_DIR)
        .join(recipe)
        .join(suite)
        .join(format!("{architecture}.toml"))
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
    /// The sandbox inputs the run's builds ran under, absent when the run built
    /// nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxRecord>,
    /// Each component's record, in build order.
    #[serde(rename = "component", default)]
    pub components: Vec<ComponentRecord>,
}

/// The sandbox inputs a run's builds ran under: the environment the build
/// command carried, and every mount its sandbox established.
///
/// What a build produces depends on the environment and the filesystem it sees,
/// and neither follows from the source revisions alone. ferroday-cage's base
/// environment and managed mount profile are both outside its compatibility
/// promise and may change between releases, so a library version does not state
/// them either. Recording them says what a build actually ran under rather than
/// leaving it to be inferred.
///
/// The record is run-level. Every component's build pass applies the same
/// environment and the same mount sequence, differing only in the host paths of
/// the source and output binds, so one component's stands for the run and
/// [`component`](Self::component) names which one it was taken from.
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
    /// The build command's complete environment, by variable name.
    pub env: BTreeMap<String, String>,
    /// Every mount the sandbox established, in the order it established them:
    /// the managed profile first, then src2deb's own binds.
    #[serde(rename = "mount", default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountRecord>,
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
            env: inputs
                .env
                .iter()
                .map(|(name, value)| (text(name), text(value)))
                .collect(),
            mounts: inputs.mounts.iter().map(mount_record).collect(),
        }
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

/// One component's record: its resolved commit, its status, and either the
/// packages it produced or the reason it failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentRecord {
    /// The component name.
    pub name: String,
    /// The commit the component's source resolved to.
    pub commit: String,
    /// The status: [`STATUS_BUILT`], [`STATUS_FAILED`], or [`STATUS_SKIPPED`].
    pub status: String,
    /// The packages the component produced, present only when it built.
    #[serde(rename = "package", default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<PackageRecord>,
    /// The failure reason, present only when the component failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One produced package: its name and version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageRecord {
    /// The binary package name.
    pub name: String,
    /// The package version.
    pub version: String,
}

impl ComponentRecord {
    /// Whether this record marks the component as built at `commit`, so a
    /// `--skip-published` run may skip it when its source resolves to the same
    /// commit. A failed or skipped record is not a reason to skip.
    pub fn is_built_at(&self, commit: &str) -> bool {
        self.status == STATUS_BUILT && self.commit == commit
    }
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
            sandbox: None,
            components: records,
        }
    }

    /// Records the sandbox inputs the run's builds ran under. A run that built
    /// nothing has none to record.
    pub fn with_sandbox(mut self, sandbox: Option<SandboxRecord>) -> Manifest {
        self.sandbox = sandbox;
        self
    }

    /// Loads the manifest a prior run wrote at `path` (see [`manifest_path`]),
    /// or `None` when none is present. A manifest that cannot be parsed is an
    /// error, so a resumed run does not silently proceed on a corrupt record.
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
                std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()),
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

    fn built(name: &str, commit: &str, version: &str) -> ComponentRecord {
        ComponentRecord {
            name: name.to_string(),
            commit: commit.to_string(),
            status: STATUS_BUILT.to_string(),
            packages: vec![PackageRecord {
                name: name.to_string(),
                version: version.to_string(),
            }],
            error: None,
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
                    commit: "def456".to_string(),
                    status: STATUS_FAILED.to_string(),
                    packages: Vec::new(),
                    error: Some("boom".to_string()),
                },
            ],
        );
        let toml = manifest.to_toml();
        let parsed = Manifest::load_from_str(&toml);
        assert_eq!(parsed.recipe, "cosmic-epoch");
        assert_eq!(parsed.components.len(), 2);
        assert!(parsed.components[0].is_built_at("abc123"));
        assert!(!parsed.components[0].is_built_at("other"));
        assert_eq!(parsed.components[0].packages[0].version, "1.0-1");
        assert_eq!(parsed.components[1].status, STATUS_FAILED);
        assert_eq!(parsed.components[1].error.as_deref(), Some("boom"));
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
        assert!(loaded.components[0].is_built_at("def"));
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
            component: "c".to_string(),
            env: BTreeMap::new(),
            mounts: vec![unknown.clone()],
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
        assert!(first.components[0].is_built_at("abc123"));

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
    fn only_a_built_record_at_the_same_commit_is_skippable() {
        let record = built("c", "abc", "1.0");
        assert!(record.is_built_at("abc"));
        // A different commit, or a non-built status, is not skippable.
        assert!(!record.is_built_at("xyz"));
        let mut failed = record.clone();
        failed.status = STATUS_FAILED.to_string();
        assert!(!failed.is_built_at("abc"));
        let mut skipped = record;
        skipped.status = STATUS_SKIPPED.to_string();
        assert!(!skipped.is_built_at("abc"));
    }

    impl Manifest {
        /// Parses a manifest from TOML text, for tests.
        fn load_from_str(text: &str) -> Manifest {
            toml::from_str(text).expect("valid manifest toml")
        }
    }
}
