//! Build-time version stamping.
//!
//! A component's version comes from its own `debian/changelog`, which upstream
//! controls and which does not move when only the toolchain or the packaging
//! around it does. Two builds of the same pinned revision would otherwise
//! produce the same version, and apt would never offer the second as an upgrade
//! over the first.
//!
//! src2deb therefore stamps each build, prepending a changelog entry whose
//! version carries the target suite, the build date, and the source revision:
//!
//! ```text
//! 1.0.0~alpha.7-1+deb13.20260731.abc1234
//! ```
//!
//! The parts are chosen for how `dpkg` orders versions, not for looks:
//!
//! - `+` starts the suffix, and an empty string sorts before any character
//!   other than `~`, so a stamped build sorts *after* the plain upstream
//!   version it was built from. A component rebuilding a source the archive
//!   also ships starts it with `~` instead — see [`VersionStamp`].
//! - The suite appears as `deb13`/`deb14`, never as `trixie`/`forky`. Spelled
//!   out, `forky` sorts before `trixie`, so a user moving from trixie to forky
//!   would see the forky packages as a downgrade. The release numbers sort the
//!   way the releases do.
//! - The date is `YYYYMMDD`, which compares numerically within its digit run,
//!   so each build sorts after the one before it.
//! - The abbreviated source fingerprint makes what a package was built from
//!   legible from `apt policy` alone. See [`crate::fingerprint`].
//!
//! The date is the build date rather than the revision's commit date, so a
//! rebuild of unchanged pinned sources still supersedes its predecessor. That
//! is deliberate: it is what lets a rebuild ship a fixed toolchain or a patched
//! vendored dependency to users who already installed the previous build. The
//! cost is that a rebuild which changes nothing still looks like an upgrade, so
//! how often the stamp moves is decided by how often a build runs.
//!
//! The stamp is applied to the build's private copy of the source tree, inside
//! the cage, so the resolved checkout on the host keeps upstream's changelog.
//!
//! # Packaging that carries no changelog
//!
//! Not every component has a `debian/changelog` to extend. A source with no
//! `debian/` of its own, packaged from a
//! [second tree](crate::Component::packaging) or from a directory kept beside
//! the recipe, has a `control` and a `rules` but no release history — and the
//! version stamp has nothing to build on.
//!
//! Such a component names its version in the recipe, and src2deb writes the
//! changelog it would otherwise have read: one entry, declaring that version,
//! signed with a declared maintainer identity. See
//! [`synthesized_changelog`]. The stamping path above then extends that entry
//! exactly as it extends upstream's, so one code path produces every version
//! src2deb stamps.
//!
//! The identity is still never invented. It comes from the recipe's own
//! `maintainer` setting, or failing that from the `Maintainer` field the
//! component's `debian/control` already declares — which Debian policy makes
//! mandatory, so a component that can be built at all carries one.
//!
//! # Rebuilding a source the archive also ships
//!
//! A stamped version outranks the version it was built from, which is what a
//! build of software the archive does not carry wants: each rebuild supersedes
//! the last, and nothing else claims the name. A component rebuilt from a
//! Debian source package is the other case — the archive ships that package
//! too, or will — and there the stamp outranking the archive's own copy is the
//! trap Debian's `~bpo` convention exists to avoid: the rebuild would win
//! forever, including after an upgrade to the suite whose package it was built
//! from.
//!
//! Such a component names [`VersionStamp::Backport`], which joins the stamp
//! with `~` in place of `+` and so sorts the build *below* the archive's own
//! package of the same version. Nothing else about the stamp changes.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fingerprint::Fingerprint;

/// The Debian releases whose suite names src2deb maps to a version tag.
///
/// A recipe targeting a suite absent from this table names its own tag with
/// `version-tag`, because there is no safe guess: falling back to the suite
/// name would reintroduce the ordering trap the tag exists to avoid.
const DEBIAN_SUITES: &[(&str, &str)] = &[
    ("buster", "deb10"),
    ("bullseye", "deb11"),
    ("bookworm", "deb12"),
    ("trixie", "deb13"),
    ("forky", "deb14"),
    ("duke", "deb15"),
];

/// The version tag for a Debian suite name, or `None` when the suite is not a
/// numbered Debian release src2deb knows.
///
/// A qualified suite takes the tag of the release it qualifies, so
/// `trixie-backports` and `trixie-security` both tag as `deb13`: they are
/// archives *for* that release, and a package built from one installs alongside
/// packages built from the other.
///
/// Rolling suites (`sid`, `unstable`, `testing`) are deliberately absent: they
/// carry no release number, and any tag invented for them would not order
/// against the numbered ones.
pub fn suite_tag(suite: &str) -> Option<&'static str> {
    let release = suite.split('-').next().unwrap_or(suite);
    DEBIAN_SUITES
        .iter()
        .find(|(name, _)| *name == release)
        .map(|(_, tag)| *tag)
}

/// How a stamped version orders against the archive's own package of the same
/// version.
///
/// The stamp joins onto the base version with a single character, and that
/// character settles the whole relationship: `+` sorts above the version it
/// extends, `~` sorts below it. Nothing else the stamp claims depends on the
/// choice — under either, a later build date supersedes an earlier one, a later
/// suite supersedes an earlier one, and a later Debian revision supersedes an
/// earlier one.
///
/// The choice is per component, defaulting to [`Supersede`](Self::Supersede),
/// with a recipe-level default of its own. See
/// [`Component::version_stamp`](crate::Component::version_stamp) and
/// [Rebuilding a source the archive also
/// ships](self#rebuilding-a-source-the-archive-also-ships).
///
/// Changing it changes the version a component builds as while every input its
/// fingerprint names stays exactly where it was, so it is recorded in the
/// manifest and compared by `--skip-published` — the same treatment a declared
/// [`version`](crate::Component::version) gets, and for the same reason. See
/// [`ComponentRecord::is_built_at`](crate::manifest::ComponentRecord::is_built_at).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VersionStamp {
    /// `1.2.3-4` stamps as `1.2.3-4+deb14.20260802.abc1234`, which sorts
    /// **above** `1.2.3-4`.
    ///
    /// The default, and what a build of software the archive does not carry
    /// wants: nothing else publishes that package, so there is nothing for the
    /// build to defer to and each rebuild supersedes the one before it.
    #[default]
    Supersede,
    /// `1.2.3-4` stamps as `1.2.3-4~deb14.20260802.abc1234`, which sorts
    /// **below** `1.2.3-4`.
    ///
    /// What a rebuild of a source the archive also ships wants, and the
    /// ordering Debian's own `~bpo` suffix produces. The archive's package of
    /// that version wins wherever it is available, so the rebuild fills a gap
    /// rather than occupying the name permanently — while still superseding the
    /// archive's *earlier* versions, and still superseding the rebuild before
    /// it.
    ///
    /// Turning it on for a package already published under
    /// [`Supersede`](Self::Supersede) lowers that package's version, which apt
    /// treats as a downgrade and does not offer. Choose it when the component is
    /// first declared.
    Backport,
}

