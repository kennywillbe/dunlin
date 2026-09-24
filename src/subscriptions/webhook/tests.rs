use std::str::FromStr;
use std::sync::Mutex;

use axum::http::StatusCode;
use axum::response::IntoResponse;

use super::*;
use crate::subscriptions::dispatch::{dispatch_due, Channels, Pacer};

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn only_public_addresses_pass() {
    let blocked = [
        // IPv4
        "0.0.0.0",
        "0.1.2.3",
        "10.0.0.1",
        "10.255.255.255",
        "100.64.0.1",
        "100.127.255.254",
        "127.0.0.1",
        "127.255.255.254",
        "169.254.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "172.31.255.255",
        "192.0.0.1",
        "192.0.2.1",
        "192.88.99.1",
        "192.168.1.1",
        "198.18.0.1",
        "198.19.255.255",
        "198.51.100.7",
        "203.0.113.9",
        "224.0.0.1",
        "239.255.255.255",
        "240.0.0.1",
        "255.255.255.255",
        // IPv6
        "::",
        "::1",
        "::127.0.0.1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::ffff:169.254.169.254",
        "64:ff9b::7f00:1",
        "64:ff9b::a00:1",
        "64:ff9b:1::1",
        "100::1",
        "2001::1",
        "2001:2::1",
        "2001:db8::1",
        "2002:7f00:1::1",
        "2002:a00:1::1",
        "3fff::1",
        "fc00::1",
        "fd12:3456::1",
        "fe80::1",
        "fec0::1",
        "ff02::1",
        "ff0e::1",
    ];
    for a in blocked {
        assert!(!is_public(ip(a)), "{a} should be blocked");
    }
    let public = [
        "1.1.1.1",
        "8.8.8.8",
        "93.184.216.34",
        "100.63.255.255",
        "100.128.0.1",
        "172.15.255.255",
        "172.32.0.1",
        "192.0.3.1",
        "192.169.0.1",
        "198.17.255.255",
        "198.20.0.1",
        "223.255.255.255",
        "::ffff:8.8.8.8",
        "64:ff9b::808:808",
        "2002:808:808::1",
        "2606:4700:4700::1111",
        "2a00:1450:4001:80b::200e",
        "2001:4860:4860::8888",
    ];
    for a in public {
        assert!(is_public(ip(a)), "{a} should be allowed");
    }
}

#[test]
fn urls_are_checked_and_normalised() {
    let ok = |u: &str| check_url(u, true).unwrap().to_string();
    assert_eq!(
        ok("  HTTPS://Hooks.Slack.COM/services/T0/B0/Xy?x=1  "),
        "https://hooks.slack.com/services/T0/B0/Xy?x=1"
    );
    assert_eq!(
        ok("https://example.org:443/hook"),
        "https://example.org/hook"
    );
    assert_eq!(
        ok("https://93.184.216.34/hook"),
        "https://93.184.216.34/hook"
    );

    let refused = [
        "http://example.org/hook",
        "ftp://example.org/hook",
        "example.org/hook",
        "https://user:pw@example.org/hook",
        "https://user@example.org/hook",
        "https://example.org:8443/hook",
        // IP literals never reach the resolver, so they are checked here,
        // in every spelling the URL parser accepts.
        "https://127.0.0.1/hook",
        "https://2130706433/hook",
        "https://0x7f.1/hook",
        "https://[::1]/hook",
        "https://[::ffff:127.0.0.1]/hook",
        "https://169.254.169.254/latest/meta-data",
        "https://10.0.0.1/hook",
        "https://[fd00::1]/hook",
        "https://[fe80::1]/hook",
    ];
    for u in refused {
        assert!(check_url(u, true).is_err(), "{u}");
    }
    let long = format!("https://example.org/{}", "a".repeat(MAX_URL_LEN));
    assert!(check_url(&long, true).is_err());
    // Tests post to local mocks over http; only the test constructor allows it.
    assert!(check_url("http://127.0.0.1:9/hook", false).is_ok());
}

#[test]
fn formats_follow_the_host() {
    let f = |u: &str| detect(&Url::parse(u).unwrap());
    assert_eq!(f("https://hooks.slack.com/services/T0/B0/x"), Format::Slack);
    assert_eq!(f("https://discord.com/api/webhooks/1/x"), Format::Discord);
    assert_eq!(
        f("https://discordapp.com/api/webhooks/1/x"),
        Format::Discord
    );
    assert_eq!(f("https://discord.com/other/1/x"), Format::Json);
    assert_eq!(f("https://slack.com/services/x"), Format::Json);
    assert_eq!(f("https://example.org/hook"), Format::Json);
}

fn incident() -> IncidentEvent {
    IncidentEvent {
        incident_id: 7,
        title: "API <down> & out".into(),
        component: "api".into(),
        component_name: "Public API".into(),
        impact: "partial_outage".into(),
        state: "investigating".into(),
        message: "@everyone it broke".into(),
        url: Some("https://status.example.org/incidents/7".into()),
    }
}

