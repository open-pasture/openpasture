//! S's columns in the grazing records: planned days (from the strip schedule
//! the herd ran in the paddock) beside the actual days, and the residual
//! height measured when the herd left.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Via, with_identity};
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let router = with_identity(op_core::router().merge(op_reports::router()), Identity::owner(Via::Local)).with_state(ctx.clone());
        Self { _dir: dir, ctx, router }
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = self.router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn ok(&self, method: &str, path: &str, body: Value) -> Value {
        let (s, v) = self.call(method, path, Some(body)).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    async fn exec(&self, sql: &str, binds: &[&str]) {
        let mut q = sqlx::query(sql);
        for b in binds {
            q = q.bind(*b);
        }
        q.execute(self.ctx.db()).await.unwrap();
    }

    async fn section(&self, report: &str) -> Value {
        let (s, v) = self.call("GET", &format!("/api/reports/{report}?from=2026-09-01&to=2026-09-30"), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["sections"][0].clone()
    }
}

fn square(lon: f64, lat: f64) -> Value {
    json!({"type": "Polygon", "coordinates": [[[lon, lat], [lon + 0.005, lat], [lon + 0.005, lat + 0.0036], [lon, lat + 0.0036], [lon, lat]]]})
}

fn col(section: &Value, key: &str) -> Option<usize> {
    section["columns"].as_array().unwrap().iter().position(|c| c["key"] == key)
}

/// Cows in P1 from Sep 1 to Sep 6 (a strip schedule planned them to Sep 8),
/// then P2; P1 measured 8 cm of residual on the afternoon they left.
async fn farm(app: &App) -> (String, String) {
    app.ok("POST", "/api/farm", json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]})).await;
    let p1 = app.ok("POST", "/api/paddocks", json!({"name": "P1", "geometry": square(-93.625, 42.03)})).await["id"].as_str().unwrap().to_owned();
    let p2 = app.ok("POST", "/api/paddocks", json!({"name": "P2", "geometry": square(-93.62, 42.03)})).await["id"].as_str().unwrap().to_owned();
    let herd =
        app.ok("POST", "/api/herds", json!({"name": "Cows", "species": "cattle", "count": 100, "paddock_id": p1})).await["id"].as_str().unwrap().to_owned();
    app.ok("PATCH", &format!("/api/herds/{herd}"), json!({"paddock_id": p2})).await;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM herd_history WHERE herd_id = ? ORDER BY id").bind(&herd).fetch_all(app.ctx.db()).await.unwrap();
    for (id, t) in ids.iter().zip(["2026-09-01T12:00:00.000Z", "2026-09-06T12:00:00.000Z"]) {
        sqlx::query("UPDATE herd_history SET at = ? WHERE id = ?").bind(t).bind(id).execute(app.ctx.db()).await.unwrap();
    }
    (herd, p1)
}

async fn scheduled(app: &App, herd: &str, p1: &str) {
    app.exec(
        "INSERT INTO schedules (id, herd_id, paddock_id, strips, next_index, cadence, starts_at, back_fence, status, planned_end, created_by, created_at, updated_at, ended_at)
         VALUES ('sch_1', ?, ?, '[]', 6, '{}', '2026-09-02T12:00:00.000Z', '{}', 'done', '2026-09-08T12:00:00.000Z', '{\"via\":\"local\"}',
                 '2026-09-01T13:00:00.000Z', '2026-09-06T12:00:00.000Z', '2026-09-06T12:00:00.000Z')",
        &[herd, p1],
    )
    .await;
}

async fn measured(app: &App, p1: &str) {
    let by = r#"{"via":"local"}"#;
    // The afternoon they left: 20 cm with 8 cm of residual; a week later, outside the window.
    app.exec(
        "INSERT INTO paddock_heights (id, paddock_id, at, height_cm, residual_cm, by, created_at) VALUES ('hgt_1', ?, '2026-09-06T20:00:00.000Z', 20, 8, ?, '2026-09-06T20:00:00.000Z')",
        &[p1, by],
    )
    .await;
    app.exec(
        "INSERT INTO paddock_heights (id, paddock_id, at, height_cm, by, created_at) VALUES ('hgt_2', ?, '2026-09-12T20:00:00.000Z', 15, ?, '2026-09-12T20:00:00.000Z')",
        &[p1, by],
    )
    .await;
}

