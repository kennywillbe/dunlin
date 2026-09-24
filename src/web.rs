//! HTTP surface: read pages, write actions, heartbeat endpoint and assets.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;

use askama::Template;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use tokio::sync::watch;

use crate::auth;
use crate::collector::configured_mounts;
use crate::config::{CheckType, Config};
use crate::days::Day;
use crate::db::{self, Pool};
use crate::models::State as CompState;
use crate::status;
use crate::templates::*;
use crate::web_auth::{
    clear_session_cookie, client_ip, csrf_ok, session_cookie, LoginLimiter, SESSION_COOKIE,
};

#[derive(Clone)]
pub struct AppState {
    pub pool: Pool,
    pub config: watch::Receiver<Arc<Config>>,
    pub limiter: Arc<LoginLimiter>,
}

impl AppState {
    /// Returns the state and the sender used to hot-reload the config.
    pub fn new(pool: Pool, cfg: Arc<Config>) -> (Self, watch::Sender<Arc<Config>>) {
        let (tx, rx) = watch::channel(cfg);
        (
            Self {
                pool,
                config: rx,
                limiter: Arc::new(LoginLimiter::new()),
            },
            tx,
        )
    }

    pub fn cfg(&self) -> Arc<Config> {
        self.config.borrow().clone()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(status_page))
        .route("/metrics", get(metrics_page))
        .route("/api/metrics", get(api_metrics))
        .route("/incidents", get(incidents_page))
        .route("/incidents/{id}", get(incident_page))
        .route("/manage", get(manage_page))
        .route("/feed.xml", get(feed))
        .route("/badge/{file}", get(badge))
        .route("/badge/{component}/uptime.svg", get(uptime_badge))
        .route("/health", get(health))
        .route("/assets/{file}", get(asset))
        .route("/assets/fonts/{file}", get(font))
        .route("/hb/{token}", get(heartbeat).post(heartbeat))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .route("/incidents", post(create_incident))
        .route("/incidents/{id}/updates", post(add_update))
        .route("/incidents/{id}/resolve", post(resolve_incident))
        .route("/maintenance", post(start_maintenance))
        .route("/maintenance/{id}/end", post(end_maintenance))
        .fallback(not_found)
        .with_state(state)
}

