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
async fn manual_incident_on_a_removed_component_still_renders() {
    use dunlin::models::{IncidentState, State};
    let (state, pool) = state(false).await;
    let id = db::create_incident(
        &pool,
        "removed-db",
        "Database migration",
        State::PartialOutage,
        IncidentState::Identified,
        false,
        100,
    )
    .await
    .unwrap();
    db::add_update(&pool, id, 100, IncidentState::Identified, "on it", false)
        .await
        .unwrap();
    let app = app(state);
    let cookie = login(&app).await;

    // No component row owns it, so the status page only links it from the
    // detail line; what matters is that nothing errors or goes missing.
    let (status, _, body) = send(app.clone(), get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!("/incidents/{id}")));
    for uri in ["/incidents", &format!("/incidents/{id}"), "/feed.xml"] {
        let (status, _, body) = send(app.clone(), get(uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(body.contains("Database migration"), "{uri}");
    }
    let mut manage = get("/manage");
    manage.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, body) = send(app, manage).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Database migration"));
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

#[tokio::test]
async fn huge_maintenance_duration_is_clamped() {
    let (state, pool) = state(false).await;
    let app = app(state);
    let cookie = login(&app).await;
    let mut req = post_form(
        "/maintenance",
        &format!(
            "component=web&note=x&duration_minutes={}&starts_in_seconds=-5",
            i64::MAX
        ),
    );
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, _) = send(app, req).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let now = dunlin::now_ts();
    let windows = db::active_maintenance(&pool, now).await.unwrap();
    assert_eq!(windows.len(), 1);
    let w = &windows[0];
    assert!(w.starts_at >= now - 5, "{w:?}");
    assert!(w.ends_at <= now + 366 * 86_400 + 5, "{w:?}");
}

