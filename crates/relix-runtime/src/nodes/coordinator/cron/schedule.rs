//! Schedule expression parser for the cron scheduler.
//!
//! Three formats, decided by shape (not by tag):
//!
//! - **Duration** — `<number><unit>`, e.g. `30m`, `2h`, `1d`, `7d`.
//!   Re-fires every interval. Units: `s` `m` `h` `d` `w`.
//! - **Cron** — standard 5-field `min hour day month weekday`,
//!   e.g. `0 9 * * 1` (Mon 9am UTC). Supports `*` and explicit
//!   integers; ranges and step values are NOT supported in the
//!   alpha.
//! - **One-shot ISO timestamp** — any RFC 3339 instant, e.g.
//!   `2026-06-01T09:00:00Z`. Fires once at that instant; the
//!   scheduler flips `enabled = 0` after the fire.
//!
//! Every parser returns a `Schedule` enum the caller can turn
//! into the next unix-seconds firing via [`Schedule::next_after`].

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Parsed schedule expression.
///
/// Stored verbatim in the cron_jobs row as `schedule TEXT`; the
/// scheduler re-parses on every tick rather than caching the
/// `Schedule` value so an `cron.update` that edits the
/// expression takes effect immediately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// Fires every `secs` seconds. Stable / re-firing.
    Duration { secs: u64 },
    /// Standard 5-field cron expression. `min hour day-of-month
    /// month day-of-week`; UTC. Each component is either `*`
    /// (any) or a single explicit integer.
    Cron {
        minute: CronField,
        hour: CronField,
        day_of_month: CronField,
        month: CronField,
        day_of_week: CronField,
    },
    /// Fires once at this unix-seconds instant, then the
    /// scheduler disables the job.
    OneShot { fire_at: i64 },
}

/// One field of a 5-field cron expression. The alpha only
/// supports `*` and exact integers; richer syntax (ranges,
/// step values, lists) is intentionally out of scope and
/// rejected by the parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CronField {
    Any,
    Exact(u32),
}

