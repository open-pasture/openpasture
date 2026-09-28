//! Telemetry reads at 250 collars (stream L): a day the rollup has written
//! to Parquet but not yet finished deleting is counted once, by the scans
//! and by the SQL console; scans by time return the same rows as scans by
//! collar, each collar's in time order, and read the table by its time
//! indexes without sorting.

use op_analytics::range::TimeRange;
use op_analytics::telemetry::{Order, Scope, Source, collect, each_fix, hot_scan_sql, scan_in};
use op_core::*;
use sqlx::Row;

const DAY_MS: i64 = 86_400_000;

async fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    (dir, ctx)
}

/// `n` fixes a collar for `collars` collars from `from`, `every` ms apart, collars interleaved as they report.
async fn fill(ctx: &Ctx, collars: i64, from: i64, every: i64, n: i64) {
    sqlx::query(
        "WITH RECURSIVE c(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM c WHERE i < ?2 - 1),
                       s(j) AS (SELECT 0 UNION ALL SELECT j + 1 FROM s WHERE j < ?4 - 1)
         INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m, sats)
         SELECT printf('col_%d', i), 'herd_1', 'x', ?1 + j * ?3 + i * 7, -93.62 + i * 0.0001, 42.03 + j * 0.000001, 3.0, 9
         FROM s CROSS JOIN c",
    )
    .bind(from)
    .bind(collars)
    .bind(every)
    .bind(n)
    .execute(ctx.db())
    .await
    .unwrap();
}

async fn scanned(ctx: &Ctx, range: TimeRange, order: Order) -> Vec<(String, i64)> {
    let batches = collect(scan_in(ctx, "fixes", range, Scope::default(), Source::All, order)).await.unwrap();
    let mut out = Vec::new();
    for b in &batches {
        each_fix(b, |f| out.push((f.collar_id.to_owned(), f.t)));
    }
    out
}

#[tokio::test]
async fn a_day_the_rollup_is_still_deleting_is_counted_once() {
    let (_d, ctx) = ctx().await;
    let now = time::now();
    let day = (now - chrono::Duration::days(5)).date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis();
    fill(&ctx, 3, day + 3_600_000, 60_000, 100).await;
    // Keep the rows as they were, roll the day, then put them back: the
    // state while the rollup's deletes are still running.
    let rows: Vec<(i64, String, i64, f64, f64)> = sqlx::query_as("SELECT id, collar_id, t, lon, lat FROM fixes ORDER BY id").fetch_all(ctx.db()).await.unwrap();
    let rolled = op_analytics::rollup::rollup(&ctx, 3, now).await.unwrap();
    assert_eq!(rolled.iter().find(|r| r.table == "fixes").map(|r| r.rows), Some(300));
    for (id, c, t, lon, lat) in &rows {
        sqlx::query("INSERT INTO fixes (id, collar_id, herd_id, at, t, lon, lat, accuracy_m, sats) VALUES (?, ?, 'herd_1', 'x', ?, ?, ?, 3.0, 9)")
            .bind(id)
            .bind(c)
            .bind(t)
            .bind(lon)
            .bind(lat)
            .execute(ctx.db())
            .await
            .unwrap();
    }
    // And a late fix for that day, stored after the rollup: in SQLite only.
    sqlx::query("INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m, sats) VALUES ('col_0', 'herd_1', 'x', ?, -93.62, 42.03, 3.0, 9)")
        .bind(day + 20 * 3_600_000)
        .execute(ctx.db())
        .await
        .unwrap();
    let range = TimeRange::new(time::from_unix_ms(day), time::from_unix_ms(day + DAY_MS));
    for order in [Order::Collar, Order::Time] {
        assert_eq!(scanned(&ctx, range, order).await.len(), 301, "{order:?}");
    }
    // Only SQLite asked for: all of it.
    let hot = collect(scan_in(&ctx, "fixes", range, Scope::default(), Source::Hot, Order::Collar)).await.unwrap();
    assert_eq!(hot.iter().map(|b| b.num_rows()).sum::<usize>(), 301);
    let r = op_analytics::sql::run(&ctx, "SELECT COUNT(*) AS n FROM fixes", 10).await.unwrap();
    assert_eq!(r.rows[0][0], serde_json::json!(301), "the SQL console counts each fix once");
}

