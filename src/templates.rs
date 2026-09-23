//! Askama template structs and their view models.

use askama::Template;

use crate::config::Config;
use crate::models::{Incident, IncidentUpdate};
pub use crate::sentence::{Part, Say};

pub fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

pub fn iso8601(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// `Sep 24, 2026`, used where a date stands alone (tooltips, aria labels).
pub fn fmt_day(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%b %-d, %Y").to_string())
        .unwrap_or_default()
}

/// `Sep 24`: the date column of incident rows, where the year is noise.
pub fn fmt_short_day(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%b %-d").to_string())
        .unwrap_or_default()
}

/// Compact duration such as `38 min`, `2 h 5 min` or `3 d 4 h`.
pub fn human_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, 0) => "under a minute".to_string(),
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, 0, _) => format!("{d} d"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

/// A timestamp rendered in UTC on the server; the page script rewrites it in
/// the viewer's zone, so the UTC text is only the no-script fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeView {
    pub iso: String,
    pub text: String,
}

impl TimeView {
    pub fn datetime(ts: i64) -> Self {
        Self {
            iso: iso8601(ts),
            text: fmt_ts(ts),
        }
    }

    /// With seconds, for the "checked" fact where a minute is too coarse.
    pub fn clock(ts: i64) -> Self {
        Self {
            iso: iso8601(ts),
            text: chrono::DateTime::from_timestamp(ts, 0)
                .map(|d| d.format("%H:%M:%S UTC").to_string())
                .unwrap_or_default(),
        }
    }

    pub fn time(ts: i64) -> Self {
        Self {
            iso: iso8601(ts),
            text: chrono::DateTime::from_timestamp(ts, 0)
                .map(|d| d.format("%H:%M UTC").to_string())
                .unwrap_or_default(),
        }
    }
}

// Stroke follows `currentColor` so CSS decides the colour.
pub const ICON_INFO: &str = r#"<svg class="icon" viewBox="0 0 16 16" aria-hidden="true" focusable="false"><circle cx="8" cy="8" r="6.2" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M8 7.2v4" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><circle cx="8" cy="4.9" r=".9" fill="currentColor"/></svg>"#;

/// Everything the shared layout needs; passed to every page.
#[derive(Debug, Clone)]
pub struct SiteView {
    pub title: String,
    /// Generated from a validated `#rrggbb`, so safe to inline.
    pub accent_css: String,
    pub has_logo: bool,
    pub has_custom_css: bool,
    pub logged_in: bool,
    /// Current nav section, for `aria-current`.
    pub section: &'static str,
    pub logo_svg: &'static str,
}

impl SiteView {
    pub fn new(cfg: &Config, logged_in: bool, section: &'static str) -> Self {
        Self {
            title: cfg.theme.title.trim().to_string(),
            accent_css: crate::theme::accent_css(&cfg.theme.accent),
            has_logo: cfg.theme_files.logo.is_some(),
            has_custom_css: cfg.theme_files.custom_css.is_some(),
            logged_in,
            section,
            logo_svg: crate::theme::LOGO_SVG,
        }
    }
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
    pub active: bool,
    pub starts: TimeView,
    pub ends: TimeView,
}

#[derive(Debug, Clone)]
pub struct BarView {
    pub class: String,
    pub date: String,
    /// Empty when the day has no data.
    pub uptime: String,
    pub label: String,
    pub aria: String,
}

#[derive(Debug, Clone)]
pub struct ComponentView {
    pub name: String,
    pub description: Option<String>,
    pub state_class: String,
    pub state_label: String,
    /// The line under the name: latency or kind when healthy, the state when not.
    pub sub: String,
    /// Colour hook for `sub`; empty when healthy.
    pub sub_class: &'static str,
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
    pub state_class: String,
    pub impact_class: String,
    pub impact_label: String,
    /// `Sep 24`, for the date column.
    pub day: String,
    pub created: TimeView,
    pub resolved: Option<TimeView>,
    pub duration: String,
    pub auto: bool,
    pub last_message: Option<String>,
}

impl IncidentView {
    /// `component` is the display name; the caller maps ids to names.
    pub fn new(inc: &Incident, component: String, last_message: Option<String>, now: i64) -> Self {
        let end = inc.resolved_at.unwrap_or(now);
        Self {
            id: inc.id,
            title: inc.title.clone(),
            component,
            state_label: inc.state.label().to_string(),
            state_class: inc.state.as_str().to_string(),
            impact_class: inc.impact.as_str().to_string(),
            impact_label: inc.impact.label().to_string(),
            day: fmt_short_day(inc.created_at),
            created: TimeView::datetime(inc.created_at),
            resolved: inc.resolved_at.map(TimeView::datetime),
            duration: human_duration(end - inc.created_at),
            auto: inc.auto,
            last_message,
        }
    }
}