fn render<T: Template>(template: &T) -> Response {
    match template.render() {
        Ok(body) => ([(CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "template rendering failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

fn error_page(cfg: &Config, status: StatusCode, heading: &str, side: SideView) -> Response {
    let page = ErrorTemplate {
        site: SiteView::new(cfg, false, ""),
        side,
        code: status.as_u16(),
        heading: heading.to_string(),
    };
    (status, render(&page)).into_response()
}

fn internal(state: &AppState, err: impl std::fmt::Display) -> Response {
    tracing::error!(error = %err, "request failed");
    error_page(
        &state.cfg(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong",
        SideView::plain(
            "Something broke on this side.",
            "The error has been logged. Trying again in a minute may work.",
        ),
    )
}

async fn not_found(State(state): State<AppState>, uri: axum::http::Uri) -> Response {
    // Long junk paths would push the sentence off the screen.
    let mut path: String = uri.path().chars().take(60).collect();
    if uri.path().chars().count() > 60 {
        path.push('…');
    }
    error_page(
        &state.cfg(),
        StatusCode::NOT_FOUND,
        "Page not found",
        SideView::plain(
            &format!("There is nothing at {path}."),
            "The address may be old or mistyped.",
        ),
    )
}

async fn is_logged_in(state: &AppState, jar: &CookieJar) -> bool {
    let Some(cookie) = jar.get(SESSION_COOKIE) else {
        return false;
    };
    let hash = auth::token_hash(cookie.value());
    db::session_valid(&state.pool, &hash, crate::now_ts())
        .await
        .unwrap_or(false)
}

/// Enforce `protect_read` for read pages. `None` means allowed.
async fn read_guard(state: &AppState, jar: &CookieJar) -> Option<Response> {
    if state.cfg().web.protect_read && !is_logged_in(state, jar).await {
        Some(Redirect::to("/login").into_response())
    } else {
        None
    }
}

/// Enforce CSRF then the session for write actions. `None` means allowed.
async fn write_guard(state: &AppState, headers: &HeaderMap, jar: &CookieJar) -> Option<Response> {
    if !csrf_ok(headers) {
        return Some((StatusCode::FORBIDDEN, "CSRF check failed").into_response());
    }
    if !is_logged_in(state, jar).await {
        return Some(Redirect::to("/login").into_response());
    }
    None
}

/// Display name for a component id; incidents and maintenance store ids.
fn component_name(cfg: &Config, id: &str) -> String {
    if id.is_empty() {
        return "All components".to_string();
    }
    cfg.component(id)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| id.to_string())
}

fn maintenance_views(
    cfg: &Config,
    windows: &[crate::models::Maintenance],
    now: i64,
) -> Vec<MaintenanceView> {
    windows
        .iter()
        .map(|m| MaintenanceView {
            id: m.id,
            component: component_name(cfg, &m.component),
            note: m.note.clone(),
            active: m.starts_at <= now,
            starts: TimeView::datetime(m.starts_at),
            ends: TimeView::datetime(m.ends_at),
        })
        .collect()
}

async fn incident_views(
    pool: &Pool,
    cfg: &Config,
    incidents: &[crate::models::Incident],
    now: i64,
) -> Vec<IncidentView> {
    let mut out = Vec::with_capacity(incidents.len());
    for inc in incidents {
        let msg = db::last_update_message(pool, inc.id).await.ok().flatten();
        out.push(IncidentView::new(
            inc,
            component_name(cfg, &inc.component),
            msg,
            now,
            cfg.tz(),
        ));
    }
    out
}

// -- read pages ------------------------------------------------------------

/// How many days of history the status page lists, as on Statuspage.
const PAST_DAYS: usize = 14;

fn fmt_every(secs: u64) -> String {
    if secs < 120 || !secs.is_multiple_of(60) {
        format!("{secs} s")
    } else {
        format!("{} min", secs / 60)
    }
}

/// The mono facts at the foot of the left column.
async fn facts(pool: &Pool, cfg: &Config) -> FactsView {
    let n = cfg.components.len();
    FactsView {
        checked: db::last_result_ts(pool)
            .await
            .ok()
            .flatten()
            .map(TimeView::clock),
        // Heartbeats are pushed to us, so their period is not a check rate.
        every: cfg
            .checks
            .iter()
            .filter(|c| c.kind != CheckType::Heartbeat)
            .map(|c| c.interval.as_secs())
            .min()
            .map(fmt_every),
        watching: if n == 1 {
            "1 thing".to_string()
        } else {
            format!("{n} things")
        },
        open: db::active_incidents(pool)
            .await
            .map(|v| v.len())
            .unwrap_or(0),
    }
}

async fn status_page(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Some(r) = read_guard(&state, &jar).await {
        return r;
    }
    let cfg = state.cfg();
    let logged_in = is_logged_in(&state, &jar).await;
    let now = crate::now_ts();

    let windows = db::current_and_upcoming_maintenance(&state.pool, now)
        .await
        .unwrap_or_default();
    let active = db::active_incidents(&state.pool).await.unwrap_or_default();
    let tz = cfg.tz();
    let strip = crate::days::last_days(now, crate::days::STRIP_DAYS, tz);
    let strip_from = strip.first().map_or(now, |d| d.start);
    let history = History {
        days: strip,
        incidents: db::incidents_since(&state.pool, strip_from)
            .await
            .unwrap_or_default(),
        maintenance: db::maintenance_between(&state.pool, strip_from, now + 1)
            .await
            .unwrap_or_default(),
    };

    // Group components; components without a group land in "Other".
    let mut group_names: Vec<(String, String)> = cfg
        .groups
        .iter()
        .map(|g| (g.id.clone(), g.name.clone()))
        .collect();
    group_names.push(("__other".to_string(), "Other".to_string()));

    let mut groups: Vec<GroupView> = Vec::new();
    let mut snapshots = Vec::new();
    for (gid, gname) in &group_names {
        let members: Vec<_> = cfg
            .components
            .iter()
            .filter(|c| match (&c.group, gid.as_str()) {
                (Some(g), id) => g == id,
                (None, "__other") => true,
                _ => false,
            })
            .collect();
        if members.is_empty() {
            continue;
        }
        let mut components = Vec::new();
        for comp in members {
            let (view, snap) =
                build_component(&state.pool, &cfg, comp, &windows, &active, &history, now).await;
            components.push(view);
            snapshots.push(snap);
        }
        groups.push(GroupView {
            name: gname.clone(),
            components,
        });
    }

    // The detail leads with the worst open incident, newest first on ties.
    let lead = active
        .iter()
        .max_by_key(|i| (i.impact, i.created_at))
        .map(|i| i.id);
    let incident = match lead {
        Some(id) => Some(crate::sentence::OpenIncident {
            id,
            message: db::last_update_message(&state.pool, id)
                .await
                .ok()
                .flatten(),
        }),
        None => None,
    };
    let upcoming: Vec<crate::sentence::Upcoming> = windows
        .iter()
        .filter(|m| m.starts_at > now)
        .map(|m| crate::sentence::Upcoming {
            component: component_name(&cfg, &m.component),
            starts: m.starts_at,
        })
        .collect();
    let say = crate::sentence::status_say(&crate::sentence::StatusFacts {
        components: &snapshots,
        incident,
        last_resolved: db::last_resolved_at(&state.pool).await.ok().flatten(),
        upcoming: &upcoming,
        now,
        tz,
    });

    let past_days = past_days(&cfg, &history, now);

    render(&StatusTemplate {
        site: SiteView::new(&cfg, logged_in, "status"),
        side: SideView {
            say,
            facts: Some(facts(&state.pool, &cfg).await),
        },
        groups,
        past_days,
        info_icon: ICON_INFO,
    })
}

/// The strip's days with their incidents and maintenance, fetched once per
/// page.
struct History {
    days: Vec<Day>,
    incidents: Vec<crate::models::Incident>,
    maintenance: Vec<crate::models::Maintenance>,
}

/// "What happened lately": one row per day, newest first.
fn past_days(cfg: &Config, h: &History, now: i64) -> Vec<DayView> {
    let mut out = Vec::with_capacity(PAST_DAYS);
    for day in h.days.iter().rev().take(PAST_DAYS) {
        let mut items: Vec<(i64, PastItem)> = h
            .incidents
            .iter()
            .filter(|i| day.contains(i.created_at))
            .map(|i| {
                let note = match i.resolved_at {
                    Some(r) => format!("Resolved · {}", human_duration(r - i.created_at)),
                    None => i.state.label().to_string(),
                };
                (
                    i.created_at,
                    PastItem {
                        href: Some(format!("/incidents/{}", i.id)),
                        title: i.title.clone(),
                        swatch: i.impact.as_str().to_string(),
                        note,
                    },
                )
            })
            .collect();
        items.extend(
            h.maintenance
                .iter()
                .filter(|m| day.contains(m.starts_at) && m.starts_at <= now)
                .map(|m| {
                    let title = if m.note.trim().is_empty() {
                        format!("Maintenance on {}", component_name(cfg, &m.component))
                    } else {
                        m.note.clone()
                    };
                    let note = if m.ends_at <= now {
                        format!("Completed · {}", human_duration(m.ends_at - m.starts_at))
                    } else {
                        "In progress".to_string()
                    };
                    (
                        m.starts_at,
                        PastItem {
                            href: None,
                            title,
                            swatch: CompState::Maintenance.as_str().to_string(),
                            note,
                        },
                    )
                }),
        );
        items.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
        out.push(DayView {
            heading: day.date.format("%b %-d").to_string(),
            iso: day.iso(),
            items: items.into_iter().map(|(_, i)| i).collect(),
        });
    }
    out
}

fn kind_word(kind: CheckType) -> &'static str {
    match kind {
        CheckType::Http => "http",
        CheckType::Tcp => "tcp",
        CheckType::Systemd => "systemd",
        CheckType::Docker => "container",
        CheckType::Disk => "disk",
        CheckType::Ram => "memory",
        CheckType::Swap => "swap",
        CheckType::Load => "load",
        CheckType::Heartbeat => "heartbeat",
    }
}

/// A component's state and 90-day uptime, as both the status page and the
/// badges show them, so the two can never disagree.
struct ComponentNow<'a> {
    check: Option<&'a crate::config::CheckConfig>,
    state: CompState,
    /// `uptime_slots` since `from`, for the day bars.
    daily: Vec<(i64, i64, i64)>,
    uptime_90: Option<f64>,
    last: Option<crate::models::CheckResult>,
    since: Option<i64>,
    window: Option<&'a crate::models::Maintenance>,
}

async fn component_now<'a>(
    pool: &Pool,
    cfg: &'a Config,
    comp: &crate::config::ComponentConfig,
    maintenance: &'a [crate::models::Maintenance],
    active: &[crate::models::Incident],
    from: i64,
    now: i64,
) -> ComponentNow<'a> {
    let check = comp.check.as_ref().and_then(|id| cfg.check(id));

    let (base, daily, last) = match check {
        Some(check) => {
            let last = db::last_check_result(pool, &check.id).await.ok().flatten();
            let failures = db::consecutive_failures(pool, &check.id, check.failures_to_open as i64)
                .await
                .unwrap_or(0);
            let base = status::check_state(last.as_ref(), failures, check.failures_to_open);
            let slots = db::uptime_slots(pool, &check.id, from)
                .await
                .unwrap_or_default();
            (base, slots, last)
        }
        None => (CompState::Operational, Vec::new(), None),
    };
    let total: i64 = daily.iter().map(|(_, t, _)| t).sum();
    let up: i64 = daily.iter().map(|(_, _, u)| u).sum();
    let uptime_90 = (total > 0).then(|| up as f64 / total as f64 * 100.0);

    let window = maintenance.iter().find(|m| m.covers(&comp.id, now));
    let mine: Vec<&crate::models::Incident> = active
        .iter()
        .filter(|i| affects(&i.component, &comp.id))
        .collect();
    let impact = mine.iter().map(|i| i.impact).max();
    let state = status::component_state(base, window.is_some(), impact);
    let since = mine.iter().map(|i| i.created_at).min();
    ComponentNow {
        check,
        state,
        daily,
        uptime_90,
        last,
        since,
        window,
    }
}

async fn build_component(
    pool: &Pool,
    cfg: &Config,
    comp: &crate::config::ComponentConfig,
    maintenance: &[crate::models::Maintenance],
    active: &[crate::models::Incident],
    history: &History,
    now: i64,
) -> (ComponentView, crate::sentence::Snapshot) {
    let from = history.days.first().map_or(now, |d| d.start);
    let ComponentNow {
        check,
        state,
        daily,
        uptime_90,
        last,
        since,
        window,
    } = component_now(pool, cfg, comp, maintenance, active, from, now).await;
    let uptime_90 = uptime_90.map(format_pct);
    let latency = last.as_ref().and_then(|r| r.latency_ms);

    let (sub, sub_class) = match state {
        CompState::Operational => (
            match (check, latency) {
                (Some(c), Some(ms)) if matches!(c.kind, CheckType::Http | CheckType::Tcp) => {
                    crate::sentence::fmt_latency(ms)
                }
                (Some(c), _) => kind_word(c.kind).to_string(),
                (None, _) => "manual".to_string(),
            },
            "",
        ),
        CompState::MajorOutage => (
            match since {
                Some(t) => format!("Down · {}", human_duration(now - t)),
                None => "Down".to_string(),
            },
            "down",
        ),
        CompState::PartialOutage => ("Having trouble".to_string(), "bad"),
        CompState::Degraded => (
            match latency {
                _ if check.is_some_and(|c| {
                    matches!(
                        c.kind,
                        CheckType::Disk | CheckType::Ram | CheckType::Swap | CheckType::Load
                    )
                }) =>
                {
                    "Running high".to_string()
                }
                Some(ms) => format!("Slow · {}", crate::sentence::fmt_latency(ms)),
                None => "Slow".to_string(),
            },
            "warn",
        ),
        CompState::Maintenance => ("Maintenance".to_string(), "mnt"),
    };

    let incidents: Vec<&crate::models::Incident> = history
        .incidents
        .iter()
        .filter(|i| affects(&i.component, &comp.id))
        .collect();
    let windows: Vec<&crate::models::Maintenance> = history
        .maintenance
        .iter()
        .filter(|m| m.component == comp.id || m.component.is_empty())
        .collect();

    let view = ComponentView {
        name: comp.name.clone(),
        description: comp.description.clone(),
        state_class: state.as_str().to_string(),
        state_label: state.label().to_string(),
        sub,
        sub_class,
        uptime_90,
        bars: build_bars(&daily, &history.days, cfg.tz(), &incidents, &windows, now),
    };
    let snap = crate::sentence::Snapshot {
        name: comp.name.clone(),
        state,
        since,
        latency_ms: latency,
        maintenance_until: window.map(|m| m.ends_at),
        resource: check.is_some_and(|c| {
            matches!(
                c.kind,
                CheckType::Disk | CheckType::Ram | CheckType::Swap | CheckType::Load
            )
        }),
    };
    (view, snap)
}

/// Whether an incident filed against `target` counts for component `id`. An
/// empty target is "All components", like an empty maintenance component.
fn affects(target: &str, id: &str) -> bool {
    target.is_empty() || target == id
}

/// Two decimals, but never round a day with downtime up to a clean 100%.
fn format_pct(p: f64) -> String {
    let s = format!("{p:.2}");
    if s == "100.00" && p < 100.0 {
        "99.99".to_string()
    } else {
        s
    }
}

/// The worst state a day's uptime implies.
fn day_state(uptime: f64) -> CompState {
    if uptime >= 99.99 {
        CompState::Operational
    } else if uptime >= 95.0 {
        CompState::Degraded
    } else if uptime >= 50.0 {
        CompState::PartialOutage
    } else {
        CompState::MajorOutage
    }
}

/// One tick per day. An incident counts for every day it was open, so a
/// manual incident shows even when the probes kept passing; maintenance only
/// shows on a day that had nothing worse.
fn build_bars(
    slots: &[(i64, i64, i64)],
    strip: &[Day],
    tz: chrono_tz::Tz,
    incidents: &[&crate::models::Incident],
    windows: &[&crate::models::Maintenance],
    now: i64,
) -> Vec<BarView> {
    status::uptime_bars(slots, strip, tz)
        .into_iter()
        .map(|b| {
            let date = b.day.date.format("%b %-d, %Y").to_string();
            let (start, end) = (b.day.start, b.day.end);
            let impact = incidents
                .iter()
                .filter(|i| i.created_at < end && i.resolved_at.unwrap_or(now) >= start)
                .map(|i| i.impact)
                .filter(|s| *s != CompState::Maintenance)
                .max();
            let maint = windows
                .iter()
                .any(|m| b.day.overlaps(m.starts_at, m.ends_at));
            let from_uptime = b.uptime.map(day_state);
            let state = match (from_uptime, impact) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            let state = match state {
                Some(CompState::Operational) | None if maint => Some(CompState::Maintenance),
                s => s,
            };
            let uptime = b
                .uptime
                .map(|u| format!("{}%", format_pct(u)))
                .unwrap_or_default();
            match state {
                None => BarView {
                    class: "empty".to_string(),
                    aria: format!("{date}: no data"),
                    date,
                    uptime,
                    label: "No data".to_string(),
                },
                Some(state) => {
                    let label = match state {
                        CompState::Operational => "Nothing went wrong".to_string(),
                        CompState::Maintenance => "Maintenance".to_string(),
                        s => s.label().to_string(),
                    };
                    let aria = if uptime.is_empty() {
                        format!("{date}: {}", label.to_lowercase())
                    } else {
                        format!("{date}: {uptime} uptime, {}", label.to_lowercase())
                    };
                    BarView {
                        class: state.as_str().to_string(),
                        aria,
                        date,
                        uptime,
                        label,
                    }
                }
            }
        })
        .collect()
}

#[derive(Deserialize)]
struct RangeQuery {
    range: Option<String>,
}

fn valid_range(r: Option<&str>) -> &'static str {
    match r {
        Some("7d") => "7d",
        Some("90d") => "90d",
        _ => "24h",
    }
}

