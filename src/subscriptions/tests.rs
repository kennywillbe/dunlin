use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sqlx::Row;

use super::dispatch::*;
use super::*;
use crate::db;

fn cfg() -> Config {
    let mut c = Config {
        public_url: Some("https://status.example.org".into()),
        ..Default::default()
    };
    c.subscriptions.enabled = true;
    c
}

/// Records deliveries; fails the first `fail` sends.
#[derive(Default)]
struct Fake {
    sent: Mutex<Vec<Delivery>>,
    fail: AtomicUsize,
    pacing: Pacing,
}

#[async_trait]
impl SubscriberChannel for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn label(&self) -> &str {
        "Fake"
    }
    fn address_hint(&self) -> &str {
        "anything"
    }
    fn normalize_address(&self, input: &str) -> std::result::Result<String, String> {
        Ok(input.trim().to_lowercase())
    }
    fn pacing(&self) -> Pacing {
        self.pacing
    }
    async fn send(&self, delivery: &Delivery) -> anyhow::Result<()> {
        if self
            .fail
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            anyhow::bail!("boom");
        }
        self.sent.lock().unwrap().push(delivery.clone());
        Ok(())
    }
}

fn channels(fake: &Arc<Fake>) -> Channels {
    Channels::new(vec![fake.clone() as Arc<dyn SubscriberChannel>])
}

fn signup(address: &str, all: bool, components: &[&str], maintenance: bool) -> SignUp {
    SignUp {
        channel: "fake".into(),
        address: address.into(),
        all_components: all,
        components: components.iter().map(|c| c.to_string()).collect(),
        maintenance,
    }
}

/// Sign up and mark active straight away, skipping the confirmation.
async fn active(
    pool: &Pool,
    address: &str,
    all: bool,
    components: &[&str],
    maintenance: bool,
) -> i64 {
    sign_up(pool, &signup(address, all, components, maintenance), 0)
        .await
        .unwrap();
    let row =
        sqlx::query("UPDATE subscribers SET status = 'active' WHERE address = ? RETURNING id")
            .bind(address)
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query("DELETE FROM outbox")
        .execute(pool)
        .await
        .unwrap();
    row.get("id")
}

