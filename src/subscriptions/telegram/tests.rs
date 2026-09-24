use std::collections::VecDeque;
use std::sync::Mutex;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{json, Value};

use super::*;
use crate::subscriptions::dispatch::{dispatch_due, Channels, DispatchStats, Pacer};
use crate::subscriptions::{self as subs, IncidentEvent, SignUp, SubscriberEvent};

const TOKEN: &str = "123456:SECRETtoken";

/// A scripted Bot API: `getUpdates` answers come from `updates`, and
/// `sendMessage` answers from `replies` (ok when empty).
#[derive(Default)]
struct Bot {
    updates: Mutex<VecDeque<(u16, Value)>>,
    replies: Mutex<VecDeque<(u16, Value)>>,
    sent: Mutex<Vec<Value>>,
    polls: Mutex<Vec<Value>>,
    paths: Mutex<Vec<String>>,
}

async fn mock() -> (String, Arc<Bot>) {
    let bot = Arc::new(Bot::default());
    let state = bot.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, body: String| {
        let bot = state.clone();
        async move {
            bot.paths.lock().unwrap().push(uri.path().to_string());
            let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let method = uri.path().rsplit('/').next().unwrap_or("").to_string();
            let ok = |result: Value| (200u16, json!({ "ok": true, "result": result }));
            let (status, answer) = match method.as_str() {
                "getUpdates" => {
                    bot.polls.lock().unwrap().push(body);
                    bot.updates
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(ok(json!([])))
                }
                "sendMessage" => {
                    bot.sent.lock().unwrap().push(body);
                    bot.replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(ok(json!({})))
                }
                "getMe" => ok(json!({ "username": "acme_status_bot" })),
                _ => (404, json!({ "ok": false, "description": "Not Found" })),
            };
            (StatusCode::from_u16(status).unwrap(), answer.to_string()).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), bot)
}

fn api(base: &str) -> BotApi {
    BotApi::new(reqwest::Client::new(), base.to_string(), TOKEN.to_string())
}

fn cfg() -> Config {
    let toml = format!(
        "public_url = \"https://status.example.org\"\n{}\n\
         [[components]]\nid = \"web\"\nname = \"Website\"\n\
         [[components]]\nid = \"db\"\nname = \"Data <&> base\"\n\
         [subscriptions]\nenabled = true\n",
        crate::config::tests::base_with_real_hash()
    );
    crate::config::parse_str(&toml).unwrap()
}

fn signup(components: &[&str], maintenance: bool) -> SignUp {
    SignUp {
        channel: "telegram".into(),
        address: String::new(),
        all_components: components.is_empty(),
        components: components.iter().map(|c| c.to_string()).collect(),
        maintenance,
    }
}

fn update(id: i64, chat: i64, text: &str) -> Value {
    json!({ "update_id": id, "message": { "message_id": 1, "chat": { "id": chat, "type": "private" }, "text": text } })
}

fn batch(updates: Vec<Value>) -> (u16, Value) {
    (200, json!({ "ok": true, "result": updates }))
}