impl VersionStamp {
    /// The character that joins the stamp onto the base version.
    ///
    /// The whole of this type comes down to this one character; see the variant
    /// documentation for what each produces.
    fn joiner(self) -> char {
        match self {
            VersionStamp::Supersede => '+',
            VersionStamp::Backport => '~',
        }
    }
}

/// The head of a `debian/changelog`: what src2deb needs in order to write an
/// entry above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogHead {
    /// The source package name, from the first line.
    pub source: String,
    /// The version the entry declares.
    pub version: String,
    /// The maintainer identity from the entry's trailer, reused verbatim so the
    /// stamped entry is a valid changelog without src2deb inventing an identity
    /// of its own. The entry text records that the build was automated.
    pub maintainer: String,
}

/// Parses the topmost entry of a `debian/changelog`.
///
/// The first line is `source (version) distribution; urgency=...`, and the
/// entry ends with a trailer line ` -- Maintainer Name <email>  Date`. Returns
/// `None` when the text does not open with a well-formed entry.
pub fn parse_changelog(text: &str) -> Option<ChangelogHead> {
    let first = text.lines().find(|line| !line.trim().is_empty())?;
    let open = first.find(" (")?;
    let source = first[..open].trim().to_string();
    let rest = &first[open + 2..];
    let close = rest.find(')')?;
    let version = rest[..close].trim().to_string();
    if source.is_empty() || version.is_empty() {
        return None;
    }
    // The trailer of the first entry: the first line introduced by " -- ". The
    // date follows the maintainer after two spaces.
    let trailer = text.lines().find(|line| line.starts_with(" -- "))?;
    let body = &trailer[4..];
    let maintainer = match body.find("  ") {
        Some(end) => body[..end].trim(),
        None => body.trim(),
    };
    if maintainer.is_empty() {
        return None;
    }
    Some(ChangelogHead {
        source,
        version,
        maintainer: maintainer.to_string(),
    })
}

/// A run's build stamp: the suite tag and the date every component in the run
/// is stamped with.
///
/// Held at run level rather than computed per component so that every package a
/// run produces carries the same date, even when the run spans midnight or
/// builds components in parallel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildStamp {
    /// The suite tag, such as `deb13`.
    tag: String,
    /// The instant the stamp was made at, in seconds since the Unix epoch.
    seconds: i64,
    /// The build date as `YYYYMMDD`.
    date: String,
    /// The same instant formatted for a changelog trailer (RFC 2822).
    timestamp: String,
}

impl BuildStamp {
    /// The stamp for `tag` at the current time, in UTC.
    ///
    /// UTC rather than local time so the same run started either side of a
    /// timezone boundary produces the same date everywhere, and so the date in
    /// a version means one thing across build machines.
    pub fn now(tag: impl Into<String>) -> BuildStamp {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or(0);
        BuildStamp::at(tag, seconds)
    }

    /// The stamp for `tag` at `seconds` since the Unix epoch, in UTC.
    pub fn at(tag: impl Into<String>, seconds: i64) -> BuildStamp {
        let days = seconds.div_euclid(86_400);
        let time_of_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let (hour, minute, second) = (
            time_of_day / 3600,
            (time_of_day % 3600) / 60,
            time_of_day % 60,
        );
        BuildStamp {
            tag: tag.into(),
            seconds,
            date: format!("{year:04}{month:02}{day:02}"),
            timestamp: format!(
                "{}, {day:02} {} {year:04} {hour:02}:{minute:02}:{second:02} +0000",
                WEEKDAYS[weekday_from_days(days)],
                MONTHS[(month - 1) as usize],
            ),
        }
    }

    /// The suite tag this stamp carries.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// The run's instant, in seconds since the Unix epoch.
    ///
    /// The one clock a run has. The stamped version carries its date, the
    /// changelog trailer carries it in full, `dpkg-buildpackage` derives
    /// `SOURCE_DATE_EPOCH` from that trailer, and the pool's `Release` carries
    /// it as the archive `Date`. A second clock anywhere would mean a run pinned
    /// to one instant still produced output that differed between two runs of
    /// it, which is the whole thing the pin exists to prevent.
    pub fn seconds(&self) -> i64 {
        self.seconds
    }

    /// The run's instant formatted for a `debian/changelog` trailer (RFC 2822,
    /// in UTC).
    ///
    /// The same instant every entry a run writes is dated with, so a
    /// [synthesized changelog](synthesized_changelog) and the stamped entry
    /// above it agree — and so a run given `--build-date` writes the same text
    /// twice over.
    pub fn timestamp(&self) -> &str {
        &self.timestamp
    }

    /// The build date as `YYYYMMDD`, which is the form the version carries.
    pub fn date(&self) -> &str {
        &self.date
    }

    /// The build date as `YYYY-MM-DD`, which is the form the manifest records
    /// and `--build-date` accepts, so a recorded date can be handed straight
    /// back to a later run.
    pub fn calendar_date(&self) -> String {
        format!(
            "{}-{}-{}",
            &self.date[..4],
            &self.date[4..6],
            &self.date[6..]
        )
    }

    /// The stamped version for a component: `base` with the suite tag, build
    /// date, and abbreviated `source` fingerprint appended.
    ///
    /// `base` is the version from the component's own changelog, so the result
    /// keeps upstream's version and only extends its Debian revision.
    /// `version_stamp` decides whether the result sorts above or below `base`
    /// itself; see [`VersionStamp`].
    pub fn version(&self, base: &str, source: &Fingerprint, version_stamp: VersionStamp) -> String {
        format!(
            "{base}{}{}.{}.{}",
            version_stamp.joiner(),
            self.tag,
            self.date,
            source.short(),
        )
    }