fn maintenance() -> MaintenanceEvent {
    MaintenanceEvent {
        maintenance_id: 3,
        component: String::new(),
        component_name: "All components".into(),
        note: "Upgrade".into(),
        starts_at: 1000,
        ends_at: 2000,
        url: Some("https://status.example.org/".into()),
    }
}

fn delivery(message: Message) -> Delivery {
    Delivery {
        address: "https://example.org/hook".into(),
        site_title: "Acme".into(),
        timezone: chrono_tz::UTC,
        site_url: Some("https://status.example.org".into()),
        message,
    }
}

fn event(e: SubscriberEvent) -> Delivery {
    delivery(Message::Event {
        event: e,
        unsubscribe_url: "https://status.example.org/unsubscribe/tok".into(),
    })
}

#[test]
fn slack_payload_shape() {
    let v = slack_payload(&event(SubscriberEvent::IncidentOpened(incident())), 42);
    assert_eq!(v["text"], "[Acme] Incident: API &lt;down&gt; &amp; out");
    let a = &v["attachments"][0];
    assert_eq!(a["color"], "#e2461f");
    assert_eq!(a["title"], v["text"]);
    assert_eq!(a["title_link"], "https://status.example.org/incidents/7");
    assert_eq!(a["ts"], 42);
    let text = a["text"].as_str().unwrap();
    assert!(text.contains("@everyone it broke"), "{text}");
    assert!(
        text.ends_with("Unsubscribe: https://status.example.org/unsubscribe/tok"),
        "{text}"
    );
    let resolved = slack_payload(&event(SubscriberEvent::IncidentResolved(incident())), 42);
    assert_eq!(resolved["attachments"][0]["color"], "#2e9e5b");
}

#[test]
fn discord_payload_shape() {
    let v = discord_payload(
        &event(SubscriberEvent::MaintenanceStarted(maintenance())),
        0,
    );
    assert_eq!(v["allowed_mentions"], serde_json::json!({ "parse": [] }));
    let e = &v["embeds"][0];
    assert_eq!(e["title"], "[Acme] Maintenance started: All components");
    assert_eq!(e["color"], 0x2f64d8);
    assert_eq!(e["url"], "https://status.example.org/");
    assert_eq!(e["timestamp"], "1970-01-01T00:00:00Z");
    assert!(e["description"].as_str().unwrap().contains("Unsubscribe: "));
}

#[test]
fn json_payload_schema() {
    let v = json_payload(&event(SubscriberEvent::IncidentUpdated(incident())), 99);
    assert_eq!(
        v,
        serde_json::json!({
            "event": "incident_updated",
            "meta": { "unsubscribe": "https://status.example.org/unsubscribe/tok", "generated_at": 99 },
            "page": { "title": "Acme", "url": "https://status.example.org" },
            "incident": {
                "id": 7,
                "title": "API <down> & out",
                "component": { "id": "api", "name": "Public API" },
                "impact": "partial_outage",
                "state": "investigating",
                "message": "@everyone it broke",
                "url": "https://status.example.org/incidents/7",
            },
        })
    );
    let v = json_payload(
        &event(SubscriberEvent::MaintenanceCancelled(maintenance())),
        99,
    );
    assert_eq!(v["event"], "maintenance_cancelled");
    assert_eq!(
        v["maintenance"],
        serde_json::json!({
            "id": 3,
            "component": { "id": "", "name": "All components" },
            "note": "Upgrade",
            "starts_at": 1000,
            "ends_at": 2000,
            "url": "https://status.example.org/",
        })
    );
    assert!(v.get("incident").is_none());
    let v = json_payload(
        &delivery(Message::Confirm {
            confirm_url: "https://status.example.org/subscribe/confirm/c".into(),
            unsubscribe_url: "https://status.example.org/unsubscribe/tok".into(),
        }),
        99,
    );
    assert_eq!(v["event"], "confirm");
    assert_eq!(
        v["confirm"]["url"],
        "https://status.example.org/subscribe/confirm/c"
    );
}

/// A resolver whose "DNS" answers every name with `answer`, noting names.
fn stub(answer: &'static [&'static str], asked: Arc<Mutex<Vec<String>>>) -> PublicResolver {
    PublicResolver::with_lookup(Arc::new(move |host: String| {
        asked.lock().unwrap().push(host);
        Box::pin(async move { Ok(answer.iter().map(|a| a.parse().unwrap()).collect()) })
    }))
}

#[tokio::test]
async fn the_resolver_drops_private_answers() {
    let asked = Arc::default();
    let r = stub(&["127.0.0.1", "10.0.0.5"], Arc::clone(&asked));
    let err = r
        .resolve(Name::from_str("evil.example").unwrap())
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("no public address"), "{err}");

    let r = stub(&["127.0.0.1", "93.184.216.34", "::1"], Arc::clone(&asked));
    let addrs: Vec<SocketAddr> = r
        .resolve(Name::from_str("mixed.example").unwrap())
        .await
        .unwrap()
        .collect();
    assert_eq!(addrs, [SocketAddr::new(ip("93.184.216.34"), 0)]);
}

