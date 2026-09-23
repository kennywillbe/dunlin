//! Daily summary message. Sending it also proves dunlin itself is alive.

use chrono::{DateTime, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;

use crate::config::SummaryConfig;
use crate::models::{Notification, State};

#[derive(Debug, Clone)]
pub struct ComponentUptime {
    pub name: String,
    pub up: i64,
    pub total: i64,
}

impl ComponentUptime {
    pub fn percent(&self) -> Option<f64> {
        if self.total > 0 {
            Some(self.up as f64 / self.total as f64 * 100.0)
        } else {
            None
        }
    }
}

/// Returns the local date to send for, or `None` when it is not yet due or was
/// already sent today. Uses `>=` so a process that started after the configured
/// time still sends once.
pub fn should_send(
    summary: &SummaryConfig,
    now: DateTime<Utc>,
    last_sent: Option<NaiveDate>,
) -> Option<NaiveDate> {
    let tz: Tz = summary.timezone.parse().ok()?;
    let (hour, minute) = crate::config::parse_hhmm(&summary.time)?;
    let local = now.with_timezone(&tz);
    let today = local.date_naive();
    if last_sent == Some(today) {
        return None;
    }
    if local.hour() * 60 + local.minute() >= hour * 60 + minute {
        Some(today)
    } else {
        None
    }
}

pub fn compose(
    uptimes: &[ComponentUptime],
    disks: &[(String, f64)],
    mem_pct: Option<f64>,
    swap_pct: Option<f64>,
    incident_count: i64,
) -> Notification {
    let mut lines = Vec::new();
    lines.push("Uptime (24h):".to_string());
    if uptimes.is_empty() {
        lines.push("  (no components)".to_string());
    }
    for u in uptimes {
        match u.percent() {
            Some(p) => lines.push(format!("  {} {:.2}% ({}/{})", u.name, p, u.up, u.total)),
            None => lines.push(format!("  {} no data", u.name)),
        }
    }
    for (mount, pct) in disks {
        lines.push(format!("Disk {mount}: {pct:.1}%"));
    }
    match mem_pct {
        Some(m) => lines.push(format!("Memory: {m:.1}%")),
        None => lines.push("Memory: unknown".to_string()),
    }
    match swap_pct {
        Some(s) => lines.push(format!("Swap: {s:.1}%")),
        None => lines.push("Swap: unknown".to_string()),
    }
    lines.push(format!("Incidents (24h): {incident_count}"));

    Notification {
        event: "summary".to_string(),
        title: "Dunlin daily summary".to_string(),
        message: lines.join("\n"),
        component: String::new(),
        state: State::Operational,
        incident_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn summary() -> SummaryConfig {
        SummaryConfig {
            time: "09:30".into(),
            timezone: "Europe/Berlin".into(),
        }
    }

    #[test]
    fn due_only_after_time_and_once_per_day() {
        let s = summary();
        // 07:00 UTC = 09:00 Berlin (CEST, +2) -> before 09:30
        let before = Utc.with_ymd_and_hms(2026, 6, 1, 7, 0, 0).unwrap();
        assert_eq!(should_send(&s, before, None), None);
        // 07:31 UTC = 09:31 Berlin -> due
        let due = Utc.with_ymd_and_hms(2026, 6, 1, 7, 31, 0).unwrap();
        let date = should_send(&s, due, None).unwrap();
        // already sent today -> no
        assert_eq!(should_send(&s, due, Some(date)), None);
        // next day -> due again
        let next = Utc.with_ymd_and_hms(2026, 6, 2, 8, 0, 0).unwrap();
        assert!(should_send(&s, next, Some(date)).is_some());
    }

    #[test]
    fn message_contains_facts() {
        let n = compose(
            &[
                ComponentUptime {
                    name: "Web".into(),
                    up: 1438,
                    total: 1440,
                },
                ComponentUptime {
                    name: "DB".into(),
                    up: 0,
                    total: 0,
                },
            ],
            &[("/".to_string(), 42.1)],
            Some(55.0),
            Some(12.0),
            2,
        );
        assert!(
            n.message.contains("Web 99.86% (1438/1440)"),
            "{}",
            n.message
        );
        assert!(n.message.contains("DB no data"));
        assert!(n.message.contains("Disk /: 42.1%"));
        assert!(n.message.contains("Memory: 55.0%"));
        assert!(n.message.contains("Swap: 12.0%"));
        assert!(n.message.contains("Incidents (24h): 2"));
    }
}
