//! Fix rate and cell signal on the coverage grid (field-ready H): each health
//! report counts in the cell of that collar's fix nearest in time; cells need
//! 5 samples; the maps still read only `coverage_days`.

mod g_support;

use axum::http::StatusCode;
use g_support::*;
use op_core::time;
use serde_json::{Value, json};

fn base() -> i64 {
    midnight(time::now()).timestamp_millis() - 2 * DAY
}

fn q(ms: i64) -> String {
    time::to_db(&time::from_unix_ms(ms)).replace(':', "%3A")
}

/// Health reports `(collar, t, fix_attempts, fix_ok, rsrp_dbm)`, as op-ingest stores a v1 report.
async fn reports(app: &App, rows: &[(&str, i64, Option<i64>, Option<i64>, Option<f64>)]) {
    for (c, t, att, ok, rsrp) in rows {
        sqlx::query("INSERT INTO health (collar_id, herd_id, at, t, battery, fixes, cues, fix_attempts, fix_ok, rsrp_dbm, cell_mode) VALUES (?, 'herd_1', ?, ?, 0.8, 12, 0, ?, ?, ?, ?)")
            .bind(*c)
            .bind(time::to_db(&time::from_unix_ms(*t)))
            .bind(*t)
            .bind(*att)
            .bind(*ok)
            .bind(*rsrp)
            .bind(rsrp.map(|_| "ltem"))
            .execute(app.ctx.db())
            .await
            .unwrap();
    }
}

/// col_1: an hour in cell (0,0) then an hour in (1,0), a fix every 5 s and a
/// report every minute: 12 attempts, 11 got and -104 dBm in (0,0), 12 of 12
/// and -96/-98 in (1,0). col_2: fixes in (4,4) but no health fields (a 0.1
/// collar); col_3: GNSS-only board, fix counts, no cell. Plus one report of
/// col_1 an hour after its last fix, counted where it last was.
async fn farm() -> (App, i64) {
    let app = App::new(3).await;
    let d = base();
    app.steady("col_1", d + 8 * HOUR, d + 9 * HOUR, 5 * SEC, cell(0, 0), 2.5).await;
    app.steady("col_1", d + 9 * HOUR, d + 10 * HOUR, 5 * SEC, cell(1, 0), 3.0).await;
    app.steady("col_2", d + 8 * HOUR, d + 9 * HOUR, 5 * SEC, cell(4, 4), 2.0).await;
    app.steady("col_3", d + 8 * HOUR, d + 8 * HOUR + 30 * MIN, 5 * SEC, cell(7, 7), 2.0).await;
    let mut rows = Vec::new();
    for m in 1..=60 {
        rows.push(("col_1", d + 8 * HOUR + m * MIN - 30 * SEC, Some(12), Some(11), Some(-104.0)));
        rows.push(("col_1", d + 9 * HOUR + m * MIN - 30 * SEC, Some(12), Some(12), Some(if m % 2 == 0 { -96.0 } else { -98.0 })));
        rows.push(("col_2", d + 8 * HOUR + m * MIN - 30 * SEC, None, None, None));
        if m <= 30 {
            rows.push(("col_3", d + 8 * HOUR + m * MIN - 30 * SEC, Some(12), Some(6), None));
        }
    }
    // No fix for this report's minute: GNSS lost after 10:00.
    rows.push(("col_1", d + 10 * HOUR + 30 * MIN, Some(12), Some(0), Some(-112.0)));
    reports(&app, &rows).await;
    op_analytics::days::aggregate(&app.ctx, time::now()).await.unwrap();
    (app, d)
}

fn value_at(map: &Value, p: op_geo::LonLat) -> Option<(f64, u64)> {
    map["cells"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| (c[0].as_f64().unwrap() - p[0]).abs() < 1e-6 && (c[1].as_f64().unwrap() - p[1]).abs() < 1e-6)
        .map(|c| (c[2].as_f64().unwrap(), c[3].as_u64().unwrap()))
}

