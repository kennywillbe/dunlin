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
        .route("/health", get(health))
        .route("/assets/{file}", get(asset))
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

fn error_page(cfg: &Config, status: StatusCode, heading: &str, message: &str) -> Response {
    let page = ErrorTemplate {
        site: SiteView::new(cfg, false, ""),
        code: status.as_u16(),
        heading: heading.to_string(),
        message: message.to_string(),
    };
    (status, render(&page)).into_response()
}

fn internal(state: &AppState, err: impl std::fmt::Display) -> Response {
    tracing::error!(error = %err, "request failed");
    error_page(
        &state.cfg(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong",
        "The server could not complete this request. The error has been logged.",
    )
}

async fn not_found(State(state): State<AppState>) -> Response {
    error_page(
        &state.cfg(),
        StatusCode::NOT_FOUND,
        "Page not found",
        "There is nothing at this address.",
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
        ));
    }
    out
}

// -- read pages ------------------------------------------------------------

/// How many days of history the status page lists, as on Statuspage.
const PAST_DAYS: i64 = 14;

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
    let active_windows: Vec<_> = windows
        .iter()
        .filter(|m| m.starts_at <= now)
        .cloned()
        .collect();
    let active = db::active_incidents(&state.pool).await.unwrap_or_default();

    // Group components; components without a group land in "Other".
    let mut group_names: Vec<(String, String)> = cfg
        .groups
        .iter()
        .map(|g| (g.id.clone(), g.name.clone()))
        .collect();
    group_names.push(("__other".to_string(), "Other".to_string()));

    let mut groups: Vec<GroupView> = Vec::new();
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
            components.push(
                build_component(&state.pool, &cfg, comp, &active_windows, &active, now).await,
            );
        }
        groups.push(GroupView {
            name: gname.clone(),
            components,
        });
    }

    let overall = status::overall(groups.iter().flat_map(|g| {
        g.components
            .iter()
            .filter_map(|c| CompState::from_name(&c.state_class))
    }));
    let overall_label = match overall {
        CompState::Operational => "All systems operational",
        CompState::Maintenance => "Maintenance in progress",
        CompState::Degraded => "Degraded performance",
        CompState::PartialOutage => "Partial outage",
        CompState::MajorOutage => "Major outage",
    };

    let active_views = incident_views(&state.pool, &cfg, &active, now).await;

    let today = now.div_euclid(86_400) * 86_400;
    let since = today - (PAST_DAYS - 1) * 86_400;
    let past = db::incidents_since(&state.pool, since)
        .await
        .unwrap_or_default();
    let past_views = incident_views(&state.pool, &cfg, &past, now).await;
    let past_days = (0..PAST_DAYS)
        .map(|offset| {
            let day = today - offset * 86_400;
            DayView {
                heading: fmt_day(day),
                incidents: past
                    .iter()
                    .zip(&past_views)
                    .filter(|(i, _)| i.created_at.div_euclid(86_400) * 86_400 == day)
                    .map(|(_, v)| v.clone())
                    .collect(),
            }
        })
        .collect();

    render(&StatusTemplate {
        site: SiteView::new(&cfg, logged_in, "status"),
        overall_class: overall.as_str().to_string(),
        overall_label: overall_label.to_string(),
        overall_icon: state_icon(overall),
        updated: TimeView::time(now),
        active_incidents: active_views,
        maintenance: maintenance_views(&cfg, &windows, now),
        groups,
        past_days,
        info_icon: ICON_INFO,
        wrench_icon: ICON_WRENCH,
    })
}

