//! Relative and exact timestamps for the folder detail (#43).
//!
//! The detail shows when a folder last reconciled and last uploaded twice: as a
//! relative time a person reads at a glance, and as an exact RFC-3339 local
//! timestamp they can compare or write down. Both are pure formatting over a
//! stored nanosecond instant, so they are testable without a window. The local
//! offset is read from the platform (`time`'s `local-offset` feature) and never
//! from the network (R9.2).

use time::{OffsetDateTime, UtcOffset};

/// What a stored instant reads as when the folder has never done the thing.
pub const NOT_YET: &str = "not yet";

/// The current time in nanoseconds since the Unix epoch, like the stored ones.
#[must_use]
pub fn now_ns() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
        // A clock set before the epoch still has to render something.
        Err(error) => -i64::try_from(error.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

/// One detail line: the label, then the relative and exact time, or
/// [`NOT_YET`].
///
/// For example `Last successful upload: 3 minutes ago ·
/// 2026-02-05T14:03:07+01:00`.
#[must_use]
pub fn moment_line(label: &str, at_ns: Option<i64>, now_ns: i64) -> String {
    match at_ns {
        Some(at_ns) => format!(
            "{label}: {} · {}",
            relative_time(now_ns, at_ns),
            rfc3339_local(at_ns)
        ),
        None => format!("{label}: {NOT_YET}"),
    }
}

/// The relative time from `now_ns` back to `at_ns`, at the coarsest sensible
/// scale.
///
/// A future instant (a clock that moved backwards) reads as "just now" rather
/// than as a negative time.
#[must_use]
pub fn relative_time(now_ns: i64, at_ns: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    let seconds = (now_ns.saturating_sub(at_ns) / 1_000_000_000).max(0);
    match seconds {
        0..MINUTE => "just now".to_string(),
        s if s < 2 * MINUTE => "1 minute ago".to_string(),
        s if s < HOUR => format!("{} minutes ago", s / MINUTE),
        s if s < 2 * HOUR => "1 hour ago".to_string(),
        s if s < DAY => format!("{} hours ago", s / HOUR),
        s if s < 2 * DAY => "1 day ago".to_string(),
        s => format!("{} days ago", s / DAY),
    }
}

/// The exact local timestamp `at_ns` names, in RFC-3339 form.
///
/// The offset is the one in force at that instant, so a time recorded before a
/// daylight-saving change keeps the offset it was recorded under.
#[must_use]
pub fn rfc3339_local(at_ns: i64) -> String {
    let Ok(instant) = OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ns)) else {
        return "an unreadable time".to_string();
    };
    let offset = UtcOffset::local_offset_at(instant).unwrap_or(UtcOffset::UTC);
    rfc3339_in(at_ns, offset)
}

/// [`rfc3339_local`] under an explicit offset, so the exactness is testable
/// without depending on the machine's time zone.
#[must_use]
pub fn rfc3339_in(at_ns: i64, offset: UtcOffset) -> String {
    let Ok(instant) = OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ns)) else {
        return "an unreadable time".to_string();
    };
    let instant = instant.to_offset(offset);
    let seconds = offset.whole_seconds();
    let sign = if seconds < 0 { '-' } else { '+' };
    let absolute = seconds.unsigned_abs();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{sign}{:02}:{:02}",
        instant.year(),
        u8::from(instant.month()),
        instant.day(),
        instant.hour(),
        instant.minute(),
        instant.second(),
        absolute / 3600,
        absolute % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::{NOT_YET, moment_line, relative_time, rfc3339_in};
    use time::UtcOffset;

    const SECOND: i64 = 1_000_000_000;
    /// 2026-02-05T13:03:07Z, so the exact form is readable in a failure.
    const AT: i64 = 1_770_296_587 * SECOND;

    #[test]
    fn a_folder_that_has_never_run_reads_as_not_yet() {
        assert_eq!(
            moment_line("Last successful upload", None, AT),
            format!("Last successful upload: {NOT_YET}")
        );
    }

    #[test]
    fn a_relative_time_keeps_the_coarsest_scale() {
        for (elapsed, expected) in [
            (0, "just now"),
            (59, "just now"),
            (60, "1 minute ago"),
            (119, "1 minute ago"),
            (120, "2 minutes ago"),
            (59 * 60, "59 minutes ago"),
            (60 * 60, "1 hour ago"),
            (2 * 60 * 60 - 1, "1 hour ago"),
            (2 * 60 * 60, "2 hours ago"),
            (24 * 60 * 60 - 1, "23 hours ago"),
            (24 * 60 * 60, "1 day ago"),
            (48 * 60 * 60, "2 days ago"),
            (400 * 24 * 60 * 60, "400 days ago"),
        ] {
            assert_eq!(
                relative_time(AT, AT - elapsed * SECOND),
                expected,
                "an instant {elapsed} seconds old"
            );
        }
    }

    /// A clock that moved backwards must not read as a negative age.
    #[test]
    fn a_future_instant_reads_as_just_now() {
        assert_eq!(relative_time(AT, AT + 90 * SECOND), "just now");
    }

    #[test]
    fn the_exact_time_is_rfc3339_in_the_offset_it_names() {
        assert_eq!(rfc3339_in(AT, UtcOffset::UTC), "2026-02-05T13:03:07+00:00");
        assert_eq!(
            rfc3339_in(AT, UtcOffset::from_hms(2, 0, 0).expect("a valid offset")),
            "2026-02-05T15:03:07+02:00"
        );
        assert_eq!(
            rfc3339_in(AT, UtcOffset::from_hms(-5, -30, 0).expect("a valid offset")),
            "2026-02-05T07:33:07-05:30"
        );
    }

    #[test]
    fn a_detail_line_carries_both_the_relative_and_the_exact_time() {
        let line = moment_line("Last reconciliation scan", Some(AT - 3 * 60 * SECOND), AT);
        assert_eq!(
            line,
            format!(
                "Last reconciliation scan: 3 minutes ago · {}",
                super::rfc3339_local(AT - 3 * 60 * SECOND)
            ),
            "the line shows the age and the exact local time"
        );
    }
}
