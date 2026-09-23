//! SQLite persistence. Runtime queries only, so the crate builds without a
//! database (no compile-time macros).

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::models::{
    CheckResult, Incident, IncidentState, IncidentUpdate, Maintenance, Sample, State,
};

pub type Pool = SqlitePool;

/// Open (creating if needed) the SQLite database and run migrations.
pub async fn connect(path: &Path) -> Result<Pool> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating data dir {}", parent.display()))?;
        }
    }
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening database {}", path.display()))?;
    migrate(&pool).await?;
    Ok(pool)
}

/// In-memory database for tests; one connection so the schema persists.
pub async fn connect_memory() -> Result<Pool> {
    let opts = SqliteConnectOptions::new().in_memory(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

async fn migrate(pool: &Pool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("running migrations")?;
    Ok(())
}

// -- samples ---------------------------------------------------------------

pub async fn insert_samples(pool: &Pool, samples: &[Sample]) -> Result<()> {
    for s in samples {
        sqlx::query(
            "INSERT INTO samples (ts, scope, metric, key, value) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(ts, scope, metric, key) DO UPDATE SET value = excluded.value",
        )
        .bind(s.ts)
        .bind(&s.scope)
        .bind(&s.metric)
        .bind(&s.key)
        .bind(s.value)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn latest_sample(
    pool: &Pool,
    scope: &str,
    metric: &str,
    key: &str,
) -> Result<Option<f64>> {
    let row = sqlx::query(
        "SELECT value FROM samples WHERE scope = ? AND metric = ? AND key = ?
         ORDER BY ts DESC LIMIT 1",
    )
    .bind(scope)
    .bind(metric)
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.get::<f64, _>("value")))
}

/// Raw samples ordered by time.
pub async fn series(
    pool: &Pool,
    scope: &str,
    metric: &str,
    key: &str,
    from: i64,
    to: i64,
) -> Result<Vec<(i64, f64)>> {
    let rows = sqlx::query(
        "SELECT ts, value FROM samples
         WHERE scope = ? AND metric = ? AND key = ? AND ts >= ? AND ts <= ?
         ORDER BY ts",
    )
    .bind(scope)
    .bind(metric)
    .bind(key)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("ts"), r.get("value")))
        .collect())
}

/// Hourly aggregates ordered by time.
pub async fn hourly_series(
    pool: &Pool,
    scope: &str,
    metric: &str,
    key: &str,
    from: i64,
    to: i64,
) -> Result<Vec<(i64, f64, f64, f64)>> {
    let rows = sqlx::query(
        "SELECT hour, min, avg, max FROM hourly
         WHERE scope = ? AND metric = ? AND key = ? AND hour >= ? AND hour <= ?
         ORDER BY hour",
    )
    .bind(scope)
    .bind(metric)
    .bind(key)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("hour"), r.get("min"), r.get("avg"), r.get("max")))
        .collect())
}