    /// The `debian/changelog` entry that declares the stamped version,
    /// formatted to be prepended above `head`'s own entry.
    ///
    /// `suite` is the distribution the entry names, which is the suite the
    /// build targets rather than whatever upstream's entry names.
    pub fn changelog_entry(
        &self,
        head: &ChangelogHead,
        suite: &str,
        source: &Fingerprint,
        version_stamp: VersionStamp,
    ) -> String {
        let version = self.version(&head.version, source, version_stamp);
        // The entry ships inside the package, so it names the inputs as
        // `Fingerprint::describe` renders them for publication rather than as
        // the manifest records them.
        //
        // Each input carries the part it played, so the sentence reads
        // "from source abc1234, packaging def5678" and needs no word of its own
        // for what an input is. That matters because not every input is a
        // revision: a build from a tree on disk is described as `local`, and
        // "from source revision local" would read as a defect rather than as
        // the plain statement it is.
        format!(
            "{source_name} ({version}) {suite}; urgency=medium\n\
             \n\
             \x20 * Automated build from {described}.\n\
             \n\
             \x20-- {maintainer}  {timestamp}\n\
             \n",
            source_name = head.source,
            described = source.describe(),
            maintainer = head.maintainer,
            timestamp = self.timestamp,
        )
    }
}

/// The distribution a [synthesized](synthesized_changelog) entry names.
///
/// The entry declares a version that was never uploaded anywhere — src2deb
/// writes it because the component's packaging carries no changelog to take one
/// from — so it names no suite. The stamped entry above it names the suite the
/// build targets, and that is the entry `dpkg-buildpackage` reads.
const SYNTHESIZED_DISTRIBUTION: &str = "UNRELEASED";

/// The `debian/changelog` for a component whose recipe declares its version:
/// one entry, naming `version` for source package `source` over `maintainer`'s
/// identity.
///
/// Written into the assembled tree so that everything downstream — the vendor
/// pass, the version stamp, `dpkg-buildpackage` — reads a changelog whether or
/// not the packaging shipped one. It is a base rather than a build record:
/// [`stamped_entry`] reads it and prepends the entry that declares the version
/// the packages are actually built as, so the changelog inside the `.deb` reads
/// as any other stamped one does.
///
/// One entry, and only one. A history invented from a git log would claim
/// releases that never happened, and nothing downstream reads past the top
/// entry in any case.
///
/// `version` is the caller's to validate; see [`declared_version_error`].
pub fn synthesized_changelog(
    source: &str,
    version: &str,
    maintainer: &str,
    stamp: &BuildStamp,
) -> String {
    format!(
        "{source} ({version}) {SYNTHESIZED_DISTRIBUTION}; urgency=medium\n\
         \n\
         \x20 * Version declared by the build recipe; this source carries no \
         changelog of its own.\n\
         \n\
         \x20-- {maintainer}  {timestamp}\n",
        timestamp = stamp.timestamp,
    )
}

/// Reports why a declared version cannot be stamped, or `None` when it can.
///
/// The value is spliced into the changelog entry src2deb writes —
/// `source (version) UNRELEASED; urgency=medium` — which `dpkg` then parses, so
/// it has to be a version `dpkg` accepts.
///
/// Two rules, both of which a git tag routinely breaks:
///
/// - **It begins with a digit**, as Debian's grammar requires of an upstream
///   version. `v1.2.3` is the ordinary spelling of a tag and is not a version;
///   it is refused here rather than stamped into something `dpkg` rejects deep
///   inside a build.
/// - **It carries only the characters a version may** — alphanumerics, `.`,
///   `+`, `~`, `-`, and `:`. Whitespace or a `)` would end the field somewhere
///   other than where it reads as ending, and the entry would parse as
///   something else entirely.
///
/// `-` and `:` are admitted because a declared version stands where a
/// changelog's own would: it may carry a Debian revision (`1.2.3-1`) and an
/// epoch (`1:1.2.3`), and the stamp appends to it without disturbing either.
pub fn declared_version_error(version: &str) -> Option<&'static str> {
    if version.is_empty() {
        Some("is empty")
    } else if !version.starts_with(|c: char| c.is_ascii_digit()) {
        Some("does not begin with a digit, which a Debian version must")
    } else if !version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '~' | '-' | ':'))
    {
        Some("contains a character a Debian version may not")
    } else {
        None
    }
}

/// Orders two Debian version strings as `dpkg` does.
///
/// This module's whole design rests on how versions sort, and until now every
/// claim it makes about that ordering was reasoned about rather than computed.
/// Two callers need it computed: an export deduplicating an `Architecture: all`
/// package built by more than one architecture has to know which copy is the
/// later one, and pool retention has to know which versions of a package are
/// superseded. Both are decisions about what to publish, so neither may use an
/// approximation of `dpkg`'s rule.
///
/// The rule is Debian Policy 5.6.12. A version is `[epoch:]upstream[-revision]`:
/// the epoch is the digits before the first `:` (absent is `0`), the revision is
/// what follows the *last* `-` (absent is empty), and the upstream version is
/// what lies between. The three are compared in that order: the epoch
/// numerically, and the other two as alternating runs of non-digits and digits,
/// the non-digits in a modified character order where `~` sorts before
/// everything — even before the end of a part — and letters sort before every
/// other character, and the digit runs as numbers.
pub fn compare(a: &str, b: &str) -> std::cmp::Ordering {
    let (a_epoch, a_upstream, a_revision) = split_version(a);
    let (b_epoch, b_upstream, b_revision) = split_version(b);
    a_epoch
        .cmp(&b_epoch)
        .then_with(|| compare_part(a_upstream, b_upstream))
        .then_with(|| compare_part(a_revision, b_revision))
}