async fn metrics_page(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(q): Query<RangeQuery>,
) -> Response {
    if let Some(r) = read_guard(&state, &jar).await {
        return r;
    }
    let cfg = state.cfg();
    let logged_in = is_logged_in(&state, &jar).await;
    let sections = build_sections(&state.pool, &cfg).await;
    let cards: Vec<&CardView> = sections.iter().flat_map(|s| &s.cards).collect();
    // `</` would end the inline script early; JSON allows the escaped form.
    let cards_json = serde_json::to_string(&cards)
        .unwrap_or_else(|_| "[]".to_string())
        .replace("</", "<\\/");
    let say = crate::sentence::host_say(&host_metrics(&state.pool, &cfg).await);
    let has_lines = cards.iter().any(|c| c.threshold.is_some());
    render(&MetricsTemplate {
        site: SiteView::new(&cfg, logged_in, "metrics"),
        side: SideView {
            say,
            facts: Some(facts(&state.pool, &cfg).await),
        },
        range: valid_range(q.range.as_deref()).to_string(),
        has_lines,
        sections,
        cards_json,
    })
}

/// Percent at which a host number counts as trouble when no check says so.
const DEFAULT_HOST_WARN: f64 = 90.0;

/// The `warn` of the first resource check of `kind` (and `mount` for disks).
fn resource_warn(cfg: &Config, kind: CheckType, mount: Option<&str>) -> Option<f64> {
    cfg.checks
        .iter()
        .filter(|c| c.kind == kind)
        .filter(|c| mount.is_none() || c.mount.as_deref() == mount)
        .find_map(|c| c.warn)
}

