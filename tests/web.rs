//! End-to-end web tests over the real router with an in-memory database.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::header::{CONTENT_TYPE, COOKIE, HOST, ORIGIN};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Router;
use dunlin::config::{self, Config};
use dunlin::db::{self, Pool};
use dunlin::web::{self, AppState};
use http_body_util::BodyExt;
use tower::ServiceExt;

const PASSWORD: &str = "correcthorsebattery";

fn test_config(protect_read: bool) -> Arc<Config> {
    test_config_with(protect_read, "", std::path::Path::new("."))
}

/// `extra` is appended to the base TOML (e.g. a `[theme]` table); relative
/// theme paths resolve against `base`.
fn test_config_with(protect_read: bool, extra: &str, base: &std::path::Path) -> Arc<Config> {
    Arc::new(config::parse_str_at(&config_toml(protect_read, extra), base).unwrap())
}

fn config_toml(protect_read: bool, extra: &str) -> String {
    let hash = dunlin::auth::hash_password(PASSWORD).unwrap();
    format!(
        r#"
listen = "127.0.0.1:0"
[web]
protect_read = {protect_read}
password_hash = "{hash}"

[[groups]]
id = "g"
name = "Core"

[[checks]]
id = "c1"
name = "Homepage"
type = "tcp"
host = "127.0.0.1"
port = 9

[[checks]]
id = "hb"
name = "Cron job"
type = "heartbeat"
period = "1h"
grace = "5m"
token = "super-secret-token"

[[components]]
id = "web"
name = "Website"
group = "g"
check = "c1"

{extra}
"#
    )
}

async fn state(protect_read: bool) -> (AppState, Pool) {
    state_from(test_config(protect_read)).await
}

async fn state_from(cfg: Arc<Config>) -> (AppState, Pool) {
    let pool = db::connect_memory().await.unwrap();
    let (state, _tx) = AppState::new(pool.clone(), cfg);
    (state, pool)
}

/// Test client address used for socket-based logins.
fn default_ip() -> std::net::SocketAddr {
    "127.0.0.1:5555".parse().unwrap()
}

/// Router with a mocked peer address, as axum does in its own tests.
fn app(state: AppState) -> Router {
    web::router(state).layer(MockConnectInfo(default_ip()))
}

async fn send(app: Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(HOST, "localhost")
        .body(Body::empty())
        .unwrap()
}