/// Splits a version into its epoch, upstream version, and Debian revision.
///
/// A `:` with anything but digits before it is not an epoch separator — it is a
/// character of the upstream version, which policy admits once an epoch is
/// present — so the split is only taken when what precedes it reads as one.
///
/// A digit run too large for the epoch to hold is read as `0`, and its text is
/// dropped along with the separator, so such a version compares as though it
/// carried no epoch at all. Saturating instead would order a malformed version
/// above every well-formed one, which is the outcome worth ruling out. `dpkg`
/// refuses such a version rather than ordering it, and no version src2deb reads
/// is one — every version it compares was written by dpkg into a file name or a
/// manifest — so what matters here is only that the ordering stays total.
fn split_version(version: &str) -> (u64, &str, &str) {
    let (epoch, rest) = match version.split_once(':') {
        Some((epoch, rest)) if !epoch.is_empty() && epoch.bytes().all(|b| b.is_ascii_digit()) => {
            (epoch.parse().unwrap_or(0), rest)
        }
        _ => (0, version),
    };
    match rest.rsplit_once('-') {
        Some((upstream, revision)) => (epoch, upstream, revision),
        None => (epoch, rest, ""),
    }
}

/// Orders one part of a version — an upstream version or a Debian revision — as
/// `dpkg` does.
///
/// The part is walked as alternating runs: a run of non-digits compared by the
/// modified character order [`order_of`] describes, then a run of digits
/// compared numerically. Comparing digit runs as numbers rather than as text is
/// what makes `1.10` sort above `1.9`, and it is why the build stamp's date is
/// `YYYYMMDD`.
fn compare_part(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        // The non-digit run. Compared a byte at a time, and past the end of
        // either run, so a part that ends where the other continues is ordered
        // by what the longer one continues with — which is how `~` comes to
        // sort before the end of a part.
        let a_text = take_while(&mut a, |byte| !byte.is_ascii_digit());
        let b_text = take_while(&mut b, |byte| !byte.is_ascii_digit());
        for index in 0..a_text.len().max(b_text.len()) {
            let ordering =
                order_of(a_text.get(index).copied()).cmp(&order_of(b_text.get(index).copied()));
            if ordering != Ordering::Equal {
                return ordering;
            }
        }

        // The digit run. Leading zeros carry no value, so they are dropped
        // before the comparison; the longer remainder is then the larger
        // number, and equal lengths compare lexically.
        let a_digits = trim_zeros(take_while(&mut a, |byte| byte.is_ascii_digit()));
        let b_digits = trim_zeros(take_while(&mut b, |byte| byte.is_ascii_digit()));
        let ordering = a_digits
            .len()
            .cmp(&b_digits.len())
            .then_with(|| a_digits.cmp(b_digits));
        if ordering != Ordering::Equal {
            return ordering;
        }

        if a.is_empty() && b.is_empty() {
            return Ordering::Equal;
        }
    }
}

/// Splits the leading run of bytes satisfying `keep` off the front of `bytes`,
/// advancing it past what it returns.
fn take_while<'a>(bytes: &mut &'a [u8], keep: impl Fn(u8) -> bool) -> &'a [u8] {
    let end = bytes
        .iter()
        .position(|byte| !keep(*byte))
        .unwrap_or(bytes.len());
    let (run, rest) = bytes.split_at(end);
    *bytes = rest;
    run
}

/// A digit run with its leading zeros removed, so `007` and `7` compare equal.
fn trim_zeros(digits: &[u8]) -> &[u8] {
    let start = digits
        .iter()
        .position(|byte| *byte != b'0')
        .unwrap_or(digits.len());
    &digits[start..]
}

/// The sort key of one character of a non-digit run, or of the end of one.
///
/// Policy modifies the ASCII order twice: `~` sorts before everything, even
/// before the end of a part, so `1.0~rc1` precedes `1.0`; and letters sort
/// before every other non-digit character, so `1.0a` precedes `1.0+b`. Mapping
/// each case onto a key rather than branching at the comparison keeps the whole
/// rule in one place.
fn order_of(byte: Option<u8>) -> i16 {
    match byte {
        Some(b'~') => -1,
        None => 0,
        Some(byte) if byte.is_ascii_alphabetic() => byte as i16,
        Some(byte) => byte as i16 + 256,
    }
}

/// The upstream version a `git describe --tags` output names, or `None` when it
/// does not name one that can be stamped.
///
/// `git describe` renders `<tag>` on a tagged commit and
/// `<tag>-<commits>-g<hash>` anywhere after one. Two substitutions turn that
/// into a Debian version:
///
/// - **A leading `v` is dropped**, since `v1.2.3` is the conventional spelling
///   of the tag for version `1.2.3`. Only when a digit follows, so a project
///   whose tags read `vulkan-1.0` keeps its name.
/// - **Every `-` becomes `.`**. A version's Debian revision begins at its
///   *last* hyphen, so `1.2.3-4-gabc1234` would split as upstream `1.2.3-4`
///   over revision `gabc1234`, which is not where it reads as splitting. `.`
///   leaves no revision boundary to move and orders the same way: it compares
///   component-wise, and digit runs compare numerically, so `1.2.3.10.gabc1234`
///   still sorts above `1.2.3.9.gdef5678` and both above the bare tag `1.2.3`.
///
/// Anything the result cannot be stamped as — a tag not beginning with a digit,
/// or carrying a character a version may not — is `None`, so the caller reports
/// the tag it found rather than stamping something that does not order.
pub fn version_from_describe(described: &str) -> Option<String> {
    let described = described.trim();
    let unprefixed = described
        .strip_prefix('v')
        .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(described);
    let version = unprefixed.replace('-', ".");
    declared_version_error(&version)
        .is_none()
        .then_some(version)
}

/// Reads `tree/debian/changelog` and renders the entry that stamps a build of
/// `source` for `suite`.
///
/// Fails when the changelog is unreadable or does not open with a well-formed
/// entry: src2deb cannot name a version it cannot derive, and a build that
/// silently kept upstream's version would publish a package apt never offers as
/// an upgrade — the failure this whole module exists to prevent.
///
/// A tree with no changelog at all is the one failure with a remedy in the
/// recipe rather than in the source, so it is reported as such: the component
/// declares its version and src2deb writes the changelog. See [Packaging that
/// carries no changelog](self#packaging-that-carries-no-changelog).
pub fn stamped_entry(
    component: &str,
    tree: &Path,
    stamp: &BuildStamp,
    suite: &str,
    source: &Fingerprint,
    version_stamp: VersionStamp,
) -> Result<String> {
    let path = tree.join("debian/changelog");
    let text = std::fs::read_to_string(&path).map_err(|err| Error::Changelog {
        component: component.to_string(),
        reason: match err.kind() {
            std::io::ErrorKind::NotFound => format!(
                "{} does not exist; a component whose packaging carries no \
                 changelog names its version in the recipe, with `version` or \
                 `version-from`",
                path.display()
            ),
            _ => format!("{}: {err}", path.display()),
        },
    })?;
    let head = parse_changelog(&text).ok_or_else(|| Error::Changelog {
        component: component.to_string(),
        reason: format!(
            "{}: does not begin with a well-formed entry",
            path.display()
        ),
    })?;
    Ok(stamp.changelog_entry(&head, suite, source, version_stamp))
}

