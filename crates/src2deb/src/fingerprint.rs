//! Source identity: what a component was built from.
//!
//! Three parts of a run need to name a component's source. The version stamp
//! carries an abbreviation of it, so the source a package came from is legible
//! from `apt policy` alone; the provenance manifest records it in full, so a run
//! can be traced back to its inputs; and `--skip-published` compares it against
//! what a prior run recorded, so an unchanged component is not rebuilt.
//!
//! A git checkout answers all three with a commit hash, and nothing else does.
//! Source identity is therefore a [`Fingerprint`]: a sequence of
//! [`SourceInput`]s, each naming its [`SourceKind`] alongside the value that
//! identifies it.
//!
//! # Pinned and unpinned inputs
//!
//! An input is *pinned* when its value names the exact content that went into
//! the build, so a later run can tell the same input from a different one. A
//! commit hash and a content hash both do. A path does not: it names where a
//! tree was read from, and the tree may be anything by the time the record is
//! read.
//!
//! The distinction is load-bearing rather than descriptive. `--skip-published`
//! skips a component only when its fingerprint is pinned and matches
//! ([`ComponentRecord::is_built_at`](crate::manifest::ComponentRecord::is_built_at)),
//! because an unpinned input that compares equal says nothing about whether the
//! source changed. The manifest records the flag as well as the kind, so a
//! reader can tell a reproducible build from one that only looks like one
//! without knowing which kinds are which.
//!
//! # Composition
//!
//! A component may have more than one input, and the fingerprint is over the
//! set: whichever input changes, the fingerprint changes with it. Inputs keep
//! the order they were resolved in, so two fingerprints over the same inputs
//! compare equal and the abbreviation is stable from run to run.
//!
//! Each input also names its [`SourceRole`] — what part it played in assembling
//! the tree. The kind and the role answer different questions, and one does not
//! imply the other: a component whose packaging comes from a second repository
//! has two `git` inputs, and only the role says which is the source it was
//! built from and which is the packaging it was built with.

use serde::{Deserialize, Serialize};

/// How many characters of a hash an abbreviation carries.
///
/// Seven is git's own conventional abbreviation, and enough to identify a
/// revision within a single component's history.
const SHORT_HASH: usize = 7;

/// The abbreviation an unpinned input carries, in place of a hash it has none
/// of.
///
/// It reaches the package version, so it is chosen to be unmistakable in `apt
/// policy` output: a package built from a working tree announces itself rather
/// than passing for one built from a pinned revision.
const LOCAL: &str = "local";

/// Renders a digest as the lowercase hexadecimal the pinned kinds take.
///
/// Kept here rather than beside either producer, so that every value recorded
/// as a hash is spelled the same way and two records of the same bytes always
/// compare equal.
pub(crate) fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What part a [`SourceInput`] played in assembling a component's tree.
///
/// Independent of the [`SourceKind`] that identifies it: a git repository can
/// be either of the first two, and a tree on disk can as well. The role is what
/// tells two inputs of the same kind apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceRole {
    /// The component's own source: the tree everything else is applied to.
    ///
    /// The default a record with no role reads as, since it is the one role
    /// every component has.
    #[default]
    Source,
    /// A packaging overlay, whose `debian/` directory became the component's.
    Packaging,
    /// A patch series applied over the assembled tree.
    Patches,
}

impl SourceRole {
    /// The role's name, as the manifest records it.
    pub fn label(self) -> &'static str {
        match self {
            SourceRole::Source => "source",
            SourceRole::Packaging => "packaging",
            SourceRole::Patches => "patches",
        }
    }
}

