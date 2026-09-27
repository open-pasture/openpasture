//! The day aggregator (field-ready G): `coverage_days` and `battery_days`
//! from fixture fixes and health, in SQLite and in day files.

mod g_support;

use g_support::*;
use op_analytics::days::{Aggregated, aggregate};
use op_core::time;
use serde_json::{Value, json};

/// A completed day three days back: still hot, and no rollup touches it.
fn base() -> i64 {
    midnight(time::now()).timestamp_millis() - 3 * DAY
}

async fn run(app: &App) -> Aggregated {
    aggregate(&app.ctx, time::now()).await.unwrap()
}

/// `coverage_days` of one date as `{"herd cx cy": [n, hist, expected, got]}`.
async fn cells(app: &App, day_ms: i64) -> Value {
    let rows: Vec<(String, i64, i64, i64, String, i64, i64)> =
        sqlx::query_as("SELECT herd_id, cx, cy, n, acc_hist, expected, got FROM coverage_days WHERE date = ? ORDER BY herd_id, cx, cy")
            .bind(day_str(day_ms))
            .fetch_all(app.ctx.db())
            .await
            .unwrap();
    let mut out = serde_json::Map::new();
    for (h, x, y, n, hist, e, g) in rows {
        out.insert(format!("{h} {x} {y}"), json!([n, serde_json::from_str::<Value>(&hist).unwrap(), e, g]));
    }
    Value::Object(out)
}

