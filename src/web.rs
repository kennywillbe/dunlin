//! HTTP surface: read pages, write actions, heartbeat endpoint and assets.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use askama::Template;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header::CONTENT_TYPE, HeaderMap, StatusCode};
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
        .with_state(state)
}

fn internal(err: impl std::fmt::Display) -> Response {
    tracing::error!(error = %err, "request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

fn render<T: Template>(template: &T) -> Response {
    match template.render() {
        Ok(body) => ([(CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response(),
        Err(e) => internal(e),
    }
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

// -- read pages ------------------------------------------------------------

async fn status_page(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Some(r) = read_guard(&state, &jar).await {
        return r;
    }
    let cfg = state.cfg();
    let logged_in = is_logged_in(&state, &jar).await;
    let now = crate::now_ts();

    let maintenance = db::active_maintenance(&state.pool, now)
        .await
        .unwrap_or_default();
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
            components
                .push(build_component(&state.pool, &cfg, comp, &maintenance, &active, now).await);
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

    let mut active_views = Vec::new();
    for inc in &active {
        let msg = db::last_update_message(&state.pool, inc.id)
            .await
            .ok()
            .flatten();
        active_views.push(IncidentView::from_incident(inc, msg));
    }
    let recent = db::recent_incidents(&state.pool, 10)
        .await
        .unwrap_or_default();
    let recent_views: Vec<IncidentView> = recent
        .iter()
        .map(|i| IncidentView::from_incident(i, None))
        .collect();

    let all_components = cfg
        .components
        .iter()
        .map(|c| NavComponent {
            id: c.id.clone(),
            name: c.name.clone(),
        })
        .collect();

    let maintenance_windows = maintenance
        .iter()
        .map(|m| MaintenanceView {
            id: m.id,
            component: if m.component.is_empty() {
                "all components".to_string()
            } else {
                m.component.clone()
            },
            note: m.note.clone(),
            ends: fmt_ts(m.ends_at),
        })
        .collect();

    render(&StatusTemplate {
        overall_class: overall.as_str().to_string(),
        overall_label: overall.label().to_string(),
        groups,
        has_active_incidents: !active_views.is_empty(),
        has_recent_incidents: !recent_views.is_empty(),
        active_incidents: active_views,
        recent_incidents: recent_views,
        logged_in,
        all_components,
        maintenance_windows,
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

    let (base, bars, uptime_90) = match check {
        Some(check) => {
            let last = db::last_check_result(pool, &check.id).await.ok().flatten();
            let failures = db::consecutive_failures(pool, &check.id, check.failures_to_open as i64)
                .await
                .unwrap_or(0);
            let base = status::check_state(last.as_ref(), failures, check.failures_to_open);
            let daily = db::daily_uptime(pool, &check.id, now - 90 * 86_400)
                .await
                .unwrap_or_default();
            let bars = build_bars(&daily, now);
            let total: i64 = daily.iter().map(|(_, t, _)| t).sum();
            let up: i64 = daily.iter().map(|(_, _, u)| u).sum();
            let u = if total > 0 {
                Some(format!("{:.2}", up as f64 / total as f64 * 100.0))
            } else {
                None
            };
            (base, bars, u)
        }
        None => (CompState::Operational, Vec::new(), None),
    };

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
        bars,
    }
}

fn build_bars(daily: &[(i64, i64, i64)], now: i64) -> Vec<BarView> {
    status::uptime_bars(daily, 90, now)
        .into_iter()
        .map(|b| {
            let (class, title) = match b.uptime {
                None => ("empty".to_string(), "no data".to_string()),
                Some(u) => {
                    let class = if u >= 99.99 {
                        "operational"
                    } else if u >= 95.0 {
                        "degraded"
                    } else if u >= 50.0 {
                        "partial_outage"
                    } else {
                        "major_outage"
                    };
                    let day = chrono::DateTime::from_timestamp(b.day, 0)
                        .map(|d| d.format("%Y-%m-%d").to_string())
                        .unwrap_or_default();
                    (class.to_string(), format!("{day}: {u:.2}%"))
                }
            };
            BarView { class, title }
        })
        .collect()
}

async fn metrics_page(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Some(r) = read_guard(&state, &jar).await {
        return r;
    }
    let cfg = state.cfg();
    let logged_in = is_logged_in(&state, &jar).await;
    let series = build_series_list(&state.pool, &cfg).await;
    let series_json = serde_json::to_string(&series).unwrap_or_else(|_| "[]".to_string());
    render(&MetricsTemplate {
        logged_in,
        series,
        series_json,
    })
}

async fn build_series_list(pool: &Pool, cfg: &Config) -> Vec<SeriesView> {
    let mut entries: Vec<(String, String, String, String)> = Vec::new();
    let mut push = |scope: &str, metric: &str, key: &str, label: String| {
        entries.push((
            scope.to_string(),
            metric.to_string(),
            key.to_string(),
            label,
        ));
    };

    for (scope, metric, key) in db::distinct_series(pool).await.unwrap_or_default() {
        let label = series_label(cfg, &scope, &metric, &key);
        push(&scope, &metric, &key, label);
    }
    // Defaults so a fresh install has something to chart before the first probe.
    for metric in [
        "cpu_pct",
        "mem_pct",
        "swap_pct",
        "load1",
        "net_rx_bps",
        "net_tx_bps",
    ] {
        push("host", metric, "", series_label(cfg, "host", metric, ""));
    }
    for mount in configured_mounts(cfg) {
        push(
            "host",
            "disk_used_pct",
            &mount,
            format!("disk {mount} usage"),
        );
    }

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (scope, metric, key, label) in entries {
        if seen.insert((scope.clone(), metric.clone(), key.clone())) {
            out.push(SeriesView {
                index: out.len(),
                label,
                scope,
                metric,
                key,
            });
        }
    }
    out
}

fn series_label(cfg: &Config, scope: &str, metric: &str, key: &str) -> String {
    if scope == "check" {
        if let Some(c) = cfg.check(key) {
            return format!("{} {}", c.name, metric);
        }
    }
    let base = match metric {
        "cpu_pct" => "host CPU".to_string(),
        "mem_pct" => "host memory".to_string(),
        "swap_pct" => "host swap".to_string(),
        "load1" => "host load (1m)".to_string(),
        "net_rx_bps" => "network in".to_string(),
        "net_tx_bps" => "network out".to_string(),
        "disk_used_pct" => format!("disk {key} usage"),
        "latency_ms" => "latency".to_string(),
        "up" => "up".to_string(),
        "running" => "running".to_string(),
        "restarts" => "restarts".to_string(),
        "active" => "systemd active".to_string(),
        other => other.to_string(),
    };
    if key.is_empty() {
        base
    } else {
        format!("{base} · {key}")
    }
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

    let (t, v) = if range == "24h" {
        match db::series(&state.pool, &scope, &metric, &key, now - 86_400, now).await {
            Ok(rows) => rows.into_iter().unzip(),
            Err(e) => return internal(e),
        }
    } else {
        let days = if range == "7d" { 7 } else { 90 };
        match db::hourly_series(&state.pool, &scope, &metric, &key, now - days * 86_400, now).await
        {
            Ok(rows) => {
                let t: Vec<i64> = rows.iter().map(|r| r.0).collect();
                let v: Vec<f64> = rows.iter().map(|r| r.2).collect();
                (t, v)
            }
            Err(e) => return internal(e),
        }
    };
    let unit = match metric.as_str() {
        "cpu_pct" | "mem_pct" | "swap_pct" | "disk_used_pct" => "%",
        "net_rx_bps" | "net_tx_bps" | "mem_bytes" => "bytes/s",
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
    });
    ([(CONTENT_TYPE, "application/json")], payload.to_string()).into_response()
}

async fn incidents_page(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Some(r) = read_guard(&state, &jar).await {
        return r;
    }
    let logged_in = is_logged_in(&state, &jar).await;
    let incidents = db::recent_incidents(&state.pool, 100)
        .await
        .unwrap_or_default();
    let views: Vec<IncidentView> = incidents
        .iter()
        .map(|i| IncidentView::from_incident(i, None))
        .collect();
    render(&IncidentsTemplate {
        logged_in,
        incidents: views,
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
    let logged_in = is_logged_in(&state, &jar).await;
    let inc = match db::incident(&state.pool, id).await {
        Ok(Some(i)) => i,
        Ok(None) => return (StatusCode::NOT_FOUND, "incident not found").into_response(),
        Err(e) => return internal(e),
    };
    let updates = db::incident_updates(&state.pool, id)
        .await
        .unwrap_or_default();
    let updates: Vec<UpdateView> = updates.iter().map(UpdateView::from).collect();
    render(&IncidentTemplate {
        logged_in,
        id: inc.id,
        title: inc.title.clone(),
        impact_class: inc.impact.as_str().to_string(),
        impact_label: inc.impact.label().to_string(),
        component: if inc.component.is_empty() {
            "all components".to_string()
        } else {
            inc.component.clone()
        },
        state_label: inc.state.label().to_string(),
        created: fmt_ts(inc.created_at),
        has_resolved: inc.resolved_at.is_some(),
        resolved: inc.resolved_at.map(fmt_ts).unwrap_or_default(),
        auto: inc.auto,
        updates,
    })
}

async fn feed(State(state): State<AppState>, headers: HeaderMap) -> Response {
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
        Err(e) => internal(e),
    }
}

fn iso8601(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
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

async fn asset(Path(file): Path<String>) -> Response {
    let (content_type, body): (&str, &[u8]) = match file.as_str() {
        "style.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../assets/style.css"),
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
    ([(CONTENT_TYPE, content_type)], body).into_response()
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
                    return internal(e);
                }
                return (StatusCode::OK, "ok").into_response();
            }
        }
    }
    (StatusCode::NOT_FOUND, "unknown heartbeat token").into_response()
}

// -- auth ------------------------------------------------------------------

async fn login_page(State(state): State<AppState>, jar: CookieJar) -> Response {
    render(&LoginTemplate {
        logged_in: is_logged_in(&state, &jar).await,
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
        return (StatusCode::TOO_MANY_REQUESTS, "too many login attempts").into_response();
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
                logged_in: false,
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
        return internal(e);
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
        Err(e) => return internal(e),
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
        return internal(e);
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
        return internal(e);
    }
    if new_state == crate::models::IncidentState::Resolved {
        if let Err(e) = db::resolve_incident(&state.pool, id, now).await {
            return internal(e);
        }
    } else if let Err(e) = db::set_incident_state(&state.pool, id, new_state).await {
        return internal(e);
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
        return internal(e);
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
        return internal(e);
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
        return internal(e);
    }
    Redirect::to("/").into_response()
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
        return internal(e);
    }
    Redirect::to("/").into_response()
}

/// Exposed for tests to inspect the available series without HTTP.
pub async fn series_for_tests(pool: &Pool, cfg: &Config) -> Vec<BTreeMap<String, String>> {
    build_series_list(pool, cfg)
        .await
        .into_iter()
        .map(|s| {
            let mut m = BTreeMap::new();
            m.insert("scope".to_string(), s.scope);
            m.insert("metric".to_string(), s.metric);
            m.insert("key".to_string(), s.key);
            m
        })
        .collect()
}
