//! Routine schedules (spec §11.5, §11.7): RRULEs in local time, what they
//! mean in words, and the UTC cron Claude cloud routines take.

use chrono::{Datelike, Offset, TimeZone, Timelike};
use rrule::{RRule, RRuleSet, Tz, Unvalidated};

/// Every rule is anchored here (a Monday, local midnight), so a rule
/// without BYDAY/BYHOUR behaves the same wherever it was created.
fn anchor() -> chrono::DateTime<Tz> {
    Tz::LOCAL.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).single().expect("a valid local time")
}

fn set(rule: &str) -> Result<RRuleSet, String> {
    let rule = rule.trim().trim_start_matches("RRULE:");
    let parsed: RRule<Unvalidated> = rule.parse().map_err(|e| format!("{e}"))?;
    parsed.build(anchor()).map_err(|e| format!("{e}"))
}

fn at(ms: i64) -> chrono::DateTime<Tz> {
    Tz::LOCAL.timestamp_millis_opt(ms).single().unwrap_or_else(anchor)
}

/// Whether the rule parses.
pub fn validate(rule: &str) -> Result<(), String> {
    set(rule).map(|_| ())
}

/// Occurrences strictly after `after_ms` and at or before `until_ms`.
pub fn occurrences(rule: &str, after_ms: i64, until_ms: i64, max: u16) -> Result<Vec<i64>, String> {
    let dates = set(rule)?.after(at(after_ms + 1000)).before(at(until_ms)).all(max).dates;
    Ok(dates.into_iter().map(|d| d.timestamp_millis()).filter(|t| *t > after_ms && *t <= until_ms).collect())
}

/// The first occurrence after `after_ms`, within a year.
pub fn next_after(rule: &str, after_ms: i64) -> Result<Option<i64>, String> {
    Ok(occurrences(rule, after_ms, after_ms + 366 * 86_400_000, 1)?.into_iter().next())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Parts {
    freq: String,
    minute: Option<u32>,
    hour: Option<u32>,
    days: Vec<String>,
    interval: u32,
}

fn parts(rule: &str) -> Parts {
    let mut p = Parts { interval: 1, ..Default::default() };
    for kv in rule.trim().trim_start_matches("RRULE:").split(';') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        match k {
            "FREQ" => p.freq = v.to_owned(),
            "BYMINUTE" => p.minute = v.split(',').next().and_then(|x| x.parse().ok()),
            "BYHOUR" => p.hour = v.split(',').next().and_then(|x| x.parse().ok()),
            "BYDAY" => p.days = v.split(',').map(str::to_owned).collect(),
            "INTERVAL" => p.interval = v.parse().unwrap_or(1),
            _ => {}
        }
    }
    p
}

const DAYS: [(&str, &str); 7] = [
    ("MO", "Monday"),
    ("TU", "Tuesday"),
    ("WE", "Wednesday"),
    ("TH", "Thursday"),
    ("FR", "Friday"),
    ("SA", "Saturday"),
    ("SU", "Sunday"),
];

fn time(hour: u32, minute: u32) -> String {
    format!("{hour}:{minute:02}")
}

/// "Every hour at :44", "Weekdays at 8:30", … (local time).
pub fn describe(rule: &str) -> String {
    let p = parts(rule);
    let minute = p.minute.unwrap_or(0);
    let days: Vec<&str> = p.days.iter().filter_map(|d| DAYS.iter().find(|(k, _)| k == d).map(|(_, n)| *n)).collect();
    let weekdays = p.days.len() == 5 && ["MO", "TU", "WE", "TH", "FR"].iter().all(|d| p.days.iter().any(|x| x == d));
    match p.freq.as_str() {
        "HOURLY" if p.interval > 1 => format!("Every {} hours at :{minute:02}", p.interval),
        "HOURLY" => format!("Every hour at :{minute:02}"),
        "DAILY" if weekdays || (p.freq == "DAILY" && days.len() == 5 && weekdays) => {
            format!("Weekdays at {}", time(p.hour.unwrap_or(0), minute))
        }
        "DAILY" if !days.is_empty() => format!("{} at {}", days.join(", "), time(p.hour.unwrap_or(0), minute)),
        "DAILY" => format!("Every day at {}", time(p.hour.unwrap_or(0), minute)),
        "WEEKLY" if weekdays => format!("Weekdays at {}", time(p.hour.unwrap_or(0), minute)),
        "WEEKLY" if !days.is_empty() => format!("Every {} at {}", days.join(", "), time(p.hour.unwrap_or(0), minute)),
        "WEEKLY" => format!("Every Monday at {}", time(p.hour.unwrap_or(0), minute)),
        _ => rule.to_owned(),
    }
}