async fn outbox(pool: &Pool) -> Vec<(i64, String)> {
    sqlx::query("SELECT subscriber_id, kind FROM outbox ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get("subscriber_id"), r.get("kind")))
        .collect()
}

fn incident(component: &str) -> SubscriberEvent {
    SubscriberEvent::IncidentOpened(IncidentEvent {
        incident_id: 1,
        title: "Down".into(),
        component: component.into(),
        component_name: component.into(),
        impact: "major_outage".into(),
        state: "investigating".into(),
        message: "boom".into(),
        url: None,
    })
}

fn maintenance(component: &str) -> SubscriberEvent {
    SubscriberEvent::MaintenanceScheduled(MaintenanceEvent {
        maintenance_id: 1,
        component: component.into(),
        component_name: component.into(),
        note: String::new(),
        starts_at: 10,
        ends_at: 20,
        url: None,
    })
}

fn recipients(rows: &[(i64, String)]) -> Vec<i64> {
    let mut ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn fan_out_matches_components_all_and_maintenance() {
    let pool = db::connect_memory().await.unwrap();
    let everything = active(&pool, "all", true, &[], false).await;
    let web = active(&pool, "web", false, &["web"], false).await;
    let db_only = active(&pool, "db", false, &["db"], true).await;
    let web_mnt = active(&pool, "web-mnt", false, &["web", "api"], true).await;
    // Signed up but never confirmed: hears nothing.
    sign_up(&pool, &signup("pending", true, &[], true), 0)
        .await
        .unwrap();
    sqlx::query("DELETE FROM outbox")
        .execute(&pool)
        .await
        .unwrap();
    let cfg = cfg();

    let check = |event: SubscriberEvent, want: Vec<i64>| {
        let pool = pool.clone();
        let cfg = cfg.clone();
        async move {
            sqlx::query("DELETE FROM outbox")
                .execute(&pool)
                .await
                .unwrap();
            let n = publish(&pool, &cfg, &event, 5).await.unwrap();
            let rows = outbox(&pool).await;
            assert_eq!(n as usize, rows.len());
            assert!(rows.iter().all(|r| r.1 == event.kind()));
            let mut want = want;
            want.sort();
            assert_eq!(recipients(&rows), want, "{event:?}");
        }
    };
    check(incident("web"), vec![everything, web, web_mnt]).await;
    check(incident("db"), vec![everything, db_only]).await;
    // An incident on all components goes to every active subscriber.
    check(incident(""), vec![everything, web, db_only, web_mnt]).await;
    // Maintenance only to those who asked for it.
    check(maintenance("web"), vec![web_mnt]).await;
    check(maintenance(""), vec![db_only, web_mnt]).await;

    let mut off = cfg.clone();
    off.subscriptions.enabled = false;
    sqlx::query("DELETE FROM outbox")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(publish(&pool, &off, &incident(""), 5).await.unwrap(), 0);
    assert!(outbox(&pool).await.is_empty());
}

#[tokio::test]
async fn events_round_trip_through_the_payload() {
    let event = incident("web");
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["kind"], "incident_opened");
    assert_eq!(json["component"], "web");
    assert_eq!(
        serde_json::from_value::<SubscriberEvent>(json).unwrap(),
        event
    );
}

#[tokio::test]
async fn dispatcher_sends_with_an_unsubscribe_link() {
    let pool = db::connect_memory().await.unwrap();
    let id = active(&pool, "someone", true, &[], false).await;
    let cfg = cfg();
    publish(&pool, &cfg, &incident("web"), 100).await.unwrap();
    let fake = Arc::new(Fake::default());
    let stats = dispatch_due(&pool, &cfg, &channels(&fake), &mut Pacer::default(), 100)
        .await
        .unwrap();
    assert_eq!(stats.sent, 1);

    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].address, "someone");
    let Message::Event {
        event,
        unsubscribe_url,
    } = &sent[0].message
    else {
        panic!("{sent:?}");
    };
    assert_eq!(event, &incident("web"));
    let token = unsubscribe_url
        .strip_prefix("https://status.example.org/unsubscribe/")
        .unwrap();
    assert!(check_unsubscribe(&pool, token).await.unwrap());
    assert!(!check_unsubscribe(&pool, "nope").await.unwrap());

    // Sent once, not again on the next poll.
    dispatch_due(&pool, &cfg, &channels(&fake), &mut Pacer::default(), 200)
        .await
        .unwrap();
    assert_eq!(fake.sent.lock().unwrap().len(), 1);

    // Unsubscribing removes the subscriber and anything still queued.
    publish(&pool, &cfg, &incident("web"), 300).await.unwrap();
    assert!(unsubscribe(&pool, token).await.unwrap());
    assert!(!unsubscribe(&pool, token).await.unwrap());
    assert!(outbox(&pool).await.is_empty());
    assert!(list(&pool).await.unwrap().iter().all(|s| s.id != id));
}

#[tokio::test]
async fn dispatcher_backs_off_then_gives_up() {
    let pool = db::connect_memory().await.unwrap();
    active(&pool, "someone", true, &[], false).await;
    let cfg = cfg();
    publish(&pool, &cfg, &incident("web"), 0).await.unwrap();
    let fake = Arc::new(Fake {
        fail: AtomicUsize::new(usize::MAX),
        ..Default::default()
    });
    let ch = channels(&fake);
    let row = || async {
        let r = sqlx::query("SELECT attempts, next_attempt_at, failed_at, last_error FROM outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        (
            r.get::<i64, _>("attempts"),
            r.get::<i64, _>("next_attempt_at"),
            r.get::<Option<i64>, _>("failed_at"),
            r.get::<Option<String>, _>("last_error"),
        )
    };

    let mut now = 0;
    for attempt in 1..MAX_ATTEMPTS {
        let stats = dispatch_due(&pool, &cfg, &ch, &mut Pacer::default(), now)
            .await
            .unwrap();
        assert_eq!(stats.retried, 1);
        let (attempts, next, failed, error) = row().await;
        assert_eq!(attempts, attempt);
        assert_eq!(next, now + retry_delay(attempt));
        assert!(failed.is_none());
        assert_eq!(error.as_deref(), Some("boom"));
        // Not due a second early.
        let stats = dispatch_due(&pool, &cfg, &ch, &mut Pacer::default(), next - 1)
            .await
            .unwrap();
        assert_eq!(stats, DispatchStats::default());
        now = next;
    }
    let stats = dispatch_due(&pool, &cfg, &ch, &mut Pacer::default(), now)
        .await
        .unwrap();
    assert_eq!(stats.given_up, 1);
    let (attempts, _, failed, _) = row().await;
    assert_eq!((attempts, failed), (MAX_ATTEMPTS, Some(now)));
    // Given up means given up.
    fake.fail.store(0, Ordering::SeqCst);
    dispatch_due(&pool, &cfg, &ch, &mut Pacer::default(), now + 86_400)
        .await
        .unwrap();
    assert!(fake.sent.lock().unwrap().is_empty());
}