#[tokio::test]
async fn the_records_show_planned_days_and_the_residual_at_exit() {
    let app = App::new().await;
    let (herd, p1) = farm(&app).await;
    scheduled(&app, &herd, &p1).await;
    measured(&app, &p1).await;
    app.ctx.update_settings(&json!({"units": "metric"})).await.unwrap();
    for report in ["paddock_record", "nrcs_528"] {
        let s = app.section(report).await;
        let (planned, residual) = (col(&s, "planned_days").unwrap(), col(&s, "residual_exit").unwrap());
        assert_eq!(s["columns"][planned]["label"], "Planned days");
        assert_eq!(s["columns"][residual]["unit"], "cm", "{report}");
        let rows = s["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "{report}");
        // P1: 5 days in, 7 planned; 8 cm residual measured the day they left.
        assert_eq!((rows[0][planned].as_f64(), rows[0][residual].as_f64()), (Some(7.0), Some(8.0)), "{report}");
        // P2: still there, no schedule.
        assert!(rows[1][planned].is_null() && rows[1][residual].is_null());
        // Totals still land on the days columns.
        let days = col(&s, "days").unwrap();
        assert!(s["totals"][days].as_f64().is_some(), "{report}: {}", s["totals"]);
    }
    let (_, doc) = app.call("GET", "/api/reports/nrcs_528?from=2026-09-01&to=2026-09-30", None).await;
    assert!(doc["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().starts_with("Planned days")));
    // In inches on an imperial farm.
    app.ctx.update_settings(&json!({"units": "imperial"})).await.unwrap();
    let s = app.section("paddock_record").await;
    let r = col(&s, "residual_exit").unwrap();
    assert_eq!((s["columns"][r]["unit"].as_str(), s["rows"][0][r].as_f64()), (Some("in"), Some(3.1)));
}

#[tokio::test]
async fn nothing_to_show_leaves_the_columns_out() {
    let app = App::new().await;
    farm(&app).await;
    for report in ["paddock_record", "nrcs_528"] {
        let s = app.section(report).await;
        assert!(col(&s, "planned_days").is_none() && col(&s, "residual_exit").is_none(), "{report}");
    }
}

#[tokio::test]
async fn a_height_taken_before_the_herd_left_is_not_the_residual() {
    // Cows graze P1 for 36 hours: Sep 1 12:00 to Sep 3 00:00 UTC, then P2.
    let app = App::new().await;
    let (herd, p1) = farm(&app).await;
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM herd_history WHERE herd_id = ? ORDER BY id").bind(&herd).fetch_all(app.ctx.db()).await.unwrap();
    sqlx::query("UPDATE herd_history SET at = '2026-09-03T00:00:00.000Z' WHERE id = ?").bind(ids[1]).execute(app.ctx.db()).await.unwrap();
    let by = r#"{"via":"local"}"#;
    // Measured to size strips the hour before they went in (10 in), and a
    // residual left by the grazing before this one (Sep 1 06:00).
    app.exec(
        "INSERT INTO paddock_heights (id, paddock_id, at, height_cm, by, created_at) VALUES ('hgt_pre', ?, '2026-09-01T11:00:00.000Z', 25.4, ?, '2026-09-01T11:00:00.000Z')",
        &[&p1, by],
    )
    .await;
    app.exec(
        "INSERT INTO paddock_heights (id, paddock_id, at, height_cm, residual_cm, by, created_at) VALUES ('hgt_old', ?, '2026-09-01T06:00:00.000Z', 20, 5, ?, '2026-09-01T06:00:00.000Z')",
        &[&p1, by],
    )
    .await;
    app.ctx.update_settings(&json!({"units": "imperial"})).await.unwrap();
    for report in ["paddock_record", "nrcs_528"] {
        assert!(col(&app.section(report).await, "residual_exit").is_none(), "{report}: neither is what the herd left");
    }
    // 3 in (7.62 cm) measured the morning after they left: the residual.
    app.exec(
        "INSERT INTO paddock_heights (id, paddock_id, at, height_cm, by, created_at) VALUES ('hgt_post', ?, '2026-09-03T14:00:00.000Z', 7.62, ?, '2026-09-03T14:00:00.000Z')",
        &[&p1, by],
    )
    .await;
    for report in ["paddock_record", "nrcs_528"] {
        let s = app.section(report).await;
        let r = col(&s, "residual_exit").unwrap();
        assert_eq!((s["columns"][r]["unit"].as_str(), s["rows"][0][r].as_f64(), s["rows"][1][r].as_f64()), (Some("in"), Some(3.0), None), "{report}");
    }
}