#[tokio::test]
async fn the_guarded_client_refuses_a_name_pointing_home() {
    let asked: Arc<Mutex<Vec<String>>> = Arc::default();
    let client = guarded_client(stub(&["127.0.0.1"], asked.clone())).unwrap();
    let err = client
        .post("https://evil.example/hook")
        .send()
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("no public address"), "{err:?}");
    assert_eq!(*asked.lock().unwrap(), ["evil.example"]);
    // And it will not speak plain http at all.
    assert!(client.post("http://example.org/hook").send().await.is_err());
}

#[tokio::test]
async fn send_refuses_a_private_literal_even_if_stored() {
    let channel = WebhookChannel::new().unwrap();
    let mut d = event(SubscriberEvent::IncidentOpened(incident()));
    d.address = "https://127.0.0.1/hook".into();
    let err = channel.send(&d).await.unwrap_err();
    assert!(err.downcast_ref::<PermanentFailure>().is_some(), "{err:#}");
}

type Hits = Arc<Mutex<Vec<serde_json::Value>>>;

/// A mock endpoint answering `status`, recording JSON bodies.
async fn mock(status: StatusCode) -> (String, Hits) {
    let hits: Hits = Arc::default();
    let seen = hits.clone();
    let app = axum::Router::new().fallback(move |body: String| {
        let seen = seen.clone();
        async move {
            seen.lock()
                .unwrap()
                .push(serde_json::from_str(&body).unwrap());
            (status, "a body that is never read").into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/hook"), hits)
}

#[tokio::test]
async fn statuses_map_to_retry_count_or_give_up() {
    let channel = WebhookChannel::local(reqwest::Client::new());
    let send = |url: String| {
        let mut d = event(SubscriberEvent::IncidentOpened(incident()));
        d.address = url;
        d
    };
    let (url, _) = mock(StatusCode::NO_CONTENT).await;
    channel.send(&send(url)).await.unwrap();
    let (url, _) = mock(StatusCode::GONE).await;
    let e = channel.send(&send(url)).await.unwrap_err();
    assert!(e.downcast_ref::<PermanentFailure>().is_some());
    let (url, _) = mock(StatusCode::INTERNAL_SERVER_ERROR).await;
    let e = channel.send(&send(url)).await.unwrap_err();
    assert!(e.downcast_ref::<CountedFailure>().is_some());
    let (url, _) = mock(StatusCode::TOO_MANY_REQUESTS).await;
    let e = channel.send(&send(url)).await.unwrap_err();
    assert!(e.downcast_ref::<CountedFailure>().is_none());
    assert!(e.downcast_ref::<PermanentFailure>().is_none());
}

#[tokio::test]
async fn confirm_through_the_url_itself() {
    let pool = crate::db::connect_memory().await.unwrap();
    let mut cfg = crate::config::Config {
        public_url: Some("https://status.example.org".into()),
        ..Default::default()
    };
    cfg.subscriptions.enabled = true;
    let (url, hits) = mock(StatusCode::OK).await;
    let channel = Arc::new(WebhookChannel::local(reqwest::Client::new()));
    let address = channel.normalize_address(&url).unwrap();
    let req = crate::subscriptions::SignUp {
        channel: "webhook".into(),
        address,
        all_components: true,
        components: vec![],
        maintenance: false,
    };
    crate::subscriptions::sign_up(&pool, &req, 0).await.unwrap();
    let channels = Channels::new(vec![channel]);
    dispatch_due(&pool, &cfg, &channels, &mut Pacer::default(), 1)
        .await
        .unwrap();

    let body = hits.lock().unwrap()[0].clone();
    assert_eq!(body["event"], "confirm");
    let token = body["confirm"]["url"]
        .as_str()
        .unwrap()
        .strip_prefix("https://status.example.org/subscribe/confirm/")
        .unwrap()
        .to_string();
    assert_eq!(
        crate::subscriptions::confirm(&pool, &token, 2)
            .await
            .unwrap(),
        crate::subscriptions::ConfirmLink::Valid
    );
    crate::subscriptions::publish(&pool, &cfg, &SubscriberEvent::IncidentOpened(incident()), 3)
        .await
        .unwrap();
    dispatch_due(&pool, &cfg, &channels, &mut Pacer::default(), 3)
        .await
        .unwrap();
    let body = hits.lock().unwrap()[1].clone();
    assert_eq!(body["event"], "incident_opened");
    assert_eq!(body["incident"]["id"], 7);
    let unsubscribe = body["meta"]["unsubscribe"].as_str().unwrap();
    let token = unsubscribe.rsplit('/').next().unwrap();
    assert!(crate::subscriptions::check_unsubscribe(&pool, token)
        .await
        .unwrap());
}

#[tokio::test]
async fn built_from_config_when_enabled() {
    use crate::subscriptions::dispatch::build_channels;
    let mut cfg = crate::config::Config::default();
    cfg.subscriptions.enabled = true;
    cfg.subscriptions.webhook = Some(crate::config::SubscriberWebhookConfig { enabled: true });
    let names: Vec<&str> = build_channels(&cfg).iter().map(|c| c.name()).collect();
    assert_eq!(names, ["webhook"]);
    cfg.subscriptions.webhook = Some(Default::default());
    assert!(build_channels(&cfg).is_empty());
}
