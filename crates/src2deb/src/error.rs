//! The error type shared across src2deb.

use std::fmt;
use std::path::PathBuf;

use ferroday_cage::provision::ProvisionError;
use ferroday_cage::provision::debian::DebianError;

/// The result type for src2deb operations.
pub type Result<T> = std::result::Result<T, Error>;

/// An error from a src2deb operation.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A recipe could not be read or parsed.
    Recipe {
        /// The recipe path.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },
    /// A git source operation failed.
    Source {
        /// The component whose source was being resolved.
        component: String,
        /// What went wrong.
        reason: String,
    },
    /// A `debian/control` file could not be read or parsed.
    Control {
        /// The component whose control file was being read.
        component: String,
        /// What went wrong.
        reason: String,
    },
    /// The run's target suite has no version tag, so no build version can be
    /// stamped.
    ///
    /// Reachable past [`Recipe::validate`](crate::Recipe) because `--suite`
    /// replaces the suite a validated recipe declared.
    VersionTag {
        /// The suite with no known tag.
        suite: String,
    },
    /// A `debian/changelog` file could not be read or parsed.
    Changelog {
        /// The component whose changelog was being read.
        component: String,
        /// What went wrong.
        reason: String,
    },
    /// The component dependency graph could not be ordered (for example, it
    /// contains a cycle).
    Plan(String),
    /// A `--only` or `--from` selection cannot be built: it named a component
    /// the recipe does not contain, it named one whose source did not resolve,
    /// or it left out a component that produces a build-dependency of one it
    /// selected.
    Selection(String),
    /// Configuring the Debian provisioner failed.
    Debian(DebianError),
    /// Provisioning a build root failed.
    Provision(ProvisionError),
    /// Launching or running a build cage failed.
    Cage(ferroday_cage::Error),
    /// A build command exited unsuccessfully.
    Build {
        /// The component that failed to build.
        component: String,
        /// The command's exit status.
        status: ferroday_cage::ExitStatus,
    },
    /// Installing the recipe's pinned rustup toolchain into a build root exited
    /// unsuccessfully.
    Toolchain {
        /// The toolchain version the recipe pinned.
        version: String,
        /// What went wrong, carrying what the installer wrote. The installer's
        /// output is captured rather than streamed — it is one step of
        /// provisioning, not a build's narrative — so the failure has to carry
        /// it or it is lost.
        reason: String,
    },
    /// The vendor step exited unsuccessfully.
    Vendor {
        /// The component whose vendor step failed.
        component: String,
        /// The command's exit status.
        status: ferroday_cage::ExitStatus,
    },
    /// The run was cancelled before it finished.
    ///
    /// Distinct from a failure: the work stopped where it was asked to, and
    /// what had already been built stands. It surfaces as an error because it
    /// ends the operation that was in flight — a bootstrap, a build pass — and
    /// that operation has no result to return.
    Cancelled,
    /// A local pool operation failed.
    Pool(String),
    /// The work directory is already locked by another run.
    WorkLocked {
        /// The lockfile whose presence holds the lock.
        path: PathBuf,
        /// The process that took the lock, as the lockfile records it, or `None`
        /// when it holds nothing readable as a process id.
        holder: Option<u32>,
    },
    /// A host I/O operation failed.
    Io {
        /// What the operation was doing.
        op: &'static str,
        /// The path concerned.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Recipe { path, reason } => {
                write!(f, "recipe {}: {reason}", path.display())
            }
            Error::Source { component, reason } => {
                write!(f, "resolving source for {component}: {reason}")
            }
            Error::Control { component, reason } => {
                write!(f, "reading debian/control for {component}: {reason}")
            }
            Error::VersionTag { suite } => write!(
                f,
                "suite {suite:?} is not a numbered Debian release, so it has no \
                 known version tag; name the tag builds for it should carry"
            ),
            Error::Changelog { component, reason } => {
                write!(f, "reading debian/changelog for {component}: {reason}")
            }
            Error::Plan(reason) => write!(f, "cannot order the build: {reason}"),
            Error::Selection(reason) => write!(f, "invalid selection: {reason}"),
            Error::Debian(err) => write!(f, "the Debian provisioner: {err}"),
            Error::Provision(err) => write!(f, "provisioning a build root: {err}"),
            Error::Cage(err) => write!(f, "the build sandbox: {err}"),
            Error::Build { component, status } => {
                write!(f, "building {component}: dpkg-buildpackage {status}")
            }
            Error::Toolchain { version, reason } => write!(
                f,
                "installing the rustup {version} toolchain into a build root: {reason}"
            ),
            Error::Vendor { component, status } => {
                write!(f, "vendoring {component}: debian/rules clean {status}")
            }
            Error::Cancelled => write!(f, "the run was cancelled"),
            Error::Pool(reason) => write!(f, "the local pool: {reason}"),
            Error::WorkLocked { path, holder } => match holder {
                Some(pid) => write!(
                    f,
                    "the work directory is locked by process {pid} ({}); \
                     remove that file if that process is gone",
                    path.display()
                ),
                None => write!(
                    f,
                    "the work directory is locked by another run ({}); \
                     remove that file if no run is active",
                    path.display()
                ),
            },
            Error::Io { op, path, source } => {
                write!(f, "{op} {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Debian(err) => Some(err),
            Error::Provision(err) => Some(err),
            Error::Cage(err) => Some(err),
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<DebianError> for Error {
    fn from(err: DebianError) -> Self {
        Error::Debian(err)
    }
}

impl From<ProvisionError> for Error {
    fn from(err: ProvisionError) -> Self {
        match err {
            // A provision the run stopped is not a provisioning failure, and a
            // cancelled run must not read as a broken one. Every provisioning
            // call site converts through here, so the distinction holds
            // wherever a cancel can land.
            ProvisionError::Cancelled => Error::Cancelled,
            other => Error::Provision(other),
        }
    }
}

impl From<ferroday_cage::Error> for Error {
    fn from(err: ferroday_cage::Error) -> Self {
        Error::Cage(err)
    }
}

/// Builds an [`Error::Io`] from an operation label, a path, and an I/O error.
pub(crate) fn io_error(
    op: &'static str,
    path: impl Into<PathBuf>,
    source: std::io::Error,
) -> Error {
    Error::Io {
        op,
        path: path.into(),
        source,
    }
}
