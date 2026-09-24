//! Calendar arithmetic without a date crate: enough to show and quote dates.

use crate::Millis;

/// `(year, month, day)` for days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `2026-09-20T12:33:00Z` for a Unix time in milliseconds.
pub fn iso8601_utc(at: Millis) -> String {
    let secs = at.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s % 3600 / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_789_489_800_000), "2026-09-15T16:30:00Z");
        assert_eq!(iso8601_utc(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601_utc(-1_000), "1969-12-31T23:59:59Z");
    }
}
