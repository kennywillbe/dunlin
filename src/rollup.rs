//! Hourly aggregation and retention pruning.

use anyhow::Result;

use crate::db::{self, Pool};

const LAST_ROLLUP: &str = "last_rollup";

/// Days of probe results to keep: as long as the hourly aggregates, but never
/// less than the status page's strip, plus a day so the oldest local day is
/// still whole in any timezone.
fn result_days(hourly_days: u32) -> i64 {
    hourly_days.max(crate::days::STRIP_DAYS as u32) as i64 + 1
}

/// Aggregate completed hours into `hourly`, then drop data past retention.
///
/// Idempotent: the range starts at the last recorded rollup boundary, so a
/// restart or a long gap is caught up without losing or double counting rows.
pub async fn run_once(pool: &Pool, now: i64, raw_days: u32, hourly_days: u32) -> Result<()> {
    let hour_start = now.div_euclid(3600) * 3600;
    let from = db::get_meta(pool, LAST_ROLLUP)
        .await?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(hour_start - 7200)
        .min(hour_start);

    if from < hour_start {
        db::rollup_range(pool, from, hour_start).await?;
        db::set_meta(pool, LAST_ROLLUP, &hour_start.to_string()).await?;
    }

    db::prune_samples_before(pool, now - raw_days as i64 * 86_400).await?;
    db::prune_series_before(pool, now - raw_days as i64 * 86_400).await?;
    db::prune_hourly_before(pool, now - hourly_days as i64 * 86_400).await?;
    db::prune_check_results_before(pool, now - result_days(hourly_days) * 86_400).await?;
    db::prune_sessions(pool, now).await?;
    crate::subscriptions::prune(pool, now).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Sample;

    fn sample(ts: i64, v: f64) -> Sample {
        Sample {
            ts,
            scope: "host".into(),
            metric: "cpu_pct".into(),
            key: String::new(),
            value: v,
        }
    }

    #[tokio::test]
    async fn rolls_up_and_prunes() {
        let pool = db::connect_memory().await.unwrap();
        let now: i64 = 100 * 86_400; // arbitrary
        let hour = now.div_euclid(3600) * 3600;

        // Samples in the two hours before `now`.
        db::insert_samples(
            &pool,
            &[sample(hour - 3600, 10.0), sample(hour - 3600 + 60, 30.0)],
        )
        .await
        .unwrap();
        db::insert_samples(&pool, &[sample(hour - 7200, 5.0)])
            .await
            .unwrap();
        // A sample old enough to be pruned (raw retention 7 days).
        db::insert_samples(&pool, &[sample(now - 10 * 86_400, 1.0)])
            .await
            .unwrap();

        run_once(&pool, now, 7, 90).await.unwrap();

        let h = db::hourly_series(&pool, "host", "cpu_pct", "", 0, now)
            .await
            .unwrap();
        assert_eq!(h.len(), 2, "{h:?}");
        let two_hours = h.iter().find(|r| r.0 == hour - 7200).unwrap();
        assert_eq!(two_hours.2, 5.0);
        let one_hour = h.iter().find(|r| r.0 == hour - 3600).unwrap();
        assert_eq!(one_hour.1, 10.0); // min
        assert_eq!(one_hour.2, 20.0); // avg
        assert_eq!(one_hour.3, 30.0); // max

        // Old raw row pruned.
        let raw = db::series(&pool, "host", "cpu_pct", "", 0, now)
            .await
            .unwrap();
        assert!(raw.iter().all(|(ts, _)| *ts > now - 8 * 86_400));
        assert_eq!(
            db::get_meta(&pool, LAST_ROLLUP).await.unwrap(),
            Some(hour.to_string())
        );

        // Running again does not duplicate or move backwards.
        run_once(&pool, now, 7, 90).await.unwrap();
        let h2 = db::hourly_series(&pool, "host", "cpu_pct", "", 0, now)
            .await
            .unwrap();
        assert_eq!(h2.len(), 2);
    }

    #[tokio::test]
    async fn prunes_check_results_past_the_strip() {
        let pool = db::connect_memory().await.unwrap();
        let now: i64 = 400 * 86_400;
        let result = |ts| crate::models::CheckResult {
            ts,
            check_id: "c".into(),
            ok: true,
            degraded: false,
            latency_ms: None,
            message: None,
        };
        for ts in [now - 120 * 86_400, now - 91 * 86_400 + 60, now - 60] {
            db::insert_check_result(&pool, &result(ts)).await.unwrap();
        }
        // hourly_days below the strip still keeps the strip's 90 days.
        run_once(&pool, now, 7, 30).await.unwrap();
        let (total, _) = db::uptime_between(&pool, "c", 0, now).await.unwrap();
        assert_eq!(total, 2);

        // A longer hourly retention keeps results as long.
        db::insert_check_result(&pool, &result(now - 150 * 86_400))
            .await
            .unwrap();
        run_once(&pool, now, 7, 200).await.unwrap();
        let (total, _) = db::uptime_between(&pool, "c", 0, now).await.unwrap();
        assert_eq!(total, 3);
    }

    #[tokio::test]
    async fn prunes_hourly_past_retention() {
        let pool = db::connect_memory().await.unwrap();
        let now: i64 = 200 * 86_400;
        db::insert_samples(&pool, &[sample(now - 100 * 86_400, 1.0)])
            .await
            .unwrap();
        // Pretend we have been running: the last rollup was just before the
        // sample, so the aggregator does see it and then retention removes it.
        db::set_meta(&pool, LAST_ROLLUP, &(now - 101 * 86_400).to_string())
            .await
            .unwrap();
        run_once(&pool, now, 7, 90).await.unwrap();
        let h = db::hourly_series(&pool, "host", "cpu_pct", "", 0, now)
            .await
            .unwrap();
        assert!(h.is_empty());
    }

    #[tokio::test]
    async fn series_rows_track_and_prune_with_their_samples() {
        let pool = db::connect_memory().await.unwrap();
        let now: i64 = 300 * 86_400;

        // A current series and one whose only sample is past raw retention.
        db::insert_samples(&pool, &[sample(now - 60, 1.0)])
            .await
            .unwrap();
        db::insert_samples(
            &pool,
            &[Sample {
                ts: now - 10 * 86_400,
                scope: "host".into(),
                metric: "gone".into(),
                key: String::new(),
                value: 1.0,
            }],
        )
        .await
        .unwrap();

        let ids = db::distinct_series(&pool).await.unwrap();
        assert!(ids.contains(&("host".into(), "cpu_pct".into(), String::new())));
        assert!(ids.contains(&("host".into(), "gone".into(), String::new())));

        run_once(&pool, now, 7, 90).await.unwrap();

        let ids = db::distinct_series(&pool).await.unwrap();
        assert!(ids.contains(&("host".into(), "cpu_pct".into(), String::new())));
        assert!(
            !ids.contains(&("host".into(), "gone".into(), String::new())),
            "a series whose raw rows were pruned must leave the picker: {ids:?}"
        );
    }

    #[tokio::test]
    async fn the_picker_reads_the_series_table_not_samples() {
        let pool = db::connect_memory().await.unwrap();
        db::insert_samples(&pool, &[sample(1, 1.0)]).await.unwrap();
        // The old picker scanned `samples`; the new one reads `series`, which
        // only the rollup prunes. Dropping the raw rows directly leaves it
        // visible, so this fails if `distinct_series` goes back to `samples`.
        sqlx::query("DELETE FROM samples")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            db::distinct_series(&pool).await.unwrap(),
            vec![("host".into(), "cpu_pct".into(), String::new())]
        );
    }
}