#[test]
fn backoff_doubles_up_to_an_hour() {
    let delays: Vec<i64> = (1..=9).map(retry_delay).collect();
    assert_eq!(delays, [30, 60, 120, 240, 480, 960, 1920, 3600, 3600]);
    assert_eq!(retry_delay(100), 3600);
}

#[tokio::test]
async fn a_retry_survives_a_restart_and_nothing_is_sent_twice() {
    let pool = db::connect_memory().await.unwrap();
    active(&pool, "a", true, &[], false).await;
    active(&pool, "b", true, &[], false).await;
    let cfg = cfg();
    publish(&pool, &cfg, &incident("web"), 0).await.unwrap();
    // The first send fails, the second works.
    let first = Arc::new(Fake {
        fail: AtomicUsize::new(1),
        ..Default::default()
    });
    dispatch_due(&pool, &cfg, &channels(&first), &mut Pacer::default(), 0)
        .await
        .unwrap();
    assert_eq!(first.sent.lock().unwrap().len(), 1);

    // A new process: fresh channel and pacer, same database.
    let second = Arc::new(Fake::default());
    dispatch_due(&pool, &cfg, &channels(&second), &mut Pacer::default(), 10)
        .await
        .unwrap();
    assert!(
        second.sent.lock().unwrap().is_empty(),
        "retry is not due yet"
    );
    dispatch_due(&pool, &cfg, &channels(&second), &mut Pacer::default(), 30)
        .await
        .unwrap();
    let sent = second.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_ne!(sent[0].address, first.sent.lock().unwrap()[0].address);
}

