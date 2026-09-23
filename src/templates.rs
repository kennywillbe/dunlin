//! Askama template structs and their view models.

use askama::Template;

use crate::models::{Incident, IncidentUpdate, State};

pub fn state_label(state: State) -> String {
    state.label().to_string()
}

pub fn state_class(state: State) -> String {
    state.as_str().to_string()
}

pub fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct NavComponent {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct MaintenanceView {
    pub id: i64,
    pub component: String,
    pub note: String,
    pub ends: String,
}

#[derive(Debug, Clone)]
pub struct BarView {
    pub class: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct ComponentView {
    pub name: String,
    pub description: Option<String>,
    pub state_class: String,
    pub state_label: String,
    pub uptime_90: Option<String>,
    pub bars: Vec<BarView>,
}

#[derive(Debug, Clone)]
pub struct GroupView {
    pub name: String,
    pub components: Vec<ComponentView>,
}

#[derive(Debug, Clone)]
pub struct IncidentView {
    pub id: i64,
    pub title: String,
    pub component: String,
    pub state_label: String,
    pub impact_class: String,
    pub impact_label: String,
    pub created: String,
    pub auto: bool,
    pub last_message: Option<String>,
}

impl IncidentView {
    pub fn from_incident(inc: &Incident, last_message: Option<String>) -> Self {
        Self {
            id: inc.id,
            title: inc.title.clone(),
            component: if inc.component.is_empty() {
                "all components".to_string()
            } else {
                inc.component.clone()
            },
            state_label: inc.state.label().to_string(),
            impact_class: inc.impact.as_str().to_string(),
            impact_label: inc.impact.label().to_string(),
            created: fmt_ts(inc.created_at),
            auto: inc.auto,
            last_message,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpdateView {
    pub state_label: String,
    pub ts: String,
    pub message: String,
}

impl From<&IncidentUpdate> for UpdateView {
    fn from(u: &IncidentUpdate) -> Self {
        Self {
            state_label: u.state.label().to_string(),
            ts: fmt_ts(u.ts),
            message: u.message.clone(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SeriesView {
    pub index: usize,
    pub label: String,
    pub scope: String,
    pub metric: String,
    pub key: String,
}

#[derive(Template)]
#[template(path = "status.html")]
pub struct StatusTemplate {
    pub overall_class: String,
    pub overall_label: String,
    pub groups: Vec<GroupView>,
    pub active_incidents: Vec<IncidentView>,
    pub recent_incidents: Vec<IncidentView>,
    pub has_active_incidents: bool,
    pub has_recent_incidents: bool,
    pub logged_in: bool,
    pub all_components: Vec<NavComponent>,
    pub maintenance_windows: Vec<MaintenanceView>,
}

#[derive(Template)]
#[template(path = "metrics.html")]
pub struct MetricsTemplate {
    pub logged_in: bool,
    pub series: Vec<SeriesView>,
    pub series_json: String,
}

#[derive(Template)]
#[template(path = "incidents.html")]
pub struct IncidentsTemplate {
    pub logged_in: bool,
    pub incidents: Vec<IncidentView>,
}

#[derive(Template)]
#[template(path = "incident.html")]
pub struct IncidentTemplate {
    pub logged_in: bool,
    pub id: i64,
    pub title: String,
    pub impact_class: String,
    pub impact_label: String,
    pub component: String,
    pub state_label: String,
    pub created: String,
    pub has_resolved: bool,
    pub resolved: String,
    pub auto: bool,
    pub updates: Vec<UpdateView>,
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginTemplate {
    pub logged_in: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FeedEntry {
    pub title: String,
    pub id: String,
    pub url: String,
    pub updated: String,
    pub published: String,
    pub content: String,
}

#[derive(Template)]
#[template(path = "feed.xml")]
pub struct FeedTemplate {
    pub feed_id: String,
    pub feed_url: String,
    pub updated: String,
    pub entries: Vec<FeedEntry>,
}
