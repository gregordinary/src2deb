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

/// The kind of thing a [`SourceInput`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// A git revision, valued by its full commit hash.
    Git,
    /// Content valued by its SHA-256 digest, in lowercase hexadecimal.
    Sha256,
    /// A patch series applied over a tree, valued by a SHA-256 digest over its
    /// members' contents in series order.
    ///
    /// A kind of its own rather than a [`Sha256`](Self::Sha256), because a
    /// reader of a record should be able to tell a series of local fixes from
    /// the archive of an upstream release without knowing which digest is
    /// which.
    Patches,
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
            SourceKind::Patches => "patches",
            SourceKind::Path => "path",
        }
    }

    /// Whether a value of this kind names the exact content it stood for, so a
    /// later run can tell the same input from a different one.
    ///
    /// A hash of either sort does. A path does not, and no amount of care at
    /// the point it is read changes that.
    pub fn is_pinned(self) -> bool {
        match self {
            SourceKind::Git | SourceKind::Sha256 | SourceKind::Patches => true,
            SourceKind::Path => false,
        }
    }
}

/// One input to a component's build: its kind and the value identifying it.
///
/// Constructed through the per-kind constructors, so a value never carries a
/// kind that does not describe it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "InputRecord", into = "InputRecord")]
pub struct SourceInput {
    kind: SourceKind,
    value: String,
}

impl SourceInput {
    /// A git revision, given its full commit hash.
    pub fn git(commit: impl Into<String>) -> SourceInput {
        SourceInput {
            kind: SourceKind::Git,
            value: commit.into(),
        }
    }

    /// Content identified by its SHA-256 digest, in lowercase hexadecimal.
    pub fn sha256(digest: impl Into<String>) -> SourceInput {
        SourceInput {
            kind: SourceKind::Sha256,
            value: digest.into(),
        }
    }

    /// A patch series, given a SHA-256 digest over its members' contents in
    /// series order.
    pub fn patches(digest: impl Into<String>) -> SourceInput {
        SourceInput {
            kind: SourceKind::Patches,
            value: digest.into(),
        }
    }

    /// A tree on disk, given the path it was read from.
    pub fn path(path: impl Into<String>) -> SourceInput {
        SourceInput {
            kind: SourceKind::Path,
            value: path.into(),
        }
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
            SourceKind::Git | SourceKind::Sha256 | SourceKind::Patches => {
                self.value.chars().take(SHORT_HASH).collect()
            }
            SourceKind::Path => LOCAL.to_string(),
        }
    }

    /// The input as the generated changelog entry names it: the bare hash for a
    /// git revision, and a kind-qualified value otherwise, so a digest is not
    /// mistaken for a commit.
    ///
    /// A path is rendered as [`LOCAL`] rather than as itself. This text is
    /// written into the package's changelog and ships inside the `.deb`, and a
    /// build host's directory layout is not something a package should carry;
    /// the manifest, which stays in the work directory, records the path.
    fn describe(&self) -> String {
        match self.kind {
            SourceKind::Git => self.value.clone(),
            SourceKind::Sha256 | SourceKind::Patches => {
                format!("{}:{}", self.kind.label(), self.value)
            }
            SourceKind::Path => LOCAL.to_string(),
        }
    }
}

/// The serialized form of a [`SourceInput`], carrying the pinned-ness the kind
/// implies.
///
/// The flag is written so a reader can tell a pinned input from an unpinned one
/// without knowing src2deb's table of kinds, and it is derived from the kind on
/// the way out and dropped on the way in — so a record can neither contradict
/// itself nor have a hand-edited flag change what a run decides.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InputRecord {
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
            kind: input.kind,
            value: input.value,
        }
    }
}