/// Latest CPU, memory and disk numbers for the host sentence.
async fn host_metrics(pool: &Pool, cfg: &Config) -> Vec<crate::sentence::HostMetric> {
    use crate::sentence::{HostKind, HostMetric};
    let mut out = Vec::new();
    let latest = |metric: &'static str, key: String| async move {
        db::latest_sample(pool, "host", metric, &key)
            .await
            .ok()
            .flatten()
    };
    if let Some(v) = latest("cpu_pct", String::new()).await {
        out.push(HostMetric {
            kind: HostKind::Cpu,
            value: v,
            warn: DEFAULT_HOST_WARN,
        });
    }
    if let Some(v) = latest("mem_pct", String::new()).await {
        out.push(HostMetric {
            kind: HostKind::Memory,
            value: v,
            warn: resource_warn(cfg, CheckType::Ram, None).unwrap_or(DEFAULT_HOST_WARN),
        });
    }
    let mounts: BTreeSet<String> = configured_mounts(cfg).into_iter().collect();
    for m in mounts {
        if let Some(v) = latest("disk_used_pct", m.clone()).await {
            out.push(HostMetric {
                warn: resource_warn(cfg, CheckType::Disk, Some(&m)).unwrap_or(DEFAULT_HOST_WARN),
                kind: HostKind::Disk(m),
                value: v,
            });
        }
    }
    out
}

fn one(label: &str, scope: &str, metric: &str, key: &str) -> CardSeries {
    CardSeries {
        label: label.to_string(),
        scope: scope.to_string(),
        metric: metric.to_string(),
        key: key.to_string(),
        right: false,
    }
}

/// Title, series and the trouble line of one chart.
type Card = (String, Vec<CardSeries>, Option<f64>);

/// Chart cards per section: host metrics, one card per container seen in the
/// data, and latency for every HTTP/TCP check in the config.
async fn build_sections(pool: &Pool, cfg: &Config) -> Vec<SectionView> {
    let known = db::distinct_series(pool).await.unwrap_or_default();

    let latency_line =
        |c: &crate::config::CheckConfig| c.latency_degraded.map(|d| d.as_secs_f64() * 1000.0);
    let mut host = vec![
        ("CPU", vec![one("CPU", "host", "cpu_pct", "")]),
        ("Memory", vec![one("Memory", "host", "mem_pct", "")]),
        ("Swap", vec![one("Swap", "host", "swap_pct", "")]),
        (
            "Load average",
            vec![
                one("1 min", "host", "load1", ""),
                one("5 min", "host", "load5", ""),
                one("15 min", "host", "load15", ""),
            ],
        ),
    ];
    let mut mounts: BTreeSet<String> = configured_mounts(cfg).into_iter().collect();
    mounts.extend(
        known
            .iter()
            .filter(|(s, m, _)| s == "host" && m == "disk_used_pct")
            .map(|(_, _, k)| k.clone()),
    );
    let disk_titles: Vec<(String, String)> = mounts
        .into_iter()
        .map(|m| (format!("Disk {m}"), m))
        .collect();
    let line_for = |title: &str| match title {
        "Memory" => resource_warn(cfg, CheckType::Ram, None),
        "Swap" => resource_warn(cfg, CheckType::Swap, None),
        "Load average" => resource_warn(cfg, CheckType::Load, None),
        _ => None,
    };
    let mut host_cards: Vec<Card> = host
        .drain(..)
        .map(|(t, s)| (t.to_string(), s, line_for(t)))
        .collect();
    for (title, mount) in disk_titles {
        let line = resource_warn(cfg, CheckType::Disk, Some(&mount));
        host_cards.push((
            title,
            vec![one("Used", "host", "disk_used_pct", &mount)],
            line,
        ));
    }
    host_cards.push((
        "Network".to_string(),
        vec![
            one("In", "host", "net_rx_bps", ""),
            one("Out", "host", "net_tx_bps", ""),
        ],
        None,
    ));

    let containers: BTreeSet<String> = known
        .iter()
        .filter(|(s, m, _)| s == "container" && m == "cpu_pct")
        .map(|(_, _, k)| k.clone())
        .collect();
    let container_cards: Vec<Card> = containers
        .into_iter()
        .map(|name| {
            let mut mem = one("Memory", "container", "mem_bytes", &name);
            mem.right = true;
            (
                name.clone(),
                vec![one("CPU", "container", "cpu_pct", &name), mem],
                None,
            )
        })
        .collect();

    let check_cards: Vec<Card> = cfg
        .checks
        .iter()
        .filter(|c| matches!(c.kind, CheckType::Http | CheckType::Tcp))
        .map(|c| {
            (
                c.name.clone(),
                vec![one("Latency", "check", "latency_ms", &c.id)],
                latency_line(c),
            )
        })
        .collect();

    let mut n = 0;
    let mut section = |title: &str, empty: &str, cards: Vec<Card>| SectionView {
        title: title.to_string(),
        empty_note: empty.to_string(),
        cards: cards
            .into_iter()
            .map(|(title, series, threshold)| {
                n += 1;
                CardView {
                    id: format!("chart-{n}"),
                    title,
                    series,
                    threshold,
                }
            })
            .collect(),
    };
    vec![
        section("The server", "", host_cards),
        section(
            "Containers",
            "No container metrics yet. Enable [docker] in the config to collect them.",
            container_cards,
        ),
        section(
            "Checks",
            "No HTTP or TCP checks are configured.",
            check_cards,
        ),
    ]
}

