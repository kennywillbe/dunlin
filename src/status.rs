//! Deriving component and overall status from recorded results.

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::days::{date_of, Day};
use crate::models::{CheckResult, State};

/// State of a check from its latest result and consecutive failure count.
pub fn check_state(
    last: Option<&CheckResult>,
    consecutive_failures: i64,
    failures_to_open: u32,
) -> State {
    match last {
        None => State::Operational,
        Some(r) if !r.ok => {
            if consecutive_failures >= failures_to_open as i64 {
                State::MajorOutage
            } else {
                State::PartialOutage
            }
        }
        Some(r) if r.degraded => State::Degraded,
        Some(_) => State::Operational,
    }
}

/// Fold maintenance and manual incident impact into a component state.
pub fn component_state(base: State, maintenance: bool, incident_impact: Option<State>) -> State {
    let mut state = if maintenance {
        State::Maintenance
    } else {
        base
    };
    if let Some(impact) = incident_impact {
        // Maintenance is not a severity to escalate to.
        if impact != State::Maintenance {
            state = state.max(impact);
        }
    }
    state
}

/// Overall banner: the worst component state.
pub fn overall(states: impl IntoIterator<Item = State>) -> State {
    states.into_iter().max().unwrap_or(State::Operational)
}

/// One bar of the 90-day uptime strip.
#[derive(Debug, Clone, PartialEq)]
pub struct DayBar {
    pub day: Day,
    /// `None` when no probes were recorded that day.
    pub uptime: Option<f64>,
}

/// One bar per entry of `days`, summed from `(slot_start, total, up)` rows.
pub fn uptime_bars(slots: &[(i64, i64, i64)], days: &[Day], tz: Tz) -> Vec<DayBar> {
    use std::collections::HashMap;
    let mut per_day: HashMap<NaiveDate, (i64, i64)> = HashMap::new();
    for (slot, total, up) in slots {
        let e = per_day.entry(date_of(*slot, tz)).or_default();
        e.0 += total;
        e.1 += up;
    }
    days.iter()
        .map(|day| {
            let uptime = per_day.get(&day.date).and_then(|(total, up)| {
                if *total > 0 {
                    Some(*up as f64 / *total as f64 * 100.0)
                } else {
                    None
                }
            });
            DayBar { day: *day, uptime }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(ok: bool, degraded: bool) -> CheckResult {
        CheckResult {
            ts: 0,
            check_id: "c".into(),
            ok,
            degraded,
            latency_ms: None,
            message: None,
        }
    }

    #[test]
    fn check_state_derivation() {
        assert_eq!(check_state(None, 0, 3), State::Operational);
        assert_eq!(
            check_state(Some(&result(true, false)), 0, 3),
            State::Operational
        );
        assert_eq!(
            check_state(Some(&result(true, true)), 0, 3),
            State::Degraded
        );
        assert_eq!(
            check_state(Some(&result(false, false)), 1, 3),
            State::PartialOutage
        );
        assert_eq!(
            check_state(Some(&result(false, false)), 3, 3),
            State::MajorOutage
        );
    }

    #[test]
    fn component_and_overall() {
        assert_eq!(
            component_state(State::Operational, true, None),
            State::Maintenance
        );
        assert_eq!(
            component_state(State::Operational, false, Some(State::MajorOutage)),
            State::MajorOutage
        );
        assert_eq!(
            component_state(State::MajorOutage, false, Some(State::Degraded)),
            State::MajorOutage
        );
        assert_eq!(
            overall(vec![State::Operational, State::Degraded]),
            State::Degraded
        );
        assert_eq!(overall(Vec::new()), State::Operational);
    }

    #[test]
    fn bars_cover_window_and_compute_uptime() {
        let now = 10 * 86_400 + 3600; // day 10, 01:00 UTC
        let slots = vec![
            (9 * 86_400, 60, 60),
            (9 * 86_400 + 900, 40, 39),
            (10 * 86_400, 4, 3),
        ];
        let days = crate::days::last_days(now, 90, Tz::UTC);
        let bars = uptime_bars(&slots, &days, Tz::UTC);
        assert_eq!(bars.len(), 90);
        assert_eq!(bars[88].day.start, 9 * 86_400);
        assert_eq!(bars[88].uptime, Some(99.0));
        assert_eq!(bars[89].day.start, 10 * 86_400);
        assert_eq!(bars[89].uptime, Some(75.0));
        assert_eq!(bars[0].uptime, None);
    }

    #[test]
    fn istanbul_bars_follow_local_midnight() {
        let tz = chrono_tz::Europe::Istanbul;
        // 2026-09-23 23:15 UTC is 02:15 on Sep 24 in Istanbul.
        let late = 1_790_205_300;
        let slots = vec![(late - 3 * 3600, 10, 10), (late, 10, 0)];
        let days = crate::days::last_days(late, 2, tz);
        let bars = uptime_bars(&slots, &days, tz);
        assert_eq!(bars[0].day.iso(), "2026-09-23");
        assert_eq!(bars[0].uptime, Some(100.0));
        assert_eq!(bars[1].day.iso(), "2026-09-24");
        assert_eq!(bars[1].uptime, Some(0.0));
        // In UTC both slots are on Sep 23.
        let days = crate::days::last_days(late, 2, Tz::UTC);
        let bars = uptime_bars(&slots, &days, Tz::UTC);
        assert_eq!(bars[1].day.iso(), "2026-09-23");
        assert_eq!(bars[1].uptime, Some(50.0));
    }

    #[test]
    fn dst_day_counts_every_slot_once() {
        let tz = chrono_tz::Europe::Berlin;
        let day = Day::on(NaiveDate::from_ymd_opt(2026, 10, 25).unwrap(), tz);
        // Up only inside the long day, so a leak either way shows as < 100%
        // there or > 0% next door.
        let slots: Vec<_> = (day.start - 3600..day.end + 3600)
            .step_by(900)
            .map(|s| (s, 1, i64::from(day.contains(s))))
            .collect();
        let prev = Day::on(day.date.pred_opt().unwrap(), tz);
        let next = Day::on(day.date.succ_opt().unwrap(), tz);
        let bars = uptime_bars(&slots, &[prev, day, next], tz);
        assert_eq!(bars[0].uptime, Some(0.0));
        assert_eq!(bars[1].uptime, Some(100.0));
        assert_eq!(bars[2].uptime, Some(0.0));
        let inside = slots.iter().filter(|(s, _, _)| day.contains(*s)).count();
        assert_eq!(inside, 25 * 4);
    }
}
