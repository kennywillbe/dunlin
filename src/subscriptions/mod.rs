//! Subscriptions: visitors hear about incidents and maintenance on channels
//! they pick. Events fan out into the `outbox` table as one row per
//! subscriber, and a background task delivers the rows (see `dispatch`).
//!
//! Tokens are never stored. Confirmation and unsubscribe tokens are an
//! HMAC over the subscriber's row under a random key kept in `meta`, so the
//! dispatcher can put links in any later message, while the table only holds
//! each token's SHA-256 for lookup.

pub mod dispatch;
pub mod email;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection};

use crate::auth;
use crate::config::Config;
use crate::db::Pool;
use crate::models::{Incident, IncidentState};

/// How long a confirmation link works.
pub const CONFIRM_TTL_SECS: i64 = 24 * 3600;
/// A pending address gets at most one new confirmation per this long, so the
/// form cannot be used to flood someone else's inbox.
pub const CONFIRM_RESEND_SECS: i64 = 15 * 60;
/// Delivered and given-up outbox rows are kept this long for /manage and
/// debugging, then pruned.
pub const OUTBOX_KEEP_SECS: i64 = 30 * 86_400;
/// Pending subscribers are deleted this long after their link expired.
pub const PENDING_KEEP_SECS: i64 = 7 * 86_400;

const KEY_META: &str = "subscription_key";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IncidentEvent {
    pub incident_id: i64,
    pub title: String,
    /// Component id; empty for an incident on all components.
    pub component: String,
    pub component_name: String,
    /// Impact as a component state name, e.g. `major_outage`.
    pub impact: String,
    /// Incident state name, e.g. `investigating`.
    pub state: String,
    /// The timeline entry this event is about.
    pub message: String,
    /// The incident's page, when `public_url` is set.
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceEvent {
    pub maintenance_id: i64,
    /// Component id; empty for all components.
    pub component: String,
    pub component_name: String,
    pub note: String,
    pub starts_at: i64,
    pub ends_at: i64,
    /// The status page, when `public_url` is set.
    pub url: Option<String>,
}

/// Something subscribers are told about. Stored as the outbox payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubscriberEvent {
    IncidentOpened(IncidentEvent),
    IncidentUpdated(IncidentEvent),
    IncidentResolved(IncidentEvent),
    MaintenanceScheduled(MaintenanceEvent),
    MaintenanceStarted(MaintenanceEvent),
    MaintenanceCompleted(MaintenanceEvent),
    MaintenanceCancelled(MaintenanceEvent),
}

impl SubscriberEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::IncidentOpened(_) => "incident_opened",
            Self::IncidentUpdated(_) => "incident_updated",
            Self::IncidentResolved(_) => "incident_resolved",
            Self::MaintenanceScheduled(_) => "maintenance_scheduled",
            Self::MaintenanceStarted(_) => "maintenance_started",
            Self::MaintenanceCompleted(_) => "maintenance_completed",
            Self::MaintenanceCancelled(_) => "maintenance_cancelled",
        }
    }

    pub fn component(&self) -> &str {
        match self {
            Self::IncidentOpened(e) | Self::IncidentUpdated(e) | Self::IncidentResolved(e) => {
                &e.component
            }
            Self::MaintenanceScheduled(e)
            | Self::MaintenanceStarted(e)
            | Self::MaintenanceCompleted(e)
            | Self::MaintenanceCancelled(e) => &e.component,
        }
    }

    pub fn is_maintenance(&self) -> bool {
        matches!(
            self,
            Self::MaintenanceScheduled(_)
                | Self::MaintenanceStarted(_)
                | Self::MaintenanceCompleted(_)
                | Self::MaintenanceCancelled(_)
        )
    }
}

/// What happened to an incident, for `publish_incident`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncidentChange {
    Opened,
    Updated,
    Resolved,
}