/// Recompute hourly aggregates for one or more hour buckets.
pub async fn rollup_range(pool: &Pool, from: i64, to: i64) -> Result<u64> {
    let res = sqlx::query(
        "INSERT INTO hourly (hour, scope, metric, key, min, avg, max)
         SELECT (ts / 3600) * 3600, scope, metric, key, min(value), avg(value), max(value)
         FROM samples WHERE ts >= ? AND ts < ?
         GROUP BY (ts / 3600) * 3600, scope, metric, key
         ON CONFLICT(hour, scope, metric, key) DO UPDATE SET
            min = excluded.min, avg = excluded.avg, max = excluded.max",
    )
    .bind(from)
    .bind(to)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

pub async fn prune_samples_before(pool: &Pool, cutoff: i64) -> Result<u64> {
    let res = sqlx::query("DELETE FROM samples WHERE ts < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

pub async fn prune_hourly_before(pool: &Pool, cutoff: i64) -> Result<u64> {
    let res = sqlx::query("DELETE FROM hourly WHERE hour < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

// -- check results ---------------------------------------------------------

pub async fn insert_check_result(pool: &Pool, r: &CheckResult) -> Result<()> {
    sqlx::query(
        "INSERT INTO check_results (ts, check_id, ok, degraded, latency_ms, message)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(r.ts)
    .bind(&r.check_id)
    .bind(r.ok as i64)
    .bind(r.degraded as i64)
    .bind(r.latency_ms)
    .bind(&r.message)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn last_check_result(pool: &Pool, check_id: &str) -> Result<Option<CheckResult>> {
    let row = sqlx::query(
        "SELECT ts, check_id, ok, degraded, latency_ms, message FROM check_results
         WHERE check_id = ? ORDER BY ts DESC, id DESC LIMIT 1",
    )
    .bind(check_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| CheckResult {
        ts: r.get("ts"),
        check_id: r.get("check_id"),
        ok: r.get::<i64, _>("ok") != 0,
        degraded: r.get::<i64, _>("degraded") != 0,
        latency_ms: r.get("latency_ms"),
        message: r.get("message"),
    }))
}

/// (total, up) probe counts for a check in `[from, to)`.
pub async fn uptime_between(pool: &Pool, check_id: &str, from: i64, to: i64) -> Result<(i64, i64)> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS total, COALESCE(SUM(ok), 0) AS up
         FROM check_results WHERE check_id = ? AND ts >= ? AND ts < ?",
    )
    .bind(check_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;
    Ok((row.get("total"), row.get("up")))
}

/// Per-day (UTC) uptime as `(day_start, total, up)` for a check.
pub async fn daily_uptime(pool: &Pool, check_id: &str, from: i64) -> Result<Vec<(i64, i64, i64)>> {
    let rows = sqlx::query(
        "SELECT (ts / 86400) * 86400 AS day, COUNT(*) AS total, COALESCE(SUM(ok),0) AS up
         FROM check_results WHERE check_id = ? AND ts >= ?
         GROUP BY day ORDER BY day",
    )
    .bind(check_id)
    .bind(from)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("day"), r.get("total"), r.get("up")))
        .collect())
}

pub async fn count_results_since(pool: &Pool, from: i64) -> Result<i64> {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM check_results WHERE ts >= ?")
        .bind(from)
        .fetch_one(pool)
        .await?;
    Ok(row.get("n"))
}

/// Count leading failures from the most recent result backwards, up to `limit`.
pub async fn consecutive_failures(pool: &Pool, check_id: &str, limit: i64) -> Result<i64> {
    let rows = sqlx::query(
        "SELECT ok FROM check_results WHERE check_id = ? ORDER BY ts DESC, id DESC LIMIT ?",
    )
    .bind(check_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut count = 0;
    for r in rows {
        if r.get::<i64, _>("ok") == 0 {
            count += 1;
        } else {
            break;
        }
    }
    Ok(count)
}

/// Distinct series present in the raw samples table, for the metrics picker.
pub async fn distinct_series(pool: &Pool) -> Result<Vec<(String, String, String)>> {
    let rows =
        sqlx::query("SELECT DISTINCT scope, metric, key FROM samples ORDER BY scope, metric, key")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("scope"), r.get("metric"), r.get("key")))
        .collect())
}

pub async fn last_update_message(pool: &Pool, incident_id: i64) -> Result<Option<String>> {
    let row = sqlx::query(
        "SELECT message FROM incident_updates WHERE incident_id = ? ORDER BY ts DESC, id DESC LIMIT 1",
    )
    .bind(incident_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.get("message")))
}

// -- heartbeats ------------------------------------------------------------

pub async fn record_ping(pool: &Pool, check_id: &str, ts: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO heartbeats (check_id, last_ping) VALUES (?, ?)
         ON CONFLICT(check_id) DO UPDATE SET last_ping = excluded.last_ping",
    )
    .bind(check_id)
    .bind(ts)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn last_ping(pool: &Pool, check_id: &str) -> Result<Option<i64>> {
    let row = sqlx::query("SELECT last_ping FROM heartbeats WHERE check_id = ?")
        .bind(check_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get("last_ping")))
}

// -- incidents -------------------------------------------------------------