async fn record_results(pool: &Pool, check_id: &str, ok: usize, failed: usize) {
    let now = dunlin::now_ts();
    for i in 0..ok + failed {
        db::insert_check_result(
            pool,
            &dunlin::models::CheckResult {
                // Oldest first, passing ones last, so the check is up now.
                ts: now - 3600 + i as i64,
                check_id: check_id.into(),
                ok: i >= failed,
                degraded: false,
                latency_ms: Some(3.0),
                message: None,
            },
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn state_badge_follows_the_status_page() {
    let (state, pool) = state(false).await;
    let app = app(state);

    let (status, headers, body) = send(app.clone(), get("/badge/web.svg")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[CONTENT_TYPE], "image/svg+xml");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["cache-control"], "no-cache");
    assert!(body.starts_with("<svg"), "{body}");
    assert!(body.contains(r#"role="img""#), "{body}");
    assert!(
        body.contains("<title>Website: operational</title>"),
        "{body}"
    );
    assert!(body.contains("#2e9e5b"), "{body}");

    // Maintenance wins over a healthy check, as on the status page.
    let now = dunlin::now_ts();
    let mnt = db::create_maintenance(&pool, "web", "", now - 60, now + 600, now)
        .await
        .unwrap();
    let (_, _, body) = send(app.clone(), get("/badge/web.svg")).await;
    assert!(
        body.contains("<title>Website: maintenance</title>"),
        "{body}"
    );
    assert!(body.contains("#2f64d8"), "{body}");
    db::end_maintenance(&pool, mnt, now).await.unwrap();

    // An incident on "All components" counts for this one too.
    db::create_incident(
        &pool,
        "",
        "Everything is on fire",
        dunlin::models::State::PartialOutage,
        dunlin::models::IncidentState::Investigating,
        false,
        now - 60,
    )
    .await
    .unwrap();
    let (_, _, body) = send(app.clone(), get("/badge/web.svg")).await;
    assert!(
        body.contains("<title>Website: partial outage</title>"),
        "{body}"
    );
    assert!(body.contains("#e2461f"), "{body}");

    let (status, headers, body) = send(app, get("/badge/web.json")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[CONTENT_TYPE], "application/json");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "schemaVersion": 1,
            "label": "Website",
            "message": "partial outage",
            "color": "e2461f",
        })
    );
}

#[tokio::test]
async fn uptime_badge_shows_the_90_day_figure() {
    let (state, pool) = state(false).await;
    let app = app(state);

    let (status, headers, body) = send(app.clone(), get("/badge/web/uptime.svg")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[CONTENT_TYPE], "image/svg+xml");
    assert!(
        body.contains("<title>Website uptime: no data</title>"),
        "{body}"
    );

    record_results(&pool, "c1", 199, 1).await;
    let (_, _, body) = send(app.clone(), get("/badge/web/uptime.svg")).await;
    assert!(
        body.contains("<title>Website uptime: 99.50%</title>"),
        "{body}"
    );
    assert!(body.contains("#e0a21b"), "{body}");

    // The status page prints the same figure.
    let (_, _, page) = send(app, get("/")).await;
    assert!(page.contains("99.50"), "{page}");
}

#[tokio::test]
async fn unknown_badges_are_404() {
    let (state, _pool) = state(false).await;
    let app = app(state);
    for (uri, kind) in [
        ("/badge/nope.svg", "image/svg+xml"),
        ("/badge/nope.json", "application/json"),
        ("/badge/nope/uptime.svg", "image/svg+xml"),
        ("/badge/web.png", "image/svg+xml"),
    ] {
        let (status, headers, _) = send(app.clone(), get(uri)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(headers[CONTENT_TYPE], kind, "{uri}");
    }
}

#[tokio::test]
async fn badge_text_is_escaped() {
    let extra = "[[components]]\nid = \"odd\"\nname = 'A <&\"> B'\n";
    let (state, _pool) =
        state_from(test_config_with(false, extra, std::path::Path::new("."))).await;
    let app = app(state);
    for uri in ["/badge/odd.svg", "/badge/odd/uptime.svg"] {
        let (status, _, body) = send(app.clone(), get(uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(!body.contains("A <&\"> B"), "{body}");
        assert!(body.contains("A &lt;&amp;&quot;&gt; B"), "{body}");
        let mut reader = quick_xml::Reader::from_str(&body);
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("{uri} is not well-formed XML: {e}\n{body}"),
            }
        }
    }
    let (_, _, body) = send(app, get("/badge/odd.json")).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["label"], "A <&\"> B");
}

#[tokio::test]
async fn protect_read_locks_badges() {
    let (state, _pool) = state(true).await;
    let app = app(state);
    for uri in ["/badge/web.svg", "/badge/web.json", "/badge/web/uptime.svg"] {
        let (status, headers, _) = send(app.clone(), get(uri)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(headers["location"], "/login", "{uri}");
    }
    let cookie = login(&app).await;
    for uri in ["/badge/web.svg", "/badge/web.json", "/badge/web/uptime.svg"] {
        let mut req = get(uri);
        req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
        let (status, _, _) = send(app.clone(), req).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
}

fn sample(ts: i64, scope: &str, metric: &str, key: &str, value: f64) -> dunlin::models::Sample {
    dunlin::models::Sample {
        ts,
        scope: scope.into(),
        metric: metric.into(),
        key: key.into(),
        value,
    }
}

/// Every line is a comment or `name{labels} value` with a numeric value.
fn assert_exposition_parses(body: &str) {
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            assert!(
                rest.starts_with("HELP dunlin_") || rest.starts_with("TYPE dunlin_"),
                "bad comment: {line}"
            );
            continue;
        }
        let (series, value) = line.rsplit_once(' ').expect(line);
        assert!(
            value.parse::<f64>().is_ok() || ["+Inf", "-Inf", "NaN"].contains(&value),
            "bad value: {line}"
        );
        let (name, labels) = match series.split_once('{') {
            Some((n, rest)) => (n, Some(rest.strip_suffix('}').expect(line))),
            None => (series, None),
        };
        assert!(
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'),
            "bad name: {line}"
        );
        let Some(mut rest) = labels else { continue };
        // Walk `key="value"` pairs, honouring backslash escapes in values.
        while !rest.is_empty() {
            let (key, after) = rest.split_once("=\"").expect(line);
            assert!(
                key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "bad label name: {line}"
            );
            let mut chars = after.char_indices();
            let end = loop {
                match chars.next().expect(line) {
                    (_, '\\') => {
                        let (_, esc) = chars.next().expect(line);
                        assert!(matches!(esc, '\\' | '"' | 'n'), "bad escape: {line}");
                    }
                    (i, '"') => break i,
                    (_, c) => assert_ne!(c, '\n'),
                }
            };
            rest = &after[end + 1..];
            rest = rest.strip_prefix(',').unwrap_or(rest);
        }
    }
}

#[tokio::test]
async fn prometheus_exports_current_values() {
    let (state, pool) = state(false).await;
    let now = dunlin::now_ts();
    db::insert_check_result(
        &pool,
        &dunlin::models::CheckResult {
            ts: now - 30,
            check_id: "c1".into(),
            ok: true,
            degraded: false,
            latency_ms: Some(250.0),
            message: None,
        },
    )
    .await
    .unwrap();
    db::create_incident(
        &pool,
        "web",
        "Slow",
        dunlin::models::State::Degraded,
        dunlin::models::IncidentState::Investigating,
        false,
        now - 60,
    )
    .await
    .unwrap();
    db::insert_samples(
        &pool,
        &[
            sample(now - 3600, "host", "cpu_pct", "", 90.0),
            sample(now - 30, "host", "cpu_pct", "", 25.0),
            sample(now - 30, "host", "mem_pct", "", 50.0),
            sample(now - 30, "host", "load1", "", 0.75),
            sample(now - 30, "host", "disk_used_pct", "/", 12.5),
            sample(now - 30, "host", "net_rx_bps", "", 2048.0),
            // Stale: the collector stopped writing it a while ago.
            sample(now - 3600, "host", "swap_pct", "", 10.0),
            sample(now - 30, "container", "cpu_pct", "api", 150.0),
            sample(now - 30, "container", "mem_bytes", "api", 1048576.0),
            sample(now - 30, "container", "running", "api", 1.0),
            // A container removed an hour ago must disappear.
            sample(now - 3600, "container", "running", "gone", 1.0),
            sample(now - 30, "container", "running", "we\"ird\\name", 0.0),
        ],
    )
    .await
    .unwrap();

    let (status, headers, body) = send(app(state), get("/metrics/prometheus")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[CONTENT_TYPE],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_exposition_parses(&body);

    for family in [
        "dunlin_check_up",
        "dunlin_check_latency_seconds",
        "dunlin_component_state",
        "dunlin_incidents_open",
        "dunlin_host_cpu_usage_ratio",
        "dunlin_host_memory_used_ratio",
        "dunlin_host_swap_used_ratio",
        "dunlin_host_load1",
        "dunlin_host_load5",
        "dunlin_host_load15",
        "dunlin_host_disk_used_ratio",
        "dunlin_host_network_receive_bytes_per_second",
        "dunlin_host_network_transmit_bytes_per_second",
        "dunlin_container_cpu_usage_ratio",
        "dunlin_container_memory_bytes",
        "dunlin_container_running",
        "dunlin_build_info",
    ] {
        assert!(
            body.contains(&format!("\n# HELP {family} "))
                || body.starts_with(&format!("# HELP {family} ")),
            "{family}\n{body}"
        );
        assert!(
            body.contains(&format!("# TYPE {family} gauge\n")),
            "{family}\n{body}"
        );
    }
    let lines: Vec<&str> = body.lines().collect();
    for want in [
        "dunlin_check_up{check=\"c1\"} 1",
        "dunlin_check_latency_seconds{check=\"c1\"} 0.25",
        "dunlin_component_state{component=\"web\",state=\"degraded\"} 1",
        "dunlin_component_state{component=\"web\",state=\"operational\"} 0",
        "dunlin_incidents_open 1",
        "dunlin_host_cpu_usage_ratio 0.25",
        "dunlin_host_memory_used_ratio 0.5",
        "dunlin_host_load1 0.75",
        "dunlin_host_disk_used_ratio{mount=\"/\"} 0.125",
        "dunlin_host_network_receive_bytes_per_second 2048",
        "dunlin_container_cpu_usage_ratio{container=\"api\"} 1.5",
        "dunlin_container_memory_bytes{container=\"api\"} 1048576",
        "dunlin_container_running{container=\"api\"} 1",
        "dunlin_container_running{container=\"we\\\"ird\\\\name\"} 0",
        &format!(
            "dunlin_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        ),
    ] {
        assert!(lines.contains(&want), "missing {want}\n{body}");
    }
    // The heartbeat has never reported, and stale samples are left out.
    assert!(!body.contains("check=\"hb\""), "{body}");
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("dunlin_host_swap_used_ratio")),
        "{body}"
    );
    assert!(!body.contains("gone"), "{body}");
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with("dunlin_component_state{component=\"web\""))
            .count(),
        5
    );
}

