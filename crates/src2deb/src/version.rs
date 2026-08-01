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
//!   version it was built from.
//! - The suite appears as `deb13`/`deb14`, never as `trixie`/`forky`. Spelled
//!   out, `forky` sorts before `trixie`, so a user moving from trixie to forky
//!   would see the forky packages as a downgrade. The release numbers sort the
//!   way the releases do.
//! - The date is `YYYYMMDD`, which compares numerically within its digit run,
//!   so each build sorts after the one before it.
//! - The short revision makes the source a package was built from legible from
//!   `apt policy` alone.
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

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

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

/// How many characters of the commit hash the version carries.
///
/// Seven is git's own conventional abbreviation, and enough to identify a
/// revision within a single component's history.
const SHORT_COMMIT: usize = 7;

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

    /// The build date as `YYYYMMDD`.
    pub fn date(&self) -> &str {
        &self.date
    }

    /// The stamped version for a component: `base` with the suite tag, build
    /// date, and abbreviated `commit` appended.
    ///
    /// `base` is the version from the component's own changelog, so the result
    /// keeps upstream's version and only extends its Debian revision.
    pub fn version(&self, base: &str, commit: &str) -> String {
        let short: String = commit.chars().take(SHORT_COMMIT).collect();
        format!("{base}+{}.{}.{short}", self.tag, self.date)
    }

    /// The `debian/changelog` entry that declares the stamped version,
    /// formatted to be prepended above `head`'s own entry.
    ///
    /// `suite` is the distribution the entry names, which is the suite the
    /// build targets rather than whatever upstream's entry names.
    pub fn changelog_entry(&self, head: &ChangelogHead, suite: &str, commit: &str) -> String {
        let version = self.version(&head.version, commit);
        format!(
            "{source} ({version}) {suite}; urgency=medium\n\
             \n\
             \x20 * Automated build from source revision {commit}.\n\
             \n\
             \x20-- {maintainer}  {timestamp}\n\
             \n",
            source = head.source,
            maintainer = head.maintainer,
            timestamp = self.timestamp,
        )
    }
}

/// Reads `tree/debian/changelog` and renders the entry that stamps a build of
/// `commit` for `suite`.
///
/// Fails when the changelog is unreadable or does not open with a well-formed
/// entry: src2deb cannot name a version it cannot derive, and a build that
/// silently kept upstream's version would publish a package apt never offers as
/// an upgrade — the failure this whole module exists to prevent.
pub fn stamped_entry(
    component: &str,
    tree: &Path,
    stamp: &BuildStamp,
    suite: &str,
    commit: &str,
) -> Result<String> {
    let path = tree.join("debian/changelog");
    let text = std::fs::read_to_string(&path).map_err(|err| Error::Changelog {
        component: component.to_string(),
        reason: format!("{}: {err}", path.display()),
    })?;
    let head = parse_changelog(&text).ok_or_else(|| Error::Changelog {
        component: component.to_string(),
        reason: format!(
            "{}: does not begin with a well-formed entry",
            path.display()
        ),
    })?;
    Ok(stamp.changelog_entry(&head, suite, commit))
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

    const CHANGELOG: &str = "\
cosmic-comp (1.0.0~alpha.7-1) trixie; urgency=medium

  * Initial packaging.

 -- Pop Packaging <pop@example.invalid>  Mon, 14 Jul 2026 09:00:00 +0000
";

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
            stamp.version("1.0.0~alpha.7-1", "abc1234def5678"),
            "1.0.0~alpha.7-1+deb13.20260731.abc1234"
        );
    }

    #[test]
    fn a_native_version_stays_native_when_stamped() {
        // No hyphen in, no hyphen introduced: the suffix joins with `+`, so a
        // native package is not turned into a non-native one.
        let stamp = BuildStamp::at("deb14", 1_785_456_000);
        let stamped = stamp.version("1.2.3", "0123456789ab");
        assert_eq!(stamped, "1.2.3+deb14.20260731.0123456");
        assert!(!stamped.contains('-'));
    }

    #[test]
    fn the_entry_names_the_target_suite_not_the_one_upstream_wrote() {
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        let stamp = BuildStamp::at("deb14", 1_785_456_000);
        let entry = stamp.changelog_entry(&head, "forky", "abc1234def5678");
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
            stamp.changelog_entry(&head, "trixie", "abc1234def5678")
        );
        let reread = parse_changelog(&stamped).expect("the stamped file is a changelog");
        assert_eq!(reread.source, "cosmic-comp");
        assert_eq!(reread.version, "1.0.0~alpha.7-1+deb13.20260731.abc1234");
        assert_eq!(reread.maintainer, "Pop Packaging <pop@example.invalid>");
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
    fn the_run_timestamp_is_utc_and_formatted_for_a_changelog_trailer() {
        // 2026-07-31T12:34:56Z.
        let stamp = BuildStamp::at("deb13", 1_785_501_296);
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        let entry = stamp.changelog_entry(&head, "trixie", "abc1234");
        assert!(entry.contains("Fri, 31 Jul 2026 12:34:56 +0000"));
    }
}