fn texts(bot: &Bot) -> Vec<(String, String)> {
    bot.sent
        .lock()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b["chat_id"].as_str().unwrap().to_string(),
                b["text"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn start_links_the_chat_and_the_offset_survives_a_restart() {
    let pool = crate::db::connect_memory().await.unwrap();
    let cfg = cfg();
    let token = subs::sign_up_by_link(&pool, &signup(&["db"], true), crate::now_ts())
        .await
        .unwrap();
    assert_eq!(token.len(), 64);
    let listed = subs::list(&pool).await.unwrap();
    assert_eq!(subs::mask("telegram", &listed[0].address), "not linked yet");

    let (base, bot) = mock().await;
    bot.updates
        .lock()
        .unwrap()
        .push_back(batch(vec![update(5, 42, &format!("/start {token}"))]));
    assert_eq!(poll_once(&pool, &cfg, &api(&base), 0).await.unwrap(), 1);

    let poll = bot.polls.lock().unwrap()[0].clone();
    assert_eq!(poll["offset"], 0);
    assert_eq!(poll["allowed_updates"], json!(["message"]));
    let sent = texts(&bot);
    assert_eq!(sent[0].0, "42");
    assert_eq!(
        sent[0].1,
        "Subscribed to updates from <b>Status</b> about Data &lt;&amp;&gt; base, and maintenance.\nSend /stop to unsubscribe."
    );
    let s = &subs::list(&pool).await.unwrap()[0];
    assert_eq!((s.address.as_str(), s.status.as_str()), ("42", "active"));
    assert_eq!(s.components, ["db"]);
    // The token is spent.
    bot.updates
        .lock()
        .unwrap()
        .push_back(batch(vec![update(6, 43, &format!("/start {token}"))]));
    poll_once(&pool, &cfg, &api(&base), 0).await.unwrap();
    assert!(texts(&bot)[1].1.contains("not valid or was already used"));

    // A new process picks up after the last update it handled.
    poll_once(&pool, &cfg, &api(&base), 0).await.unwrap();
    assert_eq!(bot.polls.lock().unwrap()[2]["offset"], 7);
}

#[tokio::test]
async fn start_with_a_bad_expired_or_no_payload() {
    let pool = crate::db::connect_memory().await.unwrap();
    let cfg = cfg();
    let old = subs::sign_up_by_link(&pool, &signup(&[], false), crate::now_ts() - 25 * 3600)
        .await
        .unwrap();
    let (base, bot) = mock().await;
    bot.updates.lock().unwrap().push_back(batch(vec![
        update(1, 7, &format!("/start {old}")),
        update(2, 7, "/start nonsense"),
        update(3, 7, &format!("/start {}", "a".repeat(64))),
        update(4, 7, "/start"),
        update(5, -100123, "/start@acme_status_bot"),
        update(6, 7, "hello <b>there</b>"),
        json!({ "update_id": 7, "edited_message": { "chat": { "id": 7 }, "text": "/stop" } }),
    ]));
    poll_once(&pool, &cfg, &api(&base), 0).await.unwrap();
    let sent = texts(&bot);
    assert_eq!(sent.len(), 5, "{sent:?}");
    assert!(
        sent[0].1.starts_with("This link has expired"),
        "{}",
        sent[0].1
    );
    assert!(sent[1].1.starts_with("This link is not valid"));
    assert!(sent[2].1.starts_with("This link is not valid"));
    assert_eq!(
        sent[3].1,
        "To get updates from <b>Status</b>, choose what to follow at https://status.example.org/subscribe"
    );
    // Group chats work too.
    assert_eq!(sent[4].0, "-100123");
    // Nothing the sender typed comes back.
    assert!(sent
        .iter()
        .all(|(_, t)| !t.contains("hello") && !t.contains("nonsense")));
    assert_eq!(subs::list(&pool).await.unwrap()[0].status, "pending");
}

#[tokio::test]
async fn stop_and_a_second_start_from_the_same_chat() {
    let pool = crate::db::connect_memory().await.unwrap();
    let cfg = cfg();
    let first = subs::sign_up_by_link(&pool, &signup(&[], false), crate::now_ts())
        .await
        .unwrap();
    let second = subs::sign_up_by_link(&pool, &signup(&["web"], true), crate::now_ts())
        .await
        .unwrap();
    let (base, bot) = mock().await;
    bot.updates.lock().unwrap().push_back(batch(vec![
        update(1, 42, &format!("/start {first}")),
        update(2, 42, &format!("/start {second}")),
    ]));
    poll_once(&pool, &cfg, &api(&base), 0).await.unwrap();
    // One row for the chat, with the newer choices.
    let listed = subs::list(&pool).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].all_components, listed[0].maintenance),
        (false, true)
    );
    assert_eq!(listed[0].components, ["web"]);

    bot.updates.lock().unwrap().push_back(batch(vec![
        update(3, 42, "/stop"),
        update(4, 42, "/unsubscribe"),
    ]));
    poll_once(&pool, &cfg, &api(&base), 0).await.unwrap();
    let sent = texts(&bot);
    assert_eq!(sent[2].1, "Unsubscribed. This chat gets no more updates.");
    assert_eq!(sent[3].1, "This chat is not subscribed.");
    assert!(subs::list(&pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_conflict_is_reported_and_waited_out() {
    let pool = crate::db::connect_memory().await.unwrap();
    let (base, bot) = mock().await;
    bot.updates.lock().unwrap().push_back((
        409,
        json!({ "ok": false, "error_code": 409, "description": "Conflict: terminated by other getUpdates request; make sure that only one bot instance is running" }),
    ));
    let err = poll_once(&pool, &cfg(), &api(&base), 0).await.unwrap_err();
    assert_eq!(err.downcast_ref::<ApiError>().unwrap().status, 409);
    assert!(err.to_string().contains("only one bot instance"), "{err}");
    assert_eq!(after_error(&err), Duration::from_secs(60));
    // The offset did not move.
    assert!(db::get_meta(&pool, "telegram_offset:123456")
        .await
        .unwrap()
        .is_none());
}

fn delivery(chat: &str, title: &str, message: &str) -> Delivery {
    Delivery {
        address: chat.into(),
        site_title: "Acme".into(),
        timezone: chrono_tz::UTC,
        site_url: Some("https://status.example.org".into()),
        message: Message::Event {
            event: SubscriberEvent::IncidentOpened(IncidentEvent {
                incident_id: 1,
                title: title.into(),
                component: "web".into(),
                component_name: "Web".into(),
                impact: "major_outage".into(),
                state: "investigating".into(),
                message: message.into(),
                url: Some("https://status.example.org/incidents/1".into()),
            }),
            unsubscribe_url: "https://status.example.org/unsubscribe/u".into(),
        },
    }
}

#[test]
fn messages_are_escaped_html_within_the_limit() {
    let html = render(&delivery("1", "<b>API</b> & co", "a <script> & b"));
    assert!(
        html.starts_with("<b>[Acme] Incident: &lt;b&gt;API&lt;/b&gt; &amp; co</b>\n\n"),
        "{html}"
    );
    assert!(html.contains("a &lt;script&gt; &amp; b"), "{html}");
    assert!(html.contains("Details: https://status.example.org/incidents/1"));
    assert!(
        html.ends_with("Unsubscribe: send /stop or open https://status.example.org/unsubscribe/u")
    );

    let long = render(&delivery("1", "Down", &"x & y ".repeat(2000)));
    assert!(long.chars().count() <= MAX_TEXT, "{}", long.chars().count());
    assert!(
        long.contains("…\n\nUnsubscribe: send /stop"),
        "cut before the footer"
    );
    // Every & starts a whole entity: nothing was cut in the middle of one.
    for (i, _) in long.match_indices('&') {
        let rest = &long[i..];
        assert!(
            rest.starts_with("&amp;") || rest.starts_with("&lt;") || rest.starts_with("&gt;"),
            "{}",
            &rest[..rest.len().min(10)]
        );
    }
}

async fn one_subscriber(pool: &Pool) -> Config {
    let cfg = cfg();
    sqlx::query(
        "INSERT INTO subscribers (channel, address, status, unsub_hash, created_at)
         VALUES ('telegram', '42', 'active', 'h', 0)",
    )
    .execute(pool)
    .await
    .unwrap();
    subs::publish(
        pool,
        &cfg,
        &SubscriberEvent::IncidentOpened(match delivery("", "t", "m").message {
            Message::Event {
                event: SubscriberEvent::IncidentOpened(e),
                ..
            } => e,
            _ => unreachable!(),
        }),
        0,
    )
    .await
    .unwrap();
    cfg
}

#[tokio::test]
async fn a_blocked_bot_or_missing_chat_removes_the_subscriber() {
    for (status, description) in [
        (403, "Forbidden: bot was blocked by the user"),
        (403, "Forbidden: user is deactivated"),
        (400, "Bad Request: chat not found"),
    ] {
        let pool = crate::db::connect_memory().await.unwrap();
        let cfg = one_subscriber(&pool).await;
        let (base, bot) = mock().await;
        bot.replies.lock().unwrap().push_back((
            status,
            json!({ "ok": false, "error_code": status, "description": description }),
        ));
        let channels = Channels::new(vec![Arc::new(TelegramChannel::with_api(
            api(&base),
            Some("b".into()),
        ))]);
        let stats = dispatch_due(&pool, &cfg, &channels, &mut Pacer::default(), 1)
            .await
            .unwrap();
        assert_eq!(stats.removed, 1, "{description}");
        assert!(subs::list(&pool).await.unwrap().is_empty(), "{description}");
    }
}

#[tokio::test]
async fn other_errors_are_retried_and_429_waits_as_asked() {
    use sqlx::Row;
    let pool = crate::db::connect_memory().await.unwrap();
    let cfg = one_subscriber(&pool).await;
    let (base, bot) = mock().await;
    bot.replies.lock().unwrap().push_back((
        429,
        json!({ "ok": false, "error_code": 429, "description": "Too Many Requests: retry after 120", "parameters": { "retry_after": 120 } }),
    ));
    let channels = Channels::new(vec![Arc::new(TelegramChannel::with_api(
        api(&base),
        Some("b".into()),
    ))]);
    let stats = dispatch_due(&pool, &cfg, &channels, &mut Pacer::default(), 1000)
        .await
        .unwrap();
    assert_eq!(
        stats,
        DispatchStats {
            retried: 1,
            ..Default::default()
        }
    );
    let r = sqlx::query("SELECT next_attempt_at FROM outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(r.get::<i64, _>("next_attempt_at"), 1120);
    let r = sqlx::query("SELECT status, failures_in_window FROM subscribers")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (
            r.get::<String, _>("status"),
            r.get::<i64, _>("failures_in_window")
        ),
        ("active".into(), 0)
    );

    // A 403 for another reason is not taken as a blocked bot.
    bot.replies.lock().unwrap().push_back((
        403,
        json!({ "ok": false, "error_code": 403, "description": "Forbidden: bot can't initiate conversation with a user" }),
    ));
    let stats = dispatch_due(&pool, &cfg, &channels, &mut Pacer::default(), 2000)
        .await
        .unwrap();
    assert_eq!(stats.retried, 1);
    assert_eq!(subs::list(&pool).await.unwrap().len(), 1);
    // Then it goes through, as HTML without previews.
    dispatch_due(&pool, &cfg, &channels, &mut Pacer::default(), 9000)
        .await
        .unwrap();
    let last = bot.sent.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last["chat_id"], "42");
    assert_eq!(last["parse_mode"], "HTML");
    assert_eq!(last["disable_web_page_preview"], true);
}

#[tokio::test]
async fn the_token_never_shows_in_errors() {
    // Nothing listens here: reqwest's error would carry the URL.
    let free = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", free.local_addr().unwrap());
    drop(free);
    let err = api(&base).send_message("1", "hi").await.unwrap_err();
    for shown in [format!("{err}"), format!("{err:#}"), format!("{err:?}")] {
        assert!(!shown.contains("SECRETtoken"), "{shown}");
        assert!(!shown.contains("123456:"), "{shown}");
    }
    // Nor in a refusal from the API.
    let (base, bot) = mock().await;
    bot.replies
        .lock()
        .unwrap()
        .push_back((401, json!({ "ok": false, "description": "Unauthorized" })));
    let err = api(&base).send_message("1", "hi").await.unwrap_err();
    assert!(!format!("{err:?}").contains("SECRETtoken"));
    assert!(
        bot.paths.lock().unwrap()[0].contains("SECRETtoken"),
        "the token does go in the path"
    );
}

#[tokio::test]
async fn the_start_link_names_the_bot() {
    let (base, _bot) = mock().await;
    let named = TelegramChannel::with_api(api(&base), Some("my_status_bot".into()));
    assert_eq!(
        named.start_link().await.unwrap(),
        "https://t.me/my_status_bot?start="
    );
    let asked = TelegramChannel::with_api(api(&base), None);
    assert_eq!(
        asked.start_link().await.unwrap(),
        "https://t.me/acme_status_bot?start="
    );
    assert_eq!(
        asked.pacing(),
        Pacing {
            global: Duration::from_millis(34),
            per_address: Duration::from_secs(1),
        }
    );
    assert!(!asked.needs_address());
}
