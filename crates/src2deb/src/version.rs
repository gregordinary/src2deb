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

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub fn version(&self, base: &str, source: &Fingerprint) -> String {
        format!("{base}+{}.{}.{}", self.tag, self.date, source.short())
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
    ) -> String {
        let version = self.version(&head.version, source);
        // The entry ships inside the package, so it names the inputs as
        // `Fingerprint::describe` renders them for publication rather than as
        // the manifest records them.
        //
        // The inputs follow a colon rather than the word "revision", because not
        // every kind of input is one: a build from a tree on disk is described as
        // `local`, and "from source revision local" reads as a defect rather than
        // as the plain statement it is.
        format!(
            "{source_name} ({version}) {suite}; urgency=medium\n\
             \n\
             \x20 * Automated build from source: {described}.\n\
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

/// Reads `tree/debian/changelog` and renders the entry that stamps a build of
/// `source` for `suite`.
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
    source: &Fingerprint,
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
    Ok(stamp.changelog_entry(&head, suite, source))
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
    use crate::fingerprint::SourceInput;

    /// A git source at `commit`, the only shape a resolver produces.
    fn git(commit: &str) -> Fingerprint {
        Fingerprint::of(SourceInput::git(commit))
    }

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
            stamp.version("1.0.0~alpha.7-1", &git("abc1234def5678")),
            "1.0.0~alpha.7-1+deb13.20260731.abc1234"
        );
    }

    #[test]
    fn a_native_version_stays_native_when_stamped() {
        // No hyphen in, no hyphen introduced: the suffix joins with `+`, so a
        // native package is not turned into a non-native one.
        let stamp = BuildStamp::at("deb14", 1_785_456_000);
        let stamped = stamp.version("1.2.3", &git("0123456789ab"));
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
            SourceInput::git("abc1234def5678"),
            SourceInput::sha256("9f8e7d6c5b4a3928"),
        ]);
        assert_eq!(
            stamp.version("1.0-1", &composed),
            "1.0-1+deb13.20260731.abc1234.9f8e7d6"
        );
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        assert!(
            stamp
                .changelog_entry(&head, "trixie", &composed)
                .contains("from source: abc1234def5678, sha256:9f8e7d6c5b4a3928.")
        );
    }

    #[test]
    fn an_unpinned_source_stamps_and_reads_as_a_local_build() {
        // A path has no revision to abbreviate, so the version carries a marker
        // instead — and the changelog entry, which ships inside the package,
        // states it without pretending it is a revision or naming the build
        // host's directory layout.
        let stamp = BuildStamp::at("deb13", 1_785_456_000);
        let local = Fingerprint::of(SourceInput::path("/home/someone/cosmic-comp"));
        assert_eq!(stamp.version("1.0-1", &local), "1.0-1+deb13.20260731.local");
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        let entry = stamp.changelog_entry(&head, "trixie", &local);
        assert!(entry.contains("from source: local."), "{entry}");
        assert!(!entry.contains("/home/someone"), "{entry}");
    }

    #[test]
    fn the_entry_names_the_target_suite_not_the_one_upstream_wrote() {
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        let stamp = BuildStamp::at("deb14", 1_785_456_000);
        let entry = stamp.changelog_entry(&head, "forky", &git("abc1234def5678"));
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
            stamp.changelog_entry(&head, "trixie", &git("abc1234def5678"))
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
        let first = BuildStamp::at("deb13", seconds).version("1.0-1", &source);
        let second = BuildStamp::at("deb13", seconds).version("1.0-1", &source);
        assert_eq!(first, second);
        assert_eq!(first, "1.0-1+deb13.20260731.abc1234");
        // ...and the changelog trailer, which is what carries the clock into
        // the build, agrees too.
        let head = parse_changelog(CHANGELOG).unwrap();
        assert_eq!(
            BuildStamp::at("deb13", seconds).changelog_entry(&head, "trixie", &source),
            BuildStamp::at("deb13", seconds).changelog_entry(&head, "trixie", &source),
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
    fn the_run_timestamp_is_utc_and_formatted_for_a_changelog_trailer() {
        // 2026-07-31T12:34:56Z.
        let stamp = BuildStamp::at("deb13", 1_785_501_296);
        let head = parse_changelog(CHANGELOG).expect("well-formed changelog");
        let entry = stamp.changelog_entry(&head, "trixie", &git("abc1234"));
        assert!(entry.contains("Fri, 31 Jul 2026 12:34:56 +0000"));
    }
}
