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
    let hash = dunlin::auth::hash_password(PASSWORD).unwrap();
    let toml = format!(
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
"#
    );
    Arc::new(config::parse_str(&toml).unwrap())
}

async fn state(protect_read: bool) -> (AppState, Pool) {
    let pool = db::connect_memory().await.unwrap();
    let (state, _tx) = AppState::new(pool.clone(), test_config(protect_read));
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

    let cookie = login(&app).await;
    let mut req = get("/");
    req.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, body) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Website"));
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

    // The component is shown as under maintenance for logged-in operators.
    let mut page = get("/");
    page.headers_mut().insert(COOKIE, cookie.parse().unwrap());
    let (status, _, body) = send(app.clone(), page).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Under maintenance"), "{body}");
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
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
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
