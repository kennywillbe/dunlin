//! Delivers the outbox: a task polls for due rows and hands each to the
//! channel its subscriber signed up with. Rows live in the database, so a
//! restart picks up where it left off; a send that was cut short by one is
//! sent again, which is better than a message lost.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use sqlx::Row;
use tokio::sync::watch;

use super::SubscriberEvent;
use super::{confirm_token, confirm_url, signing_key, unsubscribe_token, unsubscribe_url};
use crate::config::Config;
use crate::db::Pool;

/// Attempts before a message is given up on. With the backoff below the
/// last one is about an hour after the first.
pub const MAX_ATTEMPTS: i64 = 8;
const RETRY_BASE_SECS: i64 = 30;
const RETRY_CAP_SECS: i64 = 3600;
/// Rows handled per poll, so one poll cannot hold the database for long.
const BATCH: i64 = 100;
/// How often the task looks for due rows and for maintenance windows that
/// started or ended.
pub const POLL: Duration = Duration::from_secs(5);
/// A channel is expected to time out on its own; this only stops a stuck
/// one from holding every other subscriber's messages.
const SEND_LIMIT: Duration = Duration::from_secs(30);

/// Wait before the next try after `attempts` failed ones: 30 s, doubling,
/// at most an hour.
pub fn retry_delay(attempts: i64) -> i64 {
    let shift = (attempts - 1).clamp(0, 20) as u32;
    (RETRY_BASE_SECS << shift).min(RETRY_CAP_SECS)
}

/// How fast a channel may be sent to. Telegram, for one, limits messages
/// overall and per chat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pacing {
    /// Least time between two sends on the channel.
    pub global: Duration,
    /// Least time between two sends to the same address.
    pub per_address: Duration,
}

/// What a channel is asked to send.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Confirm {
        confirm_url: String,
        unsubscribe_url: String,
    },
    Event {
        event: SubscriberEvent,
        unsubscribe_url: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub address: String,
    /// The status page's title, for greetings and subjects.
    pub site_title: String,
    /// The configured zone, for writing times the way the page does.
    pub timezone: chrono_tz::Tz,
    /// `public_url`, for payloads that name the page.
    pub site_url: Option<String>,
    pub message: Message,
}

/// A send that can never work for this address (the mailbox does not
/// exist, say). The dispatcher gives the message up at once instead of
/// retrying it for an hour.
#[derive(Debug)]
pub struct PermanentFailure(pub String);

impl std::fmt::Display for PermanentFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PermanentFailure {}

/// A failed send that is retried as usual but counts toward quarantine: the
/// endpoint looks broken rather than busy. A `PermanentFailure` counts too.
#[derive(Debug)]
pub struct CountedFailure(pub String);

impl std::fmt::Display for CountedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CountedFailure {}

/// Counted failures that quarantine a subscriber, and the window they are
/// counted in. The window starts at the first failure and a success clears
/// it, so a flaky endpoint that recovers starts over.
pub const QUARANTINE_FAILURES: i64 = 10;
pub const QUARANTINE_WINDOW_SECS: i64 = 3600;