/// Display name for a component id as messages show it.
pub fn component_label(cfg: &Config, id: &str) -> String {
    if id.is_empty() {
        return "All components".to_string();
    }
    cfg.component(id)
        .map_or_else(|| id.to_string(), |c| c.name.clone())
}

pub fn incident_event(cfg: &Config, incident: &Incident, message: &str) -> IncidentEvent {
    IncidentEvent {
        incident_id: incident.id,
        title: incident.title.clone(),
        component: incident.component.clone(),
        component_name: component_label(cfg, &incident.component),
        impact: incident.impact.as_str().to_string(),
        state: incident.state.as_str().to_string(),
        message: message.to_string(),
        url: cfg
            .public_url
            .as_deref()
            .map(|base| crate::alert::incident_url(base, incident.id)),
    }
}

pub fn maintenance_event(cfg: &Config, m: &crate::models::Maintenance) -> MaintenanceEvent {
    MaintenanceEvent {
        maintenance_id: m.id,
        component: m.component.clone(),
        component_name: component_label(cfg, &m.component),
        note: m.note.clone(),
        starts_at: m.starts_at,
        ends_at: m.ends_at,
        url: cfg
            .public_url
            .as_deref()
            .map(|base| format!("{}/", base.trim_end_matches('/'))),
    }
}

/// Queue one outbox row per active subscriber `event` concerns: everyone
/// for an incident on all components, otherwise those subscribed to all
/// components or to this one; maintenance only for those who asked for it.
pub async fn fan_out(
    conn: &mut SqliteConnection,
    event: &SubscriberEvent,
    now: i64,
) -> Result<u64> {
    let payload = serde_json::to_string(event)?;
    let component = event.component();
    let res = sqlx::query(
        "INSERT INTO outbox (subscriber_id, kind, payload, created_at, next_attempt_at)
         SELECT s.id, ?, ?, ?, ? FROM subscribers s
         WHERE s.status = 'active'
           AND (? = 0 OR s.maintenance = 1)
           AND (? = '' OR s.all_components = 1 OR EXISTS (
                SELECT 1 FROM subscriber_components c
                WHERE c.subscriber_id = s.id AND c.component_id = ?))",
    )
    .bind(event.kind())
    .bind(&payload)
    .bind(now)
    .bind(now)
    .bind(event.is_maintenance() as i64)
    .bind(component)
    .bind(component)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected())
}