async fn battery(app: &App, collar: &str, day_ms: i64) -> Option<(f64, f64, f64, f64, i64, i64, i64)> {
    sqlx::query_as("SELECT min, max, mean, last, n, first_t, last_t FROM battery_days WHERE collar_id = ? AND date = ?")
        .bind(collar)
        .bind(day_str(day_ms))
        .fetch_optional(app.ctx.db())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_day_of_fixes_becomes_cells_and_battery_days() {
    let app = App::new(2).await;
    let d = base();
    // col_1: an hour in cell (0,0) at 2.5 m, then an hour in (3,1) at 6 m.
    app.steady("col_1", d + 8 * HOUR, d + 9 * HOUR, 5 * SEC, cell(0, 0), 2.5).await;
    app.steady("col_1", d + 9 * HOUR, d + 10 * HOUR, 5 * SEC, cell(3, 1), 6.0).await;
    // col_2: the first hour in (0,0) at 0.5 m, with nothing from 08:30 to 08:40.
    app.steady("col_2", d + 8 * HOUR, d + 8 * HOUR + 30 * MIN, 5 * SEC, cell(0, 0), 0.5).await;
    app.steady("col_2", d + 8 * HOUR + 40 * MIN, d + 9 * HOUR, 5 * SEC, cell(0, 0), 0.5).await;
    // col_1 reports every 10 minutes, losing 0.1 % each time.
    let readings: Vec<(&str, i64, Option<f64>)> = (0..12).map(|i| ("col_1", d + 8 * HOUR + i * 10 * MIN, Some(0.9 - 0.001 * i as f64))).collect();
    app.health(&readings).await;
    app.health(&[("col_2", d + 8 * HOUR, None)]).await;

    let done = run(&app).await;
    assert_eq!(done.coverage, vec![date(d)]);
    assert_eq!(done.battery, vec![date(d)]);
    // 08:29:55 to 08:40:00 is 605 s: 120 fixes col_2 should have sent.
    assert_eq!(
        cells(&app, d).await,
        json!({
            "herd_1 0 0": [1320, [600, 0, 720, 0, 0, 0, 0, 0], 1440, 1320],
            "herd_1 3 1": [720, [0, 0, 0, 0, 720, 0, 0, 0], 720, 720],
        })
    );
    let (min, max, mean, last, n, first_t, last_t) = battery(&app, "col_1", d).await.unwrap();
    assert!((min - 0.889).abs() < 1e-9 && (max - 0.9).abs() < 1e-9 && (last - 0.889).abs() < 1e-9, "{min} {max} {last}");
    assert!((mean - 0.8945).abs() < 1e-9, "{mean}");
    assert_eq!((n, first_t, last_t), (12, d + 8 * HOUR, d + 8 * HOUR + 110 * MIN));
    // A collar that never sent a battery reading has no battery day.
    assert!(battery(&app, "col_2", d).await.is_none());
    // The grid starts at the farm's centre.
    let (lon0, lat0): (f64, f64) = sqlx::query_as("SELECT lon0, lat0 FROM coverage_grid").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!([lon0, lat0], ORIGIN);
}

#[tokio::test]
async fn a_day_is_redone_as_it_fills_and_left_alone_after() {
    let app = App::new(1).await;
    let d = base();
    app.steady("col_1", d + 8 * HOUR, d + 8 * HOUR + 30 * MIN, 5 * SEC, cell(0, 0), 2.5).await;
    app.health(&[("col_1", d + 8 * HOUR, Some(0.8))]).await;
    assert_eq!(run(&app).await.coverage, vec![date(d)]);
    assert_eq!(cells(&app, d).await["herd_1 0 0"], json!([360, [0, 0, 360, 0, 0, 0, 0, 0], 360, 360]));

    // The rest of the hour arrives: the day is counted again, not added to.
    app.steady("col_1", d + 8 * HOUR + 30 * MIN, d + 9 * HOUR, 5 * SEC, cell(0, 0), 2.5).await;
    app.health(&[("col_1", d + 9 * HOUR, Some(0.7))]).await;
    let done = run(&app).await;
    assert_eq!(done, Aggregated { coverage: vec![date(d)], battery: vec![date(d)] });
    assert_eq!(cells(&app, d).await["herd_1 0 0"], json!([720, [0, 0, 720, 0, 0, 0, 0, 0], 720, 720]));
    assert_eq!(battery(&app, "col_1", d).await.unwrap().4, 2);

    // Nothing new: nothing redone.
    assert_eq!(run(&app).await, Aggregated::default());
    // A fix early in the next day also redoes this one: it closes this day's last gap.
    app.fixes(&[("col_1", d + DAY + HOUR, cell(0, 0), 2.5)]).await;
    assert_eq!(run(&app).await.coverage, vec![date(d), date(d + DAY)]);
}

#[tokio::test]
async fn a_gap_across_midnight_splits_between_the_days() {
    let app = App::new(1).await;
    let d = base();
    // Yesterday until 00:00:35 before midnight in (0,0); today from 00:00:30 in (1,0).
    app.steady("col_1", d - 10 * MIN, d - 30 * SEC, 5 * SEC, cell(0, 0), 2.5).await;
    app.steady("col_1", d + 30 * SEC, d + 10 * MIN, 5 * SEC, cell(1, 0), 2.5).await;
    run(&app).await;
    // 12 fixes were due between 23:59:25 and 00:00:30: 6 before midnight, 6 after,
    // all where the animal was before the gap.
    assert_eq!(cells(&app, d - DAY).await, json!({ "herd_1 0 0": [114, [0, 0, 114, 0, 0, 0, 0, 0], 120, 114] }));
    assert_eq!(
        cells(&app, d).await,
        json!({
            "herd_1 0 0": [0, [0, 0, 0, 0, 0, 0, 0, 0], 6, 0],
            "herd_1 1 0": [114, [0, 0, 114, 0, 0, 0, 0, 0], 114, 114],
        })
    );
}

#[tokio::test]
async fn the_first_run_backfills_day_files_once() {
    let app = App::new(2).await;
    let d = base();
    let (old, older) = (d - 6 * DAY, d - 7 * DAY);
    for day in [older, old] {
        app.steady("col_1", day + 6 * HOUR, day + 7 * HOUR, 5 * SEC, cell(2, 2), 1.5).await;
        app.steady("col_2", day + 6 * HOUR, day + 6 * HOUR + 30 * MIN, 5 * SEC, cell(2, 2), 3.5).await;
        let r: Vec<(&str, i64, Option<f64>)> = (0..24).map(|h| ("col_1", day + h * HOUR, Some(0.6 - 0.001 * h as f64))).collect();
        app.health(&r).await;
    }
    app.steady("col_1", d + 8 * HOUR, d + 8 * HOUR + 10 * MIN, 5 * SEC, cell(0, 0), 2.5).await;
    // The rollup moves the old days' fixes to day files; health of the older
    // day goes to its file too, as a rollup of `health` does.
    let rolled = op_analytics::rollup::rollup(&app.ctx, 3, time::now()).await.unwrap();
    assert_eq!(rolled.iter().filter(|r| r.table == "fixes").count(), 2, "{rolled:?}");
    app.roll_health_day(older).await;
    assert_eq!(app.count(&format!("SELECT COUNT(*) FROM fixes WHERE t < {d}")).await, 0);
    assert_eq!(app.count(&format!("SELECT COUNT(*) FROM health WHERE t < {old}")).await, 0);

    let done = run(&app).await;
    assert_eq!(done.coverage, vec![date(older), date(old), date(d)]);
    assert_eq!(done.battery, vec![date(older), date(old)]);
    let want = json!({ "herd_1 2 2": [1080, [0, 720, 0, 360, 0, 0, 0, 0], 1080, 1080] });
    assert_eq!(cells(&app, older).await, want);
    assert_eq!(cells(&app, old).await, want);
    let from_file = battery(&app, "col_1", older).await.unwrap();
    let from_sqlite = battery(&app, "col_1", old).await.unwrap();
    assert_eq!((from_file.4, from_file.3), (24, 0.6 - 0.001 * 23.0));
    assert_eq!((from_sqlite.4, from_sqlite.3), (24, 0.6 - 0.001 * 23.0));
    assert_eq!(from_file.5 - older, from_sqlite.5 - old);

    // Once only.
    assert_eq!(run(&app).await, Aggregated::default());

    // A fix for the rolled day that arrives late (a collar that was out of
    // reach for days) is added to that day; more than six hours after the
    // collar's last fix, so no fix was due in between.
    app.fixes(&[("col_1", old + 13 * HOUR + 30 * MIN, cell(5, 5), 9.0)]).await;
    assert_eq!(run(&app).await.coverage, vec![date(old)]);
    let c = cells(&app, old).await;
    assert_eq!(c["herd_1 2 2"], want["herd_1 2 2"]);
    assert_eq!(c["herd_1 5 5"], json!([1, [0, 0, 0, 0, 0, 1, 0, 0], 1, 1]));

    // The next rollup merges it into the day file: rows the day has already
    // counted, so nothing is redone.
    op_analytics::rollup::rollup(&app.ctx, 3, time::now()).await.unwrap();
    assert_eq!(app.count(&format!("SELECT COUNT(*) FROM fixes WHERE t < {d}")).await, 0);
    assert_eq!(run(&app).await, Aggregated::default());
    assert_eq!(cells(&app, old).await, c);
}

#[tokio::test]
async fn rows_in_a_day_file_and_still_in_sqlite_count_once() {
    let app = App::new(1).await;
    let d = base() - 2 * DAY;
    app.steady("col_1", d + 6 * HOUR, d + 7 * HOUR, 5 * SEC, cell(0, 0), 2.5).await;
    let r: Vec<(&str, i64, Option<f64>)> = (0..6).map(|i| ("col_1", d + 6 * HOUR + i * 10 * MIN, Some(0.5))).collect();
    app.health(&r).await;
    // A rollup that has written its files and not yet deleted the rows.
    app.copy_day_to_parquet("fixes", d).await;
    app.copy_day_to_parquet("health", d).await;
    run(&app).await;
    assert_eq!(cells(&app, d).await, json!({ "herd_1 0 0": [720, [0, 0, 720, 0, 0, 0, 0, 0], 720, 720] }));
    assert_eq!(battery(&app, "col_1", d).await.unwrap().4, 6);
}

#[tokio::test]
async fn fixes_count_in_the_herd_the_collar_was_in() {
    let app = App::new(1).await;
    let d = base();
    app.steady("col_1", d + 6 * HOUR, d + 6 * HOUR + 10 * MIN, 5 * SEC, cell(0, 0), 2.5).await;
    let moved: Vec<(&str, i64, op_geo::LonLat, f64)> = (0..120).map(|i| ("col_1", d + 6 * HOUR + 10 * MIN + i * 5 * SEC, cell(0, 0), 2.5)).collect();
    app.fixes_in("herd_2", &moved).await;
    run(&app).await;
    assert_eq!(
        cells(&app, d).await,
        json!({
            "herd_1 0 0": [120, [0, 0, 120, 0, 0, 0, 0, 0], 120, 120],
            "herd_2 0 0": [120, [0, 0, 120, 0, 0, 0, 0, 0], 120, 120],
        })
    );
}

#[tokio::test]
async fn a_day_of_250_collars() {
    let app = App::new(250).await;
    let d = base();
    // An hour of fixes every 5 s and a report a minute, for 250 collars: 180,000 fixes.
    let mut fixes = Vec::with_capacity(180_000);
    let mut health = Vec::with_capacity(15_000);
    let names: Vec<String> = (1..=250).map(|i| format!("col_{i}")).collect();
    for (i, c) in names.iter().enumerate() {
        let p = cell((i % 25) as i64, (i / 25) as i64);
        for k in 0..720 {
            fixes.push((c.as_str(), d + 8 * HOUR + k * 5 * SEC, p, 2.5));
        }
        for k in 0..60 {
            health.push((c.as_str(), d + 8 * HOUR + k * MIN, Some(0.9)));
        }
    }
    app.fixes(&fixes).await;
    app.health(&health).await;
    let started = std::time::Instant::now();
    let done = run(&app).await;
    let took = started.elapsed();
    assert_eq!(done.coverage, vec![date(d)]);
    assert_eq!(app.count("SELECT SUM(got) FROM coverage_days").await, 180_000);
    assert_eq!(app.count("SELECT COUNT(*) FROM battery_days").await, 250);
    // Debug build; release is several times faster.
    assert!(took.as_secs_f64() < 20.0, "{took:?}");
    eprintln!("250 collars, 180k fixes, 15k reports: aggregated in {took:?}");
}