#[derive(Deserialize)]
struct MetricsQuery {
    scope: Option<String>,
    metric: Option<String>,
    key: Option<String>,
    range: Option<String>,
}

async fn api_metrics(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(q): Query<MetricsQuery>,
) -> Response {
    if let Some(r) = read_guard(&state, &jar).await {
        return r;
    }
    let now = crate::now_ts();
    let scope = q.scope.unwrap_or_else(|| "host".to_string());
    let metric = q.metric.unwrap_or_else(|| "cpu_pct".to_string());
    let key = q.key.unwrap_or_default();
    let range = q.range.unwrap_or_else(|| "24h".to_string());

    // `max` comes from the hourly max column on long ranges, so a short spike
    // is not averaged away in the card summary.
    let (t, v, max) = if range == "24h" {
        match db::series(&state.pool, &scope, &metric, &key, now - 86_400, now).await {
            Ok(rows) => {
                let max = rows.iter().map(|r| r.1).reduce(f64::max);
                let (t, v): (Vec<i64>, Vec<f64>) = rows.into_iter().unzip();
                (t, v, max)
            }
            Err(e) => return internal(&state, e),
        }
    } else {
        let days = if range == "7d" { 7 } else { 90 };
        match db::hourly_series(&state.pool, &scope, &metric, &key, now - days * 86_400, now).await
        {
            Ok(rows) => {
                let t: Vec<i64> = rows.iter().map(|r| r.0).collect();
                let v: Vec<f64> = rows.iter().map(|r| r.2).collect();
                let max = rows.iter().map(|r| r.3).reduce(f64::max);
                (t, v, max)
            }
            Err(e) => return internal(&state, e),
        }
    };
    let avg = (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    let last = db::latest_sample(&state.pool, &scope, &metric, &key)
        .await
        .ok()
        .flatten();
    let unit = match metric.as_str() {
        "cpu_pct" | "mem_pct" | "swap_pct" | "disk_used_pct" => "%",
        "net_rx_bps" | "net_tx_bps" => "bytes/s",
        "mem_bytes" => "bytes",
        "latency_ms" => "ms",
        _ => "",
    };
    let payload = serde_json::json!({
        "scope": scope,
        "metric": metric,
        "key": key,
        "unit": unit,
        "points": t.len(),
        "t": t,
        "v": v,
        "last": last,
        "avg": avg,
        "max": max,
    });
    ([(CONTENT_TYPE, "application/json")], payload.to_string()).into_response()
}

async fn incidents_page(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Some(r) = read_guard(&state, &jar).await {
        return r;
    }
    let cfg = state.cfg();
    let logged_in = is_logged_in(&state, &jar).await;
    let now = crate::now_ts();
    let incidents = db::recent_incidents(&state.pool, 100)
        .await
        .unwrap_or_default();
    let tz = cfg.tz();
    let mut months: Vec<MonthView> = Vec::new();
    for inc in &incidents {
        let name = crate::days::date_of(inc.created_at, tz)
            .format("%B %Y")
            .to_string();
        let view = IncidentView::new(inc, component_name(&cfg, &inc.component), None, now, tz);
        match months.last_mut() {
            Some(m) if m.name == name => m.incidents.push(view),
            _ => months.push(MonthView {
                name,
                incidents: vec![view],
            }),
        }
    }
    render(&IncidentsTemplate {
        site: SiteView::new(&cfg, logged_in, "incidents"),
        side: SideView {
            say: incidents_say(&incidents, now),
            facts: Some(facts(&state.pool, &cfg).await),
        },
        months,
    })
}

/// Left column of the incident list: how much went wrong in 90 days.
fn incidents_say(incidents: &[crate::models::Incident], now: i64) -> Say {
    let recent = incidents
        .iter()
        .filter(|i| i.created_at >= now - 90 * 86_400)
        .count();
    let open = incidents.iter().filter(|i| i.resolved_at.is_none()).count();
    let headline = match (recent, incidents.is_empty()) {
        (_, true) => "Nothing has gone wrong yet.".to_string(),
        (0, false) => "Nothing has gone wrong in 90 days.".to_string(),
        (1, _) => "One thing went wrong in the last 90 days.".to_string(),
        (n, _) => {
            let word = crate::sentence::number_word(n);
            let mut chars = word.chars();
            let cap = chars
                .next()
                .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default();
            format!("{cap} things went wrong in the last 90 days.")
        }
    };
    let detail = match open {
        0 if incidents.is_empty() => String::new(),
        0 => "All of them are resolved.".to_string(),
        1 => "One is still open.".to_string(),
        n => format!("{} are still open.", crate::sentence::number_word(n)),
    };
    SideView::plain(&headline, &detail).say
}

async fn incident_page(
    State(state): State<AppState>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Response {
    if let Some(r) = read_guard(&state, &jar).await {
        return r;
    }
    let cfg = state.cfg();
    let logged_in = is_logged_in(&state, &jar).await;
    let inc = match db::incident(&state.pool, id).await {
        Ok(Some(i)) => i,
        Ok(None) => {
            return error_page(
                &cfg,
                StatusCode::NOT_FOUND,
                "Incident not found",
                SideView::plain(
                    &format!("There is no incident {id}."),
                    "It may have been removed, or the link is wrong.",
                ),
            )
        }
        Err(e) => return internal(&state, e),
    };
    let updates = db::incident_updates(&state.pool, id)
        .await
        .unwrap_or_default();
    // Newest first, as status pages show it.
    let updates: Vec<UpdateView> = updates.iter().rev().map(UpdateView::from).collect();
    let now = crate::now_ts();
    let detail = match inc.resolved_at {
        Some(r) => format!(
            "Resolved after {}.",
            crate::sentence::humanize(r - inc.created_at)
        ),
        None => format!(
            "Still open, {} so far.",
            crate::sentence::humanize(now - inc.created_at)
        ),
    };
    render(&IncidentTemplate {
        site: SiteView::new(&cfg, logged_in, "incidents"),
        side: SideView::plain(&inc.title, &detail),
        incident: IncidentView::new(
            &inc,
            component_name(&cfg, &inc.component),
            None,
            now,
            cfg.tz(),
        ),
        updates,
    })
}

async fn manage_page(State(state): State<AppState>, jar: CookieJar) -> Response {
    // Operator-only page: always behind the session, whatever protect_read says.
    if !is_logged_in(&state, &jar).await {
        return Redirect::to("/login").into_response();
    }
    let cfg = state.cfg();
    let now = crate::now_ts();
    let windows = db::current_and_upcoming_maintenance(&state.pool, now)
        .await
        .unwrap_or_default();
    let active = db::active_incidents(&state.pool).await.unwrap_or_default();
    render(&ManageTemplate {
        site: SiteView::new(&cfg, true, "manage"),
        side: SideView::plain(
            "Change what people see.",
            "Open and update incidents, or start maintenance to mute alerts. Checks live in the config file.",
        ),
        components: cfg
            .components
            .iter()
            .map(|c| NavComponent {
                id: c.id.clone(),
                name: c.name.clone(),
            })
            .collect(),
        maintenance: maintenance_views(&cfg, &windows, now),
        active_incidents: incident_views(&state.pool, &cfg, &active, now).await,
    })
}

async fn feed(State(state): State<AppState>, headers: HeaderMap, jar: CookieJar) -> Response {
    // The feed carries incident titles and messages, so it is a read page too.
    if let Some(r) = read_guard(&state, &jar).await {
        return r;
    }
    let cfg = state.cfg();
    // Feed readers keep these ids, so they should not depend on which Host
    // header a request came in with when the public address is known.
    let base = match &cfg.public_url {
        Some(u) => u.trim_end_matches('/').to_string(),
        None => {
            let host = headers
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("localhost");
            format!("http://{host}")
        }
    };
    let incidents = db::recent_incidents(&state.pool, 20)
        .await
        .unwrap_or_default();
    let mut entries = Vec::new();
    for inc in &incidents {
        let updates = db::incident_updates(&state.pool, inc.id)
            .await
            .unwrap_or_default();
        let body = updates
            .last()
            .map(|u| u.message.clone())
            .unwrap_or_default();
        let updated = inc.resolved_at.unwrap_or(inc.created_at);
        entries.push(FeedEntry {
            title: inc.title.clone(),
            id: format!("{base}/incidents/{}", inc.id),
            url: format!("{base}/incidents/{}", inc.id),
            updated: iso8601(updated),
            published: iso8601(inc.created_at),
            content: format!(
                "<p><strong>{}</strong> {}</p><p>{}</p>",
                inc.state.label(),
                inc.component,
                body
            ),
        });
    }
    let updated = entries
        .first()
        .map(|e| e.updated.clone())
        .unwrap_or_else(|| iso8601(crate::now_ts()));
    let template = FeedTemplate {
        feed_title: format!("{} incidents", cfg.theme.title.trim()),
        feed_id: format!("{base}/feed.xml"),
        feed_url: format!("{base}/feed.xml"),
        updated,
        entries,
    };
    match template.render() {
        Ok(body) => (
            [(CONTENT_TYPE, "application/atom+xml; charset=utf-8")],
            body,
        )
            .into_response(),
        Err(e) => internal(&state, e),
    }
}

async fn health(State(state): State<AppState>) -> Response {
    let cfg = state.cfg();
    let payload = serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "time": iso8601(crate::now_ts()),
        "checks": cfg.checks.len(),
        "components": cfg.components.len(),
    });
    ([(CONTENT_TYPE, "application/json")], payload.to_string()).into_response()
}

