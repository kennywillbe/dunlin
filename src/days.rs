//! Calendar days in the configured timezone.
//!
//! A day is the half-open range `[start, end)` of Unix seconds between two
//! local midnights, so a DST day is 23 or 25 hours long and every instant
//! falls in exactly one day.

use chrono::{DateTime, LocalResult, NaiveDate, NaiveDateTime, TimeDelta, TimeZone};
use chrono_tz::Tz;

/// Width of the slots per-day uptime is summed from. Every offset and DST
/// transition in current use is a multiple of 15 minutes (India +05:30, Nepal
/// +05:45, Chatham +12:45), so each slot sits wholly inside one local day. Only
/// historic local mean time offsets break that; such a slot counts for the day
/// it starts in, which moves at most 15 minutes of probes and drops none.
pub const SLOT_SECS: i64 = 900;

/// Days in the status page's tick strips and its 90-day uptime figure.
pub const STRIP_DAYS: usize = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Day {
    pub date: NaiveDate,
    pub start: i64,
    pub end: i64,
}

impl Day {
    pub fn of(ts: i64, tz: Tz) -> Self {
        Self::on(date_of(ts, tz), tz)
    }

    pub fn on(date: NaiveDate, tz: Tz) -> Self {
        let start = start_of(date, tz);
        let end = date
            .succ_opt()
            .map_or(start + 86_400, |next| start_of(next, tz));
        Self {
            date,
            start,
            end: end.max(start),
        }
    }

    pub fn contains(&self, ts: i64) -> bool {
        self.start <= ts && ts < self.end
    }

    /// Whether `[from, to)` overlaps this day.
    pub fn overlaps(&self, from: i64, to: i64) -> bool {
        from < self.end && to > self.start
    }

    /// `2026-09-24`, for `<time datetime>`.
    pub fn iso(&self) -> String {
        self.date.format("%Y-%m-%d").to_string()
    }
}

pub fn date_of(ts: i64, tz: Tz) -> NaiveDate {
    DateTime::from_timestamp(ts, 0)
        .unwrap_or_default()
        .with_timezone(&tz)
        .date_naive()
}

/// First instant of `date` in `tz`.
pub fn start_of(date: NaiveDate, tz: Tz) -> i64 {
    let midnight = date.and_time(chrono::NaiveTime::MIN);
    if let Some(ts) = earliest(tz, midnight) {
        return ts;
    }
    // Zones such as America/Santiago move their clocks at midnight, so the
    // day starts at the first wall time after the gap. Samoa skipped a whole
    // date in 2011; the two-day bound makes such a date an empty day at the
    // start of the next one instead of looping.
    let step = TimeDelta::minutes(15);
    let mut t = midnight;
    for _ in 0..2 * 96 {
        t += step;
        if let Some(ts) = earliest(tz, t) {
            return ts;
        }
    }
    midnight.and_utc().timestamp()
}

fn earliest(tz: Tz, local: NaiveDateTime) -> Option<i64> {
    match tz.from_local_datetime(&local) {
        LocalResult::Single(t) => Some(t.timestamp()),
        LocalResult::Ambiguous(a, b) => Some(a.timestamp().min(b.timestamp())),
        LocalResult::None => None,
    }
}

/// The last `n` days up to and including the one holding `now`, oldest first.
pub fn last_days(now: i64, n: usize, tz: Tz) -> Vec<Day> {
    let mut days = Vec::with_capacity(n);
    let mut date = date_of(now, tz);
    for _ in 0..n {
        days.push(Day::on(date, tz));
        match date.pred_opt() {
            Some(d) => date = d,
            None => break,
        }
    }
    days.reverse();
    days
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0)
            .unwrap()
            .timestamp()
    }

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn istanbul_after_midnight_is_the_next_day() {
        // 02:28 local on Sep 24 is still Sep 23 in UTC.
        let ts = utc(2026, 9, 23, 23, 28);
        let day = Day::of(ts, chrono_tz::Europe::Istanbul);
        assert_eq!(day.date, ymd(2026, 9, 24));
        assert_eq!(day.start, utc(2026, 9, 23, 21, 0));
        assert_eq!(day.end, utc(2026, 9, 24, 21, 0));
        assert_eq!(Day::of(ts, Tz::UTC).date, ymd(2026, 9, 23));
    }

    #[test]
    fn dst_days_are_23_and_25_hours() {
        let berlin = chrono_tz::Europe::Berlin;
        let spring = Day::on(ymd(2026, 3, 29), berlin);
        assert_eq!(spring.end - spring.start, 23 * 3600);
        let autumn = Day::on(ymd(2026, 10, 25), berlin);
        assert_eq!(autumn.end - autumn.start, 25 * 3600);
        // The repeated 02:30 belongs to the long day both times.
        assert!(autumn.contains(utc(2026, 10, 25, 0, 30)));
        assert!(autumn.contains(utc(2026, 10, 25, 1, 30)));
        assert_eq!(Day::on(ymd(2026, 10, 24), berlin).end, autumn.start);
    }

    #[test]
    fn days_tile_without_gaps_across_dst() {
        let days = last_days(utc(2026, 11, 1, 12, 0), 60, chrono_tz::Europe::Berlin);
        assert_eq!(days.len(), 60);
        assert_eq!(days[59].date, ymd(2026, 11, 1));
        for w in days.windows(2) {
            assert_eq!(w[0].end, w[1].start);
            assert_eq!(w[0].date.succ_opt(), Some(w[1].date));
        }
    }

    #[test]
    fn midnight_in_a_gap_starts_after_it() {
        // Santiago springs forward at 00:00 on 2026-09-06, straight to 01:00.
        let santiago = chrono_tz::America::Santiago;
        let day = Day::on(ymd(2026, 9, 6), santiago);
        assert_eq!(date_of(day.start, santiago), ymd(2026, 9, 6));
        assert_eq!(date_of(day.start - 1, santiago), ymd(2026, 9, 5));
        assert_eq!(day.end - day.start, 23 * 3600);
    }

    #[test]
    fn half_hour_offsets_align_with_slots() {
        let kolkata = chrono_tz::Asia::Kolkata;
        let day = Day::on(ymd(2026, 9, 24), kolkata);
        assert_eq!(day.start, utc(2026, 9, 23, 18, 30));
        assert_eq!(day.start % SLOT_SECS, 0);
        assert_eq!(day.end - day.start, 86_400);
    }
}