pub async fn create_incident(
    pool: &Pool,
    component: &str,
    title: &str,
    impact: State,
    state: IncidentState,
    auto: bool,
    ts: i64,
) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO incidents (component, title, state, impact, created_at, auto)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(component)
    .bind(title)
    .bind(state.as_str())
    .bind(impact.as_str())
    .bind(ts)
    .bind(auto as i64)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn add_update(
    pool: &Pool,
    incident_id: i64,
    ts: i64,
    state: IncidentState,
    message: &str,
    auto: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO incident_updates (incident_id, ts, state, message, auto)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(incident_id)
    .bind(ts)
    .bind(state.as_str())
    .bind(message)
    .bind(auto as i64)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn resolve_incident(pool: &Pool, incident_id: i64, ts: i64) -> Result<()> {
    sqlx::query("UPDATE incidents SET resolved_at = ?, state = 'resolved' WHERE id = ?")
        .bind(ts)
        .bind(incident_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_incident_state(pool: &Pool, incident_id: i64, state: IncidentState) -> Result<()> {
    sqlx::query("UPDATE incidents SET state = ? WHERE id = ?")
        .bind(state.as_str())
        .bind(incident_id)
        .execute(pool)
        .await?;
    Ok(())
}

fn row_to_incident(r: &sqlx::sqlite::SqliteRow) -> Incident {
    Incident {
        id: r.get("id"),
        component: r.get("component"),
        title: r.get("title"),
        state: IncidentState::from_name(&r.get::<String, _>("state"))
            .unwrap_or(IncidentState::Investigating),
        impact: State::from_name(&r.get::<String, _>("impact")).unwrap_or(State::MajorOutage),
        created_at: r.get("created_at"),
        resolved_at: r.get("resolved_at"),
        auto: r.get::<i64, _>("auto") != 0,
    }
}

pub async fn incident(pool: &Pool, id: i64) -> Result<Option<Incident>> {
    let row = sqlx::query("SELECT * FROM incidents WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_incident))
}

pub async fn active_incidents(pool: &Pool) -> Result<Vec<Incident>> {
    let rows =
        sqlx::query("SELECT * FROM incidents WHERE resolved_at IS NULL ORDER BY created_at DESC")
            .fetch_all(pool)
            .await?;
    Ok(rows.iter().map(row_to_incident).collect())
}

pub async fn active_incident_for(pool: &Pool, component: &str) -> Result<Option<Incident>> {
    let row = sqlx::query(
        "SELECT * FROM incidents WHERE resolved_at IS NULL AND component = ?
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(component)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_incident))
}

pub async fn recent_incidents(pool: &Pool, limit: i64) -> Result<Vec<Incident>> {
    let rows = sqlx::query("SELECT * FROM incidents ORDER BY created_at DESC LIMIT ?")
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(row_to_incident).collect())
}

pub async fn count_incidents_since(pool: &Pool, from: i64) -> Result<i64> {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM incidents WHERE created_at >= ?")
        .bind(from)
        .fetch_one(pool)
        .await?;
    Ok(row.get("n"))
}

pub async fn incident_updates(pool: &Pool, incident_id: i64) -> Result<Vec<IncidentUpdate>> {
    let rows = sqlx::query(
        "SELECT id, incident_id, ts, state, message, auto FROM incident_updates
         WHERE incident_id = ? ORDER BY ts, id",
    )
    .bind(incident_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| IncidentUpdate {
            id: r.get("id"),
            incident_id: r.get("incident_id"),
            ts: r.get("ts"),
            state: IncidentState::from_name(&r.get::<String, _>("state"))
                .unwrap_or(IncidentState::Investigating),
            message: r.get("message"),
            auto: r.get::<i64, _>("auto") != 0,
        })
        .collect())
}

// -- maintenance -----------------------------------------------------------

pub async fn create_maintenance(
    pool: &Pool,
    component: &str,
    note: &str,
    starts_at: i64,
    ends_at: i64,
    created_at: i64,
) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO maintenance (component, note, starts_at, ends_at, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(component)
    .bind(note)
    .bind(starts_at)
    .bind(ends_at)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

fn row_to_maintenance(r: &sqlx::sqlite::SqliteRow) -> Maintenance {
    Maintenance {
        id: r.get("id"),
        component: r.get("component"),
        note: r.get("note"),
        starts_at: r.get("starts_at"),
        ends_at: r.get("ends_at"),
    }
}

pub async fn active_maintenance(pool: &Pool, now: i64) -> Result<Vec<Maintenance>> {
    let rows = sqlx::query(
        "SELECT * FROM maintenance WHERE starts_at <= ? AND ends_at > ? ORDER BY starts_at",
    )
    .bind(now)
    .bind(now)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_maintenance).collect())
}

pub async fn all_maintenance(pool: &Pool) -> Result<Vec<Maintenance>> {
    let rows = sqlx::query("SELECT * FROM maintenance ORDER BY ends_at DESC LIMIT 100")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(row_to_maintenance).collect())
}