/// One line of "What happened lately": an incident or a maintenance window.
#[derive(Debug, Clone)]
pub struct PastItem {
    pub href: Option<String>,
    pub title: String,
    /// Severity colour of the swatch, a `State` name.
    pub swatch: String,
    /// `Resolved · 18 min`, `Investigating`, `Completed · 10 min`.
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct DayView {
    pub heading: String,
    pub iso: String,
    pub items: Vec<PastItem>,
}

#[derive(Debug, Clone)]
pub struct MonthView {
    pub name: String,
    pub incidents: Vec<IncidentView>,
}

#[derive(Debug, Clone)]
pub struct UpdateView {
    pub state_label: String,
    pub state_class: String,
    pub ts: TimeView,
    pub message: String,
}

impl From<&IncidentUpdate> for UpdateView {
    fn from(u: &IncidentUpdate) -> Self {
        Self {
            state_label: u.state.label().to_string(),
            state_class: u.state.as_str().to_string(),
            ts: TimeView::datetime(u.ts),
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

/// One series drawn in a chart card.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CardSeries {
    pub label: String,
    pub scope: String,
    pub metric: String,
    pub key: String,
    /// Second y axis, for a series whose unit differs from the first.
    pub right: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CardView {
    pub id: String,
    pub title: String,
    pub series: Vec<CardSeries>,
    /// Value where the matching check starts to complain, drawn as a line.
    pub threshold: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct SectionView {
    pub title: String,
    pub empty_note: String,
    pub cards: Vec<CardView>,
}

/// The small mono list at the foot of the left column.
#[derive(Debug, Clone)]
pub struct FactsView {
    pub checked: Option<TimeView>,
    pub every: Option<String>,
    pub watching: String,
    pub open: usize,
}

/// The left column: a sentence, a paragraph and, on data pages, the facts.
#[derive(Debug, Clone, Default)]
pub struct SideView {
    pub say: Say,
    pub facts: Option<FactsView>,
}

impl SideView {
    pub fn plain(headline: &str, detail: &str) -> Self {
        Self {
            say: Say {
                headline: vec![Part::Text(headline.to_string())],
                detail: if detail.is_empty() {
                    Vec::new()
                } else {
                    vec![Part::Text(detail.to_string())]
                },
            },
            facts: None,
        }
    }
}

#[derive(Template)]
#[template(path = "status.html")]
pub struct StatusTemplate {
    pub site: SiteView,
    pub side: SideView,
    pub groups: Vec<GroupView>,
    pub past_days: Vec<DayView>,
    pub info_icon: &'static str,
}

#[derive(Template)]
#[template(path = "metrics.html")]
pub struct MetricsTemplate {
    pub site: SiteView,
    pub side: SideView,
    pub range: String,
    /// Whether any chart draws a trouble line, so the key is only shown then.
    pub has_lines: bool,
    pub sections: Vec<SectionView>,
    pub cards_json: String,
}

#[derive(Template)]
#[template(path = "incidents.html")]
pub struct IncidentsTemplate {
    pub site: SiteView,
    pub side: SideView,
    pub months: Vec<MonthView>,
}

#[derive(Template)]
#[template(path = "incident.html")]
pub struct IncidentTemplate {
    pub site: SiteView,
    pub side: SideView,
    pub incident: IncidentView,
    pub updates: Vec<UpdateView>,
}

#[derive(Template)]
#[template(path = "manage.html")]
pub struct ManageTemplate {
    pub site: SiteView,
    pub side: SideView,
    pub components: Vec<NavComponent>,
    pub maintenance: Vec<MaintenanceView>,
    pub active_incidents: Vec<IncidentView>,
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginTemplate {
    pub site: SiteView,
    pub side: SideView,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorTemplate {
    pub site: SiteView,
    pub side: SideView,
    pub code: u16,
    pub heading: String,
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
    pub feed_title: String,
    pub feed_id: String,
    pub feed_url: String,
    pub updated: String,
    pub entries: Vec<FeedEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_compact() {
        assert_eq!(human_duration(20), "under a minute");
        assert_eq!(human_duration(38 * 60), "38 min");
        assert_eq!(human_duration(2 * 3600), "2 h");
        assert_eq!(human_duration(2 * 3600 + 5 * 60), "2 h 5 min");
        assert_eq!(human_duration(3 * 86_400 + 4 * 3600 + 60), "3 d 4 h");
        assert_eq!(human_duration(86_400), "1 d");
    }

    #[test]
    fn day_format() {
        assert_eq!(fmt_day(1_758_700_000), "Sep 24, 2025");
    }
}
