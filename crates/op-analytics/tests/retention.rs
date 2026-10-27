//! Raw telemetry retention, and a report resent after its day was rolled to
//! Parquet stored once.

use chrono::{DateTime, Duration, Utc};
use op_analytics::rollup::{self, Retention};
use op_analytics::telemetry::{day_path, list_days};
use op_core::{Ctx, time};

const DAY_MS: i64 = 86_400_000;

fn midnight(t: DateTime<Utc>) -> i64 {
    t.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis()
}

fn parquet_rows(path: &std::path::Path) -> i64 {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    SerializedFileReader::new(std::fs::File::open(path).unwrap()).unwrap().metadata().file_metadata().num_rows()
}

/// `n` fixes and one warn cue a minute apart from `from`, for col_1.
async fn report(ctx: &Ctx, from: i64, n: i64) {
    for i in 0..n {
        let t = from + i * 60_000;
        sqlx::query(
            "INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m) VALUES ('col_1', 'herd_1', 'x', ?, -93.62, 42.03, 3.0) ON CONFLICT DO NOTHING",
        )
        .bind(t)
        .execute(ctx.db())
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO cues (collar_id, herd_id, at, t, level, margin_m, kind) VALUES ('col_1', 'herd_1', 'x', ?, 1, 2.0, 'warn') ON CONFLICT DO NOTHING",
    )
    .bind(from)
    .execute(ctx.db())
    .await
    .unwrap();
}

async fn count(ctx: &Ctx, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(ctx.db()).await.unwrap()
}

#[tokio::test]
async fn a_report_resent_after_its_day_rolled_is_kept_once() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let now = time::now();
    let day = midnight(now) - 5 * DAY_MS;
    report(&ctx, day + 3_600_000, 10).await;
    rollup::rollup(&ctx, 3, now).await.unwrap();
    let date = time::from_unix_ms(day).date_naive();
    assert_eq!(parquet_rows(&day_path(ctx.data_dir(), "fixes", date)), 10);
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM fixes").await, 0);

    // The collar resends the same report (the rows left SQLite, so it lands again).
    report(&ctx, day + 3_600_000, 10).await;
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM fixes").await, 10);
    rollup::rollup(&ctx, 3, now).await.unwrap();
    assert_eq!(parquet_rows(&day_path(ctx.data_dir(), "fixes", date)), 10);
    assert_eq!(parquet_rows(&day_path(ctx.data_dir(), "cues", date)), 1);
}

#[tokio::test]
async fn raw_telemetry_past_its_retention_goes_and_the_rest_stays() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let now = time::now();
    for days_ago in [40, 20, 1] {
        report(&ctx, midnight(now) - days_ago * DAY_MS + 3_600_000, 3).await;
    }
    rollup::rollup(&ctx, 3, now).await.unwrap();
    assert_eq!(list_days(ctx.data_dir(), "fixes").len(), 2);
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM fixes").await, 3, "the hot day stays in SQLite");

    // The default keeps everything.
    assert_eq!(rollup::retention(&ctx).await.unwrap(), Retention::default());
    let none = rollup::prune(&ctx, &Retention::default(), now).await.unwrap();
    assert!(none.is_empty());

    let keep = Retention { fixes_days: Some(30), cues_days: None, health_days: None };
    let gone = rollup::prune(&ctx, &keep, now).await.unwrap();
    assert_eq!(gone.len(), 1);
    assert_eq!((gone[0].table, gone[0].days, gone[0].rows), ("fixes", 1, 0));
    let left: Vec<i64> = list_days(ctx.data_dir(), "fixes").iter().map(|(d, _)| (now.date_naive() - *d).num_days()).collect();
    assert_eq!(left, [20]);
    assert_eq!(list_days(ctx.data_dir(), "cues").len(), 2, "cues keep their own retention");

    // Shorter than the hot days: SQLite rows past it go too.
    let later = now + Duration::days(9);
    let gone = rollup::prune(&ctx, &Retention { fixes_days: Some(7), ..Default::default() }, later).await.unwrap();
    assert_eq!((gone[0].days, gone[0].rows), (1, 3));
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM fixes").await, 0);

    assert!(Retention { cues_days: Some(3), ..Default::default() }.validate().is_err());
}

#[tokio::test]
async fn an_unreadable_retention_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    sqlx::query("INSERT INTO settings (key, value) VALUES ('analytics.retention', '{\"fixes_days\": \"forever\"}')").execute(ctx.db()).await.unwrap();
    assert!(rollup::retention(&ctx).await.is_err());
}