const NOSNIFF: (HeaderName, HeaderValue) = (
    HeaderName::from_static("x-content-type-options"),
    HeaderValue::from_static("nosniff"),
);

/// Access check for badges. Badges are embedded where no session cookie is
/// sent (READMEs, other dashboards), so this is where API keys will be
/// accepted under `protect_read`; until then it is the read-page guard.
async fn badge_guard(state: &AppState, jar: &CookieJar) -> Option<Response> {
    read_guard(state, jar).await
}

enum BadgeFormat {
    Svg,
    Json,
}

fn badge_response(
    status: StatusCode,
    format: BadgeFormat,
    badge: &crate::badge::Badge,
) -> Response {
    let (content_type, body) = match format {
        BadgeFormat::Svg => ("image/svg+xml", badge.svg()),
        BadgeFormat::Json => ("application/json", badge.endpoint_json()),
    };
    (
        status,
        [
            (CONTENT_TYPE, HeaderValue::from_static(content_type)),
            // A badge is only useful if it is current, and camo (GitHub's
            // image proxy) caches for as long as it is allowed to.
            (CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            NOSNIFF,
        ],
        body,
    )
        .into_response()
}

fn badge_not_found(format: BadgeFormat) -> Response {
    let badge = crate::badge::Badge {
        label: "badge".to_string(),
        message: "not found".to_string(),
        color: crate::badge::NO_DATA,
    };
    badge_response(StatusCode::NOT_FOUND, format, &badge)
}

/// The component's current state and 90-day uptime, as the status page
/// computes them.
async fn badge_facts(
    state: &AppState,
    cfg: &Config,
    id: &str,
) -> Option<(String, CompState, Option<f64>)> {
    let comp = cfg.component(id)?;
    let now = crate::now_ts();
    let windows = db::current_and_upcoming_maintenance(&state.pool, now)
        .await
        .unwrap_or_default();
    let active = db::active_incidents(&state.pool).await.unwrap_or_default();
    let strip = crate::days::last_days(now, crate::days::STRIP_DAYS, cfg.tz());
    let from = strip.first().map_or(now, |d| d.start);
    let c = component_now(&state.pool, cfg, comp, &windows, &active, from, now).await;
    Some((comp.name.clone(), c.state, c.uptime_90))
}

/// `/badge/{component}.svg` and `/badge/{component}.json`: current state.
async fn badge(
    State(state): State<AppState>,
    Path(file): Path<String>,
    jar: CookieJar,
) -> Response {
    if let Some(r) = badge_guard(&state, &jar).await {
        return r;
    }
    let (id, format) = match file.rsplit_once('.') {
        Some((id, "svg")) => (id, BadgeFormat::Svg),
        Some((id, "json")) => (id, BadgeFormat::Json),
        _ => return badge_not_found(BadgeFormat::Svg),
    };
    let cfg = state.cfg();
    let Some((name, comp_state, _)) = badge_facts(&state, &cfg, id).await else {
        return badge_not_found(format);
    };
    let badge = crate::badge::Badge {
        label: name,
        message: crate::badge::state_message(comp_state),
        color: comp_state.rgb(),
    };
    badge_response(StatusCode::OK, format, &badge)
}

/// `/badge/{component}/uptime.svg`: the status page's 90-day uptime figure.
async fn uptime_badge(
    State(state): State<AppState>,
    Path(id): Path<String>,
    jar: CookieJar,
) -> Response {
    if let Some(r) = badge_guard(&state, &jar).await {
        return r;
    }
    let cfg = state.cfg();
    let Some((name, _, uptime)) = badge_facts(&state, &cfg, &id).await else {
        return badge_not_found(BadgeFormat::Svg);
    };
    let shown = uptime.map(format_pct);
    // Colour by the figure as shown, so "99.90%" never gets the band below.
    let color = crate::badge::uptime_color(shown.as_deref().and_then(|s| s.parse().ok()));
    let badge = crate::badge::Badge {
        label: format!("{name} uptime"),
        message: shown.map_or_else(|| "no data".to_string(), |s| format!("{s}%")),
        color,
    };
    badge_response(StatusCode::OK, BadgeFormat::Svg, &badge)
}

async fn asset(State(state): State<AppState>, Path(file): Path<String>) -> Response {
    // Files that follow the config are revalidated so a hot reload shows up.
    let fresh = HeaderValue::from_static("no-cache");
    match file.as_str() {
        "favicon.svg" => {
            let svg = crate::theme::favicon_svg();
            return (
                [
                    (CONTENT_TYPE, HeaderValue::from_static("image/svg+xml")),
                    (CACHE_CONTROL, fresh),
                    NOSNIFF,
                ],
                svg,
            )
                .into_response();
        }
        "logo" => {
            let cfg = state.cfg();
            let Some(logo) = cfg.theme_files.logo.clone() else {
                return (StatusCode::NOT_FOUND, "not found").into_response();
            };
            // An SVG opened directly would run any script inside it on our
            // origin; the sandbox policy stops that without affecting <img>.
            return (
                [
                    (CONTENT_TYPE, HeaderValue::from_static(logo.content_type)),
                    (CACHE_CONTROL, fresh),
                    (
                        CONTENT_SECURITY_POLICY,
                        HeaderValue::from_static(
                            "default-src 'none'; style-src 'unsafe-inline'; sandbox",
                        ),
                    ),
                    NOSNIFF,
                ],
                logo.bytes.to_vec(),
            )
                .into_response();
        }
        "custom.css" => {
            let cfg = state.cfg();
            let Some(css) = cfg.theme_files.custom_css.clone() else {
                return (StatusCode::NOT_FOUND, "not found").into_response();
            };
            return (
                [
                    (
                        CONTENT_TYPE,
                        HeaderValue::from_static("text/css; charset=utf-8"),
                    ),
                    (CACHE_CONTROL, fresh),
                    NOSNIFF,
                ],
                css.to_string(),
            )
                .into_response();
        }
        _ => {}
    }
    let (content_type, body): (&str, &[u8]) = match file.as_str() {
        "style.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../assets/style.css"),
        ),
        "dunlin.js" => (
            "application/javascript",
            include_bytes!("../assets/dunlin.js"),
        ),
        "metrics.js" => (
            "application/javascript",
            include_bytes!("../assets/metrics.js"),
        ),
        "htmx.min.js" => (
            "application/javascript",
            include_bytes!("../assets/htmx.min.js"),
        ),
        "uplot.js" => (
            "application/javascript",
            include_bytes!("../assets/uplot.iife.min.js"),
        ),
        "uplot.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../assets/uplot.min.css"),
        ),
        _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    (
        [
            (CONTENT_TYPE, HeaderValue::from_static(content_type)),
            NOSNIFF,
        ],
        body,
    )
        .into_response()
}

