//! `GET /api/coverage`, `coverage::grid` and `get_coverage` (field-ready G),
//! read from `coverage_days` only.

mod g_support;

use axum::http::StatusCode;
use g_support::*;
use op_analytics::coverage::{Metric, grid};
use op_analytics::range::TimeRange;
use op_core::{Identity, Role, Via, time};
use serde_json::{Value, json};

fn base() -> i64 {
    midnight(time::now()).timestamp_millis() - 3 * DAY
}

fn q(ms: i64) -> String {
    time::to_db(&time::from_unix_ms(ms)).replace(':', "%3A")
}

/// col_1: an hour in (0,0) at 2.5 m, ten minutes of nothing, then 50 minutes
/// in (1,0) at 6 m. col_2: three fixes in (4,4), too few to show.
async fn farm() -> (App, i64) {
    let app = App::new(2).await;
    let d = base();
    app.steady("col_1", d + 8 * HOUR, d + 9 * HOUR, 5 * SEC, cell(0, 0), 2.5).await;
    app.steady("col_1", d + 9 * HOUR + 10 * MIN, d + 10 * HOUR, 5 * SEC, cell(1, 0), 6.0).await;
    app.steady("col_2", d + 8 * HOUR, d + 8 * HOUR + 15 * SEC, 5 * SEC, cell(4, 4), 2.0).await;
    op_analytics::days::aggregate(&app.ctx, time::now()).await.unwrap();
    (app, d)
}

fn close(a: &Value, p: op_geo::LonLat) -> bool {
    (a[0].as_f64().unwrap() - p[0]).abs() < 1e-6 && (a[1].as_f64().unwrap() - p[1]).abs() < 1e-6
}