#[tokio::test]
async fn scans_by_time_return_the_rows_by_collar_each_collar_in_time_order() {
    let (_d, ctx) = ctx().await;
    let now = time::now().timestamp_millis();
    fill(&ctx, 5, now - 3_600_000, 5_000, 200).await;
    // A late fix, stored after newer ones.
    sqlx::query("INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m, sats) VALUES ('col_2', 'herd_1', 'x', ?, -93.62, 42.03, 3.0, 9)")
        .bind(now - 3_000_000 + 1)
        .execute(ctx.db())
        .await
        .unwrap();
    let range = TimeRange::new(time::from_unix_ms(now - 7_200_000), time::from_unix_ms(now));
    let (by_collar, by_time) = (scanned(&ctx, range, Order::Collar).await, scanned(&ctx, range, Order::Time).await);
    assert_eq!(by_time.len(), 1001);
    let (mut a, mut b) = (by_collar.clone(), by_time.clone());
    a.sort();
    b.sort();
    assert_eq!(a, b, "the same rows");
    for c in 0..5 {
        let ts: Vec<i64> = by_time.iter().filter(|(id, _)| *id == format!("col_{c}")).map(|(_, t)| *t).collect();
        assert!(ts.windows(2).all(|w| w[0] <= w[1]), "col_{c} in time order");
    }
    assert!(by_time.windows(2).any(|w| w[0].0 != w[1].0), "collars interleave");
}

#[tokio::test]
async fn scans_by_time_read_the_time_indexes_without_a_sort() {
    let (_d, ctx) = ctx().await;
    let schema = op_analytics::schema::table_schema(ctx.db(), "fixes").await.unwrap();
    for (herd, collars, index) in [(true, None, "fixes_herd_t"), (false, Some(250), "fixes_t"), (false, None, "fixes_t")] {
        let sql = hot_scan_sql(&schema.select_list(), "fixes", herd, collars, Order::Time, 2);
        let plan: Vec<String> = sqlx::query(&format!("EXPLAIN QUERY PLAN {sql}")).fetch_all(ctx.db()).await.unwrap().iter().map(|r| r.get("detail")).collect();
        let plan = plan.join(" | ");
        assert!(plan.contains(&format!("INDEX {index} ")), "{index}: {plan}");
        assert!(!plan.contains("TEMP B-TREE") && !plan.contains("SCAN fixes"), "{plan}");
    }
}

/// A fix a day for about three years, rolled into a Parquet file a day, and
/// today's in SQLite: the SQL console, scans, health and the heatmap still
/// answer (one filter term a rolled day ran past SQLite's 1000-deep
/// expression limit).
#[tokio::test]
async fn years_of_rolled_days_leave_every_read_working() {
    let (_d, ctx) = ctx().await;
    let now = time::now();
    let today = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis();
    let days = 1_100;
    fill(&ctx, 1, today - days * DAY_MS + 3_600_000, DAY_MS, days).await;
    let rolled = op_analytics::rollup::rollup(&ctx, 3, now).await.unwrap();
    assert!(rolled.iter().filter(|r| r.table == "fixes").count() >= 1_000, "{} days rolled", rolled.len());
    let r = op_analytics::sql::run(&ctx, "SELECT COUNT(*) AS n FROM fixes", 10).await.unwrap();
    assert_eq!(r.rows[0][0], serde_json::json!(days));
    let range = TimeRange::new(time::from_unix_ms(today - days * DAY_MS), now);
    assert_eq!(scanned(&ctx, range, Order::Time).await.len() as i64, days);
    let heat = op_analytics::routes::heat_points(&ctx, range, None, None, 10.0, false).await.unwrap();
    assert_eq!(heat.iter().map(|p| p[2]).sum::<f64>() as i64, days);
    // The SQL console's filter only names the days that can still have rows in SQLite.
    let rolled = op_analytics::telemetry::rolled_days(ctx.data_dir(), "fixes", None).unwrap();
    assert!(op_analytics::telemetry::live(&ctx, "fixes", rolled).await.unwrap().len() <= 1);
}