#[tokio::test]
async fn prometheus_label_values_are_escaped() {
    let extra = "[[components]]\nid = 'a\"b\\c'\nname = \"Odd\"\n";
    let (state, _pool) =
        state_from(test_config_with(false, extra, std::path::Path::new("."))).await;
    let (status, _, body) = send(app(state), get("/metrics/prometheus")).await;
    assert_eq!(status, StatusCode::OK);
    assert_exposition_parses(&body);
    assert!(
        body.contains("dunlin_component_state{component=\"a\\\"b\\\\c\",state=\"operational\"} 1"),
        "{body}"
    );
}

#[tokio::test]
async fn protect_read_locks_prometheus() {
    let (state, _pool) = state(true).await;
    let app = app(state);
    let (status, headers, _) = send(app.clone(), get("/metrics/prometheus")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/login");

    let cookie = login(&app).await;
    let mut req = get("/metrics/prometheus");
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, body) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("# TYPE dunlin_build_info gauge"));
}

const API_TOKEN: &str = "test-api-token";

/// Config with one API key for `API_TOKEN`.
fn api_config(protect_read: bool) -> Arc<Config> {
    let extra = format!(
        "[[api_keys]]\nname = \"grafana\"\nhash = \"{}\"\n",
        dunlin::auth::token_hash(API_TOKEN)
    );
    test_config_with(protect_read, &extra, std::path::Path::new("."))
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

#[tokio::test]
async fn api_status_shape() {
    let (state, pool) = state_from(api_config(false)).await;
    record_results(&pool, "c1", 199, 1).await;
    let now = dunlin::now_ts();
    db::create_incident(
        &pool,
        "web",
        "Slow",
        dunlin::models::State::Degraded,
        dunlin::models::IncidentState::Investigating,
        false,
        now - 120,
    )
    .await
    .unwrap();

    let (status, headers, body) = send(app(state), get("/api/v1/status")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[CONTENT_TYPE], "application/json");
    assert_eq!(headers["cache-control"], "no-cache");
    let v = json(&body);
    assert_eq!(v["status"], "degraded");
    assert!((v["generated_at"].as_i64().unwrap() - now).abs() < 5);
    assert_eq!(
        v["components"],
        serde_json::json!([{
            "id": "web",
            "name": "Website",
            "group": "g",
            "state": "degraded",
            "since": now - 120,
            "uptime_90d": 99.5,
        }])
    );
}

#[tokio::test]
async fn api_status_without_data_uses_nulls() {
    let extra = "[[components]]\nid = \"manual\"\nname = \"Manual\"\n";
    let (state, _pool) =
        state_from(test_config_with(false, extra, std::path::Path::new("."))).await;
    let (_, _, body) = send(app(state), get("/api/v1/status")).await;
    let v = json(&body);
    assert_eq!(v["status"], "operational");
    let manual = &v["components"][1];
    assert_eq!(manual["id"], "manual");
    assert!(manual["group"].is_null());
    assert!(manual["since"].is_null());
    assert!(manual["uptime_90d"].is_null());
}

#[tokio::test]
async fn api_incidents_list_and_detail() {
    use dunlin::models::{IncidentState, State};
    let (state, pool) = state(false).await;
    let mut ids = Vec::new();
    for i in 0..3 {
        let id = db::create_incident(
            &pool,
            "web",
            &format!("Incident {i}"),
            State::PartialOutage,
            IncidentState::Investigating,
            i == 0,
            1000 + i,
        )
        .await
        .unwrap();
        ids.push(id);
    }
    db::add_update(
        &pool,
        ids[0],
        1000,
        IncidentState::Investigating,
        "looking",
        true,
    )
    .await
    .unwrap();
    db::add_update(
        &pool,
        ids[0],
        1100,
        IncidentState::Identified,
        "found it",
        false,
    )
    .await
    .unwrap();
    db::resolve_incident(&pool, ids[0], 1200).await.unwrap();
    let app = app(state);

    let (status, headers, body) = send(app.clone(), get("/api/v1/incidents")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-cache");
    let list = json(&body);
    let titles: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Incident 2", "Incident 1", "Incident 0"]);
    assert_eq!(
        list[2],
        serde_json::json!({
            "id": ids[0],
            "title": "Incident 0",
            "component": "web",
            "state": "resolved",
            "impact": "partial_outage",
            "created_at": 1000,
            "resolved_at": 1200,
            "auto": true,
        })
    );
    assert!(list[0]["resolved_at"].is_null());

    let (status, _, body) = send(app.clone(), get(&format!("/api/v1/incidents/{}", ids[0]))).await;
    assert_eq!(status, StatusCode::OK);
    let one = json(&body);
    assert_eq!(one["title"], "Incident 0");
    assert_eq!(
        one["updates"],
        serde_json::json!([
            {"state": "identified", "message": "found it", "created_at": 1100},
            {"state": "investigating", "message": "looking", "created_at": 1000},
        ])
    );

    for uri in ["/api/v1/incidents/999", "/api/v1/incidents/abc"] {
        let (status, headers, body) = send(app.clone(), get(uri)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(headers[CONTENT_TYPE], "application/json");
        assert_eq!(json(&body), serde_json::json!({"error": "not found"}));
    }
}

#[tokio::test]
async fn api_incidents_limit_is_clamped() {
    let (state, pool) = state(false).await;
    for i in 0..105 {
        db::create_incident(
            &pool,
            "web",
            &format!("n{i}"),
            dunlin::models::State::MajorOutage,
            dunlin::models::IncidentState::Investigating,
            false,
            i,
        )
        .await
        .unwrap();
    }
    let app = app(state);
    for (uri, want) in [
        ("/api/v1/incidents", 20),
        ("/api/v1/incidents?limit=5", 5),
        ("/api/v1/incidents?limit=1000", 100),
        ("/api/v1/incidents?limit=0", 1),
        ("/api/v1/incidents?limit=-3", 1),
    ] {
        let (status, _, body) = send(app.clone(), get(uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(json(&body).as_array().unwrap().len(), want, "{uri}");
    }
}

fn with_header(uri: &str, name: &str, value: &str) -> Request<Body> {
    let mut req = get(uri);
    req.headers_mut().insert(
        name.parse::<axum::http::HeaderName>().unwrap(),
        value.parse().unwrap(),
    );
    req
}

#[tokio::test]
async fn protect_read_api_needs_a_key_or_session() {
    let (state, _pool) = state_from(api_config(true)).await;
    let app = app(state);
    let api = [
        "/api/v1/status",
        "/api/v1/incidents",
        "/api/v1/incidents/1",
        "/api/metrics",
    ];
    let browser = [
        "/badge/web.svg",
        "/badge/web/uptime.svg",
        "/metrics/prometheus",
    ];

    for uri in api {
        let (status, headers, body) = send(app.clone(), get(uri)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(headers[CONTENT_TYPE], "application/json");
        assert_eq!(headers["www-authenticate"], "Bearer");
        assert_eq!(json(&body), serde_json::json!({"error": "unauthorized"}));
    }
    for uri in browser {
        let (status, headers, _) = send(app.clone(), get(uri)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(headers["location"], "/login");
    }

    // A wrong token is treated like none at all.
    for uri in api {
        let req = with_header(uri, "authorization", "Bearer not-the-token");
        assert_eq!(
            send(app.clone(), req).await.0,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
        let (status, _, _) = send(app.clone(), get(&format!("{uri}?token=nope"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    for uri in browser {
        let (status, _, _) = send(app.clone(), get(&format!("{uri}?token=nope"))).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{uri}");
    }

    let ok = |status: StatusCode, uri: &str| {
        // The incident does not exist, but the request got past the guard.
        let want = if uri == "/api/v1/incidents/1" {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::OK
        };
        assert_eq!(status, want, "{uri}");
    };
    for uri in api.iter().chain(browser.iter()) {
        let req = with_header(uri, "authorization", &format!("Bearer {API_TOKEN}"));
        ok(send(app.clone(), req).await.0, uri);
        let req = with_header(uri, "authorization", &format!("bearer {API_TOKEN}"));
        ok(send(app.clone(), req).await.0, uri);
        let sep = if uri.contains('?') { '&' } else { '?' };
        ok(
            send(app.clone(), get(&format!("{uri}{sep}token={API_TOKEN}")))
                .await
                .0,
            uri,
        );
    }

    let cookie = login(&app).await;
    for uri in api.iter().chain(browser.iter()) {
        let req = with_header(uri, "cookie", &cookie);
        ok(send(app.clone(), req).await.0, uri);
    }
}

#[tokio::test]
async fn public_api_ignores_keys() {
    let (state, _pool) = state_from(api_config(false)).await;
    let app = app(state);
    for uri in ["/api/v1/status", "/badge/web.svg", "/metrics/prometheus"] {
        assert_eq!(send(app.clone(), get(uri)).await.0, StatusCode::OK, "{uri}");
        let req = with_header(uri, "authorization", "Bearer wrong");
        assert_eq!(send(app.clone(), req).await.0, StatusCode::OK, "{uri}");
        let req = with_header(uri, "authorization", &format!("Bearer {API_TOKEN}"));
        assert_eq!(send(app.clone(), req).await.0, StatusCode::OK, "{uri}");
    }
}

async fn state_in_zone(tz: &str) -> (AppState, Pool) {
    let toml = format!("timezone = \"{tz}\"\n{}", config_toml(false, ""));
    state_from(Arc::new(config::parse_str(&toml).unwrap())).await
}

/// Post the maintenance form and return the window it created.
async fn plan(
    app: &Router,
    pool: &Pool,
    cookie: &str,
    fields: &str,
) -> dunlin::models::Maintenance {
    let mut req = post_form(
        "/maintenance",
        &format!("component=web&duration_minutes=30&note=upgrade{fields}"),
    );
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    db::all_maintenance(pool)
        .await
        .unwrap()
        .into_iter()
        .max_by_key(|m| m.id)
        .unwrap()
}

#[tokio::test]
async fn planned_maintenance_in_the_configured_zone() {
    use chrono::TimeZone;
    let (state, pool) = state_in_zone("Europe/Istanbul").await;
    let app = app(state);
    let cookie = login(&app).await;

    let tz = chrono_tz::Europe::Istanbul;
    let day = (chrono::Utc::now() + chrono::Duration::days(2))
        .with_timezone(&tz)
        .date_naive();
    let local = day.and_hms_opt(14, 30, 0).unwrap();
    let want = tz.from_local_datetime(&local).single().unwrap().timestamp();
    // Istanbul is UTC+3 all year.
    assert_eq!(want, local.and_utc().timestamp() - 3 * 3600);

    let field = format!("&starts_at={}", local.format("%Y-%m-%dT%H:%M"));
    let m = plan(&app, &pool, &cookie, &field).await;
    assert_eq!((m.starts_at, m.ends_at), (want, want + 1800));

    let mut req = get("/manage");
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (_, _, body) = send(app.clone(), req).await;
    assert!(body.contains("Starts at (Europe/Istanbul)"), "{body}");
    assert!(
        body.contains(r#"type="datetime-local" name="starts_at""#),
        "{body}"
    );
    assert!(body.contains(r#"<span class="d">planned</span>"#), "{body}");

    // The public page announces it but the component is not in maintenance yet.
    let (_, _, body) = send(app.clone(), get("/")).await;
    assert!(body.contains("Maintenance on Website starts"), "{body}");
    assert!(!body.contains("svc s-maintenance"), "{body}");
    let (_, _, badge) = send(app, get("/badge/web.svg")).await;
    assert!(badge.contains("Website: operational"), "{badge}");
}

#[tokio::test]
async fn planned_start_in_a_dst_gap_moves_to_when_the_clock_resumes() {
    use chrono::{Datelike, TimeZone};
    let (state, pool) = state_in_zone("Europe/Berlin").await;
    let app = app(state);
    let cookie = login(&app).await;

    let tz = chrono_tz::Europe::Berlin;
    let now = chrono::Utc::now();
    // The next spring-forward night: the last Sunday of March.
    let gap_day = [now.year(), now.year() + 1]
        .into_iter()
        .map(|y| {
            let mut d = chrono::NaiveDate::from_ymd_opt(y, 3, 31).unwrap();
            while d.weekday() != chrono::Weekday::Sun {
                d = d.pred_opt().unwrap();
            }
            d
        })
        .find(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc() > now)
        .unwrap();
    let in_gap = gap_day.and_hms_opt(2, 30, 0).unwrap();
    assert!(tz.from_local_datetime(&in_gap).single().is_none());
    let resumes = tz
        .from_local_datetime(&gap_day.and_hms_opt(3, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp();

    let field = format!("&starts_at={}", in_gap.format("%Y-%m-%dT%H:%M"));
    let m = plan(&app, &pool, &cookie, &field).await;
    assert_eq!(m.starts_at, resumes);
}

#[tokio::test]
async fn maintenance_start_defaults_clamps_and_rejects() {
    let (state, pool) = state_in_zone("Europe/Istanbul").await;
    let app = app(state);
    let cookie = login(&app).await;
    let year = 366 * 86_400;
    let fmt = |ts: i64| {
        chrono::DateTime::from_timestamp(ts, 0)
            .unwrap()
            .with_timezone(&chrono_tz::Europe::Istanbul)
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string()
    };
    let cases: Vec<(String, i64)> = vec![
        // Empty (or absent) starts now, as before.
        (String::new(), 0),
        ("&starts_at=".into(), 0),
        // The old offset field still works on its own...
        ("&starts_in_seconds=600".into(), 600),
        // ...but a filled-in start wins over it.
        (
            format!(
                "&starts_in_seconds=600&starts_at={}",
                fmt(dunlin::now_ts() + 7200)
            ),
            7200,
        ),
        // A start in the past begins now; one past a year is pulled in.
        (format!("&starts_at={}", fmt(dunlin::now_ts() - 86_400)), 0),
        (
            format!("&starts_at={}", fmt(dunlin::now_ts() + 3 * year)),
            year,
        ),
    ];
    for (fields, offset) in cases {
        let before = dunlin::now_ts();
        let m = plan(&app, &pool, &cookie, &fields).await;
        let after = dunlin::now_ts();
        assert!(
            m.starts_at >= before + offset - 1 && m.starts_at <= after + offset,
            "{fields}: {} not {offset} from now",
            m.starts_at - before
        );
    }

    let windows = db::all_maintenance(&pool).await.unwrap().len();
    let mut req = post_form(
        "/maintenance",
        "component=web&duration_minutes=30&note=x&starts_at=next+tuesday",
    );
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, body) = send(app, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("not valid"), "{body}");
    assert_eq!(db::all_maintenance(&pool).await.unwrap().len(), windows);
}