pub async fn end_maintenance(pool: &Pool, id: i64, now: i64) -> Result<bool> {
    let res = sqlx::query("UPDATE maintenance SET ends_at = ? WHERE id = ? AND ends_at > ?")
        .bind(now)
        .bind(id)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

// -- sessions --------------------------------------------------------------

pub async fn create_session(
    pool: &Pool,
    token_hash: &str,
    created_at: i64,
    expires_at: i64,
) -> Result<()> {
    sqlx::query("INSERT INTO sessions (token_hash, created_at, expires_at) VALUES (?, ?, ?)")
        .bind(token_hash)
        .bind(created_at)
        .bind(expires_at)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn session_valid(pool: &Pool, token_hash: &str, now: i64) -> Result<bool> {
    let row = sqlx::query("SELECT 1 FROM sessions WHERE token_hash = ? AND expires_at > ?")
        .bind(token_hash)
        .bind(now)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

pub async fn delete_session(pool: &Pool, token_hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn prune_sessions(pool: &Pool, now: i64) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

// -- meta ------------------------------------------------------------------

pub async fn get_meta(pool: &Pool, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM meta WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.get("value")))
}

pub async fn set_meta(pool: &Pool, key: &str, value: &str) -> Result<()> {
    sqlx::query("INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mem() -> Pool {
        connect_memory().await.unwrap()
    }

    #[tokio::test]
    async fn samples_series_and_latest() {
        let pool = mem().await;
        let s = |ts, v| Sample {
            ts,
            scope: "host".into(),
            metric: "cpu_pct".into(),
            key: String::new(),
            value: v,
        };
        insert_samples(&pool, &[s(60, 1.0), s(120, 2.0), s(180, 3.0)])
            .await
            .unwrap();
        // upsert same key
        insert_samples(&pool, &[s(180, 5.0)]).await.unwrap();
        assert_eq!(
            latest_sample(&pool, "host", "cpu_pct", "").await.unwrap(),
            Some(5.0)
        );
        let ser = series(&pool, "host", "cpu_pct", "", 0, 1000).await.unwrap();
        assert_eq!(ser, vec![(60, 1.0), (120, 2.0), (180, 5.0)]);
    }

    #[tokio::test]
    async fn rollup_and_prune() {
        let pool = mem().await;
        // hour 0: 0..3600
        let mut samples = Vec::new();
        for i in 0..5 {
            samples.push(Sample {
                ts: 60 * i,
                scope: "host".into(),
                metric: "m".into(),
                key: String::new(),
                value: i as f64,
            });
        }
        insert_samples(&pool, &samples).await.unwrap();
        rollup_range(&pool, 0, 3600).await.unwrap();
        let h = hourly_series(&pool, "host", "m", "", 0, 7200)
            .await
            .unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].1, 0.0); // min
        assert_eq!(h[0].2, 2.0); // avg
        assert_eq!(h[0].3, 4.0); // max
        assert_eq!(prune_samples_before(&pool, 3600).await.unwrap(), 5);
        assert_eq!(
            hourly_series(&pool, "host", "m", "", 0, 7200)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn incident_lifecycle() {
        let pool = mem().await;
        let id = create_incident(
            &pool,
            "web",
            "Down",
            State::MajorOutage,
            IncidentState::Investigating,
            true,
            100,
        )
        .await
        .unwrap();
        add_update(
            &pool,
            id,
            100,
            IncidentState::Investigating,
            "auto opened",
            true,
        )
        .await
        .unwrap();
        assert!(active_incident_for(&pool, "web").await.unwrap().is_some());
        resolve_incident(&pool, id, 200).await.unwrap();
        assert!(active_incident_for(&pool, "web").await.unwrap().is_none());
        let inc = incident(&pool, id).await.unwrap().unwrap();
        assert_eq!(inc.resolved_at, Some(200));
        assert_eq!(inc.state, IncidentState::Resolved);
        assert_eq!(incident_updates(&pool, id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn heartbeat_roundtrip() {
        let pool = mem().await;
        assert_eq!(last_ping(&pool, "hb").await.unwrap(), None);
        record_ping(&pool, "hb", 111).await.unwrap();
        record_ping(&pool, "hb", 222).await.unwrap();
        assert_eq!(last_ping(&pool, "hb").await.unwrap(), Some(222));
    }

    #[tokio::test]
    async fn maintenance_window() {
        let pool = mem().await;
        create_maintenance(&pool, "web", "note", 100, 200, 90)
            .await
            .unwrap();
        assert_eq!(active_maintenance(&pool, 150).await.unwrap().len(), 1);
        assert_eq!(active_maintenance(&pool, 250).await.unwrap().len(), 0);
        let id = all_maintenance(&pool).await.unwrap()[0].id;
        assert!(end_maintenance(&pool, id, 150).await.unwrap());
        assert_eq!(active_maintenance(&pool, 150).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn sessions() {
        let pool = mem().await;
        create_session(&pool, "h", 0, 100).await.unwrap();
        assert!(session_valid(&pool, "h", 50).await.unwrap());
        assert!(!session_valid(&pool, "h", 150).await.unwrap());
        delete_session(&pool, "h").await.unwrap();
        assert!(!session_valid(&pool, "h", 10).await.unwrap());
    }
}