/// `fan_out` in its own transaction; nothing when subscriptions are off.
pub async fn publish(pool: &Pool, cfg: &Config, event: &SubscriberEvent, now: i64) -> Result<u64> {
    if !cfg.subscriptions.enabled {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let n = fan_out(&mut tx, event, now).await?;
    tx.commit().await?;
    Ok(n)
}

/// Tell subscribers about an incident change. Failing to queue must not
/// fail what the operator or the alert engine did, so errors are logged.
pub async fn publish_incident(
    pool: &Pool,
    cfg: &Config,
    incident_id: i64,
    change: IncidentChange,
    message: &str,
    now: i64,
) {
    if !cfg.subscriptions.enabled {
        return;
    }
    let result = async {
        let Some(incident) = crate::db::incident(pool, incident_id).await? else {
            return Ok(0);
        };
        if is_quiet(pool, incident_id).await? {
            return Ok(0);
        }
        let e = incident_event(cfg, &incident, message);
        let event = match change {
            IncidentChange::Opened => SubscriberEvent::IncidentOpened(e),
            IncidentChange::Updated => SubscriberEvent::IncidentUpdated(e),
            IncidentChange::Resolved => SubscriberEvent::IncidentResolved(e),
        };
        publish(pool, cfg, &event, now).await
    }
    .await;
    if let Err(e) = result {
        tracing::warn!(incident = incident_id, error = %e, "queueing subscriber messages failed");
    }
}

/// Keep subscribers out of an incident for good. The alert engine calls
/// this for an incident it opens during maintenance, when operators are not
/// alerted either; its updates and resolution then stay quiet too, rather
/// than subscribers hearing it resolved without ever hearing it opened.
pub async fn mark_quiet(pool: &Pool, incident_id: i64) -> Result<()> {
    sqlx::query("UPDATE incidents SET quiet = 1 WHERE id = ?")
        .bind(incident_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn is_quiet(pool: &Pool, incident_id: i64) -> Result<bool> {
    let row = sqlx::query("SELECT quiet FROM incidents WHERE id = ?")
        .bind(incident_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some_and(|r| r.get::<i64, _>("quiet") != 0))
}

/// Tell subscribers about a window planned for later. Started and completed
/// come from `maintenance_tick`, since they happen with time passing.
pub async fn publish_scheduled(pool: &Pool, cfg: &Config, maintenance_id: i64, now: i64) {
    if !cfg.subscriptions.enabled {
        return;
    }
    let result = async {
        let Some(m) = maintenance_row(pool, maintenance_id).await? else {
            return Ok(0);
        };
        let event = SubscriberEvent::MaintenanceScheduled(maintenance_event(cfg, &m));
        // Recorded with the messages, so a later cancellation goes out
        // exactly when this did.
        let mut tx = pool.begin().await?;
        sqlx::query("UPDATE maintenance SET scheduled_sent = 1 WHERE id = ?")
            .bind(m.id)
            .execute(&mut *tx)
            .await?;
        let n = fan_out(&mut tx, &event, now).await?;
        tx.commit().await?;
        anyhow::Ok(n)
    }
    .await;
    if let Err(e) = result {
        tracing::warn!(maintenance = maintenance_id, error = %e, "queueing subscriber messages failed");
    }
}

fn row_to_maintenance(r: &sqlx::sqlite::SqliteRow) -> crate::models::Maintenance {
    crate::models::Maintenance {
        id: r.get("id"),
        component: r.get("component"),
        note: r.get("note"),
        starts_at: r.get("starts_at"),
        ends_at: r.get("ends_at"),
    }
}

async fn maintenance_row(pool: &Pool, id: i64) -> Result<Option<crate::models::Maintenance>> {
    let row =
        sqlx::query("SELECT id, component, note, starts_at, ends_at FROM maintenance WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.as_ref().map(row_to_maintenance))
}

/// Send "started" and "completed" for windows whose time has come. Each
/// window is claimed with its flag in the same transaction as its outbox
/// rows, so a restart neither repeats nor loses a message.
///
/// A window that was already over when first seen (dunlin was down) only
/// gets "completed". One ended before it began was called off: "cancelled"
/// goes to whoever was told it was scheduled, and nobody else hears of it.
pub async fn maintenance_tick(pool: &Pool, cfg: &Config, now: i64) -> Result<()> {
    let due = sqlx::query(
        "SELECT id, component, note, starts_at, ends_at FROM maintenance
         WHERE started_sent = 0 AND starts_at <= ? ORDER BY starts_at",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;
    for m in due.iter().map(row_to_maintenance) {
        let mut tx = pool.begin().await?;
        let claimed = sqlx::query(
            "UPDATE maintenance SET started_sent = 1 WHERE id = ? AND started_sent = 0",
        )
        .bind(m.id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if claimed && m.ends_at > now && cfg.subscriptions.enabled {
            let event = SubscriberEvent::MaintenanceStarted(maintenance_event(cfg, &m));
            fan_out(&mut tx, &event, now).await?;
        }
        tx.commit().await?;
    }

    let over = sqlx::query(
        "SELECT id, component, note, starts_at, ends_at, scheduled_sent FROM maintenance
         WHERE completed_sent = 0 AND ends_at <= ? ORDER BY ends_at",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;
    for row in &over {
        let m = row_to_maintenance(row);
        let scheduled = row.get::<i64, _>("scheduled_sent") != 0;
        let mut tx = pool.begin().await?;
        let claimed = sqlx::query(
            "UPDATE maintenance SET completed_sent = 1, started_sent = 1
             WHERE id = ? AND completed_sent = 0",
        )
        .bind(m.id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if claimed && cfg.subscriptions.enabled {
            let e = maintenance_event(cfg, &m);
            let event = if m.ends_at > m.starts_at {
                Some(SubscriberEvent::MaintenanceCompleted(e))
            } else if scheduled {
                Some(SubscriberEvent::MaintenanceCancelled(e))
            } else {
                None
            };
            if let Some(event) = event {
                fan_out(&mut tx, &event, now).await?;
            }
        }
        tx.commit().await?;
    }
    Ok(())
}

// -- tokens ------------------------------------------------------------------

/// The HMAC key, created on first use. `INSERT OR IGNORE` keeps the first
/// key if two requests race to create it.
pub async fn signing_key(conn: &mut SqliteConnection) -> Result<String> {
    sqlx::query("INSERT OR IGNORE INTO meta (key, value) VALUES (?, ?)")
        .bind(KEY_META)
        .bind(auth::random_token())
        .execute(&mut *conn)
        .await?;
    let row = sqlx::query("SELECT value FROM meta WHERE key = ?")
        .bind(KEY_META)
        .fetch_one(&mut *conn)
        .await?;
    Ok(row.get("value"))
}

/// A token bound to one subscriber row and to `nonce` (the link's expiry, or
/// the row's creation time), so a new link or a new row gets a new token.
pub fn derive_token(key: &str, purpose: &str, subscriber_id: i64, nonce: i64) -> String {
    let msg = format!("{purpose}\n{subscriber_id}\n{nonce}");
    hex::encode(auth::hmac_sha256(key.as_bytes(), msg.as_bytes()))
}

pub fn confirm_token(key: &str, subscriber_id: i64, expires: i64) -> String {
    derive_token(key, "confirm", subscriber_id, expires)
}

pub fn unsubscribe_token(key: &str, subscriber_id: i64, created_at: i64) -> String {
    derive_token(key, "unsubscribe", subscriber_id, created_at)
}

fn base_url(cfg: &Config) -> &str {
    cfg.public_url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/')
}

pub fn confirm_url(cfg: &Config, token: &str) -> String {
    format!("{}/subscribe/confirm/{token}", base_url(cfg))
}

pub fn unsubscribe_url(cfg: &Config, token: &str) -> String {
    format!("{}/unsubscribe/{token}", base_url(cfg))
}

// -- signing up --------------------------------------------------------------

/// A validated sign-up from the form.
#[derive(Debug, Clone)]
pub struct SignUp {
    pub channel: String,
    /// Already normalised by the channel.
    pub address: String,
    pub all_components: bool,
    pub components: Vec<String>,
    pub maintenance: bool,
}

/// Record a sign-up and queue its confirmation. The caller shows the same
/// page whatever happens here, so nobody learns whether an address is
/// already subscribed:
///
/// - new address: a pending row and a confirmation;
/// - pending (or quarantined) address: new choices and a new confirmation,
///   unless one went out in the last `CONFIRM_RESEND_SECS`;
/// - active address: nothing; its choices stay as they are.
pub async fn sign_up(pool: &Pool, req: &SignUp, now: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    let existing = sqlx::query(
        "SELECT id, status, confirm_expires FROM subscribers WHERE channel = ? AND address = ?",
    )
    .bind(&req.channel)
    .bind(&req.address)
    .fetch_optional(&mut *tx)
    .await?;
    let id = match existing {
        None => {
            // unsub_hash is unique and not null; a throwaway value holds its
            // place until the id the real token is derived from exists.
            let id: i64 = sqlx::query(
                "INSERT INTO subscribers
                   (channel, address, all_components, maintenance, status, unsub_hash, created_at)
                 VALUES (?, ?, ?, ?, 'pending', ?, ?) RETURNING id",
            )
            .bind(&req.channel)
            .bind(&req.address)
            .bind(req.all_components as i64)
            .bind(req.maintenance as i64)
            .bind(auth::random_token())
            .bind(now)
            .fetch_one(&mut *tx)
            .await?
            .get("id");
            let key = signing_key(&mut tx).await?;
            sqlx::query("UPDATE subscribers SET unsub_hash = ? WHERE id = ?")
                .bind(auth::token_hash(&unsubscribe_token(&key, id, now)))
                .bind(id)
                .execute(&mut *tx)
                .await?;
            id
        }
        Some(row) => {
            let id: i64 = row.get("id");
            let status: String = row.get("status");
            let expires: Option<i64> = row.get("confirm_expires");
            if status == "active" {
                tx.commit().await?;
                return Ok(());
            }
            let issued = expires.map(|e| e - CONFIRM_TTL_SECS);
            if issued.is_some_and(|t| now - t < CONFIRM_RESEND_SECS) {
                tx.commit().await?;
                return Ok(());
            }
            sqlx::query(
                "UPDATE subscribers SET all_components = ?, maintenance = ?, status = 'pending',
                   failure_window_start = NULL, failures_in_window = 0, quarantined_at = NULL
                 WHERE id = ?",
            )
            .bind(req.all_components as i64)
            .bind(req.maintenance as i64)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            id
        }
    };

    sqlx::query("DELETE FROM subscriber_components WHERE subscriber_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if !req.all_components {
        for c in &req.components {
            sqlx::query(
                "INSERT OR IGNORE INTO subscriber_components (subscriber_id, component_id) VALUES (?, ?)",
            )
            .bind(id)
            .bind(c)
            .execute(&mut *tx)
            .await?;
        }
    }

    let key = signing_key(&mut tx).await?;
    let expires = now + CONFIRM_TTL_SECS;
    sqlx::query("UPDATE subscribers SET confirm_hash = ?, confirm_expires = ? WHERE id = ?")
        .bind(auth::token_hash(&confirm_token(&key, id, expires)))
        .bind(expires)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // Any older confirmation still waiting would carry a dead link.
    sqlx::query(
        "DELETE FROM outbox WHERE subscriber_id = ? AND kind = 'confirm' AND sent_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO outbox (subscriber_id, kind, payload, created_at, next_attempt_at)
         VALUES (?, 'confirm', '{}', ?, ?)",
    )
    .bind(id)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await.context("saving sign-up")?;
    Ok(())
}

/// What a confirmation link points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmLink {
    Valid,
    Expired,
    /// Unknown, or already used.
    Invalid,
}

pub async fn check_confirm(pool: &Pool, token: &str, now: i64) -> Result<ConfirmLink> {
    let row = sqlx::query(
        "SELECT confirm_expires FROM subscribers WHERE confirm_hash = ? AND status = 'pending'",
    )
    .bind(auth::token_hash(token))
    .fetch_optional(pool)
    .await?;
    Ok(
        match row.map(|r| r.get::<Option<i64>, _>("confirm_expires")) {
            Some(Some(e)) if e > now => ConfirmLink::Valid,
            Some(_) => ConfirmLink::Expired,
            None => ConfirmLink::Invalid,
        },
    )
}

/// Activate the subscriber a confirmation link belongs to.
pub async fn confirm(pool: &Pool, token: &str, now: i64) -> Result<ConfirmLink> {
    let res = sqlx::query(
        "UPDATE subscribers SET status = 'active', confirmed_at = ?,
           confirm_hash = NULL, confirm_expires = NULL
         WHERE confirm_hash = ? AND status = 'pending' AND confirm_expires > ?",
    )
    .bind(now)
    .bind(auth::token_hash(token))
    .bind(now)
    .execute(pool)
    .await?;
    if res.rows_affected() == 1 {
        Ok(ConfirmLink::Valid)
    } else {
        check_confirm(pool, token, now).await
    }
}

/// Whether an unsubscribe link belongs to a subscriber.
pub async fn check_unsubscribe(pool: &Pool, token: &str) -> Result<bool> {
    let row = sqlx::query("SELECT 1 FROM subscribers WHERE unsub_hash = ?")
        .bind(auth::token_hash(token))
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// Delete the subscriber an unsubscribe link belongs to, with its queued
/// messages (by cascade).
pub async fn unsubscribe(pool: &Pool, token: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM subscribers WHERE unsub_hash = ?")
        .bind(auth::token_hash(token))
        .execute(pool)
        .await?;
    Ok(res.rows_affected() == 1)
}

// -- operator view -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Subscriber {
    pub id: i64,
    pub channel: String,
    pub address: String,
    pub all_components: bool,
    pub components: Vec<String>,
    pub maintenance: bool,
    pub status: String,
    pub created_at: i64,
}

/// Every subscriber, newest first.
pub async fn list(pool: &Pool) -> Result<Vec<Subscriber>> {
    let rows = sqlx::query(
        "SELECT s.id, s.channel, s.address, s.all_components, s.maintenance, s.status,
                s.created_at, GROUP_CONCAT(c.component_id, char(10)) AS components
         FROM subscribers s
         LEFT JOIN subscriber_components c ON c.subscriber_id = s.id
         GROUP BY s.id ORDER BY s.created_at DESC, s.id DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let mut components: Vec<String> = r
                .get::<Option<String>, _>("components")
                .map(|c| c.split('\n').map(str::to_string).collect())
                .unwrap_or_default();
            components.sort();
            Subscriber {
                id: r.get("id"),
                channel: r.get("channel"),
                address: r.get("address"),
                all_components: r.get::<i64, _>("all_components") != 0,
                components,
                maintenance: r.get::<i64, _>("maintenance") != 0,
                status: r.get("status"),
                created_at: r.get("created_at"),
            }
        })
        .collect())
}

pub async fn delete(pool: &Pool, id: i64) -> Result<bool> {
    let res = sqlx::query("DELETE FROM subscribers WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Enough of an address for an operator to tell subscribers apart without
/// the /manage page listing everyone's full address.
pub fn mask(channel: &str, address: &str) -> String {
    match channel {
        "email" => match address.split_once('@') {
            Some((local, domain)) => {
                let first: String = local.chars().take(1).collect();
                format!("{first}…@{domain}")
            }
            None => "…".to_string(),
        },
        "webhook" => match url::Url::parse(address) {
            Ok(u) => format!("{}://{}/…", u.scheme(), u.host_str().unwrap_or("")),
            Err(_) => "…".to_string(),
        },
        _ => {
            let chars: Vec<char> = address.chars().collect();
            if chars.len() <= 4 {
                "…".to_string()
            } else {
                let tail: String = chars[chars.len() - 3..].iter().collect();
                format!("…{tail}")
            }
        }
    }
}

/// Drop old delivered or abandoned messages and sign-ups never confirmed.
pub async fn prune(pool: &Pool, now: i64) -> Result<()> {
    sqlx::query("DELETE FROM outbox WHERE sent_at < ? OR failed_at < ?")
        .bind(now - OUTBOX_KEEP_SECS)
        .bind(now - OUTBOX_KEEP_SECS)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM subscribers WHERE status = 'pending' AND confirm_expires < ?")
        .bind(now - PENDING_KEEP_SECS)
        .execute(pool)
        .await?;
    Ok(())
}

/// The incident state a /manage update moves to, as a subscriber change.
pub fn change_for(state: IncidentState) -> IncidentChange {
    if state == IncidentState::Resolved {
        IncidentChange::Resolved
    } else {
        IncidentChange::Updated
    }
}

#[cfg(test)]
mod tests;