/// Seconds since the Unix epoch at the start of `text`, a `YYYY-MM-DD` date in
/// UTC, or `None` when `text` is not one.
///
/// Midnight rather than any other time of day, and UTC rather than local time,
/// so a date names one instant everywhere. That instant is what the build's
/// changelog trailer carries and what `dpkg-buildpackage` derives
/// `SOURCE_DATE_EPOCH` from, so two runs given the same date build against the
/// same clock.
///
/// The format is exact — four digits, two, two — because it is the form the
/// manifest records, and accepting a looser spelling would mean a date could be
/// written one way and read back another. Validity is checked by converting and
/// converting back: a date the calendar does not have, such as 30 February,
/// does not survive the round trip.
pub fn epoch_at_date(text: &str) -> Option<i64> {
    let digits = |part: Option<&str>, width: usize| -> Option<i64> {
        let part = part?;
        (part.len() == width && part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    };
    let mut parts = text.split('-');
    let year = digits(parts.next(), 4)?;
    let month = digits(parts.next(), 2)?;
    let day = digits(parts.next(), 2)?;
    if parts.next().is_some() {
        return None;
    }
    let days = days_from_civil(year, month, day);
    (civil_from_days(days) == (year, month, day)).then_some(days * 86_400)
}

/// The day count since the Unix epoch for a civil date.
///
/// The inverse of [`civil_from_days`], and Howard Hinnant's `days_from_civil`
/// alongside it. A date outside the calendar produces a day count for some other
/// date rather than failing, which is what makes the round-trip check in
/// [`epoch_at_date`] the validity test.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // Shift the year to start on 1 March, so the leap day falls at its end.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Weekday names indexed as [`weekday_from_days`] returns.
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Month names indexed from zero for January.
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The weekday of a day count since the Unix epoch, as an index into
/// [`WEEKDAYS`].
///
/// 1970-01-01 was a Thursday, which is index 4.
fn weekday_from_days(days: i64) -> usize {
    (days + 4).rem_euclid(7) as usize
}

/// The civil date for a day count since the Unix epoch, as `(year, month,
/// day)`.
///
/// Howard Hinnant's `civil_from_days`, which shifts the epoch to 0000-03-01 so
/// that the leap day falls at the end of the year and the month-length pattern
/// becomes a closed-form expression. Implemented here rather than taken from a
/// date crate: the whole of src2deb's need for calendars is one UTC date per
/// run.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    // Shift to an era-based count from 0000-03-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    // Undo the March-first shift: months 0..=9 are March..December.
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::{SourceInput, SourceRole};

    /// A git source at `commit`, the only shape a resolver produces.
    fn git(commit: &str) -> Fingerprint {
        Fingerprint::of(SourceInput::git(SourceRole::Source, commit))
    }

    const CHANGELOG: &str = "\
cosmic-comp (1.0.0~alpha.7-1) trixie; urgency=medium

  * Initial packaging.

 -- Pop Packaging <pop@example.invalid>  Mon, 14 Jul 2026 09:00:00 +0000
";

    /// Asserts that `lower` sorts below `higher`, and that the ordering is
    /// antisymmetric and each version equal to itself.
    #[track_caller]
    fn orders_below(lower: &str, higher: &str) {
        use std::cmp::Ordering;
        assert_eq!(compare(lower, higher), Ordering::Less, "{lower} < {higher}");
        assert_eq!(
            compare(higher, lower),
            Ordering::Greater,
            "{higher} > {lower}"
        );
        assert_eq!(compare(lower, lower), Ordering::Equal);
        assert_eq!(compare(higher, higher), Ordering::Equal);
    }

    #[test]
    fn versions_order_as_dpkg_orders_them() {
        // The cases Policy 5.6.12 turns on, each checked in both directions.
        orders_below("1.0", "1.1");
        // A digit run compares as a number, not as text.
        orders_below("1.9", "1.10");
        orders_below("1.0-1", "1.0-2");
        // Leading zeros carry no value.
        assert_eq!(compare("1.007", "1.7"), std::cmp::Ordering::Equal);
        // A tilde sorts before everything, including the end of a part.
        orders_below("1.0~rc1", "1.0");
        orders_below("1.0~alpha.7", "1.0~beta.1");
        // Letters sort before every other non-digit character.
        orders_below("1.0a", "1.0+b");
        // The epoch outranks everything after it.
        orders_below("2.0", "1:1.0");
        orders_below("1:1.0", "2:0.1");
        // A `:` with anything but digits before it does not start an epoch.
        // dpkg refuses such a version outright rather than ordering it, and no
        // version src2deb reads is one — every version it compares was written
        // by dpkg into a file name or a manifest — so what matters here is only
        // that the ordering stays total and treats the whole string as the
        // upstream version rather than reading `1.0` as an epoch.
        orders_below("1.0", "1.0:2");
        // An epoch too large to hold is read as none at all rather than
        // saturated, so a malformed version does not outrank every well-formed
        // one. The digits go with the separator: what is left is the version.
        assert_eq!(
            compare("99999999999999999999999999:1.0", "1.0"),
            std::cmp::Ordering::Equal
        );
        orders_below("99999999999999999999999999:1.0", "1:0.1");
    }

    #[test]
    fn stamped_versions_order_the_way_the_stamp_claims() {
        // The three orderings the module's own design rests on, now computed
        // rather than reasoned about: a stamped build sorts above the upstream
        // version it was built from, a later build date sorts above an earlier
        // one, and a later suite sorts above an earlier one.
        orders_below("1.0.0~alpha.7-1", "1.0.0~alpha.7-1+deb13.20260731.abc1234");
        orders_below(
            "1.0.0~alpha.7-1+deb13.20260731.abc1234",
            "1.0.0~alpha.7-1+deb13.20260802.abc1234",
        );
        orders_below(
            "1.0.0~alpha.7-1+deb13.20260802.abc1234",
            "1.0.0~alpha.7-1+deb14.20260802.abc1234",
        );
        // The date is a single digit run, so a month boundary orders
        // numerically rather than by text: 20260901 above 20260831.
        orders_below(
            "1.0-1+deb13.20260831.abc1234",
            "1.0-1+deb13.20260901.abc1234",
        );
    }

    #[test]
    fn a_backport_stamp_sorts_below_the_archives_own_package() {
        // The one property the `~` form exists for, and the one thing it
        // changes: the archive's package of the same version wins.
        orders_below("0.98.2-4~deb14.20260802.d6d10b4", "0.98.2-4");
        // ...where the default form would have taken it.
        orders_below("0.98.2-4", "0.98.2-4+deb14.20260802.d6d10b4");
    }

    #[test]
    fn a_backport_stamp_keeps_every_other_ordering_the_stamp_claims() {
        // Everything the module's design rests on has to survive the change of
        // joiner, or the `~` form would trade one trap for several.
        //
        // A later build supersedes an earlier one...
        orders_below(
            "0.98.2-4~deb14.20260802.d6d10b4",
            "0.98.2-4~deb14.20260803.d6d10b4",
        );
        // ...a later suite supersedes an earlier one...
        orders_below(
            "0.98.2-4~deb13.20260802.d6d10b4",
            "0.98.2-4~deb14.20260802.d6d10b4",
        );
        // ...and the archive's *earlier* versions are still superseded, which
        // is what makes the rebuild reachable at all.
        orders_below("0.98.2-3", "0.98.2-4~deb14.20260802.d6d10b4");
        // A native version stays native: `~` introduces no revision boundary,
        // exactly as `+` does not.
        let native = "1.2.3~deb14.20260802.d6d10b4";
        orders_below(native, "1.2.3");
        assert!(!native.contains('-'));
        // Turning the setting on lowers a published version, which is the one
        // hazard the documentation has to state.
        orders_below(
            "0.98.2-4~deb14.20260802.d6d10b4",
            "0.98.2-4+deb14.20260802.d6d10b4",
        );
    }

    #[test]
    fn the_joiner_is_the_whole_of_the_difference_between_the_two_stamps() {
        // Computed rather than asserted about: the two renderings differ in
        // exactly one character, so nothing else in the stamp can drift with
        // the choice.
        let stamp = BuildStamp::at("deb14", 1_785_456_000);
        let source = git("d6d10b4b70e621");
        let supersede = stamp.version("0.98.2-4", &source, VersionStamp::Supersede);
        let backport = stamp.version("0.98.2-4", &source, VersionStamp::Backport);
        assert_eq!(supersede, "0.98.2-4+deb14.20260731.d6d10b4");
        assert_eq!(backport, "0.98.2-4~deb14.20260731.d6d10b4");
        assert_eq!(supersede.replace('+', "~"), backport);
        // And the entry that ships inside the package carries it, since that is
        // the version dpkg-buildpackage reads.
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        assert!(
            stamp
                .changelog_entry(&head, "forky", &source, VersionStamp::Backport)
                .starts_with("cosmic-comp (1.0.0~alpha.7-1~deb14.20260731.d6d10b4) forky;"),
        );
    }

    #[test]
    fn suite_tags_cover_the_numbered_releases_and_reject_rolling_ones() {
        assert_eq!(suite_tag("trixie"), Some("deb13"));
        assert_eq!(suite_tag("forky"), Some("deb14"));
        // A rolling suite has no release number to sort against, so it has no
        // tag and a recipe targeting one must name its own.
        assert_eq!(suite_tag("sid"), None);
        assert_eq!(suite_tag("unstable"), None);
    }

    #[test]
    fn the_changelog_head_yields_source_version_and_maintainer() {
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        assert_eq!(head.source, "cosmic-comp");
        assert_eq!(head.version, "1.0.0~alpha.7-1");
        assert_eq!(head.maintainer, "Pop Packaging <pop@example.invalid>");
    }

    #[test]
    fn a_changelog_without_a_well_formed_head_is_rejected() {
        assert_eq!(parse_changelog(""), None);
        assert_eq!(parse_changelog("not a changelog at all\n"), None);
        // A first line without a version in parentheses.
        assert_eq!(parse_changelog("cosmic-comp trixie; urgency=low\n"), None);
        // A well-formed first line but no trailer to take an identity from.
        assert_eq!(
            parse_changelog("cosmic-comp (1.0-1) trixie; urgency=low\n"),
            None
        );
    }

    #[test]
    fn the_stamped_version_extends_the_debian_revision() {
        let stamp = BuildStamp::at("deb13", 1_785_456_000); // 2026-07-31
        assert_eq!(stamp.tag(), "deb13");
        assert_eq!(stamp.date(), "20260731");
        assert_eq!(
            stamp.version(
                "1.0.0~alpha.7-1",
                &git("abc1234def5678"),
                VersionStamp::Supersede
            ),
            "1.0.0~alpha.7-1+deb13.20260731.abc1234"
        );
    }

    #[test]
    fn a_native_version_stays_native_when_stamped() {
        // No hyphen in, no hyphen introduced: the suffix joins with `+`, so a
        // native package is not turned into a non-native one.
        let stamp = BuildStamp::at("deb14", 1_785_456_000);
        let stamped = stamp.version("1.2.3", &git("0123456789ab"), VersionStamp::Supersede);
        assert_eq!(stamped, "1.2.3+deb14.20260731.0123456");
        assert!(!stamped.contains('-'));
    }

    #[test]
    fn a_source_with_several_inputs_stamps_all_of_them() {
        // Every input appears in the version, so a package built from a source
        // that carries more than upstream's tree is not mistaken for one that
        // does not — the two would otherwise share a version within a day.
        let stamp = BuildStamp::at("deb13", 1_785_456_000);
        let composed = Fingerprint::over(vec![
            SourceInput::git(SourceRole::Source, "abc1234def5678"),
            SourceInput::git(SourceRole::Packaging, "9f8e7d6c5b4a3928"),
        ]);
        assert_eq!(
            stamp.version("1.0-1", &composed, VersionStamp::Supersede),
            "1.0-1+deb13.20260731.abc1234.9f8e7d6"
        );
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        assert!(
            stamp
                .changelog_entry(&head, "trixie", &composed, VersionStamp::Supersede)
                .contains("from source abc1234def5678, packaging 9f8e7d6c5b4a3928.")
        );
    }

    #[test]
    fn an_unpinned_source_stamps_and_reads_as_a_local_build() {
        // A path has no revision to abbreviate, so the version carries a marker
        // instead — and the changelog entry, which ships inside the package,
        // states it without pretending it is a revision or naming the build
        // host's directory layout.
        let stamp = BuildStamp::at("deb13", 1_785_456_000);
        let local = Fingerprint::of(SourceInput::path(
            SourceRole::Source,
            "/home/someone/cosmic-comp",
        ));
        assert_eq!(
            stamp.version("1.0-1", &local, VersionStamp::Supersede),
            "1.0-1+deb13.20260731.local"
        );
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        let entry = stamp.changelog_entry(&head, "trixie", &local, VersionStamp::Supersede);
        assert!(entry.contains("from source local."), "{entry}");
        assert!(!entry.contains("/home/someone"), "{entry}");
    }

    #[test]
    fn the_entry_names_the_target_suite_not_the_one_upstream_wrote() {
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        let stamp = BuildStamp::at("deb14", 1_785_456_000);
        let entry = stamp.changelog_entry(
            &head,
            "forky",
            &git("abc1234def5678"),
            VersionStamp::Supersede,
        );
        assert!(entry.starts_with(
            "cosmic-comp (1.0.0~alpha.7-1+deb14.20260731.abc1234) forky; urgency=medium\n"
        ));
        // The trailer reuses upstream's identity and ends the entry with the
        // blank line that separates it from the entry below.
        assert!(entry.contains(" -- Pop Packaging <pop@example.invalid>  Fri, 31 Jul 2026"));
        assert!(entry.ends_with("\n\n"));
    }

    #[test]
    fn the_stamped_entry_parses_as_the_new_changelog_head() {
        // The entry src2deb writes must itself be a changelog entry: prepending
        // it and re-reading has to yield the stamped version, since that is
        // what dpkg-buildpackage will do with the file.
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        let stamp = BuildStamp::at("deb13", 1_785_456_000);
        let stamped = format!(
            "{}{CHANGELOG}",
            stamp.changelog_entry(
                &head,
                "trixie",
                &git("abc1234def5678"),
                VersionStamp::Supersede
            )
        );
        let reread = parse_changelog(&stamped).expect("the stamped file is a changelog");
        assert_eq!(reread.source, "cosmic-comp");
        assert_eq!(reread.version, "1.0.0~alpha.7-1+deb13.20260731.abc1234");
        assert_eq!(reread.maintainer, "Pop Packaging <pop@example.invalid>");
    }

    #[test]
    fn a_calendar_date_parses_to_midnight_utc_and_stamps_that_day() {
        let seconds = epoch_at_date("2026-07-31").expect("an ordinary date");
        // Midnight UTC, which is what the build's changelog trailer carries and
        // what dpkg-buildpackage derives SOURCE_DATE_EPOCH from.
        assert_eq!(seconds % 86_400, 0);
        let stamp = BuildStamp::at("deb13", seconds);
        assert_eq!(stamp.date(), "20260731");
        assert_eq!(stamp.calendar_date(), "2026-07-31");
        // The form the manifest records is the form the flag accepts, so a
        // recorded date can be handed straight back to a later run.
        assert_eq!(epoch_at_date(&stamp.calendar_date()), Some(seconds));
    }

    #[test]
    fn a_pinned_date_makes_two_runs_stamp_the_same_version() {
        // The whole point of pinning: same sources, same date, same version —
        // where `now()` would put a different date in each.
        let source = git("abc1234def5678");
        let seconds = epoch_at_date("2026-07-31").unwrap();
        let first =
            BuildStamp::at("deb13", seconds).version("1.0-1", &source, VersionStamp::Supersede);
        let second =
            BuildStamp::at("deb13", seconds).version("1.0-1", &source, VersionStamp::Supersede);
        assert_eq!(first, second);
        assert_eq!(first, "1.0-1+deb13.20260731.abc1234");
        // ...and the changelog trailer, which is what carries the clock into
        // the build, agrees too.
        let head = parse_changelog(CHANGELOG).unwrap();
        assert_eq!(
            BuildStamp::at("deb13", seconds).changelog_entry(
                &head,
                "trixie",
                &source,
                VersionStamp::Supersede
            ),
            BuildStamp::at("deb13", seconds).changelog_entry(
                &head,
                "trixie",
                &source,
                VersionStamp::Supersede
            ),
        );
    }

    #[test]
    fn a_date_the_calendar_does_not_have_is_refused() {
        for text in [
            "2026-02-30", // February has no thirtieth
            "2026-13-01", // no thirteenth month
            "2026-00-01", // nor a zeroth
            "2026-01-00", // nor a zeroth day
            "2025-02-29", // 2025 is not a leap year
            "2026-7-31",  // the width is exact, so the manifest's form round-trips
            "26-07-31",
            "2026-07-31T00:00:00Z",
            "2026-07",
            "2026-07-31-01",
            "yesterday",
            "",
        ] {
            assert_eq!(epoch_at_date(text), None, "{text:?} should be refused");
        }
        // A leap day the calendar does have is accepted.
        assert!(epoch_at_date("2024-02-29").is_some());
        assert!(epoch_at_date("2000-02-29").is_some());
    }

    #[test]
    fn a_civil_date_round_trips_through_its_day_count() {
        // `days_from_civil` is the inverse of `civil_from_days`, and the
        // validity check in `epoch_at_date` rests on the pair agreeing.
        for days in [-25_000i64, -1, 0, 1, 11_016, 19_723, 25_000] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days, "{days}");
        }
    }

    #[test]
    fn dates_convert_at_epoch_boundaries_and_across_leap_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(weekday_from_days(0), 4); // a Thursday
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // 2000 is a leap year (divisible by 400); 1900 was not.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn a_synthesized_changelog_is_a_changelog_the_stamping_path_can_extend() {
        // The whole point: what src2deb writes for packaging that ships none has
        // to read back as a changelog head, because the next thing that happens
        // to it is `stamped_entry`.
        let stamp = BuildStamp::at("deb13", 1_785_456_000);
        let text = synthesized_changelog(
            "cosmic-icons",
            "1.0.0",
            "Someone <someone@example.invalid>",
            &stamp,
        );
        let head = parse_changelog(&text).expect("the synthesized file is a changelog");
        assert_eq!(head.source, "cosmic-icons");
        assert_eq!(head.version, "1.0.0");
        assert_eq!(head.maintainer, "Someone <someone@example.invalid>");
        // It names no suite: the version it declares was never uploaded
        // anywhere, and the stamped entry above it names the target.
        assert!(text.contains(") UNRELEASED; urgency=medium\n"), "{text}");
        // One entry, and nothing invented around it.
        assert_eq!(text.matches("urgency=").count(), 1);

        // ...and stamping it produces exactly the version an upstream changelog
        // declaring the same base would have produced.
        let entry = stamp.changelog_entry(
            &head,
            "trixie",
            &git("abc1234def5678"),
            VersionStamp::Supersede,
        );
        assert!(
            entry.starts_with("cosmic-icons (1.0.0+deb13.20260731.abc1234) trixie;"),
            "{entry}",
        );
    }

    #[test]
    fn a_synthesized_changelog_is_dated_by_the_run_that_wrote_it() {
        // Same stamp, same text — so a `--build-date` run rewrites the file it
        // wrote last time rather than moving the base entry beneath a stamp that
        // did not move.
        let stamp = BuildStamp::at("deb13", 1_785_456_000);
        let text = synthesized_changelog("c", "1.0", "S <s@e.invalid>", &stamp);
        assert!(text.contains("Fri, 31 Jul 2026 00:00:00 +0000"), "{text}");
        assert_eq!(
            text,
            synthesized_changelog("c", "1.0", "S <s@e.invalid>", &stamp),
        );
        assert_eq!(stamp.timestamp(), "Fri, 31 Jul 2026 00:00:00 +0000");
    }

    #[test]
    fn a_declared_version_may_carry_an_epoch_and_a_debian_revision() {
        // It stands where a changelog's own version would, so it may be
        // anything a changelog could declare.
        for version in ["1.0", "1.2.3-1", "1:1.2.3-1", "1.0.0~alpha.7-1", "2026.07"] {
            assert_eq!(
                declared_version_error(version),
                None,
                "{version:?} should be accepted",
            );
        }
    }

    #[test]
    fn a_declared_version_that_dpkg_would_not_parse_is_refused() {
        for (version, needle) in [
            ("", "is empty"),
            // The one a git tag hands you, and the reason the check exists.
            ("v1.2.3", "begin with a digit"),
            ("release-1.0", "begin with a digit"),
            ("1.0 beta", "may not"),
            ("1.0)", "may not"),
            ("1.0\n", "may not"),
            ("1.0/2", "may not"),
        ] {
            let reason = declared_version_error(version)
                .unwrap_or_else(|| panic!("{version:?} should be refused"));
            assert!(reason.contains(needle), "{version:?} gave: {reason}");
        }
    }

    #[test]
    fn a_describe_output_becomes_a_version_that_orders_the_way_the_history_does() {
        // On a tag, and after one. The `v` goes, and the hyphens become dots so
        // no Debian revision boundary is invented.
        assert_eq!(version_from_describe("v1.2.3\n").as_deref(), Some("1.2.3"));
        assert_eq!(
            version_from_describe("v1.2.3-4-gabc1234").as_deref(),
            Some("1.2.3.4.gabc1234"),
        );
        assert_eq!(
            version_from_describe("1.2.3-4-gabc1234").as_deref(),
            Some("1.2.3.4.gabc1234"),
        );
        // A tag whose leading `v` is part of a word keeps it, and is then
        // refused for not starting with a digit rather than silently truncated.
        assert_eq!(version_from_describe("vulkan-1.0"), None);
    }

    #[test]
    fn derived_versions_order_by_distance_from_the_tag() {
        // What the substitution is for. Each is strictly greater than the one
        // before it under dpkg's rules: a shorter string sorts first where the
        // longer continues with `.`, and digit runs compare numerically.
        let ordered = [
            version_from_describe("v1.2.3").unwrap(),
            version_from_describe("v1.2.3-1-gaaaaaaa").unwrap(),
            version_from_describe("v1.2.3-9-gbbbbbbb").unwrap(),
            version_from_describe("v1.2.3-10-gccccccc").unwrap(),
            version_from_describe("v1.2.4").unwrap(),
        ];
        assert_eq!(
            ordered,
            [
                "1.2.3",
                "1.2.3.1.gaaaaaaa",
                "1.2.3.9.gbbbbbbb",
                "1.2.3.10.gccccccc",
                "1.2.4",
            ],
        );
        // No hyphen survives, so the stamp appended after this cannot land on
        // the far side of a revision boundary the tag introduced.
        assert!(ordered.iter().all(|version| !version.contains('-')));
    }

    #[test]
    fn a_describe_output_that_is_not_a_version_is_refused_rather_than_stamped() {
        for described in [
            "release/1.0",      // a slashed tag
            "abc1234",          // what --always would give, which does not order
            "start",            // a tag that is a word
            "",                 //
            "v",                // a `v` with nothing after it
            "1.0 with a space", //
        ] {
            assert_eq!(
                version_from_describe(described),
                None,
                "{described:?} should be refused",
            );
        }
    }

    #[test]
    fn a_tree_with_no_changelog_at_all_is_told_where_the_version_comes_from() {
        // The failure a component packaged from a source with no `debian/` of
        // its own hits, and its remedy is in the recipe rather than the source.
        let dir = std::env::temp_dir().join(format!("src2deb-no-changelog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("debian")).unwrap();
        let err = stamped_entry(
            "c",
            &dir,
            &BuildStamp::at("deb13", 0),
            "trixie",
            &git("abc1234"),
            VersionStamp::Supersede,
        )
        .expect_err("a tree with no changelog cannot be stamped");
        let message = err.to_string();
        assert!(message.contains("does not exist"), "{message}");
        assert!(message.contains("version-from"), "{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_run_timestamp_is_utc_and_formatted_for_a_changelog_trailer() {
        // 2026-07-31T12:34:56Z.
        let stamp = BuildStamp::at("deb13", 1_785_501_296);
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        let entry =
            stamp.changelog_entry(&head, "trixie", &git("abc1234"), VersionStamp::Supersede);
        assert!(entry.contains("Fri, 31 Jul 2026 12:34:56 +0000"));
    }
}