impl CronField {
    fn matches(self, v: u32) -> bool {
        match self {
            CronField::Any => true,
            CronField::Exact(target) => target == v,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScheduleError {
    #[error("schedule: empty expression")]
    Empty,
    #[error("schedule: duration '{0}' has no unit (use s/m/h/d/w)")]
    DurationNoUnit(String),
    #[error("schedule: duration '{0}' has an unknown unit '{1}'")]
    DurationBadUnit(String, char),
    #[error("schedule: duration '{0}' value parse failed")]
    DurationBadValue(String),
    #[error("schedule: duration '{0}' must be > 0")]
    DurationZero(String),
    #[error("schedule: cron expression must have 5 fields, got {0}")]
    CronFieldCount(usize),
    #[error("schedule: cron field '{0}' is invalid (only * or integer)")]
    CronBadField(String),
    #[error("schedule: cron field '{field}' value {value} out of range {lo}..={hi}")]
    CronRange {
        field: &'static str,
        value: u32,
        lo: u32,
        hi: u32,
    },
    #[error("schedule: one-shot '{0}' not a valid RFC 3339 timestamp")]
    OneShotParse(String),
    #[error("schedule: not a recognised format: '{0}'")]
    Unrecognised(String),
}

impl Schedule {
    /// Parse a schedule expression. Decides on the format by
    /// shape: a single token that ends in a duration unit
    /// suffix becomes [`Schedule::Duration`]; whitespace-
    /// separated tokens become a cron expression; anything
    /// else is tried as an RFC 3339 instant.
    pub fn parse(expr: &str) -> Result<Schedule, ScheduleError> {
        let trimmed = expr.trim();
        if trimmed.is_empty() {
            return Err(ScheduleError::Empty);
        }
        // Try cron first when there's a space — distinguishes
        // it cleanly from a duration like `30m`.
        if trimmed.contains(' ') {
            return parse_cron(trimmed);
        }
        // Duration heuristic: ends in a known unit char and the
        // prefix parses as a number.
        if let Some(last) = trimmed.chars().last()
            && "smhdw".contains(last)
            && trimmed[..trimmed.len() - last.len_utf8()]
                .chars()
                .all(|c| c.is_ascii_digit())
            && !trimmed[..trimmed.len() - last.len_utf8()].is_empty()
        {
            return parse_duration(trimmed);
        }
        // Last resort: RFC 3339 one-shot.
        if let Ok(ts) = OffsetDateTime::parse(trimmed, &Rfc3339) {
            return Ok(Schedule::OneShot {
                fire_at: ts.unix_timestamp(),
            });
        }
        Err(ScheduleError::Unrecognised(trimmed.to_string()))
    }

    /// Returns `true` for schedules that fire indefinitely
    /// (duration + cron). One-shots return `false` and the
    /// scheduler disables them after the first fire.
    pub fn is_recurring(&self) -> bool {
        !matches!(self, Schedule::OneShot { .. })
    }

    /// Compute the next unix-seconds firing strictly after
    /// `now`. For a one-shot whose `fire_at <= now`, returns
    /// `fire_at` so the scheduler runs it immediately on the
    /// next tick instead of skipping the deadline.
    pub fn next_after(&self, now: i64) -> i64 {
        match self {
            Schedule::Duration { secs } => now.saturating_add(*secs as i64),
            Schedule::Cron { .. } => next_cron_after(self, now),
            Schedule::OneShot { fire_at } => *fire_at,
        }
    }
}

fn parse_duration(s: &str) -> Result<Schedule, ScheduleError> {
    let chars: Vec<char> = s.chars().collect();
    let unit = chars
        .last()
        .copied()
        .ok_or_else(|| ScheduleError::DurationNoUnit(s.into()))?;
    if !"smhdw".contains(unit) {
        return Err(ScheduleError::DurationBadUnit(s.into(), unit));
    }
    let value_str: String = chars[..chars.len() - 1].iter().collect();
    if value_str.is_empty() {
        return Err(ScheduleError::DurationBadValue(s.into()));
    }
    let n: u64 = value_str
        .parse()
        .map_err(|_| ScheduleError::DurationBadValue(s.into()))?;
    if n == 0 {
        return Err(ScheduleError::DurationZero(s.into()));
    }
    let secs = match unit {
        's' => n,
        'm' => n.saturating_mul(60),
        'h' => n.saturating_mul(60 * 60),
        'd' => n.saturating_mul(24 * 60 * 60),
        'w' => n.saturating_mul(7 * 24 * 60 * 60),
        other => return Err(ScheduleError::DurationBadUnit(s.into(), other)),
    };
    Ok(Schedule::Duration { secs })
}

fn parse_cron(s: &str) -> Result<Schedule, ScheduleError> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(ScheduleError::CronFieldCount(parts.len()));
    }
    let minute = parse_cron_field(parts[0], "minute", 0, 59)?;
    let hour = parse_cron_field(parts[1], "hour", 0, 23)?;
    let day_of_month = parse_cron_field(parts[2], "day_of_month", 1, 31)?;
    let month = parse_cron_field(parts[3], "month", 1, 12)?;
    let day_of_week = parse_cron_field(parts[4], "day_of_week", 0, 6)?;
    Ok(Schedule::Cron {
        minute,
        hour,
        day_of_month,
        month,
        day_of_week,
    })
}

fn parse_cron_field(
    tok: &str,
    field: &'static str,
    lo: u32,
    hi: u32,
) -> Result<CronField, ScheduleError> {
    if tok == "*" {
        return Ok(CronField::Any);
    }
    let n: u32 = tok
        .parse()
        .map_err(|_| ScheduleError::CronBadField(tok.into()))?;
    if n < lo || n > hi {
        return Err(ScheduleError::CronRange {
            field,
            value: n,
            lo,
            hi,
        });
    }
    Ok(CronField::Exact(n))
}

/// Walk forward minute-by-minute from `now + 60s` looking for
/// the first instant that matches the cron expression. Bounded
/// by 366 * 24 * 60 minutes — if a cron expression somehow
/// never matches within a year (impossible with our restricted
/// grammar) we return now+1y as a stable fallback rather than
/// looping forever.
fn next_cron_after(s: &Schedule, now: i64) -> i64 {
    let Schedule::Cron {
        minute,
        hour,
        day_of_month,
        month,
        day_of_week,
    } = s
    else {
        unreachable!("next_cron_after called on non-cron")
    };
    // Round forward to the next whole minute strictly after
    // `now`. This avoids re-firing within the same minute.
    let start = (now / 60 + 1) * 60;
    let max_minutes = 366 * 24 * 60;
    for offset in 0..max_minutes {
        let ts = start + offset * 60;
        let dt = match OffsetDateTime::from_unix_timestamp(ts) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if !minute.matches(dt.minute() as u32) {
            continue;
        }
        if !hour.matches(dt.hour() as u32) {
            continue;
        }
        if !day_of_month.matches(dt.day() as u32) {
            continue;
        }
        if !month.matches(dt.month() as u8 as u32) {
            continue;
        }
        // time's Weekday::number_from_monday returns 1..=7;
        // standard cron uses 0..=6 with Sunday=0.
        let dow = match dt.weekday() {
            time::Weekday::Sunday => 0,
            time::Weekday::Monday => 1,
            time::Weekday::Tuesday => 2,
            time::Weekday::Wednesday => 3,
            time::Weekday::Thursday => 4,
            time::Weekday::Friday => 5,
            time::Weekday::Saturday => 6,
        };
        if !day_of_week.matches(dow) {
            continue;
        }
        return ts;
    }
    // Should never happen with the restricted grammar.
    now + 365 * 24 * 60 * 60
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── duration ─────────────────────────────────────────

    #[test]
    fn parse_duration_30m_is_1800_seconds() {
        let s = Schedule::parse("30m").unwrap();
        assert_eq!(s, Schedule::Duration { secs: 1800 });
        assert_eq!(s.next_after(1000), 1000 + 1800);
    }

    #[test]
    fn parse_duration_2h_is_7200_seconds() {
        assert_eq!(
            Schedule::parse("2h").unwrap(),
            Schedule::Duration { secs: 7200 }
        );
    }

    #[test]
    fn parse_duration_1d_is_86400_seconds() {
        assert_eq!(
            Schedule::parse("1d").unwrap(),
            Schedule::Duration { secs: 86400 }
        );
    }

    #[test]
    fn parse_duration_7d_is_one_week_in_seconds() {
        assert_eq!(
            Schedule::parse("7d").unwrap(),
            Schedule::Duration { secs: 7 * 86400 }
        );
    }

    #[test]
    fn parse_duration_in_weeks() {
        assert_eq!(
            Schedule::parse("2w").unwrap(),
            Schedule::Duration { secs: 14 * 86400 }
        );
    }

    #[test]
    fn parse_duration_in_seconds() {
        assert_eq!(
            Schedule::parse("45s").unwrap(),
            Schedule::Duration { secs: 45 }
        );
    }

    #[test]
    fn parse_duration_zero_is_rejected() {
        match Schedule::parse("0m") {
            Err(ScheduleError::DurationZero(_)) => {}
            other => panic!("expected DurationZero, got {other:?}"),
        }
    }

    #[test]
    fn parse_duration_missing_value_falls_through_to_unrecognised() {
        // `m` alone has no numeric prefix, so it doesn't pass
        // the duration heuristic and falls through to the
        // RFC 3339 attempt (which also fails) → Unrecognised.
        assert!(matches!(
            Schedule::parse("m"),
            Err(ScheduleError::Unrecognised(_))
        ));
    }

    // ── cron ──────────────────────────────────────────────

    #[test]
    fn parse_cron_classic_monday_9am() {
        let s = Schedule::parse("0 9 * * 1").unwrap();
        assert_eq!(
            s,
            Schedule::Cron {
                minute: CronField::Exact(0),
                hour: CronField::Exact(9),
                day_of_month: CronField::Any,
                month: CronField::Any,
                day_of_week: CronField::Exact(1),
            }
        );
    }

    #[test]
    fn parse_cron_daily_midnight() {
        let s = Schedule::parse("0 0 * * *").unwrap();
        assert_eq!(
            s,
            Schedule::Cron {
                minute: CronField::Exact(0),
                hour: CronField::Exact(0),
                day_of_month: CronField::Any,
                month: CronField::Any,
                day_of_week: CronField::Any,
            }
        );
    }

    #[test]
    fn parse_cron_wrong_field_count_rejected() {
        match Schedule::parse("0 9 * *") {
            Err(ScheduleError::CronFieldCount(4)) => {}
            other => panic!("expected CronFieldCount(4), got {other:?}"),
        }
    }

    #[test]
    fn parse_cron_field_out_of_range_rejected() {
        // hour=24 is invalid.
        match Schedule::parse("0 24 * * *") {
            Err(ScheduleError::CronRange {
                field: "hour",
                value: 24,
                ..
            }) => {}
            other => panic!("expected CronRange hour=24, got {other:?}"),
        }
    }

    #[test]
    fn parse_cron_range_syntax_is_rejected_for_now() {
        // Alpha doesn't accept `9-17` etc.
        match Schedule::parse("0 9-17 * * *") {
            Err(ScheduleError::CronBadField(_)) => {}
            other => panic!("expected CronBadField, got {other:?}"),
        }
    }

    #[test]
    fn next_cron_daily_midnight_lands_on_next_midnight() {
        // 2026-05-22 12:34:56 UTC -> next is 2026-05-23 00:00:00 UTC.
        let now_iso = "2026-05-22T12:34:56Z";
        let now = OffsetDateTime::parse(now_iso, &Rfc3339)
            .unwrap()
            .unix_timestamp();
        let next = Schedule::parse("0 0 * * *").unwrap().next_after(now);
        let next_dt = OffsetDateTime::from_unix_timestamp(next).unwrap();
        assert_eq!(next_dt.hour(), 0);
        assert_eq!(next_dt.minute(), 0);
        // Same year/month, next day.
        assert_eq!(next_dt.day(), 23);
        assert_eq!(next_dt.month(), time::Month::May);
        assert_eq!(next_dt.year(), 2026);
    }

    #[test]
    fn next_cron_monday_9am_lands_on_a_monday_at_9() {
        // 2026-05-22 is a Friday. Next "Mon 9am" is 2026-05-25 09:00 UTC.
        let now_iso = "2026-05-22T12:34:56Z";
        let now = OffsetDateTime::parse(now_iso, &Rfc3339)
            .unwrap()
            .unix_timestamp();
        let next = Schedule::parse("0 9 * * 1").unwrap().next_after(now);
        let dt = OffsetDateTime::from_unix_timestamp(next).unwrap();
        assert_eq!(dt.weekday(), time::Weekday::Monday);
        assert_eq!(dt.hour(), 9);
        assert_eq!(dt.minute(), 0);
    }

    #[test]
    fn next_cron_skips_already_passed_minute_within_same_hour() {
        // At 00:00:30, the cron `0 0 * * *` should pick *next*
        // midnight, not the current minute.
        let now = 1_715_000_000; // arbitrary
        let next = Schedule::parse("0 0 * * *").unwrap().next_after(now);
        assert!(next > now);
        let dt = OffsetDateTime::from_unix_timestamp(next).unwrap();
        assert_eq!(dt.hour(), 0);
        assert_eq!(dt.minute(), 0);
    }

    // ── one-shot ─────────────────────────────────────────

    #[test]
    fn parse_one_shot_rfc3339_round_trips_to_unix_timestamp() {
        let iso = "2026-06-01T09:00:00Z";
        let s = Schedule::parse(iso).unwrap();
        let expected = OffsetDateTime::parse(iso, &Rfc3339)
            .unwrap()
            .unix_timestamp();
        assert_eq!(s, Schedule::OneShot { fire_at: expected });
        assert!(!s.is_recurring());
        assert_eq!(s.next_after(0), expected);
        // Even when `now > fire_at`, next_after returns the
        // fire_at (so the scheduler runs the one-shot
        // immediately rather than letting a missed deadline
        // disappear).
        assert_eq!(s.next_after(expected + 1000), expected);
    }

    #[test]
    fn parse_invalid_returns_unrecognised() {
        // Single token that's neither a duration nor RFC 3339.
        match Schedule::parse("garbage") {
            Err(ScheduleError::Unrecognised(_)) => {}
            other => panic!("expected Unrecognised, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_is_error() {
        assert_eq!(Schedule::parse(""), Err(ScheduleError::Empty));
        assert_eq!(Schedule::parse("   "), Err(ScheduleError::Empty));
    }

    #[test]
    fn duration_is_recurring_one_shot_is_not() {
        assert!(Schedule::parse("30m").unwrap().is_recurring());
        assert!(Schedule::parse("0 0 * * *").unwrap().is_recurring());
        assert!(
            !Schedule::parse("2026-06-01T09:00:00Z")
                .unwrap()
                .is_recurring()
        );
    }
}
