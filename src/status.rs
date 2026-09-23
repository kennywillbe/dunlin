//! Deriving component and overall status from recorded results.

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
    pub day: i64,
    /// `None` when no probes were recorded that day.
    pub uptime: Option<f64>,
}

/// Build `days` UTC-day bars ending today, from `(day_start, total, up)` rows.
pub fn uptime_bars(daily: &[(i64, i64, i64)], days: i64, now: i64) -> Vec<DayBar> {
    use std::collections::HashMap;
    let map: HashMap<i64, (i64, i64)> = daily
        .iter()
        .map(|(d, total, up)| (*d, (*total, *up)))
        .collect();
    let today = (now.div_euclid(86_400)) * 86_400;
    (0..days)
        .rev()
        .map(|offset| {
            let day = today - offset * 86_400;
            let uptime = map.get(&day).and_then(|(total, up)| {
                if *total > 0 {
                    Some(*up as f64 / *total as f64 * 100.0)
                } else {
                    None
                }
            });
            DayBar { day, uptime }
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
        let now = 10 * 86_400 + 3600; // day 10, 01:00
        let daily = vec![(9 * 86_400, 100, 99), (10 * 86_400, 4, 3)];
        let bars = uptime_bars(&daily, 90, now);
        assert_eq!(bars.len(), 90);
        assert_eq!(bars[88].day, 9 * 86_400);
        assert_eq!(bars[88].uptime, Some(99.0));
        assert_eq!(bars[89].day, 10 * 86_400);
        assert_eq!(bars[89].uptime, Some(75.0));
        assert_eq!(bars[0].uptime, None);
    }
}
