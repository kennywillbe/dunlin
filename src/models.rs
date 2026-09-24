//! Shared domain types.

use std::fmt;

/// Component / overall status. Discriminant order is severity order, so the
/// worst state is simply the maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum State {
    Operational,
    Maintenance,
    Degraded,
    PartialOutage,
    MajorOutage,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Operational => "operational",
            State::Maintenance => "maintenance",
            State::Degraded => "degraded",
            State::PartialOutage => "partial_outage",
            State::MajorOutage => "major_outage",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            State::Operational => "Operational",
            State::Maintenance => "Under maintenance",
            State::Degraded => "Degraded performance",
            State::PartialOutage => "Partial outage",
            State::MajorOutage => "Major outage",
        }
    }

    pub fn from_name(s: &str) -> Option<State> {
        Some(match s {
            "operational" => State::Operational,
            "maintenance" => State::Maintenance,
            "degraded" => State::Degraded,
            "partial_outage" => State::PartialOutage,
            "major_outage" => State::MajorOutage,
            _ => return None,
        })
    }

    /// State used for an automatically opened incident.
    pub fn for_incident_impact(self) -> State {
        match self {
            State::Operational | State::Maintenance => State::MajorOutage,
            other => other,
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Internal up/down state of a check's alert machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Up,
    Down,
}

/// Incident lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncidentState {
    Investigating,
    Identified,
    Monitoring,
    Resolved,
}

impl IncidentState {
    pub fn as_str(self) -> &'static str {
        match self {
            IncidentState::Investigating => "investigating",
            IncidentState::Identified => "identified",
            IncidentState::Monitoring => "monitoring",
            IncidentState::Resolved => "resolved",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            IncidentState::Investigating => "Investigating",
            IncidentState::Identified => "Identified",
            IncidentState::Monitoring => "Monitoring",
            IncidentState::Resolved => "Resolved",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "investigating" => IncidentState::Investigating,
            "identified" => IncidentState::Identified,
            "monitoring" => IncidentState::Monitoring,
            "resolved" => IncidentState::Resolved,
            _ => return None,
        })
    }
}

/// A generic time series row.
#[derive(Debug, Clone)]
pub struct Sample {
    pub ts: i64,
    pub scope: String,
    pub metric: String,
    pub key: String,
    pub value: f64,
}

/// One probe outcome.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub ts: i64,
    pub check_id: String,
    pub ok: bool,
    pub degraded: bool,
    pub latency_ms: Option<f64>,
    pub message: Option<String>,
}

/// A recorded incident.
#[derive(Debug, Clone)]
pub struct Incident {
    pub id: i64,
    pub component: String,
    pub title: String,
    pub state: IncidentState,
    pub impact: State,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
    pub auto: bool,
}

/// A timeline entry on an incident.
#[derive(Debug, Clone)]
pub struct IncidentUpdate {
    pub id: i64,
    pub incident_id: i64,
    pub ts: i64,
    pub state: IncidentState,
    pub message: String,
    pub auto: bool,
}

/// A scheduled maintenance window.
#[derive(Debug, Clone)]
pub struct Maintenance {
    pub id: i64,
    pub component: String,
    pub note: String,
    pub starts_at: i64,
    pub ends_at: i64,
}

impl Maintenance {
    pub fn covers(&self, component: &str, now: i64) -> bool {
        now >= self.starts_at
            && now < self.ends_at
            && (self.component == component || self.component.is_empty())
    }
}

/// Outbound notification built by the alert engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub event: String,
    pub title: String,
    pub message: String,
    pub component: String,
    pub state: State,
    pub incident_id: Option<i64>,
    /// Incident page under `public_url`. `message` already ends with it, so
    /// channels that show a link on its own can take it back out.
    pub link: Option<String>,
}

impl Notification {
    /// The message without the trailing incident link.
    pub fn text(&self) -> &str {
        self.link
            .as_deref()
            .and_then(|l| self.message.strip_suffix(l))
            .map(|m| m.strip_suffix('\n').unwrap_or(m))
            .unwrap_or(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_state() {
        let states = [State::Operational, State::Degraded, State::PartialOutage];
        assert_eq!(*states.iter().max().unwrap(), State::PartialOutage);
        assert_eq!(
            State::Maintenance.max(State::Operational),
            State::Maintenance
        );
        assert_eq!(State::MajorOutage.max(State::Degraded), State::MajorOutage);
    }

    #[test]
    fn round_trip_state_strings() {
        for s in [
            State::Operational,
            State::Maintenance,
            State::Degraded,
            State::PartialOutage,
            State::MajorOutage,
        ] {
            assert_eq!(State::from_name(s.as_str()), Some(s));
        }
    }

    #[test]
    fn maintenance_covers() {
        let m = Maintenance {
            id: 1,
            component: "web".into(),
            note: String::new(),
            starts_at: 100,
            ends_at: 200,
        };
        assert!(m.covers("web", 150));
        assert!(!m.covers("db", 150));
        assert!(!m.covers("web", 200));
        let all = Maintenance {
            component: String::new(),
            ..m.clone()
        };
        assert!(all.covers("db", 150));
    }
}