#[tokio::test]
async fn fix_rate_and_cell_signal_from_health_reports() {
    let _one = serial().lock().await;
    let (app, d) = farm().await;
    let range = format!("from={}&to={}", q(d), q(d + DAY));

    let rate = app.get(&format!("/api/coverage?metric=fix_rate&{range}")).await;
    assert_eq!((rate["metric"].as_str(), rate["unit"].as_str()), (Some("fix_rate"), Some("ratio")));
    // 60 reports × 11 of 12 in (0,0); in (1,0) 60 × 12 of 12 plus the late one's 0 of 12.
    assert_eq!(value_at(&rate, cell(0, 0)), Some((0.917, 720)), "{rate}");
    assert_eq!(value_at(&rate, cell(1, 0)), Some((0.984, 732)), "{rate}");
    assert_eq!(value_at(&rate, cell(7, 7)), Some((0.5, 360)), "{rate}");
    assert_eq!(value_at(&rate, cell(4, 4)), None, "a firmware 0.1 collar reports no fix counts");

    let sig = app.get(&format!("/api/coverage?metric=cell&{range}")).await;
    assert_eq!(sig["unit"], "dBm");
    assert_eq!(value_at(&sig, cell(0, 0)), Some((-104.0, 60)), "{sig}");
    // 30 at -96, 30 at -98 and one at -112: the median is -98.
    assert_eq!(value_at(&sig, cell(1, 0)), Some((-98.0, 61)), "{sig}");
    assert_eq!(value_at(&sig, cell(7, 7)), None, "a GNSS-only board measures no cell");
    assert_eq!(sig["cells"].as_array().unwrap().len(), 2);

    // 20 m cells merge (0,0) and (1,0).
    let big = app.get(&format!("/api/coverage?metric=fix_rate&cell_m=20&{range}")).await;
    assert_eq!(big["cells"][0][3], json!(1452));
    let (s, _) = app.call("GET", "/api/coverage?metric=signal", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cells_need_five_reports_and_the_maps_read_only_day_tables() {
    let _one = serial().lock().await;
    count_queries();
    let app = App::new(1).await;
    let d = base();
    app.steady("col_1", d + 8 * HOUR, d + 8 * HOUR + 10 * MIN, 5 * SEC, cell(2, 2), 2.5).await;
    // Four reports that measured the cell: too few; their fix attempts are plenty.
    let rows: Vec<_> = (1..=4).map(|m| ("col_1", d + 8 * HOUR + m * MIN, Some(12), Some(12), Some(-90.0))).collect();
    reports(&app, &rows).await;
    op_analytics::days::aggregate(&app.ctx, time::now()).await.unwrap();
    let range = format!("from={}&to={}", q(d), q(d + DAY));
    take_statements();
    let sig = app.get(&format!("/api/coverage?metric=cell&{range}")).await;
    let rate = app.get(&format!("/api/coverage?metric=fix_rate&{range}")).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let seen = take_statements();
    assert!(seen.iter().any(|s| names_table(s, "coverage_days")));
    assert!(!seen.iter().any(|s| names_table(s, "fixes") || names_table(s, "health")), "{seen:?}");
    assert!(sig["cells"].as_array().unwrap().is_empty(), "{sig}");
    assert_eq!(value_at(&rate, cell(2, 2)), Some((1.0, 48)));

    // A fifth report later, with no new fix: the day is redone from health alone.
    reports(&app, &[("col_1", d + 8 * HOUR + 9 * MIN, Some(12), Some(12), Some(-94.0))]).await;
    op_analytics::days::aggregate(&app.ctx, time::now()).await.unwrap();
    let sig = app.get(&format!("/api/coverage?metric=cell&{range}")).await;
    assert_eq!(value_at(&sig, cell(2, 2)), Some((-90.0, 5)), "{sig}");
}