#[tokio::test]
async fn maps_of_accuracy_and_of_fixes_that_arrived() {
    let _one = serial().lock().await;
    let (app, d) = farm().await;
    let range = format!("from={}&to={}", q(d), q(d + DAY));

    let acc = app.get(&format!("/api/coverage?metric=accuracy&{range}")).await;
    assert_eq!((acc["metric"].as_str(), acc["cell_m"].as_u64(), acc["unit"].as_str()), (Some("accuracy"), Some(10), Some("m")));
    let cells = acc["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 2, "{acc}");
    assert!(close(&cells[0], cell(0, 0)) && close(&cells[1], cell(1, 0)), "{acc}");
    // Medians from the histogram: 2-3 m and 5-8 m buckets.
    assert_eq!((&cells[0][2], &cells[0][3]), (&json!(2.5), &json!(720)));
    assert_eq!((&cells[1][2], &cells[1][3]), (&json!(6.5), &json!(600)));
    // A cell spans 10 m each way.
    let size = acc["size"].as_array().unwrap();
    let (w, h) = (size[0].as_f64().unwrap(), size[1].as_f64().unwrap());
    assert!((op_geo::projection::distance_m(cell(0, 0), [cell(0, 0)[0] + w, cell(0, 0)[1]]) - 10.0).abs() < 0.01, "{w}");
    assert!((op_geo::projection::distance_m(cell(0, 0), [cell(0, 0)[0], cell(0, 0)[1] + h]) - 10.0).abs() < 0.01, "{h}");

    // 09:00 to 09:10 held 120 fixes that never came, counted where col_1 was before.
    let fixes = app.get(&format!("/api/coverage?metric=fixes&{range}")).await;
    assert_eq!(fixes["unit"], "ratio");
    let cells = fixes["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 2, "{fixes}");
    assert_eq!((&cells[0][2], &cells[0][3]), (&json!(0.857), &json!(840)));
    assert_eq!((&cells[1][2], &cells[1][3]), (&json!(1.0), &json!(600)));

    // The default range is the last 7 days, which holds this day.
    assert_eq!(app.get("/api/coverage").await["cells"], acc["cells"]);
}

#[tokio::test]
async fn coarser_cells_herds_ranges_and_bad_asks() {
    let _one = serial().lock().await;
    let (app, d) = farm().await;
    let range = format!("from={}&to={}", q(d), q(d + DAY));

    // 20 m cells: (0,0) and (1,0) are one.
    let big = app.get(&format!("/api/coverage?metric=accuracy&cell_m=20&{range}")).await;
    let cells = big["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 1, "{big}");
    assert!(close(&cells[0], at(10.0, 10.0)));
    assert_eq!((&cells[0][2], &cells[0][3]), (&json!(2.92), &json!(1320)));
    let big = app.get(&format!("/api/coverage?metric=fixes&cell_m=20&{range}")).await;
    assert_eq!((&big["cells"][0][2], &big["cells"][0][3]), (&json!(0.917), &json!(1440)));
    // With col_2's three fixes, 50 m cells hold enough to show (4,4)'s too.
    let wide = app.get(&format!("/api/coverage?cell_m=50&{range}")).await;
    assert_eq!(wide["cells"].as_array().unwrap().len(), 1);
    assert_eq!(wide["cells"][0][3], json!(1323));

    assert_eq!(app.get(&format!("/api/coverage?herd_id=herd_1&{range}")).await["cells"].as_array().unwrap().len(), 2);
    assert_eq!(app.get(&format!("/api/coverage?herd_id=herd_2&{range}")).await["cells"], json!([]));
    assert_eq!(app.get(&format!("/api/coverage?from={}&to={}", q(d + DAY), q(d + 2 * DAY))).await["cells"], json!([]));
    assert_eq!(app.get(&format!("/api/coverage?from={}&to={}", q(d - DAY), q(d + 2 * DAY))).await["cells"].as_array().unwrap().len(), 2);

    for bad in ["cell_m=15", "cell_m=0", "cell_m=5", "cell_m=2000", "cell_m=x", "metric=signal", "from=-1h&to=-2h", "from=yesterday"] {
        let (status, body) = app.call("GET", &format!("/api/coverage?{bad}"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert!(body["error"].is_string());
    }
}

#[tokio::test]
async fn nothing_before_the_first_fix() {
    let _one = serial().lock().await;
    let app = App::new(1).await;
    op_analytics::days::aggregate(&app.ctx, time::now()).await.unwrap();
    let v = app.get("/api/coverage").await;
    assert_eq!(v, json!({ "metric": "accuracy", "cell_m": 10, "unit": "m", "cells": [] }));
}

#[tokio::test]
async fn the_presend_check_gets_cells_inside_a_box() {
    let _one = serial().lock().await;
    let (app, d) = farm().await;
    let range = TimeRange::new(time::from_unix_ms(d), time::from_unix_ms(d + DAY));
    let p = cell(1, 0);
    let bbox = [p[0] - 1e-5, p[1] - 1e-5, p[0] + 1e-5, p[1] + 1e-5];
    let got = grid(&app.ctx, bbox, range, Metric::Accuracy).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].value, got[0].n), (6.5, 600));
    let b = got[0].bbox;
    assert!(b[0] < p[0] && p[0] < b[2] && b[1] < p[1] && p[1] < b[3], "{b:?}");
    // The whole map's box holds both; a box elsewhere nothing.
    assert_eq!(grid(&app.ctx, [ORIGIN[0] - 0.01, ORIGIN[1] - 0.01, ORIGIN[0] + 0.01, ORIGIN[1] + 0.01], range, Metric::Fixes).await.unwrap().len(), 2);
    assert!(grid(&app.ctx, [ORIGIN[0] + 0.1, ORIGIN[1] + 0.1, ORIGIN[0] + 0.2, ORIGIN[1] + 0.2], range, Metric::Fixes).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_map_never_reads_raw_telemetry() {
    let _one = serial().lock().await;
    count_queries();
    let (app, d) = farm().await;
    take_statements();
    for path in [
        "/api/coverage".to_owned(),
        "/api/coverage?metric=fixes".to_owned(),
        format!("/api/coverage?metric=accuracy&cell_m=50&herd_id=herd_1&from={}&to={}", q(d), q(d + DAY)),
    ] {
        assert_eq!(app.call("GET", &path, None).await.0, StatusCode::OK);
    }
    let range = TimeRange::new(time::from_unix_ms(d), time::from_unix_ms(d + DAY));
    assert_eq!(grid(&app.ctx, [ORIGIN[0] - 0.01, ORIGIN[1] - 0.01, ORIGIN[0] + 0.01, ORIGIN[1] + 0.01], range, Metric::Fixes).await.unwrap().len(), 2);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let seen = take_statements();
    assert!(seen.iter().any(|s| names_table(s, "coverage_days")), "the counter saw nothing: {seen:?}");
    let raw: Vec<&String> = seen.iter().filter(|s| names_table(s, "fixes") || names_table(s, "health")).collect();
    assert!(raw.is_empty(), "{raw:?}");
    // The counter does see a read of fixes when there is one.
    app.count("SELECT COUNT(*) FROM fixes").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(take_statements().iter().any(|s| names_table(s, "fixes")));
}

#[tokio::test]
async fn get_coverage_through_the_tool_registry() {
    let _one = serial().lock().await;
    let (app, d) = farm().await;
    op_analytics::register_tools(&app.ctx);
    let viewer = Identity { role: Role::Viewer, user_id: None, name: None, via: Via::UserToken };
    let spec = app.ctx.tools().get("get_coverage").unwrap();
    assert!(spec.read && !spec.brain && spec.min_role == Role::Viewer);
    let scope = op_core::tools::ToolScope::Full;
    let args = json!({ "metric": "fixes", "from": time::to_db(&time::from_unix_ms(d)), "to": time::to_db(&time::from_unix_ms(d + DAY)) });
    let v = app.ctx.tools().call(&app.ctx, "get_coverage", args, None, viewer.clone(), &scope).await.unwrap();
    assert_eq!(v["cells"].as_array().unwrap().len(), 2);
    assert!(v.get("truncated").is_none());

    // Over 500 cells: the 500 weakest, and how many there were. col_2 walks
    // through 600 cells, five fixes in each, at 1 to 12 m.
    let mut walk = Vec::new();
    for i in 0..600i64 {
        for k in 0..5 {
            walk.push(("col_2", d + 12 * HOUR + (i * 5 + k) * 5 * SEC, cell(10 + i % 30, 10 + i / 30), 1.0 + (i % 12) as f64));
        }
    }
    app.fixes(&walk).await;
    op_analytics::days::aggregate(&app.ctx, time::now()).await.unwrap();
    let args = json!({ "metric": "accuracy", "from": time::to_db(&time::from_unix_ms(d)), "to": time::to_db(&time::from_unix_ms(d + DAY)) });
    let v = app.ctx.tools().call(&app.ctx, "get_coverage", args, None, viewer, &scope).await.unwrap();
    assert_eq!(v["truncated"], json!(602));
    let cells = v["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 500);
    let values: Vec<f64> = cells.iter().map(|c| c[2].as_f64().unwrap()).collect();
    assert!(values.windows(2).all(|w| w[0] >= w[1]), "weakest first");
    assert!(values[0] > 11.0);
}