/// Self-hosted fonts, so the page makes no third-party requests. Only the
/// names below are served; anything else is a 404.
async fn font(Path(file): Path<String>) -> Response {
    macro_rules! fonts {
        ($($name:literal),* $(,)?) => {
            match file.as_str() {
                $($name => include_bytes!(concat!("../assets/fonts/", $name)).as_slice(),)*
                _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
            }
        };
    }
    let body: &[u8] = fonts!(
        "bricolage-grotesque-latin-400-normal.woff2",
        "bricolage-grotesque-latin-600-normal.woff2",
        "bricolage-grotesque-latin-800-normal.woff2",
        "bricolage-grotesque-latin-ext-400-normal.woff2",
        "bricolage-grotesque-latin-ext-600-normal.woff2",
        "bricolage-grotesque-latin-ext-800-normal.woff2",
        "martian-mono-latin-400-normal.woff2",
        "martian-mono-latin-ext-400-normal.woff2",
        "bricolage-grotesque-OFL.txt",
        "martian-mono-OFL.txt",
    );
    let content_type = if file.ends_with(".txt") {
        "text/plain; charset=utf-8"
    } else {
        "font/woff2"
    };
    (
        [
            (CONTENT_TYPE, HeaderValue::from_static(content_type)),
            // File names carry no version, but the files never change for a
            // given build; a day keeps upgrades visible without refetching
            // on every page.
            (
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            ),
            NOSNIFF,
        ],
        body,
    )
        .into_response()
}

// -- heartbeat -------------------------------------------------------------

async fn heartbeat(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let cfg = state.cfg();
    let now = crate::now_ts();
    for check in &cfg.checks {
        if check.kind != CheckType::Heartbeat {
            continue;
        }
        if let Some(expected) = &check.token {
            if auth::secret_eq(expected, &token) {
                if let Err(e) = db::record_ping(&state.pool, &check.id, now).await {
                    return internal(&state, e);
                }
                return (StatusCode::OK, "ok").into_response();
            }
        }
    }
    (StatusCode::NOT_FOUND, "unknown heartbeat token").into_response()
}

// -- auth ------------------------------------------------------------------

async fn login_page(State(state): State<AppState>, jar: CookieJar) -> Response {
    let logged_in = is_logged_in(&state, &jar).await;
    render(&LoginTemplate {
        site: SiteView::new(&state.cfg(), logged_in, "login"),
        side: login_side(),
        error: None,
    })
}