#[tokio::test]
async fn rows_for_channels_that_are_off_wait() {
    let pool = db::connect_memory().await.unwrap();
    active(&pool, "a", true, &[], false).await;
    let cfg = cfg();
    publish(&pool, &cfg, &incident("web"), 0).await.unwrap();
    let none = Channels::default();
    let stats = dispatch_due(&pool, &cfg, &none, &mut Pacer::default(), 1000)
        .await
        .unwrap();
    assert_eq!(stats, DispatchStats::default());
    let r = sqlx::query("SELECT attempts, sent_at, failed_at FROM outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(r.get::<i64, _>("attempts"), 0);
    assert!(r.get::<Option<i64>, _>("sent_at").is_none());
    assert!(r.get::<Option<i64>, _>("failed_at").is_none());
    // Switched on later, the message still goes out.
    let fake = Arc::new(Fake::default());
    dispatch_due(&pool, &cfg, &channels(&fake), &mut Pacer::default(), 2000)
        .await
        .unwrap();
    assert_eq!(fake.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn pacing_holds_back_a_busy_address() {
    let pool = db::connect_memory().await.unwrap();
    active(&pool, "a", true, &[], false).await;
    active(&pool, "b", true, &[], false).await;
    let cfg = cfg();
    publish(&pool, &cfg, &incident("web"), 0).await.unwrap();
    publish(&pool, &cfg, &incident("web"), 0).await.unwrap();
    let fake = Arc::new(Fake {
        pacing: Pacing {
            global: Duration::from_millis(20),
            per_address: Duration::from_secs(3600),
        },
        ..Default::default()
    });
    let ch = channels(&fake);
    let mut pacer = Pacer::default();
    let started = std::time::Instant::now();
    let stats = dispatch_due(&pool, &cfg, &ch, &mut pacer, 0).await.unwrap();
    // One message per address this round; the channel-wide gap was kept.
    assert_eq!(stats.sent, 2);
    assert!(started.elapsed() >= Duration::from_millis(20));
    let mut to: Vec<String> = fake
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|d| d.address.clone())
        .collect();
    to.sort();
    assert_eq!(to, ["a", "b"]);
    // The second message per address stays queued, not failed.
    let waiting: i64 =
        sqlx::query("SELECT COUNT(*) AS n FROM outbox WHERE sent_at IS NULL AND attempts = 0")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("n");
    assert_eq!(waiting, 2);
}

/// The confirmation link the dispatcher sends for the newest sign-up.
async fn confirmation_token(pool: &Pool, now: i64) -> String {
    let fake = Arc::new(Fake::default());
    dispatch_due(pool, &cfg(), &channels(&fake), &mut Pacer::default(), now)
        .await
        .unwrap();
    let sent = fake.sent.lock().unwrap().clone();
    let Some(Message::Confirm { confirm_url, .. }) = sent.last().map(|d| &d.message) else {
        panic!("no confirmation in {sent:?}");
    };
    confirm_url
        .strip_prefix("https://status.example.org/subscribe/confirm/")
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn confirmation_activates_once_and_expires() {
    let pool = db::connect_memory().await.unwrap();
    sign_up(&pool, &signup("me", false, &["web"], true), 1000)
        .await
        .unwrap();
    let token = confirmation_token(&pool, 1000).await;
    assert_eq!(
        check_confirm(&pool, &token, 1001).await.unwrap(),
        ConfirmLink::Valid
    );
    assert_eq!(
        check_confirm(&pool, "nope", 1001).await.unwrap(),
        ConfirmLink::Invalid
    );
    assert_eq!(
        confirm(&pool, &token, 1002).await.unwrap(),
        ConfirmLink::Valid
    );
    // Used up.
    assert_eq!(
        confirm(&pool, &token, 1003).await.unwrap(),
        ConfirmLink::Invalid
    );
    let subs = list(&pool).await.unwrap();
    assert_eq!(subs[0].status, "active");
    assert_eq!(subs[0].components, ["web"]);
    assert!(subs[0].maintenance);

    sign_up(&pool, &signup("late", true, &[], false), 1000)
        .await
        .unwrap();
    let token = confirmation_token(&pool, 1000).await;
    let expiry = 1000 + CONFIRM_TTL_SECS;
    assert_eq!(
        check_confirm(&pool, &token, expiry).await.unwrap(),
        ConfirmLink::Expired
    );
    assert_eq!(
        confirm(&pool, &token, expiry).await.unwrap(),
        ConfirmLink::Expired
    );
    assert_eq!(list(&pool).await.unwrap()[0].status, "pending");
}

#[tokio::test]
async fn signing_up_again_never_says_who_is_subscribed() {
    let pool = db::connect_memory().await.unwrap();
    sign_up(&pool, &signup("me", true, &[], false), 0)
        .await
        .unwrap();
    let first = confirmation_token(&pool, 0).await;
    // Within the resend window: no second message, same pending row.
    sign_up(&pool, &signup("me", false, &["web"], false), 60)
        .await
        .unwrap();
    assert_eq!(outbox(&pool).await.len(), 1);
    assert!(list(&pool).await.unwrap()[0].all_components);

    // Later: a new link replaces the old one, with the new choices.
    let later = CONFIRM_RESEND_SECS + 1;
    sign_up(&pool, &signup("me", false, &["web"], false), later)
        .await
        .unwrap();
    let second = confirmation_token(&pool, later).await;
    assert_ne!(first, second);
    assert_eq!(
        check_confirm(&pool, &first, later).await.unwrap(),
        ConfirmLink::Invalid
    );
    assert_eq!(
        confirm(&pool, &second, later).await.unwrap(),
        ConfirmLink::Valid
    );
    assert_eq!(list(&pool).await.unwrap()[0].components, ["web"]);

    // Active: nothing changes and nothing is sent.
    let queued = outbox(&pool).await.len();
    sign_up(&pool, &signup("me", true, &[], true), later * 10)
        .await
        .unwrap();
    assert_eq!(outbox(&pool).await.len(), queued);
    let s = &list(&pool).await.unwrap()[0];
    assert_eq!((s.all_components, s.maintenance), (false, false));
    assert_eq!(list(&pool).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_confirmation_for_an_active_subscriber_is_dropped() {
    let pool = db::connect_memory().await.unwrap();
    sign_up(&pool, &signup("me", true, &[], false), 0)
        .await
        .unwrap();
    sqlx::query("UPDATE subscribers SET status = 'active'")
        .execute(&pool)
        .await
        .unwrap();
    let fake = Arc::new(Fake::default());
    dispatch_due(&pool, &cfg(), &channels(&fake), &mut Pacer::default(), 1)
        .await
        .unwrap();
    assert!(fake.sent.lock().unwrap().is_empty());
    let r = sqlx::query("SELECT failed_at FROM outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(r.get::<Option<i64>, _>("failed_at"), Some(1));
}

async fn kinds(pool: &Pool) -> Vec<String> {
    outbox(pool).await.into_iter().map(|r| r.1).collect()
}

#[tokio::test]
async fn maintenance_start_and_end_are_sent_once() {
    let pool = db::connect_memory().await.unwrap();
    active(&pool, "m", true, &[], true).await;
    let cfg = cfg();
    let id = db::create_maintenance(&pool, "web", "upgrade", 100, 200, 0)
        .await
        .unwrap();

    maintenance_tick(&pool, &cfg, 99).await.unwrap();
    assert!(kinds(&pool).await.is_empty());
    maintenance_tick(&pool, &cfg, 100).await.unwrap();
    maintenance_tick(&pool, &cfg, 150).await.unwrap();
    assert_eq!(kinds(&pool).await, ["maintenance_started"]);
    maintenance_tick(&pool, &cfg, 200).await.unwrap();
    // Ticking again, as after a restart, repeats nothing.
    maintenance_tick(&pool, &cfg, 201).await.unwrap();
    maintenance_tick(&pool, &cfg, 5000).await.unwrap();
    assert_eq!(
        kinds(&pool).await,
        ["maintenance_started", "maintenance_completed"]
    );

    let payload: String = sqlx::query("SELECT payload FROM outbox ORDER BY id DESC")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("payload");
    let SubscriberEvent::MaintenanceCompleted(e) = serde_json::from_str(&payload).unwrap() else {
        panic!("{payload}");
    };
    assert_eq!((e.maintenance_id, e.note.as_str()), (id, "upgrade"));
    assert_eq!(e.url.as_deref(), Some("https://status.example.org/"));
}

#[tokio::test]
async fn ended_early_missed_and_cancelled_windows() {
    let pool = db::connect_memory().await.unwrap();
    active(&pool, "m", true, &[], true).await;
    let cfg = cfg();

    // Ended early from /manage: started, then completed at the new end.
    let early = db::create_maintenance(&pool, "web", "", 100, 1000, 0)
        .await
        .unwrap();
    maintenance_tick(&pool, &cfg, 150).await.unwrap();
    db::end_maintenance(&pool, early, 300).await.unwrap();
    maintenance_tick(&pool, &cfg, 301).await.unwrap();
    assert_eq!(
        kinds(&pool).await,
        ["maintenance_started", "maintenance_completed"]
    );
    sqlx::query("DELETE FROM outbox")
        .execute(&pool)
        .await
        .unwrap();

    // Over before dunlin looked (it was down): only "completed".
    db::create_maintenance(&pool, "web", "", 400, 500, 0)
        .await
        .unwrap();
    maintenance_tick(&pool, &cfg, 600).await.unwrap();
    assert_eq!(kinds(&pool).await, ["maintenance_completed"]);
    sqlx::query("DELETE FROM outbox")
        .execute(&pool)
        .await
        .unwrap();

    // Planned, then ended before it began: cancelled, nothing to say.
    let planned = db::create_maintenance(&pool, "web", "", 2000, 3000, 0)
        .await
        .unwrap();
    db::end_maintenance(&pool, planned, 700).await.unwrap();
    for t in [701, 2000, 2500, 3500] {
        maintenance_tick(&pool, &cfg, t).await.unwrap();
    }
    assert!(kinds(&pool).await.is_empty());
}

#[tokio::test]
async fn a_scheduled_window_ended_before_it_began_is_cancelled_once() {
    let pool = db::connect_memory().await.unwrap();
    let mnt = active(&pool, "mnt", false, &["web"], true).await;
    active(&pool, "no-mnt", true, &[], false).await;
    active(&pool, "other", false, &["db"], true).await;
    let cfg = cfg();
    let id = db::create_maintenance(&pool, "web", "upgrade", 2000, 3000, 0)
        .await
        .unwrap();
    publish_scheduled(&pool, &cfg, id, 0).await;
    db::end_maintenance(&pool, id, 700).await.unwrap();
    for t in [701, 702, 2500, 3500] {
        maintenance_tick(&pool, &cfg, t).await.unwrap();
    }
    let rows = outbox(&pool).await;
    assert_eq!(
        rows,
        [
            (mnt, "maintenance_scheduled".to_string()),
            (mnt, "maintenance_cancelled".to_string())
        ]
    );
}

#[tokio::test]
async fn a_window_nobody_heard_of_is_not_cancelled() {
    let pool = db::connect_memory().await.unwrap();
    active(&pool, "mnt", true, &[], true).await;
    let mut off = cfg();
    off.subscriptions.enabled = false;
    // Planned while subscriptions were off, so no "scheduled" went out.
    let id = db::create_maintenance(&pool, "web", "", 2000, 3000, 0)
        .await
        .unwrap();
    publish_scheduled(&pool, &off, id, 0).await;
    db::end_maintenance(&pool, id, 700).await.unwrap();
    maintenance_tick(&pool, &cfg(), 701).await.unwrap();
    assert!(kinds(&pool).await.is_empty());
}

#[tokio::test]
async fn a_quiet_incident_sends_nothing_ever() {
    let pool = db::connect_memory().await.unwrap();
    active(&pool, "a", true, &[], false).await;
    let cfg = cfg();
    let open = |title: &'static str| {
        let pool = pool.clone();
        async move {
            db::create_incident(
                &pool,
                "web",
                title,
                crate::models::State::MajorOutage,
                IncidentState::Investigating,
                true,
                0,
            )
            .await
            .unwrap()
        }
    };
    let quiet = open("during maintenance").await;
    mark_quiet(&pool, quiet).await.unwrap();
    let loud = open("normal").await;
    for change in [
        IncidentChange::Opened,
        IncidentChange::Updated,
        IncidentChange::Resolved,
    ] {
        publish_incident(&pool, &cfg, quiet, change, "x", 1).await;
    }
    assert!(outbox(&pool).await.is_empty());
    publish_incident(&pool, &cfg, loud, IncidentChange::Resolved, "x", 1).await;
    assert_eq!(kinds(&pool).await, ["incident_resolved"]);
}

#[tokio::test]
async fn ticks_with_subscriptions_off_still_mark_windows() {
    let pool = db::connect_memory().await.unwrap();
    active(&pool, "m", true, &[], true).await;
    let mut off = cfg();
    off.subscriptions.enabled = false;
    db::create_maintenance(&pool, "web", "", 100, 200, 0)
        .await
        .unwrap();
    maintenance_tick(&pool, &off, 150).await.unwrap();
    // Turning subscriptions on later does not announce a window long begun.
    maintenance_tick(&pool, &cfg(), 160).await.unwrap();
    assert!(kinds(&pool).await.is_empty());
    maintenance_tick(&pool, &cfg(), 250).await.unwrap();
    assert_eq!(kinds(&pool).await, ["maintenance_completed"]);
}

#[tokio::test]
async fn prune_drops_old_messages_and_stale_sign_ups() {
    let pool = db::connect_memory().await.unwrap();
    active(&pool, "a", true, &[], false).await;
    let cfg = cfg();
    publish(&pool, &cfg, &incident("web"), 0).await.unwrap();
    let fake = Arc::new(Fake::default());
    dispatch_due(&pool, &cfg, &channels(&fake), &mut Pacer::default(), 0)
        .await
        .unwrap();
    publish(&pool, &cfg, &incident("web"), 10).await.unwrap();
    sign_up(&pool, &signup("never", true, &[], false), 0)
        .await
        .unwrap();

    prune(&pool, 1).await.unwrap();
    assert_eq!(list(&pool).await.unwrap().len(), 2);
    let later = CONFIRM_TTL_SECS + PENDING_KEEP_SECS + OUTBOX_KEEP_SECS;
    prune(&pool, later).await.unwrap();
    let left = list(&pool).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].address, "a");
    // Only the unsent message is left.
    assert_eq!(outbox(&pool).await.len(), 1);
}

#[test]
fn addresses_are_masked() {
    assert_eq!(mask("email", "jane@example.org"), "j…@example.org");
    assert_eq!(mask("email", "broken"), "…");
    assert_eq!(
        mask("webhook", "https://hooks.slack.com/services/T0/B0/secret"),
        "https://hooks.slack.com/…"
    );
    assert_eq!(mask("telegram", "123456789"), "…789");
    assert_eq!(mask("telegram", "1234"), "…");
}

#[test]
fn tokens_depend_on_key_row_and_nonce() {
    let a = confirm_token("k", 1, 100);
    assert_eq!(a.len(), 64);
    assert_eq!(a, confirm_token("k", 1, 100));
    assert_ne!(a, confirm_token("k", 1, 101));
    assert_ne!(a, confirm_token("k", 2, 100));
    assert_ne!(a, confirm_token("other", 1, 100));
    assert_ne!(a, unsubscribe_token("k", 1, 100));
}