/// Count one failure against a subscriber, quarantining it at the limit.
/// Quarantined subscribers get nothing; their queued messages are held
/// until they sign up again or the rollup deletes them.
async fn count_failure(pool: &Pool, subscriber_id: i64, now: i64) -> Result<bool> {
    let row = sqlx::query(
        "UPDATE subscribers SET
           failures_in_window = CASE
             WHEN failure_window_start IS NULL OR failure_window_start <= ?1 - ?2 THEN 1
             ELSE failures_in_window + 1 END,
           failure_window_start = CASE
             WHEN failure_window_start IS NULL OR failure_window_start <= ?1 - ?2 THEN ?1
             ELSE failure_window_start END
         WHERE id = ?3 RETURNING failures_in_window",
    )
    .bind(now)
    .bind(QUARANTINE_WINDOW_SECS)
    .bind(subscriber_id)
    .fetch_optional(pool)
    .await?;
    let Some(failures) = row.map(|r| r.get::<i64, _>("failures_in_window")) else {
        return Ok(false);
    };
    if failures < QUARANTINE_FAILURES {
        return Ok(false);
    }
    let res = sqlx::query(
        "UPDATE subscribers SET status = 'quarantined', quarantined_at = ?
         WHERE id = ? AND status <> 'quarantined'",
    )
    .bind(now)
    .bind(subscriber_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

async fn clear_failures(pool: &Pool, subscriber_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE subscribers SET failures_in_window = 0, failure_window_start = NULL
         WHERE id = ? AND failures_in_window > 0",
    )
    .bind(subscriber_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[async_trait]
pub trait SubscriberChannel: Send + Sync {
    /// Stored in `subscribers.channel`, e.g. "email".
    fn name(&self) -> &'static str;
    /// Shown on the subscribe form, e.g. "Email".
    fn label(&self) -> &str;
    /// What to type in the address field, e.g. "your email address".
    fn address_hint(&self) -> &str;
    /// Check and normalise what the visitor typed. The error is shown to
    /// them as is.
    fn normalize_address(&self, input: &str) -> std::result::Result<String, String>;
    fn pacing(&self) -> Pacing {
        Pacing::default()
    }
    async fn send(&self, delivery: &Delivery) -> Result<()>;
}

/// The channels that are switched on, shared by the web pages and the
/// dispatcher and swapped when the config is reloaded.
#[derive(Default)]
pub struct Channels {
    inner: RwLock<BTreeMap<&'static str, Arc<dyn SubscriberChannel>>>,
}

impl Channels {
    pub fn new(list: Vec<Arc<dyn SubscriberChannel>>) -> Self {
        let c = Self::default();
        c.replace(list);
        c
    }

    pub fn replace(&self, list: Vec<Arc<dyn SubscriberChannel>>) {
        *self.inner.write().unwrap() = list.into_iter().map(|c| (c.name(), c)).collect();
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn SubscriberChannel>> {
        self.inner.read().unwrap().get(name).cloned()
    }

    pub fn list(&self) -> Vec<Arc<dyn SubscriberChannel>> {
        self.inner.read().unwrap().values().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }
}

/// Channels switched on in the config. One that cannot be built is left
/// out and logged; its queued messages wait for a config that works.
pub fn build_channels(cfg: &Config) -> Vec<Arc<dyn SubscriberChannel>> {
    let mut out: Vec<Arc<dyn SubscriberChannel>> = Vec::new();
    if !cfg.subscriptions.enabled {
        return out;
    }
    if let Some(email) = cfg.subscriptions.email.as_ref().filter(|e| e.enabled) {
        match super::email::EmailChannel::new(email) {
            Ok(c) => out.push(Arc::new(c)),
            Err(e) => tracing::warn!(error = %e, "email subscriptions unavailable"),
        }
    }
    if cfg
        .subscriptions
        .webhook
        .as_ref()
        .is_some_and(|w| w.enabled)
    {
        match super::webhook::WebhookChannel::new() {
            Ok(c) => out.push(Arc::new(c)),
            Err(e) => tracing::warn!(error = %e, "webhook subscriptions unavailable"),
        }
    }
    out
}

/// Rebuild the channels when `[subscriptions]` changes, so a new SMTP
/// server or password applies without a restart. `built_from` is what
/// `channels` was made from.
pub async fn channel_reload_loop(
    mut config_rx: watch::Receiver<Arc<Config>>,
    channels: Arc<Channels>,
    built_from: crate::config::SubscriptionsConfig,
) {
    let mut current = built_from;
    while config_rx.changed().await.is_ok() {
        let cfg = config_rx.borrow_and_update().clone();
        if cfg.subscriptions != current {
            channels.replace(build_channels(&cfg));
            current = cfg.subscriptions.clone();
            tracing::info!("subscription channels reloaded");
        }
    }
}

/// When each channel and each address was last sent to, for `Pacing`.
/// In memory only: after a restart the first sends simply go out at once.
#[derive(Default)]
pub struct Pacer {
    last_channel: HashMap<String, Instant>,
    last_address: HashMap<(String, String), Instant>,
}

impl Pacer {
    fn address_ready(&self, channel: &str, address: &str, pacing: Pacing, now: Instant) -> bool {
        self.last_address
            .get(&(channel.to_string(), address.to_string()))
            .is_none_or(|t| now.duration_since(*t) >= pacing.per_address)
    }

    fn channel_wait(&self, channel: &str, pacing: Pacing, now: Instant) -> Duration {
        self.last_channel.get(channel).map_or(Duration::ZERO, |t| {
            pacing.global.saturating_sub(now.duration_since(*t))
        })
    }

    fn record(&mut self, channel: &str, address: &str, now: Instant) {
        self.last_channel.insert(channel.to_string(), now);
        self.last_address
            .insert((channel.to_string(), address.to_string()), now);
        // Addresses only matter for a moment; forget them so the map does
        // not grow with every subscriber ever sent to.
        if self.last_address.len() > 10_000 {
            self.last_address
                .retain(|_, t| now.duration_since(*t) < Duration::from_secs(60));
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DispatchStats {
    pub sent: usize,
    pub retried: usize,
    pub given_up: usize,
    pub quarantined: usize,
}

/// Send every due outbox row once. Rows for channels that are not switched
/// on stay queued untouched, so turning a channel back on delivers them.
pub async fn dispatch_due(
    pool: &Pool,
    cfg: &Config,
    channels: &Channels,
    pacer: &mut Pacer,
    now: i64,
) -> Result<DispatchStats> {
    let mut stats = DispatchStats::default();
    let enabled = channels.list();
    if enabled.is_empty() {
        return Ok(stats);
    }
    let names: Vec<&str> = enabled.iter().map(|c| c.name()).collect();
    let rows = sqlx::query(
        "SELECT o.id, o.kind, o.payload, o.attempts, s.id AS sid, s.channel, s.address,
                s.status, s.created_at, s.confirm_expires
         FROM outbox o JOIN subscribers s ON s.id = o.subscriber_id
         WHERE o.sent_at IS NULL AND o.failed_at IS NULL AND o.next_attempt_at <= ?
           AND s.status IN ('pending', 'active')
           AND s.channel IN (SELECT value FROM json_each(?))
         ORDER BY o.id LIMIT ?",
    )
    .bind(now)
    .bind(serde_json::to_string(&names)?)
    .bind(BATCH)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(stats);
    }
    let key = signing_key(&mut *pool.acquire().await?).await?;

    for row in rows {
        let id: i64 = row.get("id");
        let kind: String = row.get("kind");
        let sid: i64 = row.get("sid");
        let status: String = row.get("status");
        let channel_name: String = row.get("channel");
        let address: String = row.get("address");
        let Some(channel) = channels.get(&channel_name) else {
            continue;
        };
        let unsubscribe =
            unsubscribe_url(cfg, &unsubscribe_token(&key, sid, row.get("created_at")));
        let message = if kind == "confirm" {
            let expires: Option<i64> = row.get("confirm_expires");
            match expires {
                Some(e) if status == "pending" && e > now => Message::Confirm {
                    confirm_url: confirm_url(cfg, &confirm_token(&key, sid, e)),
                    unsubscribe_url: unsubscribe,
                },
                // Confirmed some other way meanwhile, or the link has lapsed.
                _ => {
                    give_up(pool, id, now, "confirmation no longer needed").await?;
                    continue;
                }
            }
        } else {
            if status != "active" {
                continue;
            }
            match serde_json::from_str::<SubscriberEvent>(&row.get::<String, _>("payload")) {
                Ok(event) => Message::Event {
                    event,
                    unsubscribe_url: unsubscribe,
                },
                Err(e) => {
                    give_up(pool, id, now, &format!("bad payload: {e}")).await?;
                    stats.given_up += 1;
                    continue;
                }
            }
        };

        let pacing = channel.pacing();
        if !pacer.address_ready(&channel_name, &address, pacing, Instant::now()) {
            continue;
        }
        let wait = pacer.channel_wait(&channel_name, pacing, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        let delivery = Delivery {
            address: address.clone(),
            site_title: cfg.theme.title.trim().to_string(),
            timezone: cfg.tz(),
            site_url: cfg.public_url.clone(),
            message,
        };
        let result = match tokio::time::timeout(SEND_LIMIT, channel.send(&delivery)).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "send did not finish in {}s",
                SEND_LIMIT.as_secs()
            )),
        };
        pacer.record(&channel_name, &address, Instant::now());

        let attempts: i64 = row.get::<i64, _>("attempts") + 1;
        match result {
            Ok(()) => {
                sqlx::query(
                    "UPDATE outbox SET sent_at = ?, attempts = ?, last_error = NULL WHERE id = ?",
                )
                .bind(now)
                .bind(attempts)
                .bind(id)
                .execute(pool)
                .await?;
                clear_failures(pool, sid).await?;
                stats.sent += 1;
            }
            Err(e) => {
                let error: String = format!("{e:#}").chars().take(500).collect();
                tracing::warn!(outbox = id, channel = %channel_name, attempts, error = %error, "subscriber message failed");
                let permanent = e.downcast_ref::<PermanentFailure>().is_some();
                if (permanent || e.downcast_ref::<CountedFailure>().is_some())
                    && count_failure(pool, sid, now).await?
                {
                    tracing::warn!(subscriber = sid, channel = %channel_name, "subscriber quarantined after repeated failures");
                    stats.quarantined += 1;
                }
                if permanent || attempts >= MAX_ATTEMPTS {
                    sqlx::query("UPDATE outbox SET failed_at = ?, attempts = ?, last_error = ? WHERE id = ?")
                        .bind(now)
                        .bind(attempts)
                        .bind(&error)
                        .bind(id)
                        .execute(pool)
                        .await?;
                    stats.given_up += 1;
                } else {
                    sqlx::query(
                        "UPDATE outbox SET next_attempt_at = ?, attempts = ?, last_error = ? WHERE id = ?",
                    )
                    .bind(now + retry_delay(attempts))
                    .bind(attempts)
                    .bind(&error)
                    .bind(id)
                    .execute(pool)
                    .await?;
                    stats.retried += 1;
                }
            }
        }
    }
    Ok(stats)
}

async fn give_up(pool: &Pool, id: i64, now: i64, error: &str) -> Result<()> {
    sqlx::query("UPDATE outbox SET failed_at = ?, last_error = ? WHERE id = ?")
        .bind(now)
        .bind(error)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The background task: maintenance start/end checks, then delivery.
pub async fn dispatcher_loop(
    pool: Pool,
    config_rx: watch::Receiver<Arc<Config>>,
    channels: Arc<Channels>,
) {
    let mut pacer = Pacer::default();
    loop {
        let cfg = config_rx.borrow().clone();
        let now = crate::now_ts();
        if let Err(e) = super::maintenance_tick(&pool, &cfg, now).await {
            tracing::warn!(error = %e, "maintenance subscriber check failed");
        }
        if let Err(e) = dispatch_due(&pool, &cfg, &channels, &mut pacer, now).await {
            tracing::warn!(error = %e, "subscriber delivery failed");
        }
        tokio::time::sleep(POLL).await;
    }
}