/// The kind of thing a [`SourceInput`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// A git revision, valued by its full commit hash.
    Git,
    /// Content valued by its SHA-256 digest, in lowercase hexadecimal.
    Sha256,
    /// A Debian source package, valued by the SHA-256 digest of its `.dsc`.
    ///
    /// A kind of its own rather than a [`Sha256`](Self::Sha256), even though it
    /// is the same declared-and-verified digest over a fetched file, because
    /// what it identifies is not the same kind of thing. A `.dsc` names its
    /// component tarballs and the digest of each, so this one value pins a whole
    /// source package rather than one archive — and a component built from one
    /// is built without the [vendor pass](crate::build), which is a claim about
    /// how the packages were produced that the provenance record should carry.
    Dsc,
    /// A patch series applied over a tree, valued by a SHA-256 digest over its
    /// members' contents in series order.
    ///
    /// A kind of its own rather than a [`Sha256`](Self::Sha256), because a
    /// reader of a record should be able to tell a series of local fixes from
    /// the archive of an upstream release without knowing which digest is
    /// which.
    Patches,
    /// A directory on disk, valued by a SHA-256 digest over its contents.
    ///
    /// What a [packaging overlay](crate::Component::packaging) taken from a
    /// path records. Only the `debian/` tree the overlay supplied is digested,
    /// which is exactly what src2deb copied out of it, so the value names what
    /// reached the build and not what happened to sit beside it.
    ///
    /// Pinned, and a kind of its own for the same reason
    /// [`Patches`](Self::Patches) is: a [`Sha256`](Self::Sha256) is a digest
    /// something else declared and src2deb verified — the recipe names the
    /// artefact and the archive still serves it — while this one src2deb
    /// measured off a directory the recipe pointed at. Both pin content; only
    /// the first can be fetched again from what the record holds.
    Tree,
    /// A tree on disk, valued by its path. The only unpinned kind: the path
    /// says where the tree was read from and nothing about what it held.
    Path,
}

impl SourceKind {
    /// The kind's name, as the manifest records it.
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Git => "git",
            SourceKind::Sha256 => "sha256",
            SourceKind::Dsc => "dsc",
            SourceKind::Patches => "patches",
            SourceKind::Tree => "tree",
            SourceKind::Path => "path",
        }
    }

    /// Whether a value of this kind names the exact content it stood for, so a
    /// later run can tell the same input from a different one.
    ///
    /// A hash of any sort does. A path does not, and no amount of care at the
    /// point it is read changes that.
    pub fn is_pinned(self) -> bool {
        match self {
            SourceKind::Git
            | SourceKind::Sha256
            | SourceKind::Dsc
            | SourceKind::Patches
            | SourceKind::Tree => true,
            SourceKind::Path => false,
        }
    }
}

/// One input to a component's build: the part it played, its kind, and the
/// value identifying it.
///
/// Constructed through the per-kind constructors, so a value never carries a
/// kind that does not describe it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "InputRecord", into = "InputRecord")]
pub struct SourceInput {
    role: SourceRole,
    kind: SourceKind,
    value: String,
}

impl SourceInput {
    /// A git revision in `role`, given its full commit hash.
    pub fn git(role: SourceRole, commit: impl Into<String>) -> SourceInput {
        SourceInput {
            role,
            kind: SourceKind::Git,
            value: commit.into(),
        }
    }

    /// Content in `role` identified by its SHA-256 digest, in lowercase
    /// hexadecimal.
    pub fn sha256(role: SourceRole, digest: impl Into<String>) -> SourceInput {
        SourceInput {
            role,
            kind: SourceKind::Sha256,
            value: digest.into(),
        }
    }

    /// A Debian source package in `role`, given the SHA-256 digest of its
    /// `.dsc`.
    ///
    /// See [`SourceKind::Dsc`] for what separates this from
    /// [`sha256`](Self::sha256), which carries the same shape of value.
    pub fn dsc(role: SourceRole, digest: impl Into<String>) -> SourceInput {
        SourceInput {
            role,
            kind: SourceKind::Dsc,
            value: digest.into(),
        }
    }

    /// A directory on disk in `role`, given a SHA-256 digest over its contents.
    ///
    /// See [`SourceKind::Tree`] for what separates this from
    /// [`sha256`](Self::sha256), which carries the same shape of value.
    pub fn tree(role: SourceRole, digest: impl Into<String>) -> SourceInput {
        SourceInput {
            role,
            kind: SourceKind::Tree,
            value: digest.into(),
        }
    }

    /// A tree on disk in `role`, given the path it was read from.
    pub fn path(role: SourceRole, path: impl Into<String>) -> SourceInput {
        SourceInput {
            role,
            kind: SourceKind::Path,
            value: path.into(),
        }
    }

    /// A patch series, given a SHA-256 digest over its members' contents in
    /// series order.
    ///
    /// The only constructor that takes no role, because a patch series is only
    /// ever in one: nothing else applies a series, and a series is applied as
    /// nothing else.
    pub fn patches(digest: impl Into<String>) -> SourceInput {
        SourceInput {
            role: SourceRole::Patches,
            kind: SourceKind::Patches,
            value: digest.into(),
        }
    }

