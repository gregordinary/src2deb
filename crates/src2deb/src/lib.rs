//! src2deb builds Debian `.deb` packages from source, each in an unprivileged
//! [ferroday-cage](ferroday_cage) sandbox.
//!
//! Its first target is the COSMIC desktop (cosmic-epoch): 27 components, built
//! from source for Debian Trixie and Forky using the `debian/` trees upstream
//! ships. Each component is built with a hermetic bake — a build root of
//! `base system + toolchain + that component's build-dependencies`, then
//! `dpkg-buildpackage` in a cage with an isolated network — and the one
//! inter-component build edge is carried by a local `.deb` [`pool`] that each
//! build feeds and the next build resolves against.
//!
//! # Pipeline
//!
//! [`engine::Engine`] drives the loop. For a [`recipe::Recipe`] it:
//!
//! 1. resolves each component's [`source`] into an unpacked tree — a git
//!    checkout, or a copy of a tree already on disk,
//! 2. reads every `debian/control`, computes the component dependency graph,
//!    and topologically orders it ([`plan`]),
//! 3. for each component in order, provisions a build root ([`provision`]),
//!    runs the build ([`build`]), and publishes the results to the local
//!    [`pool`].
//!
//! # The ferroday-cage seam
//!
//! Everything that touches ferroday-cage sits behind [`provision`] and
//! [`pool`], so the provisioning strategy and the pool writer can change
//! without touching the engine. [`provision::BuildRootProvider`] has two
//! implementations: [`provision::LayeredProvision`], which bootstraps one
//! shared base and stages each component's build-dependency delta into a
//! disposable overlay, and [`provision::FullReprovision`], which bakes a fresh
//! rootfs per component. The engine prefers the layered strategy and falls back
//! to full reprovisioning on a host without unprivileged overlay support. The
//! [`pool`] writes with ferroday-cage's `Pool`.

pub mod arch;
pub mod build;
pub mod cancel;
pub mod engine;
pub mod error;
pub mod export;
pub mod fingerprint;
pub mod lock;
pub mod manifest;
pub mod observer;
pub mod plan;
pub mod pool;
pub mod provision;
pub mod recipe;
mod schedule;
pub mod source;
mod tarball;
pub mod toolchain;
pub mod version;

pub use build::{Artifact, BuildInfo};
pub use cancel::Cancel;
pub use engine::{
    BuildDate, Built, Engine, Failed, Package, PlanReport, PlannedComponent, Progress, RunOptions,
    RunReport, Selection, SkipReason, Skipped,
};
pub use error::{Error, Result};
pub use export::{ExportOptions, ExportReport};
pub use fingerprint::{Fingerprint, SourceInput, SourceKind, SourceRole};
pub use manifest::Manifest;
pub use recipe::{Component, Origin, Recipe, Source, VersionFrom, VersionSource};
