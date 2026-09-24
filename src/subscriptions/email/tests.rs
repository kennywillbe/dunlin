use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::subscriptions::dispatch::{dispatch_due, Channels, Pacer};

/// One mail as the fake server received it.
#[derive(Debug, Clone, Default)]
struct Received {
    from: String,
    to: Vec<String>,
    data: String,
}

/// A tiny SMTP server: EHLO/HELO, MAIL, RCPT, DATA, RSET, NOOP, QUIT, no
/// TLS or AUTH. `rcpt_reply` is what RCPT TO gets, to act out a refused
/// mailbox.
async fn fake_smtp(rcpt_reply: &'static str) -> (u16, Arc<Mutex<Vec<Received>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let got: Arc<Mutex<Vec<Received>>> = Arc::default();
    let store = got.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let store = store.clone();
            tokio::spawn(async move {
                let (read, mut write) = sock.into_split();
                let mut lines = BufReader::new(read).lines();
                write.write_all(b"220 fake ESMTP\r\n").await.unwrap();
                let mut mail = Received::default();
                while let Ok(Some(line)) = lines.next_line().await {
                    let upper = line.to_ascii_uppercase();
                    let reply: &str = if upper.starts_with("EHLO") {
                        "250-fake\r\n250 8BITMIME\r\n"
                    } else if upper.starts_with("HELO") || upper.starts_with("NOOP") {
                        "250 OK\r\n"
                    } else if upper.starts_with("RSET") {
                        mail = Received::default();
                        "250 OK\r\n"
                    } else if upper.starts_with("MAIL FROM:") {
                        mail.from = line[10..].trim().to_string();
                        "250 OK\r\n"
                    } else if upper.starts_with("RCPT TO:") {
                        mail.to.push(line[8..].trim().to_string());
                        rcpt_reply
                    } else if upper == "DATA" {
                        write.write_all(b"354 go ahead\r\n").await.unwrap();
                        let mut data = String::new();
                        while let Ok(Some(l)) = lines.next_line().await {
                            if l == "." {
                                break;
                            }
                            data.push_str(&l);
                            data.push('\n');
                        }
                        mail.data = data;
                        store.lock().unwrap().push(std::mem::take(&mut mail));
                        "250 queued\r\n"
                    } else if upper.starts_with("QUIT") {
                        write.write_all(b"221 bye\r\n").await.unwrap();
                        break;
                    } else {
                        "500 what\r\n"
                    };
                    write.write_all(reply.as_bytes()).await.unwrap();
                }
            });
        }
    });
    (port, got)
}

fn channel(port: u16) -> EmailChannel {
    // Plaintext to a local fake server; production builds TLS transports.
    let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("127.0.0.1")
        .port(port)
        .build();
    EmailChannel::with_transport(transport, "Status <status@example.org>".parse().unwrap())
}