/// A fix stored after its day went to Parquet is in SQLite only until the
/// next rollup: behaviour counts it, as the scans do.
#[tokio::test]
async fn behaviour_counts_a_late_fix_for_a_day_already_in_parquet() {
    let (_d, ctx) = ctx().await;
    let now = time::now();
    let day = (now - chrono::Duration::days(5)).date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis();
    fill(&ctx, 3, day + 3_600_000, 60_000, 100).await;
    op_analytics::rollup::rollup(&ctx, 3, now).await.unwrap();
    sqlx::query("INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m, sats) VALUES ('col_0', 'herd_1', 'x', ?, -93.62, 42.03, 3.0, 9)")
        .bind(day + 20 * 3_600_000)
        .execute(ctx.db())
        .await
        .unwrap();
    let range = TimeRange::new(time::from_unix_ms(day), time::from_unix_ms(day + DAY_MS));
    assert_eq!(scanned(&ctx, range, Order::Time).await.len(), 301);
    let b = op_analytics::routes::animal_behaviour(&ctx, range, None).await.unwrap();
    assert_eq!(b.iter().map(|a| a.fixes).sum::<i64>(), 301, "behaviour counts the late fix too");
}

/// A collar with no configured cadence that reported a fix a minute on the
/// older days (in Parquet now) and every 5 s for the last day and a half:
/// most of its gaps are 5 s, so that is its cadence, not the cold days'.
#[tokio::test]
async fn health_cadence_counts_the_hot_fixes_too() {
    let (_d, ctx) = ctx().await;
    let now = time::now();
    let ms = now.timestamp_millis();
    let day = (now - chrono::Duration::days(6)).date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis();
    fill(&ctx, 1, day, 60_000, 3 * 1_440).await;
    op_analytics::rollup::rollup(&ctx, 3, now).await.unwrap();
    fill(&ctx, 1, ms - 36 * 3_600_000, 5_000, 36 * 720).await;
    let info = op_analytics::routes::CollarInfo {
        id: "col_0".into(),
        name: "c0".into(),
        herd_id: "herd_1".into(),
        animal_id: None,
        battery: None,
        boundary_version: None,
        created_ms: None,
        cadence_s: None,
    };
    let range = TimeRange::new(now - chrono::Duration::days(7), now);
    let h = op_analytics::routes::collar_health(&ctx, range, 6 * 3_600_000, vec![info]).await.unwrap();
    assert_eq!(h[0].cadence_s, Some(5.0));
}

#[tokio::test]
async fn health_cadence_reads_the_collar_index_alone() {
    let (_d, ctx) = ctx().await;
    let sql = op_analytics::routes::health_gaps_sql();
    let plan: Vec<String> = sqlx::query(&format!("EXPLAIN QUERY PLAN {sql}")).fetch_all(ctx.db()).await.unwrap().iter().map(|r| r.get("detail")).collect();
    let plan = plan.join(" | ");
    assert!(plan.contains("COVERING INDEX fixes_collar_t") && !plan.contains("SCAN fixes"), "{plan}");
}

/// An animal's collar taken off partway through the day keeps reporting with
/// no animal: the day it tracked the animal is still that animal's.
#[tokio::test]
async fn behaviour_keeps_the_animal_a_collar_tracked_before_it_was_unlinked() {
    let (_d, ctx) = ctx().await;
    let now = time::now().timestamp_millis();
    fill(&ctx, 1, now - 6 * 3_600_000, 5_000, 4_000).await;
    sqlx::query("UPDATE fixes SET animal_id = 'ani_a' WHERE t < ?").bind(now - 6 * 3_600_000 + 2_000 * 5_000).execute(ctx.db()).await.unwrap();
    let range = TimeRange::new(time::from_unix_ms(now - 12 * 3_600_000), time::from_unix_ms(now));
    let b = op_analytics::routes::animal_behaviour(&ctx, range, None).await.unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!((b[0].animal_id.as_deref(), b[0].fixes), (Some("ani_a"), 4_000));
}