    /// The part this input played in assembling the tree.
    pub fn role(&self) -> SourceRole {
        self.role
    }

    /// The kind of input this is.
    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    /// The value identifying the input: a commit hash, a digest, or a path.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Whether the input names the exact content it stood for. See
    /// [`SourceKind::is_pinned`].
    pub fn is_pinned(&self) -> bool {
        self.kind.is_pinned()
    }

    /// The input abbreviated for a package version: the first
    /// [`SHORT_HASH`] characters of a hash, or [`LOCAL`] for a kind that has
    /// none.
    fn short(&self) -> String {
        match self.kind {
            SourceKind::Git
            | SourceKind::Sha256
            | SourceKind::Dsc
            | SourceKind::Patches
            | SourceKind::Tree => self.value.chars().take(SHORT_HASH).collect(),
            SourceKind::Path => LOCAL.to_string(),
        }
    }

    /// The input as the generated changelog entry names it: the role it played,
    /// then the value.
    ///
    /// The value is the bare hash for a git revision, which is what a revision
    /// looks like, and a kind-qualified value where the kind says something the
    /// role does not — so a digest is not mistaken for a commit. A patch series'
    /// kind only repeats its role, so it is left off.
    ///
    /// A path is rendered as [`LOCAL`] rather than as itself. This text is
    /// written into the package's changelog and ships inside the `.deb`, and a
    /// build host's directory layout is not something a package should carry;
    /// the manifest, which stays in the work directory, records the path.
    fn describe(&self) -> String {
        let value = match self.kind {
            SourceKind::Git | SourceKind::Patches => self.value.clone(),
            SourceKind::Sha256 | SourceKind::Dsc | SourceKind::Tree => {
                format!("{}:{}", self.kind.label(), self.value)
            }
            SourceKind::Path => LOCAL.to_string(),
        };
        format!("{} {value}", self.role.label())
    }
}

/// The serialized form of a [`SourceInput`], carrying the pinned-ness the kind
/// implies.
///
/// The flag is written so a reader can tell a pinned input from an unpinned one
/// without knowing src2deb's table of kinds, and it is derived from the kind on
/// the way out and dropped on the way in — so a record can neither contradict
/// itself nor have a hand-edited flag change what a run decides.
///
/// The role, unlike the flag, is carried both ways: it is something the run
/// established rather than something the kind implies.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InputRecord {
    /// The part the input played in assembling the tree.
    #[serde(default)]
    role: SourceRole,
    /// The kind of input.
    kind: SourceKind,
    /// The value identifying it.
    value: String,
    /// Whether the value names the exact content it stood for.
    #[serde(default)]
    pinned: bool,
}

impl From<SourceInput> for InputRecord {
    fn from(input: SourceInput) -> InputRecord {
        InputRecord {
            pinned: input.kind.is_pinned(),
            role: input.role,
            kind: input.kind,
            value: input.value,
        }
    }
}

impl From<InputRecord> for SourceInput {
    fn from(record: InputRecord) -> SourceInput {
        SourceInput {
            role: record.role,
            kind: record.kind,
            value: record.value,
        }
    }
}

/// What a component was built from: every input, in the order they were
/// resolved.
///
/// Compares as the sequence it holds, which is what `--skip-published` asks of
/// it: two runs resolving the same inputs produce equal fingerprints, and any
/// input that moves makes them differ.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fingerprint {
    inputs: Vec<SourceInput>,
}

impl Fingerprint {
    /// A fingerprint over a single input.
    pub fn of(input: SourceInput) -> Fingerprint {
        Fingerprint {
            inputs: vec![input],
        }
    }

    /// A fingerprint over several inputs, in the order they were resolved.
    pub fn over(inputs: Vec<SourceInput>) -> Fingerprint {
        Fingerprint { inputs }
    }

    /// The fingerprint of a component that resolved no source at all.
    ///
    /// Recorded for a component that failed before it had one — its repository
    /// would not clone, or its `debian/control` would not read — so the
    /// manifest says it never got that far rather than naming an input it never
    /// reached.
    pub fn none() -> Fingerprint {
        Fingerprint::default()
    }

    /// The inputs, in resolution order.
    pub fn inputs(&self) -> &[SourceInput] {
        &self.inputs
    }