/// Headers unfolded into (lowercased name, value), and the decoded body.
fn parse(data: &str) -> (Vec<(String, String)>, String) {
    let (head, body) = data.split_once("\n\n").unwrap();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in head.lines() {
        if line.starts_with([' ', '\t']) {
            headers.last_mut().unwrap().1.push_str(line.trim_start());
        } else {
            let (k, v) = line.split_once(':').unwrap();
            headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let encoding = headers
        .iter()
        .find(|h| h.0 == "content-transfer-encoding")
        .map(|h| h.1.to_ascii_lowercase());
    let body = match encoding.as_deref() {
        Some("quoted-printable") => decode_qp(body),
        Some("base64") => base64_decode(&body.replace('\n', "")),
        _ => body.to_string(),
    };
    (headers, body)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
    &headers
        .iter()
        .find(|h| h.0 == name)
        .unwrap_or_else(|| panic!("no {name}: {headers:?}"))
        .1
}

fn decode_qp(s: &str) -> String {
    let joined = s.replace("=\n", "");
    let bytes = joined.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(&joined[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn base64_decode(s: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes().filter(|c| *c != b'=') {
        buf = (buf << 6) | ALPHABET.iter().position(|a| *a == c).unwrap() as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn event_delivery(event: SubscriberEvent) -> Delivery {
    Delivery {
        address: "jane@example.org".into(),
        site_title: "Acme Status".into(),
        timezone: chrono_tz::Europe::Istanbul,
        message: Message::Event {
            event,
            unsubscribe_url: "https://status.example.org/unsubscribe/abc123".into(),
        },
    }
}

fn opened() -> SubscriberEvent {
    SubscriberEvent::IncidentOpened(IncidentEvent {
        incident_id: 7,
        title: "API is down".into(),
        component: "api".into(),
        component_name: "Public API".into(),
        impact: "major_outage".into(),
        state: "investigating".into(),
        message: "Connection refused.".into(),
        url: Some("https://status.example.org/incidents/7".into()),
    })
}

fn window() -> MaintenanceEvent {
    MaintenanceEvent {
        maintenance_id: 3,
        component: "db".into(),
        component_name: "Database".into(),
        note: "Upgrading to v16".into(),
        // 2026-09-24 11:30 and 12:30 UTC.
        starts_at: 1_790_249_400,
        ends_at: 1_790_253_000,
        url: Some("https://status.example.org/".into()),
    }
}

#[tokio::test]
async fn sends_an_incident_mail_with_one_click_unsubscribe() {
    let (port, got) = fake_smtp("250 OK\r\n").await;
    channel(port).send(&event_delivery(opened())).await.unwrap();

    let mails = got.lock().unwrap().clone();
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].from, "<status@example.org>");
    assert_eq!(mails[0].to, ["<jane@example.org>"]);
    let (headers, body) = parse(&mails[0].data);
    assert_eq!(
        header(&headers, "subject"),
        "[Acme Status] Incident: API is down"
    );
    assert_eq!(header(&headers, "from"), "Status <status@example.org>");
    assert_eq!(header(&headers, "to"), "jane@example.org");
    assert_eq!(
        header(&headers, "list-unsubscribe"),
        "<https://status.example.org/unsubscribe/abc123>"
    );
    assert_eq!(
        header(&headers, "list-unsubscribe-post"),
        "List-Unsubscribe=One-Click"
    );
    assert!(header(&headers, "content-type").starts_with("text/plain"));
    assert!(
        body.contains("API is down\nPublic API · Major outage · Investigating\n"),
        "{body}"
    );
    assert!(body.contains("Connection refused."), "{body}");
    assert!(
        body.contains("Details: https://status.example.org/incidents/7"),
        "{body}"
    );
    assert!(
        body.contains("Unsubscribe: https://status.example.org/unsubscribe/abc123"),
        "{body}"
    );
}

#[tokio::test]
async fn confirmation_mail_carries_the_link() {
    let (port, got) = fake_smtp("250 OK\r\n").await;
    let d = Delivery {
        message: Message::Confirm {
            confirm_url: "https://status.example.org/subscribe/confirm/tok".into(),
            unsubscribe_url: "https://status.example.org/unsubscribe/abc123".into(),
        },
        ..event_delivery(opened())
    };
    channel(port).send(&d).await.unwrap();
    let (headers, body) = parse(&got.lock().unwrap()[0].data);
    assert_eq!(
        header(&headers, "subject"),
        "Confirm your subscription to Acme Status"
    );
    assert!(
        body.contains("Confirm here: https://status.example.org/subscribe/confirm/tok"),
        "{body}"
    );
    assert!(body.contains("works for 24 hours"), "{body}");
    assert!(header(&headers, "list-unsubscribe").contains("/unsubscribe/abc123"));
}

#[test]
fn subjects_and_maintenance_times() {
    let subject = |event| compose(&event_delivery(event)).0;
    let e = match opened() {
        SubscriberEvent::IncidentOpened(e) => e,
        _ => unreachable!(),
    };
    assert_eq!(
        subject(SubscriberEvent::IncidentUpdated(e.clone())),
        "[Acme Status] Update: API is down"
    );
    assert_eq!(
        subject(SubscriberEvent::IncidentResolved(e)),
        "[Acme Status] Resolved: API is down"
    );
    assert_eq!(
        subject(SubscriberEvent::MaintenanceScheduled(window())),
        "[Acme Status] Maintenance planned: Database"
    );
    assert_eq!(
        subject(SubscriberEvent::MaintenanceStarted(window())),
        "[Acme Status] Maintenance started: Database"
    );
    assert_eq!(
        subject(SubscriberEvent::MaintenanceCompleted(window())),
        "[Acme Status] Maintenance completed: Database"
    );
    assert_eq!(
        subject(SubscriberEvent::MaintenanceCancelled(window())),
        "[Acme Status] Maintenance cancelled: Database"
    );

    let (_, body) = compose(&event_delivery(SubscriberEvent::MaintenanceScheduled(
        window(),
    )));
    // Istanbul is UTC+3.
    assert_eq!(
        body,
        "Maintenance is planned on Database.\n\
         From Thu 24 Sep 2026, 14:30 (Europe/Istanbul)\n\
         to   Thu 24 Sep 2026, 15:30 (Europe/Istanbul)\n\
         \nUpgrading to v16\n\
         \nStatus page: https://status.example.org/\n"
    );
}

#[tokio::test]
async fn a_refused_mailbox_is_permanent_but_a_busy_server_is_not() {
    let (port, _) = fake_smtp("550 5.1.1 no such user\r\n").await;
    let err = channel(port)
        .send(&event_delivery(opened()))
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<PermanentFailure>().is_some(), "{err:#}");

    let (port, _) = fake_smtp("451 4.3.0 try later\r\n").await;
    let err = channel(port)
        .send(&event_delivery(opened()))
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<PermanentFailure>().is_none(), "{err:#}");

    // Nothing listening: a connection error, retried.
    let free = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let err = channel(port)
        .send(&event_delivery(opened()))
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<PermanentFailure>().is_none(), "{err:#}");
}

#[tokio::test]
async fn the_dispatcher_gives_a_dead_address_up_at_once() {
    use sqlx::Row;
    let pool = crate::db::connect_memory().await.unwrap();
    let mut cfg = crate::config::Config {
        public_url: Some("https://status.example.org".into()),
        ..Default::default()
    };
    cfg.subscriptions.enabled = true;
    let req = crate::subscriptions::SignUp {
        channel: "email".into(),
        address: "gone@example.org".into(),
        all_components: true,
        components: vec![],
        maintenance: false,
    };
    crate::subscriptions::sign_up(&pool, &req, 0).await.unwrap();
    let (port, _) = fake_smtp("550 5.1.1 no such user\r\n").await;
    let channels = Channels::new(vec![Arc::new(channel(port))]);
    let stats = dispatch_due(&pool, &cfg, &channels, &mut Pacer::default(), 1)
        .await
        .unwrap();
    assert_eq!(stats.given_up, 1);
    let r = sqlx::query("SELECT attempts, failed_at, last_error FROM outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(r.get::<i64, _>("attempts"), 1);
    assert_eq!(r.get::<Option<i64>, _>("failed_at"), Some(1));
    assert!(r.get::<String, _>("last_error").contains("no such user"));
}

#[test]
fn addresses_are_normalised() {
    assert_eq!(
        normalize("  Jane.Doe@Example.ORG ").unwrap(),
        "Jane.Doe@example.org"
    );
    assert_eq!(
        normalize("a+tag@sub.example.org").unwrap(),
        "a+tag@sub.example.org"
    );
    for bad in [
        "",
        "jane",
        "@example.org",
        "jane@",
        "jane@localhost",
        "two words@example.org",
        "jane@exa mple.org",
    ] {
        assert!(normalize(bad).is_err(), "{bad:?}");
    }
    assert!(normalize(&format!("{}@example.org", "a".repeat(250))).is_err());
}

// The pooled transport needs a runtime, as it has in the server.
#[tokio::test]
async fn builds_tls_transports_from_config() {
    let cfg = |tls| EmailConfig {
        enabled: true,
        host: "smtp.example.org".into(),
        port: None,
        tls,
        username: Some("u".into()),
        password: Some("p".into()),
        from: "Status <status@example.org>".into(),
    };
    assert!(EmailChannel::new(&cfg(SmtpTls::Starttls)).is_ok());
    assert!(EmailChannel::new(&cfg(SmtpTls::Implicit)).is_ok());
}

#[tokio::test]
async fn the_channel_is_on_only_when_both_tables_say_so() {
    use crate::subscriptions::dispatch::build_channels;
    let email = |enabled| EmailConfig {
        enabled,
        host: "smtp.example.org".into(),
        port: None,
        tls: SmtpTls::Starttls,
        username: None,
        password: None,
        from: "status@example.org".into(),
    };
    let names = |subs: bool, mail: Option<bool>| {
        let mut cfg = crate::config::Config::default();
        cfg.subscriptions.enabled = subs;
        cfg.subscriptions.email = mail.map(email);
        build_channels(&cfg)
            .iter()
            .map(|c| c.name())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(true, Some(true)), ["email"]);
    assert!(names(true, Some(false)).is_empty());
    assert!(names(true, None).is_empty());
    assert!(names(false, Some(true)).is_empty());
}