async fn build_component(
    pool: &Pool,
    cfg: &Config,
    comp: &crate::config::ComponentConfig,
    maintenance: &[crate::models::Maintenance],
    active: &[crate::models::Incident],
    now: i64,
) -> ComponentView {
    let check = comp.check.as_ref().and_then(|id| cfg.check(id));

    let (base, daily) = match check {
        Some(check) => {
            let last = db::last_check_result(pool, &check.id).await.ok().flatten();
            let failures = db::consecutive_failures(pool, &check.id, check.failures_to_open as i64)
                .await
                .unwrap_or(0);
            let base = status::check_state(last.as_ref(), failures, check.failures_to_open);
            let daily = db::daily_uptime(pool, &check.id, now - 90 * 86_400)
                .await
                .unwrap_or_default();
            (base, daily)
        }
        None => (CompState::Operational, Vec::new()),
    };
    let total: i64 = daily.iter().map(|(_, t, _)| t).sum();
    let up: i64 = daily.iter().map(|(_, _, u)| u).sum();
    let uptime_90 = (total > 0).then(|| format_pct(up as f64 / total as f64 * 100.0));

    let in_maintenance = maintenance.iter().any(|m| m.covers(&comp.id, now));
    let impact = active
        .iter()
        .filter(|i| i.component == comp.id)
        .map(|i| i.impact)
        .max();
    let state = status::component_state(base, in_maintenance, impact);

    ComponentView {
        name: comp.name.clone(),
        description: comp.description.clone(),
        state_class: state.as_str().to_string(),
        state_label: state.label().to_string(),
        uptime_90,
        bars: build_bars(&daily, now),
    }
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

/// The day's colour: the worst state its uptime implies.
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

fn build_bars(daily: &[(i64, i64, i64)], now: i64) -> Vec<BarView> {
    status::uptime_bars(daily, 90, now)
        .into_iter()
        .map(|b| {
            let date = fmt_day(b.day);
            match b.uptime {
                None => BarView {
                    class: "empty".to_string(),
                    aria: format!("{date}: no data"),
                    date,
                    uptime: String::new(),
                    label: "No data".to_string(),
                },
                Some(u) => {
                    let state = day_state(u);
                    let label = if state == CompState::Operational {
                        "No downtime".to_string()
                    } else {
                        state.label().to_string()
                    };
                    let uptime = format!("{}%", format_pct(u));
                    BarView {
                        class: state.as_str().to_string(),
                        aria: format!("{date}: {uptime} uptime, {}", label.to_lowercase()),
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
    render(&MetricsTemplate {
        site: SiteView::new(&cfg, logged_in, "metrics"),
        range: valid_range(q.range.as_deref()).to_string(),
        sections,
        cards_json,
    })
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

/// Chart cards per section: host metrics, one card per container seen in the
/// data, and latency for every HTTP/TCP check in the config.
async fn build_sections(pool: &Pool, cfg: &Config) -> Vec<SectionView> {
    let known = db::distinct_series(pool).await.unwrap_or_default();

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
    let mut host_cards: Vec<(String, Vec<CardSeries>)> =
        host.drain(..).map(|(t, s)| (t.to_string(), s)).collect();
    for (title, mount) in disk_titles {
        host_cards.push((title, vec![one("Used", "host", "disk_used_pct", &mount)]));
    }
    host_cards.push((
        "Network".to_string(),
        vec![
            one("In", "host", "net_rx_bps", ""),
            one("Out", "host", "net_tx_bps", ""),
        ],
    ));

    let containers: BTreeSet<String> = known
        .iter()
        .filter(|(s, m, _)| s == "container" && m == "cpu_pct")
        .map(|(_, _, k)| k.clone())
        .collect();
    let container_cards: Vec<(String, Vec<CardSeries>)> = containers
        .into_iter()
        .map(|name| {
            let mut mem = one("Memory", "container", "mem_bytes", &name);
            mem.right = true;
            (
                name.clone(),
                vec![one("CPU", "container", "cpu_pct", &name), mem],
            )
        })
        .collect();

    let check_cards: Vec<(String, Vec<CardSeries>)> = cfg
        .checks
        .iter()
        .filter(|c| matches!(c.kind, CheckType::Http | CheckType::Tcp))
        .map(|c| {
            (
                c.name.clone(),
                vec![one("Latency", "check", "latency_ms", &c.id)],
            )
        })
        .collect();

    let mut n = 0;
    let mut section =
        |title: &str, empty: &str, cards: Vec<(String, Vec<CardSeries>)>| SectionView {
            title: title.to_string(),
            empty_note: empty.to_string(),
            cards: cards
                .into_iter()
                .map(|(title, series)| {
                    n += 1;
                    CardView {
                        id: format!("chart-{n}"),
                        title,
                        series,
                    }
                })
                .collect(),
        };
    vec![
        section("Host", "", host_cards),
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
    let mut months: Vec<MonthView> = Vec::new();
    for inc in &incidents {
        let name = chrono::DateTime::from_timestamp(inc.created_at, 0)
            .map(|d| d.format("%B %Y").to_string())
            .unwrap_or_default();
        let view = IncidentView::new(inc, component_name(&cfg, &inc.component), None, now);
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
        months,
    })
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
                "This incident does not exist or has been removed.",
            )
        }
        Err(e) => return internal(&state, e),
    };
    let updates = db::incident_updates(&state.pool, id)
        .await
        .unwrap_or_default();
    // Newest first, as status pages show it.
    let updates: Vec<UpdateView> = updates.iter().rev().map(UpdateView::from).collect();
    render(&IncidentTemplate {
        site: SiteView::new(&cfg, logged_in, "incidents"),
        incident: IncidentView::new(
            &inc,
            component_name(&cfg, &inc.component),
            None,
            crate::now_ts(),
        ),
        impact_icon: state_icon(inc.impact),
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

async fn feed(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let cfg = state.cfg();
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    let base = format!("http://{host}");
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

async fn asset(State(state): State<AppState>, Path(file): Path<String>) -> Response {
    // Files that follow the config are revalidated so a hot reload shows up.
    let fresh = HeaderValue::from_static("no-cache");
    match file.as_str() {
        "favicon.svg" => {
            let cfg = state.cfg();
            let svg = crate::theme::favicon_svg(&cfg.theme.accent);
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
        error: None,
    })
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
    let duration = form.duration_minutes.max(1) * 60;
    let starts = now + form.starts_in_seconds.unwrap_or(0);
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