    /// Whether the fingerprint names no input at all. See
    /// [`none`](Self::none).
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }

    /// Whether every input is pinned, so the fingerprint names exactly what the
    /// build consumed.
    ///
    /// A fingerprint with no inputs is not pinned: nothing was recorded, which
    /// is not the same as everything being reproducible.
    pub fn is_pinned(&self) -> bool {
        !self.inputs.is_empty() && self.inputs.iter().all(SourceInput::is_pinned)
    }

    /// The commit the component's own source was checked out at, or `None` when
    /// it did not come from git.
    ///
    /// Deliberately git-specific, for the one place that is: packaging that
    /// stamps an upstream revision into the binary it builds reads a commit
    /// hash, and nothing else stands in for one.
    ///
    /// Matched on the role as well as the kind. A component whose packaging
    /// comes from a second repository has two git inputs, and packaging asking
    /// for the revision it was built from means the source's, never the
    /// packaging repository's own.
    pub fn git_commit(&self) -> Option<&str> {
        self.inputs
            .iter()
            .find(|input| input.role == SourceRole::Source && input.kind == SourceKind::Git)
            .map(SourceInput::value)
    }

    /// The fingerprint abbreviated for a package version: each input's
    /// abbreviation, joined with `.`.
    ///
    /// A single git input gives git's own seven-character revision, so an
    /// ordinary build's version reads as it always has. Every input appears, so
    /// a package built from a patched or overlaid source is distinguishable
    /// from one built from the upstream tree alone.
    ///
    /// The separator and the abbreviations are all characters a Debian revision
    /// allows, and `.` compares component-wise, so a version carrying one
    /// orders the way the parts do. See [`crate::version`].
    pub fn short(&self) -> String {
        self.inputs
            .iter()
            .map(SourceInput::short)
            .collect::<Vec<_>>()
            .join(".")
    }

    /// The fingerprint as the generated changelog entry names it: each input as
    /// the part it played and the value identifying it, separated by commas.
    ///
    /// This text ships inside the package, so it carries no host path: a path
    /// input is named by the same marker the version carries, and the manifest
    /// keeps the path itself.
    pub fn describe(&self) -> String {
        self.inputs
            .iter()
            .map(SourceInput::describe)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// How many inputs the fingerprint holds.
    pub fn len(&self) -> usize {
        self.inputs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "abc1234def5678901234567890abcdef12345678";
    const DIGEST: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0";
    const PACKAGING_COMMIT: &str = "def5678abc1234567890abcdef1234567890abcd";

    /// A component's own source, at `COMMIT`.
    fn source() -> SourceInput {
        SourceInput::git(SourceRole::Source, COMMIT)
    }

    /// A fingerprint as the manifest carries one: a field of a table. A TOML
    /// document is a table, so a sequence has to be held by something to be
    /// written at all.
    #[derive(Debug, Serialize, Deserialize)]
    struct Held {
        source: Fingerprint,
    }

    /// The TOML a fingerprint is recorded as.
    fn record(source: &Fingerprint) -> String {
        toml::to_string(&Held {
            source: source.clone(),
        })
        .expect("a fingerprint serializes")
    }

    #[test]
    fn a_git_fingerprint_abbreviates_exactly_as_git_does() {
        // The version stamp carries this, so it is the one abbreviation that
        // cannot move: it is what every published version already reads as.
        let source = Fingerprint::of(source());
        assert_eq!(source.short(), "abc1234");
        assert_eq!(source.describe(), format!("source {COMMIT}"));
        assert_eq!(source.git_commit(), Some(COMMIT));
        assert!(source.is_pinned());
    }

    #[test]
    fn a_digest_abbreviates_like_a_commit_but_names_its_kind_in_prose() {
        let source = Fingerprint::of(SourceInput::sha256(SourceRole::Source, DIGEST));
        assert_eq!(source.short(), "9f8e7d6");
        assert_eq!(source.describe(), format!("source sha256:{DIGEST}"));
        // Nothing here is a commit, so packaging that wants one gets nothing
        // rather than a digest standing in for it.
        assert_eq!(source.git_commit(), None);
        assert!(source.is_pinned());
    }

    #[test]
    fn a_digested_directory_is_pinned_and_names_itself_as_one() {
        // What a packaging overlay taken from a path records. It abbreviates
        // like any other hash, and its prose names the kind, so a reader is not
        // left to take a digest for a commit.
        let source = Fingerprint::over(vec![
            source(),
            SourceInput::tree(SourceRole::Packaging, DIGEST),
        ]);
        assert_eq!(source.short(), "abc1234.9f8e7d6");
        assert_eq!(
            source.describe(),
            format!("source {COMMIT}, packaging tree:{DIGEST}"),
        );
        assert!(source.is_pinned());
        // ...and it is not the same input as a declared checksum of the same
        // bytes. One was measured off a directory the recipe pointed at; the
        // other pins an artefact that can be fetched again.
        assert_ne!(
            Fingerprint::of(SourceInput::tree(SourceRole::Packaging, DIGEST)),
            Fingerprint::of(SourceInput::sha256(SourceRole::Packaging, DIGEST)),
        );
    }

    #[test]
    fn a_path_is_unpinned_and_says_so_in_the_version() {
        let source = Fingerprint::of(SourceInput::path(
            SourceRole::Source,
            "/home/someone/cosmic-comp",
        ));
        assert!(!source.is_pinned());
        assert_eq!(source.short(), "local");
        // The description ships inside the package, so it carries the marker
        // and not the build host's directory layout.
        assert_eq!(source.describe(), "source local");
        // The manifest keeps the path itself, which is where it is useful.
        assert_eq!(source.inputs()[0].value(), "/home/someone/cosmic-comp");
    }

    #[test]
    fn a_composite_fingerprint_names_every_input_it_is_over() {
        let source = Fingerprint::over(vec![
            source(),
            SourceInput::sha256(SourceRole::Packaging, DIGEST),
        ]);
        assert_eq!(source.short(), "abc1234.9f8e7d6");
        assert_eq!(
            source.describe(),
            format!("source {COMMIT}, packaging sha256:{DIGEST}"),
        );
        assert_eq!(source.len(), 2);
        // The git input is still reachable for the one place that wants a
        // commit, even alongside inputs of other kinds.
        assert_eq!(source.git_commit(), Some(COMMIT));
    }

    #[test]
    fn two_inputs_of_one_kind_are_told_apart_by_the_part_they_played() {
        // A component whose packaging comes from a second repository has two
        // git inputs, and nothing but the role distinguishes them.
        let overlaid = Fingerprint::over(vec![
            source(),
            SourceInput::git(SourceRole::Packaging, PACKAGING_COMMIT),
        ]);
        assert_eq!(
            overlaid.describe(),
            format!("source {COMMIT}, packaging {PACKAGING_COMMIT}"),
        );
        // Packaging asking for the revision it was built from means the
        // source's, never the packaging repository's own.
        assert_eq!(overlaid.git_commit(), Some(COMMIT));

        // The order the roles appear in is not what answers that question: a
        // fingerprint assembled the other way round still names the source.
        let reversed = Fingerprint::over(vec![
            SourceInput::git(SourceRole::Packaging, PACKAGING_COMMIT),
            source(),
        ]);
        assert_eq!(reversed.git_commit(), Some(COMMIT));

        // A component with no source of its own in git has no commit to give,
        // whatever its packaging came from.
        let local = Fingerprint::over(vec![
            SourceInput::path(SourceRole::Source, "/home/someone/tree"),
            SourceInput::git(SourceRole::Packaging, PACKAGING_COMMIT),
        ]);
        assert_eq!(local.git_commit(), None);
    }

    #[test]
    fn a_patch_series_carries_the_one_role_it_can_have() {
        // Its kind only repeats the part it played, so the description states
        // that once rather than twice.
        let patches = SourceInput::patches(DIGEST);
        assert_eq!(patches.role(), SourceRole::Patches);
        assert_eq!(patches.kind(), SourceKind::Patches);
        assert_eq!(
            Fingerprint::over(vec![source(), patches]).describe(),
            format!("source {COMMIT}, patches {DIGEST}"),
        );
    }

    #[test]
    fn one_unpinned_input_unpins_the_whole_fingerprint() {
        // A build is only as reproducible as its least reproducible input, so a
        // pinned upstream overlaid with a working tree is not a pinned build.
        let source = Fingerprint::over(vec![
            source(),
            SourceInput::path(SourceRole::Packaging, "/home/someone/packaging"),
        ]);
        assert!(!source.is_pinned());
        assert_eq!(source.short(), "abc1234.local");
    }

    #[test]
    fn an_empty_fingerprint_is_neither_pinned_nor_a_revision() {
        // What a component that failed before resolving anything records.
        // Vacuously pinned would be a lie: nothing was recorded at all.
        let none = Fingerprint::none();
        assert!(none.is_empty());
        assert!(!none.is_pinned());
        assert_eq!(none.git_commit(), None);
        assert_eq!(none.short(), "");
    }

    #[test]
    fn fingerprints_compare_over_their_whole_input_set() {
        let upstream = source();
        let patches = SourceInput::patches(DIGEST);
        let alone = Fingerprint::of(upstream.clone());
        assert_eq!(alone, Fingerprint::of(source()));
        // A second input makes it a different source, which is what makes a
        // changed patch or overlay trigger a rebuild.
        assert_ne!(alone, Fingerprint::over(vec![upstream, patches]));
        // A value that moved is a different source at the same kind.
        assert_ne!(
            alone,
            Fingerprint::of(SourceInput::git(SourceRole::Source, "0000000")),
        );
    }

    #[test]
    fn a_kind_and_a_value_do_not_compare_across_kinds() {
        // The same bytes reached by two routes are two different inputs: a
        // digest is not the commit that happens to spell the same.
        assert_ne!(
            Fingerprint::of(SourceInput::git(SourceRole::Source, DIGEST)),
            Fingerprint::of(SourceInput::sha256(SourceRole::Source, DIGEST)),
        );
    }

    #[test]
    fn the_same_revision_in_two_roles_is_two_different_inputs() {
        // A repository serving as both a component's source and its packaging
        // contributes twice, and a run that stopped doing one of the two is a
        // run that built something else.
        assert_ne!(
            Fingerprint::of(source()),
            Fingerprint::of(SourceInput::git(SourceRole::Packaging, COMMIT)),
        );
    }

    #[test]
    fn a_short_form_carries_only_characters_a_debian_revision_allows() {
        // The abbreviation is spliced into a package version, where the grammar
        // admits alphanumerics and `+`, `.`, `~` alone.
        let source = Fingerprint::over(vec![
            source(),
            SourceInput::path(SourceRole::Packaging, "/home/someone/tree with spaces"),
            SourceInput::patches(DIGEST),
        ]);
        assert!(
            source
                .short()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '~'),
            "{}",
            source.short(),
        );
    }

    #[test]
    fn an_input_records_the_pinned_ness_its_kind_implies() {
        let toml = record(&Fingerprint::over(vec![
            source(),
            SourceInput::path(SourceRole::Packaging, "/home/someone/tree"),
        ]));
        // The flag is written beside the kind, so the record states which
        // inputs a reproducibility claim can rest on.
        assert!(toml.contains("kind = \"git\""), "{toml}");
        assert!(toml.contains("pinned = true"), "{toml}");
        assert!(toml.contains("kind = \"path\""), "{toml}");
        assert!(toml.contains("pinned = false"), "{toml}");
        // ...and the role beside both, so a reader of the record needs neither
        // the order nor the guide to tell the two apart.
        assert!(toml.contains("role = \"source\""), "{toml}");
        assert!(toml.contains("role = \"packaging\""), "{toml}");
    }

    #[test]
    fn a_fingerprint_round_trips_through_toml() {
        let source = Fingerprint::over(vec![
            source(),
            SourceInput::git(SourceRole::Packaging, PACKAGING_COMMIT),
            SourceInput::patches(DIGEST),
        ]);
        let parsed: Held = toml::from_str(&record(&source)).unwrap();
        assert_eq!(parsed.source, source);
    }

    #[test]
    fn a_recorded_pinned_flag_never_overrides_the_kind() {
        // The flag is for the reader. A record claiming a path is pinned must
        // not make `--skip-published` skip a working tree, so the kind decides
        // and the flag is dropped on the way in.
        let held: Held =
            toml::from_str("[[source]]\nkind = \"path\"\nvalue = \"/tmp/t\"\npinned = true\n")
                .expect("a hand-written record parses");
        assert!(!held.source.is_pinned());
        // ...and re-serializing states the kind's own answer.
        assert!(record(&held.source).contains("pinned = false"));
    }

    #[test]
    fn a_record_naming_no_role_reads_as_the_component_s_own_source() {
        // The role every component has, and the only one a record written
        // before roles existed could have described.
        let held: Held = toml::from_str(&format!(
            "[[source]]\nkind = \"git\"\nvalue = \"{COMMIT}\"\n"
        ))
        .expect("a record with no role parses");
        assert_eq!(held.source.inputs()[0].role(), SourceRole::Source);
        assert_eq!(held.source.git_commit(), Some(COMMIT));
    }
}