impl From<InputRecord> for SourceInput {
    fn from(record: InputRecord) -> SourceInput {
        SourceInput {
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

    /// The commit this fingerprint's git input names, or `None` when it has
    /// none.
    ///
    /// Deliberately git-specific, for the one place that is: packaging that
    /// stamps an upstream revision into the binary it builds reads a commit
    /// hash, and nothing else stands in for one.
    pub fn git_commit(&self) -> Option<&str> {
        self.inputs
            .iter()
            .find(|input| input.kind == SourceKind::Git)
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

    /// The fingerprint as the generated changelog entry names it: a git
    /// revision bare, an input of another kind qualified by its kind, separated
    /// by commas.
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
        let source = Fingerprint::of(SourceInput::git(COMMIT));
        assert_eq!(source.short(), "abc1234");
        assert_eq!(source.describe(), COMMIT);
        assert_eq!(source.git_commit(), Some(COMMIT));
        assert!(source.is_pinned());
    }

    #[test]
    fn a_digest_abbreviates_like_a_commit_but_names_its_kind_in_prose() {
        let source = Fingerprint::of(SourceInput::sha256(DIGEST));
        assert_eq!(source.short(), "9f8e7d6");
        assert_eq!(source.describe(), format!("sha256:{DIGEST}"));
        // Nothing here is a commit, so packaging that wants one gets nothing
        // rather than a digest standing in for it.
        assert_eq!(source.git_commit(), None);
        assert!(source.is_pinned());
    }

    #[test]
    fn a_path_is_unpinned_and_says_so_in_the_version() {
        let source = Fingerprint::of(SourceInput::path("/home/someone/cosmic-comp"));
        assert!(!source.is_pinned());
        assert_eq!(source.short(), "local");
        // The description ships inside the package, so it carries the marker
        // and not the build host's directory layout.
        assert_eq!(source.describe(), "local");
        // The manifest keeps the path itself, which is where it is useful.
        assert_eq!(source.inputs()[0].value(), "/home/someone/cosmic-comp");
    }

    #[test]
    fn a_composite_fingerprint_names_every_input_it_is_over() {
        let source = Fingerprint::over(vec![SourceInput::git(COMMIT), SourceInput::sha256(DIGEST)]);
        assert_eq!(source.short(), "abc1234.9f8e7d6");
        assert_eq!(source.describe(), format!("{COMMIT}, sha256:{DIGEST}"));
        assert_eq!(source.len(), 2);
        // The git input is still reachable for the one place that wants a
        // commit, even alongside inputs of other kinds.
        assert_eq!(source.git_commit(), Some(COMMIT));
    }

    #[test]
    fn one_unpinned_input_unpins_the_whole_fingerprint() {
        // A build is only as reproducible as its least reproducible input, so a
        // pinned upstream overlaid with a working tree is not a pinned build.
        let source = Fingerprint::over(vec![
            SourceInput::git(COMMIT),
            SourceInput::path("/home/someone/packaging"),
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
        let upstream = SourceInput::git(COMMIT);
        let patches = SourceInput::sha256(DIGEST);
        let alone = Fingerprint::of(upstream.clone());
        assert_eq!(alone, Fingerprint::of(SourceInput::git(COMMIT)));
        // A second input makes it a different source, which is what makes a
        // changed patch or overlay trigger a rebuild.
        assert_ne!(alone, Fingerprint::over(vec![upstream, patches]));
        // A value that moved is a different source at the same kind.
        assert_ne!(alone, Fingerprint::of(SourceInput::git("0000000")));
    }

    #[test]
    fn a_kind_and_a_value_do_not_compare_across_kinds() {
        // The same bytes reached by two routes are two different inputs: a
        // digest is not the commit that happens to spell the same.
        assert_ne!(
            Fingerprint::of(SourceInput::git(DIGEST)),
            Fingerprint::of(SourceInput::sha256(DIGEST)),
        );
    }

    #[test]
    fn a_short_form_carries_only_characters_a_debian_revision_allows() {
        // The abbreviation is spliced into a package version, where the grammar
        // admits alphanumerics and `+`, `.`, `~` alone.
        let source = Fingerprint::over(vec![
            SourceInput::git(COMMIT),
            SourceInput::path("/home/someone/tree with spaces"),
            SourceInput::sha256(DIGEST),
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
            SourceInput::git(COMMIT),
            SourceInput::path("/home/someone/tree"),
        ]));
        // The flag is written beside the kind, so the record states which
        // inputs a reproducibility claim can rest on.
        assert!(toml.contains("kind = \"git\""), "{toml}");
        assert!(toml.contains("pinned = true"), "{toml}");
        assert!(toml.contains("kind = \"path\""), "{toml}");
        assert!(toml.contains("pinned = false"), "{toml}");
    }

    #[test]
    fn a_fingerprint_round_trips_through_toml() {
        let source = Fingerprint::over(vec![
            SourceInput::git(COMMIT),
            SourceInput::sha256(DIGEST),
            SourceInput::path("/home/someone/tree"),
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
}