fn post_form(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(HOST, "localhost")
        .header(ORIGIN, "http://localhost")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn cookie_from(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("set-cookie")?.to_str().ok()?;
    let first = raw.split(';').next()?;
    if first.is_empty() || first.ends_with('=') {
        None
    } else {
        Some(first.to_string())
    }
}

async fn login(app: &Router) -> String {
    let req = post_form("/login", &format!("password={PASSWORD}"));
    let (status, headers, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "login should redirect");
    cookie_from(&headers).expect("session cookie")
}

#[tokio::test]
async fn read_pages_are_public() {
    let (state, pool) = state(false).await;
    let id = db::create_incident(
        &pool,
        "web",
        "Disk full",
        dunlin::models::State::MajorOutage,
        dunlin::models::IncidentState::Investigating,
        false,
        1,
    )
    .await
    .unwrap();
    let app = app(state);
    for uri in [
        "/".to_string(),
        "/metrics".to_string(),
        "/incidents".to_string(),
        format!("/incidents/{id}"),
        "/health".to_string(),
        "/feed.xml".to_string(),
        "/assets/style.css".to_string(),
        "/assets/htmx.min.js".to_string(),
        "/assets/uplot.js".to_string(),
        "/assets/uplot.css".to_string(),
        "/assets/dunlin.js".to_string(),
        "/assets/metrics.js".to_string(),
        "/assets/favicon.svg".to_string(),
        "/assets/fonts/bricolage-grotesque-latin-800-normal.woff2".to_string(),
        "/assets/fonts/martian-mono-OFL.txt".to_string(),
        "/metrics?range=7d".to_string(),
    ] {
        let (status, _, _) = send(app.clone(), get(&uri)).await;
        assert_eq!(status, StatusCode::OK, "GET {uri}");
    }
}

#[tokio::test]
async fn write_routes_redirect_without_session() {
    let (state, _pool) = state(false).await;
    let app = app(state);
    let cases = [
        ("/incidents", "component=web&title=x&impact=degraded"),
        ("/incidents/1/updates", "state=identified&message=hi"),
        ("/incidents/1/resolve", ""),
        ("/maintenance", "component=web&duration_minutes=30&note=n"),
        ("/maintenance/1/end", ""),
    ];
    for (uri, body) in cases {
        let (status, _, _) = send(app.clone(), post_form(uri, body)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "POST {uri}");
    }
}

#[tokio::test]
async fn csrf_rejected_without_origin() {
    let (state, _pool) = state(false).await;
    let app = app(state);
    let req = Request::builder()
        .method("POST")
        .uri("/incidents")
        .header(HOST, "localhost")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("component=web&title=x&impact=degraded"))
        .unwrap();
    let (status, _, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn login_flow_allows_write_and_logout() {
    let (state, pool) = state(false).await;
    let app = app(state);
    let cookie = login(&app).await;

    let mut req = post_form(
        "/incidents",
        "component=web&title=Manual&impact=degraded&message=hello",
    );
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, headers, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = headers
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(location.starts_with("/incidents/"));
    assert_eq!(db::recent_incidents(&pool, 10).await.unwrap().len(), 1);

    let mut logout = post_form("/logout", "");
    logout.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, _) = send(app.clone(), logout).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn login_rejects_wrong_password_then_rate_limits_per_ip() {
    let (state, _pool) = state(false).await;
    let addr: std::net::SocketAddr = "10.0.0.1:1234".parse().unwrap();
    let app = web::router(state.clone()).layer(MockConnectInfo(addr));

    for _ in 0..5 {
        let (status, _, _) = send(app.clone(), post_form("/login", "password=wrongpassword")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, _, _) = send(app.clone(), post_form("/login", "password=wrongpassword")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // A different IP is not blocked just because another one failed.
    let other: std::net::SocketAddr = "10.0.0.2:1234".parse().unwrap();
    let app2 = web::router(state.clone()).layer(MockConnectInfo(other));
    let (status, _, _) = send(app2, post_form("/login", "password=wrongpassword")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn protect_read_locks_read_pages() {
    let (state, _pool) = state(true).await;
    let app = app(state);
    let (status, headers, _) = send(app.clone(), get("/")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers.get("location").unwrap(), "/login");

    // The feed repeats incident titles and messages, so it is locked too.
    let (status, headers, _) = send(app.clone(), get("/feed.xml")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers.get("location").unwrap(), "/login");

    let cookie = login(&app).await;
    let mut req = get("/");
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Website"));

    let mut req = get("/feed.xml");
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn heartbeat_endpoint_records_ping() {
    let (state, pool) = state(false).await;
    let app = app(state);

    let (status, _, _) = send(app.clone(), get("/hb/wrong-token")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(db::last_ping(&pool, "hb").await.unwrap(), None);

    let (status, _, _) = send(app.clone(), get("/hb/super-secret-token")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(db::last_ping(&pool, "hb").await.unwrap().is_some());

    // POST works too, for clients that cannot send a body-less GET.
    let (status, _, _) = send(app, post_form("/hb/super-secret-token", "")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn manual_incident_updates_and_resolve() {
    use dunlin::models::{IncidentState, State};

    let (state, pool) = state(false).await;
    let app = app(state);
    let cookie = login(&app).await;

    let mut req = post_form(
        "/incidents",
        "component=web&title=Manual&impact=major_outage&message=start",
    );
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (_, headers, _) = send(app.clone(), req).await;
    let id: i64 = headers
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();

    let mut update = post_form(
        &format!("/incidents/{id}/updates"),
        "state=identified&message=found%20the%20cause",
    );
    update.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, _) = send(app.clone(), update).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let inc = db::incident(&pool, id).await.unwrap().unwrap();
    assert_eq!(inc.state, IncidentState::Identified);
    assert_eq!(inc.impact, State::MajorOutage);
    assert_eq!(db::incident_updates(&pool, id).await.unwrap().len(), 2);

    let mut resolve = post_form(&format!("/incidents/{id}/resolve"), "");
    resolve
        .headers_mut()
        .insert(COOKIE, cookie.parse().unwrap());
    let (status, _, _) = send(app, resolve).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let inc = db::incident(&pool, id).await.unwrap().unwrap();
    assert!(inc.resolved_at.is_some());
}

#[tokio::test]
async fn start_and_end_maintenance_window() {
    let (state, pool) = state(false).await;
    let app = app(state);
    let cookie = login(&app).await;

    let mut req = post_form(
        "/maintenance",
        "component=web&duration_minutes=30&note=upgrade",
    );
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let now = dunlin::now_ts();
    let active = db::active_maintenance(&pool, now).await.unwrap();
    assert_eq!(active.len(), 1);

    // The component is shown as under maintenance on the public page...
    let (status, _, body) = send(app.clone(), get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("<em class=\"mnt\">Website</em> is under maintenance until"),
        "{body}"
    );
    assert!(
        body.contains("Planned disk") || body.contains("upgrade"),
        "{body}"
    );
    assert!(
        !body.contains("/maintenance/"),
        "write controls leaked to /"
    );

    // ...and the end control lives on the operator page.
    let mut page = get("/manage");
    page.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, body) = send(app.clone(), page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/maintenance/"), "end control missing");

    let mut end = post_form(&format!("/maintenance/{}/end", active[0].id), "");
    end.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, _) = send(app, end).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(db::active_maintenance(&pool, now).await.unwrap().is_empty());
}

#[tokio::test]
async fn feed_is_valid_atom() {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let (state, pool) = state(false).await;
    let id = db::create_incident(
        &pool,
        "web",
        "Bad & worse <incident>",
        dunlin::models::State::MajorOutage,
        dunlin::models::IncidentState::Investigating,
        true,
        1,
    )
    .await
    .unwrap();
    db::add_update(
        &pool,
        id,
        2,
        dunlin::models::IncidentState::Identified,
        "Looking into it",
        false,
    )
    .await
    .unwrap();

    let app = app(state);
    let (status, headers, body) = send(app, get("/feed.xml")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .get(CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("application/atom+xml"));

    // Parse as XML: no errors, root is feed in the Atom namespace, one entry.
    let mut reader = Reader::from_str(&body);
    reader.config_mut().trim_text(true);
    let mut entries = 0;
    let mut root = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name().as_ref().to_string();
                if root.is_empty() {
                    root = name.clone();
                }
                if name == "entry" {
                    entries += 1;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => panic!("invalid Atom XML: {e}"),
        }
    }
    assert_eq!(root, "feed");
    assert_eq!(entries, 1);
    assert!(body.contains("http://www.w3.org/2005/Atom"));
    // Special characters are escaped (askama emits numeric entities for XML).
    assert!(
        body.contains("&#38;") || body.contains("&amp;"),
        "title should be escaped: {body}"
    );
    assert!(!body.contains("Bad & worse"));
}

#[tokio::test]
async fn manage_requires_login() {
    let (state, _pool) = state(false).await;
    let app = app(state);
    let (status, headers, _) = send(app.clone(), get("/manage")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers.get("location").unwrap(), "/login");

    let cookie = login(&app).await;
    let mut req = get("/manage");
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, body) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    for action in [
        "action=\"/incidents\"",
        "action=\"/maintenance\"",
        "name=\"duration_minutes\"",
        "name=\"component\"",
        "name=\"title\"",
    ] {
        assert!(body.contains(action), "{action} missing");
    }
}

#[tokio::test]
async fn write_forms_are_not_on_the_status_page() {
    let (state, _pool) = state(false).await;
    let app = app(state);
    let cookie = login(&app).await;
    let mut req = get("/");
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("action=\"/incidents\""));
    assert!(!body.contains("action=\"/maintenance\""));
    assert!(
        body.contains("href=\"/manage\""),
        "manage link for operators"
    );

    let (_, _, body) = send(app, get("/")).await;
    assert!(!body.contains("href=\"/manage\""));
    assert!(body.contains("href=\"/login\""));
}

#[tokio::test]
async fn status_page_structure() {
    let (state, pool) = state(false).await;
    let now = dunlin::now_ts();
    dunlin::db::insert_check_result(
        &pool,
        &dunlin::models::CheckResult {
            ts: now - 60,
            check_id: "c1".into(),
            ok: true,
            degraded: false,
            latency_ms: Some(3.0),
            message: None,
        },
    )
    .await
    .unwrap();
    db::create_incident(
        &pool,
        "web",
        "Slow pages",
        dunlin::models::State::Degraded,
        dunlin::models::IncidentState::Investigating,
        false,
        now - 120,
    )
    .await
    .unwrap();
    let app = app(state);
    let (status, _, body) = send(app, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.matches("class=\"bar ").count(),
        90,
        "one strip of 90 days"
    );
    assert!(body.contains("100.00%"), "90-day uptime");
    assert!(body.contains("What happened lately"));
    assert_eq!(
        body.matches("<time class=\"d\"").count(),
        14,
        "one row per day"
    );
    assert!(body.contains("Nothing."));
    assert!(body.contains("Slow pages"));
    assert!(
        body.contains("<h1 class=\"say\"><em class=\"warn\">Website</em> is slow.</h1>"),
        "the sentence follows the worst state: {body}"
    );
    assert!(
        body.contains("<dt>Open incidents</dt><dd>1</dd>"),
        "facts list"
    );
}

/// The `<div class="entry">` of the day row whose `datetime` is `iso`.
fn day_row<'a>(body: &'a str, iso: &str) -> &'a str {
    let start = body
        .find(&format!("datetime=\"{iso}\""))
        .unwrap_or_else(|| panic!("no row for {iso}"));
    let rest = &body[start..];
    &rest[..rest.find("<div class=\"entry\">").unwrap_or(rest.len())]
}

#[tokio::test]
async fn days_follow_the_configured_timezone() {
    use chrono::{TimeZone, Utc};
    let tz = chrono_tz::Europe::Istanbul;
    let cfg = Arc::new(
        config::parse_str(&format!(
            "timezone = \"Europe/Istanbul\"\n{}",
            config_toml(false, "")
        ))
        .unwrap(),
    );
    let (state, pool) = state_from(cfg).await;
    let now = dunlin::now_ts();

    // 02:28 local yesterday, which is still the day before in UTC.
    let yesterday = Utc
        .timestamp_opt(now, 0)
        .unwrap()
        .with_timezone(&tz)
        .date_naive()
        .pred_opt()
        .unwrap();
    let opened = tz
        .from_local_datetime(&yesterday.and_hms_opt(2, 28, 0).unwrap())
        .unwrap()
        .timestamp();
    db::create_incident(
        &pool,
        "web",
        "Night outage",
        dunlin::models::State::MajorOutage,
        dunlin::models::IncidentState::Resolved,
        false,
        opened,
    )
    .await
    .unwrap();
    // The first of a month just after local midnight is the previous month
    // in UTC.
    let first = tz
        .with_ymd_and_hms(2026, 9, 1, 0, 30, 0)
        .unwrap()
        .timestamp();
    db::create_incident(
        &pool,
        "web",
        "Month edge",
        dunlin::models::State::Degraded,
        dunlin::models::IncidentState::Resolved,
        false,
        first,
    )
    .await
    .unwrap();

    let app = app(state);
    let (_, _, body) = send(app.clone(), get("/")).await;
    let iso = yesterday.format("%Y-%m-%d").to_string();
    let row = day_row(&body, &iso);
    assert!(row.contains("Night outage"), "{row}");
    assert!(
        row.contains(&yesterday.format(">%b %-d<").to_string()),
        "{row}"
    );
    let day_before = yesterday.pred_opt().unwrap().format("%Y-%m-%d").to_string();
    assert!(!day_row(&body, &day_before).contains("Night outage"));

    let (_, _, body) = send(app, get("/incidents")).await;
    let before = &body[..body.find("Month edge").unwrap()];
    let entry = &before[before.rfind("<div class=\"entry\">").unwrap()..];
    assert!(entry.contains(">Sep 1<"), "{entry}");
    let month = &before[before.rfind("class=\"month-name\"").unwrap()..];
    assert!(month.contains(">September 2026<"), "{month}");
}

#[tokio::test]
async fn incident_page_renders_timeline_newest_first() {
    use dunlin::models::{IncidentState, State};
    let (state, pool) = state(false).await;
    let id = db::create_incident(
        &pool,
        "web",
        "Outage",
        State::MajorOutage,
        IncidentState::Investigating,
        false,
        100,
    )
    .await
    .unwrap();
    db::add_update(
        &pool,
        id,
        100,
        IncidentState::Investigating,
        "first note",
        false,
    )
    .await
    .unwrap();
    db::add_update(
        &pool,
        id,
        200,
        IncidentState::Identified,
        "second note",
        false,
    )
    .await
    .unwrap();
    let app = app(state);
    let (status, _, body) = send(app.clone(), get(&format!("/incidents/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    let (first, second) = (
        body.find("second note").unwrap(),
        body.find("first note").unwrap(),
    );
    assert!(first < second, "newest update first");
    assert!(body.contains("Website"), "component shown by name");
    assert!(!body.contains("/resolve"), "no write form for visitors");

    let (status, _, body) = send(app.clone(), get("/incidents/99999")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Incident not found"));

    let (status, _, body) = send(app.clone(), get("/incidents")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("January 1970"), "grouped by month");
}

#[tokio::test]
async fn unknown_page_is_a_designed_404() {
    let (state, _pool) = state(false).await;
    let app = app(state);
    let (status, headers, body) = send(app.clone(), get("/no/such/page")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(headers
        .get(CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    assert!(body.contains("Page not found"));
    assert!(body.contains("There is nothing at /no/such/page."));
    let (status, _, _) = send(app.clone(), get("/assets/nope.js")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Fonts come from a fixed list, never from a path on disk.
    for uri in ["/assets/fonts/nope.woff2", "/assets/fonts/..%2FCargo.toml"] {
        let (status, _, _) = send(app.clone(), get(uri)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    let (status, headers, _) = send(
        app,
        get("/assets/fonts/martian-mono-latin-400-normal.woff2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "font/woff2");
}

#[tokio::test]
async fn metrics_api_returns_series_and_summary() {
    let (state, pool) = state(false).await;
    let now = dunlin::now_ts();
    let samples: Vec<dunlin::models::Sample> = [10.0, 30.0, 20.0]
        .iter()
        .enumerate()
        .map(|(i, v)| dunlin::models::Sample {
            ts: now - 300 + i as i64 * 60,
            scope: "host".into(),
            metric: "cpu_pct".into(),
            key: String::new(),
            value: *v,
        })
        .collect();
    db::insert_samples(&pool, &samples).await.unwrap();
    let app = app(state);
    let (status, _, body) = send(
        app.clone(),
        get("/api/metrics?scope=host&metric=cpu_pct&key=&range=24h"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["unit"], "%");
    assert_eq!(v["points"], 3);
    assert_eq!(v["t"].as_array().unwrap().len(), 3);
    assert_eq!(v["v"][1], 30.0);
    assert_eq!(v["last"], 20.0);
    assert_eq!(v["max"], 30.0);
    assert_eq!(v["avg"], 20.0);

    // An empty series still has the full shape, with null summaries.
    let (_, _, body) = send(
        app.clone(),
        get("/api/metrics?scope=host&metric=swap_pct&range=7d"),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["points"], 0);
    assert!(v["last"].is_null() && v["avg"].is_null() && v["max"].is_null());

    let (_, _, body) = send(app, get("/metrics")).await;
    assert!(body.contains("id=\"chart-cards\""));
    assert!(
        body.contains("\"metric\":\"latency_ms\",\"key\":\"c1\""),
        "check latency card"
    );
    assert!(body.contains("\"metric\":\"cpu_pct\""));
}

#[tokio::test]
async fn favicon_and_default_theme() {
    let (state, _pool) = state(false).await;
    let app = app(state);
    let (status, headers, body) = send(app.clone(), get("/assets/favicon.svg")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "image/svg+xml");
    assert!(body.contains("#17150f"));

    let (_, _, body) = send(app.clone(), get("/")).await;
    // Light only: no theme switch is rendered.
    assert!(body.contains("<html lang=\"en\">"));
    assert!(!body.contains("data-theme"));
    assert!(body.contains("<title>Status</title>"));
    assert!(body.contains("--accent:#17150f"));
    assert!(!body.contains("/assets/custom.css"));
    assert!(!body.contains("/assets/logo"));

    // Nothing configured, so nothing served.
    let (status, _, _) = send(app.clone(), get("/assets/custom.css")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = send(app, get("/assets/logo")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn configured_theme_applies_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("brand.png"), b"\x89PNG fake").unwrap();
    std::fs::write(dir.path().join("brand.css"), ".banner{border-width:2px}").unwrap();
    let cfg = test_config_with(
        false,
        "[theme]\ntitle = \"Acme status\"\naccent = \"#7a3cff\"\nlogo = \"brand.png\"\ncustom_css = \"brand.css\"\n",
        dir.path(),
    );
    let (state, _pool) = state_from(cfg).await;
    let app = app(state);

    for uri in ["/", "/metrics", "/incidents", "/login", "/nope"] {
        let (_, _, body) = send(app.clone(), get(uri)).await;
        assert!(body.contains("Acme status</title>"), "{uri}");
        assert!(body.contains("class=\"brand-title\">Acme status<"), "{uri}");
        assert!(body.contains("href=\"/assets/custom.css\""), "{uri}");
        assert!(body.contains("src=\"/assets/logo\""), "{uri}");
        assert!(body.contains("--accent:#7a3cff"), "{uri}");
    }

    let (status, headers, body) = send(app.clone(), get("/assets/custom.css")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .get(CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/css"));
    assert_eq!(body, ".banner{border-width:2px}");

    let (status, headers, _) = send(app.clone(), get("/assets/logo")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "image/png");
    assert!(headers.get("content-security-policy").is_some());

    let (_, _, body) = send(app, get("/feed.xml")).await;
    assert!(body.contains("<title>Acme status incidents</title>"));
}

#[tokio::test]
async fn theme_follows_hot_reload() {
    let pool = db::connect_memory().await.unwrap();
    let (state, tx) = AppState::new(pool, test_config(false));
    let app = app(state);
    let (_, _, body) = send(app.clone(), get("/")).await;
    assert!(body.contains("<title>Status</title>"));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dunlin.toml");
    std::fs::write(dir.path().join("x.css"), "body{}").unwrap();
    let hash = dunlin::auth::hash_password(PASSWORD).unwrap();
    std::fs::write(
        &path,
        format!("listen = \"127.0.0.1:0\"\n[web]\npassword_hash = \"{hash}\"\n[theme]\ntitle = \"Renamed\"\ncustom_css = \"x.css\"\n"),
    )
    .unwrap();
    dunlin::app::apply_reload(&path, &tx);

    let (_, _, body) = send(app.clone(), get("/")).await;
    assert!(body.contains("<title>Renamed</title>"));
    let (status, _, body) = send(app, get("/assets/custom.css")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "body{}");
}

#[tokio::test]
async fn incident_on_all_components_shows_on_every_component() {
    let (state, pool) = state(false).await;
    db::create_incident(
        &pool,
        "",
        "Everything is on fire",
        dunlin::models::State::MajorOutage,
        dunlin::models::IncidentState::Investigating,
        false,
        dunlin::now_ts() - 60,
    )
    .await
    .unwrap();

    let (status, _, body) = send(app(state), get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("Everything is up."), "{body}");
    assert!(body.contains("svc s-major_outage"), "{body}");
    assert!(!body.contains("svc s-operational"), "{body}");
    assert!(body.contains("bar s-major_outage today"), "{body}");
}