fn login_side() -> SideView {
    SideView::plain("Sign in to change things.", "Reading needs no password.")
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn login_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    let cfg = state.cfg();
    let ip = client_ip(&headers, Some(addr), cfg.trusted_proxy);
    let now = crate::now_ts();
    if state.limiter.is_blocked(&ip, now) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            render(&LoginTemplate {
                site: SiteView::new(&cfg, false, "login"),
                side: login_side(),
                error: Some("Too many login attempts. Try again in 15 minutes.".to_string()),
            }),
        )
            .into_response();
    }
    let ok = match &cfg.web.password_hash {
        Some(hash) => auth::verify_password(&form.password, hash).unwrap_or(false),
        None => false,
    };
    if !ok {
        state.limiter.record_failure(&ip, now);
        return (
            StatusCode::UNAUTHORIZED,
            render(&LoginTemplate {
                site: SiteView::new(&cfg, false, "login"),
                side: login_side(),
                error: Some("Incorrect password.".to_string()),
            }),
        )
            .into_response();
    }
    state.limiter.record_success(&ip);
    let token = auth::random_token();
    if let Err(e) = db::create_session(
        &state.pool,
        &auth::token_hash(&token),
        now,
        now + crate::web_auth::SESSION_TTL_SECS,
    )
    .await
    {
        return internal(&state, e);
    }
    let cookie = session_cookie(&token, cfg.secure_cookies);
    (jar.add(cookie), Redirect::to("/")).into_response()
}

async fn logout(State(state): State<AppState>, headers: HeaderMap, jar: CookieJar) -> Response {
    if !csrf_ok(&headers) {
        return (StatusCode::FORBIDDEN, "CSRF check failed").into_response();
    }
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        let _ = db::delete_session(&state.pool, &auth::token_hash(cookie.value())).await;
    }
    (jar.add(clear_session_cookie()), Redirect::to("/")).into_response()
}

// -- write actions ---------------------------------------------------------

#[derive(Deserialize)]
struct CreateIncidentForm {
    component: String,
    title: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    impact: Option<StateParam>,
    // `state` is accepted as an alias so the form matches the documented
    // create-incident fields (component, title, state, message).
    #[serde(default)]
    state: Option<StateParam>,
}

#[derive(Deserialize)]
#[serde(transparent)]
struct StateParam(String);

impl StateParam {
    fn state(&self) -> CompState {
        CompState::from_name(&self.0).unwrap_or(CompState::MajorOutage)
    }
}

async fn create_incident(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    Form(form): Form<CreateIncidentForm>,
) -> Response {
    if let Some(r) = write_guard(&state, &headers, &jar).await {
        return r;
    }
    let now = crate::now_ts();
    let impact = form
        .impact
        .as_ref()
        .or(form.state.as_ref())
        .map(StateParam::state)
        .unwrap_or(CompState::MajorOutage);
    let component = if form.component == "__all" {
        String::new()
    } else {
        form.component
    };
    let id = match db::create_incident(
        &state.pool,
        &component,
        &form.title,
        impact,
        crate::models::IncidentState::Investigating,
        false,
        now,
    )
    .await
    {
        Ok(id) => id,
        Err(e) => return internal(&state, e),
    };
    let message = if form.message.trim().is_empty() {
        "Incident created.".to_string()
    } else {
        form.message
    };
    if let Err(e) = db::add_update(
        &state.pool,
        id,
        now,
        crate::models::IncidentState::Investigating,
        &message,
        false,
    )
    .await
    {
        return internal(&state, e);
    }
    Redirect::to(&format!("/incidents/{id}")).into_response()
}

#[derive(Deserialize)]
struct UpdateForm {
    state: String,
    message: String,
}

async fn add_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    Path(id): Path<i64>,
    Form(form): Form<UpdateForm>,
) -> Response {
    if let Some(r) = write_guard(&state, &headers, &jar).await {
        return r;
    }
    let now = crate::now_ts();
    let new_state = crate::models::IncidentState::from_name(&form.state)
        .unwrap_or(crate::models::IncidentState::Investigating);
    if let Err(e) = db::add_update(&state.pool, id, now, new_state, &form.message, false).await {
        return internal(&state, e);
    }
    if new_state == crate::models::IncidentState::Resolved {
        if let Err(e) = db::resolve_incident(&state.pool, id, now).await {
            return internal(&state, e);
        }
    } else if let Err(e) = db::set_incident_state(&state.pool, id, new_state).await {
        return internal(&state, e);
    }
    Redirect::to(&format!("/incidents/{id}")).into_response()
}

async fn resolve_incident(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Response {
    if let Some(r) = write_guard(&state, &headers, &jar).await {
        return r;
    }
    let now = crate::now_ts();
    if let Err(e) = db::resolve_incident(&state.pool, id, now).await {
        return internal(&state, e);
    }
    if let Err(e) = db::add_update(
        &state.pool,
        id,
        now,
        crate::models::IncidentState::Resolved,
        "Resolved by operator.",
        false,
    )
    .await
    {
        return internal(&state, e);
    }
    Redirect::to(&format!("/incidents/{id}")).into_response()
}

/// Longest maintenance window, and furthest start, that the form accepts.
const MAX_MAINTENANCE_SECS: i64 = 366 * 86_400;

#[derive(Deserialize)]
struct MaintenanceForm {
    #[serde(default)]
    component: String,
    note: String,
    duration_minutes: i64,
    /// Optional explicit start offset in seconds; defaults to now.
    #[serde(default)]
    starts_in_seconds: Option<i64>,
}

async fn start_maintenance(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    Form(form): Form<MaintenanceForm>,
) -> Response {
    if let Some(r) = write_guard(&state, &headers, &jar).await {
        return r;
    }
    let now = crate::now_ts();
    // Clamped so a typo cannot overflow the timestamps; a window longer than
    // a year, or starting further out, is not a maintenance window.
    let duration = form.duration_minutes.clamp(1, MAX_MAINTENANCE_SECS / 60) * 60;
    let starts = now
        + form
            .starts_in_seconds
            .unwrap_or(0)
            .clamp(0, MAX_MAINTENANCE_SECS);
    if let Err(e) = db::create_maintenance(
        &state.pool,
        &form.component,
        &form.note,
        starts,
        starts + duration,
        now,
    )
    .await
    {
        return internal(&state, e);
    }
    Redirect::to("/manage").into_response()
}

async fn end_maintenance(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Response {
    if let Some(r) = write_guard(&state, &headers, &jar).await {
        return r;
    }
    let now = crate::now_ts();
    if let Err(e) = db::end_maintenance(&state.pool, id, now).await {
        return internal(&state, e);
    }
    Redirect::to("/manage").into_response()
}