/// The UTC cron expression for a Claude cloud routine, which runs at most
/// hourly. Local hours are converted with the current UTC offset.
pub fn to_utc_cron(rule: &str) -> Result<String, String> {
    validate(rule)?;
    let p = parts(rule);
    let offset_minutes = chrono::Local::now().offset().fix().local_minus_utc() / 60;
    let minute = p.minute.unwrap_or(0) as i32;
    match p.freq.as_str() {
        "HOURLY" => {
            // Shift the minute; whole-hour offsets leave it unchanged.
            let m = (minute - offset_minutes).rem_euclid(60);
            let hours = if p.interval > 1 { format!("*/{}", p.interval) } else { "*".to_owned() };
            Ok(format!("{m} {hours} * * *"))
        }
        "DAILY" | "WEEKLY" => {
            let local = p.hour.unwrap_or(0) as i32 * 60 + minute;
            let utc = local - offset_minutes;
            let day_shift = utc.div_euclid(24 * 60);
            let utc = utc.rem_euclid(24 * 60);
            let (h, m) = (utc / 60, utc % 60);
            let mut days: Vec<i32> = p
                .days
                .iter()
                .filter_map(|d| DAYS.iter().position(|(k, _)| k == d))
                // cron: 0 = Sunday; our list starts at Monday.
                .map(|i| ((i as i32 + 1) + day_shift).rem_euclid(7))
                .collect();
            days.sort_unstable();
            let dow = if days.is_empty() {
                if p.freq == "WEEKLY" { (1 + day_shift).rem_euclid(7).to_string() } else { "*".to_owned() }
            } else {
                days.iter().map(i32::to_string).collect::<Vec<_>>().join(",")
            };
            Ok(format!("{m} {h} * * {dow}"))
        }
        other => Err(format!("{other} schedules cannot run on Claude cloud (hourly minimum)")),
    }
}

/// Minute and hour of `ms` in local time (for tests and the editor).
pub fn local_hour_minute(ms: i64) -> (u32, u32) {
    let d = at(ms);
    (d.hour(), d.minute())
}

/// Day of week (Monday = 0) of `ms` in local time.
pub fn local_weekday(ms: i64) -> u32 {
    at(ms).weekday().num_days_from_monday()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;

    #[test]
    fn hourly_occurrences_fall_on_the_minute() {
        let start = anchor().timestamp_millis() + 30 * 86_400_000;
        let times = occurrences("FREQ=HOURLY;BYMINUTE=44", start, start + 3 * HOUR, 10).unwrap();
        assert_eq!(times.len(), 3);
        assert!(times.iter().all(|t| local_hour_minute(*t).1 == 44));
        // Strictly after: the run just done does not fire again.
        let again = occurrences("FREQ=HOURLY;BYMINUTE=44", times[0], times[0] + HOUR, 10).unwrap();
        assert_eq!(again, vec![times[1]]);
        assert_eq!(next_after("FREQ=HOURLY;BYMINUTE=44", times[0]).unwrap(), Some(times[1]));
    }

    #[test]
    fn weekday_rules_skip_weekends() {
        let monday = anchor().timestamp_millis() + 7 * 86_400_000;
        let week =
            occurrences("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=8;BYMINUTE=30", monday, monday + 7 * 86_400_000, 20)
                .unwrap();
        assert_eq!(week.len(), 5);
        assert!(week.iter().all(|t| local_weekday(*t) < 5 && local_hour_minute(*t) == (8, 30)));
        assert!(validate("FREQ=SOMETIMES").is_err());
    }

    #[test]
    fn descriptions() {
        assert_eq!(describe("FREQ=HOURLY;BYMINUTE=44"), "Every hour at :44");
        assert_eq!(describe("FREQ=HOURLY;INTERVAL=3;BYMINUTE=0"), "Every 3 hours at :00");
        assert_eq!(describe("FREQ=DAILY;BYHOUR=7;BYMINUTE=0"), "Every day at 7:00");
        assert_eq!(describe("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=8;BYMINUTE=30"), "Weekdays at 8:30");
        assert_eq!(describe("FREQ=WEEKLY;BYDAY=FR;BYHOUR=16;BYMINUTE=5"), "Every Friday at 16:05");
    }

    #[test]
    fn utc_cron_conversion() {
        let offset = chrono::Local::now().offset().fix().local_minus_utc() / 60;
        let hourly = to_utc_cron("FREQ=HOURLY;BYMINUTE=44").unwrap();
        assert_eq!(hourly, format!("{} * * * *", (44 - offset).rem_euclid(60)));
        let daily = to_utc_cron("FREQ=DAILY;BYHOUR=7;BYMINUTE=0").unwrap();
        let utc = (7 * 60 - offset).rem_euclid(24 * 60);
        assert_eq!(daily, format!("{} {} * * *", utc % 60, utc / 60));
        let weekdays = to_utc_cron("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=8;BYMINUTE=30").unwrap();
        assert_eq!(weekdays.split(' ').nth(4).unwrap().split(',').count(), 5);
        assert!(to_utc_cron("FREQ=MINUTELY;INTERVAL=5").is_err());
    }
}
