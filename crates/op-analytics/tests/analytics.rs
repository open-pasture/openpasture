//! Analytics over synthetic telemetry written straight into SQLite, the way
//! op-ingest stores it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use http_body_util::BodyExt;
use op_core::*;
use op_geo::{LonLat, Projection};
use serde_json::{Value, json};
use tower::ServiceExt;

const ORIGIN: LonLat = [-79.2, 38.1];
const MIN: i64 = 60_000;

fn at(east: f64, north: f64) -> LonLat {
    Projection::new(ORIGIN).offset(east, north)
}

fn square(e0: f64, n0: f64, size: f64) -> Polygon {
    Polygon::from_ring(vec![at(e0, n0), at(e0 + size, n0), at(e0 + size, n0 + size), at(e0, n0 + size)])
}

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
}

impl App {
    /// Farm, paddocks A (0-100 m east) and B (100-200 m east), herd h1 of 10
    /// cattle with animals a1 and a2 on collars c1 and c2.
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let now = time::now();
        let s = ctx.store();
        s.insert_farm(&Farm { id: "farm_1".into(), name: "Test".into(), timezone: "UTC".into(), center: ORIGIN, created_at: now }).await.unwrap();
        for (id, e0) in [("pad_a", 0.0), ("pad_b", 100.0)] {
            let geometry = square(e0, 0.0, 100.0);
            s.insert_paddock(&Paddock {
                id: id.into(),
                name: id.into(),
                area_ha: geometry.area_ha(),
                geometry,
                status: PaddockStatus::Resting,
                notes: None,
                grazed_until: None,
                created_at: now,
            })
            .await
            .unwrap();
        }
        s.insert_herd(&Herd {
            id: "herd_1".into(),
            name: "Cows".into(),
            species: Species::Cattle,
            count: 10,
            paddock_id: None,
            autonomy: Autonomy::Propose,
            timer_minutes: 60,
            created_at: now - Duration::days(30),
        })
        .await
        .unwrap();
        for (c, a) in [("col_1", "ani_1"), ("col_2", "ani_2")] {
            sqlx::query("INSERT INTO collars (id, name, herd_id, created_at) VALUES (?, ?, 'herd_1', ?)")
                .bind(c)
                .bind(c)
                .bind(time::to_db(&(now - Duration::days(30))))
                .execute(ctx.db())
                .await
                .unwrap();
            s.insert_animal(&Animal { id: a.into(), tag: a.to_uppercase(), name: None, herd_id: "herd_1".into(), collar_id: Some(c.into()) }).await.unwrap();
        }
        let router = op_analytics::router().with_state(ctx.clone());
        Self { _dir: dir, ctx, router }
    }

    async fn fix(&self, collar: &str, t: i64, p: LonLat, acc: f64) {
        let animal = collar.replace("col", "ani");
        sqlx::query(
            "INSERT INTO fixes (collar_id, herd_id, animal_id, at, t, lon, lat, accuracy_m, sats, cn0) VALUES (?, 'herd_1', ?, ?, ?, ?, ?, ?, 9, 38.5)",
        )
        .bind(collar)
        .bind(animal)
        .bind(time::to_db(&time::from_unix_ms(t)))
        .bind(t)
        .bind(p[0])
        .bind(p[1])
        .bind(acc)
        .execute(self.ctx.db())
        .await
        .unwrap();
    }

    async fn cue(&self, collar: &str, t: i64) {
        sqlx::query("INSERT INTO cues (collar_id, herd_id, animal_id, at, t, level, margin_m) VALUES (?, 'herd_1', ?, ?, ?, 1, 2.5)")
            .bind(collar)
            .bind(collar.replace("col", "ani"))
            .bind(time::to_db(&time::from_unix_ms(t)))
            .bind(t)
            .execute(self.ctx.db())
            .await
            .unwrap();
    }

    async fn raw(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut req = Request::builder().method(method).uri(path);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = self.router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        (status, headers, res.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    async fn get(&self, path: &str) -> Value {
        let (status, _, body) = self.raw("GET", path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }

    async fn sql(&self, q: &str) -> (StatusCode, Value) {
        let (status, _, body) = self.raw("POST", "/api/sql", Some(json!({ "query": q }))).await;
        (status, serde_json::from_slice(&body).unwrap())
    }

    async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}")).fetch_one(self.ctx.db()).await.unwrap()
    }
}

fn q(t: DateTime<Utc>) -> String {
    time::to_db(&t).replace(':', "%3A")
}

fn midnight(t: DateTime<Utc>) -> DateTime<Utc> {
    t.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc()
}

#[tokio::test]
async fn rollup_moves_old_days_to_parquet_and_queries_still_see_them() {
    let app = App::new().await;
    let now = time::now();
    let old_day = midnight(now - Duration::days(6));
    let old = old_day.timestamp_millis();
    // A full old day in paddock A, one fix every 10 minutes: 144 fixes.
    for i in 0..144 {
        app.fix("col_1", old + i * 10 * MIN, at(50.0, 50.0), 4.0).await;
    }
    app.cue("col_1", old + 30 * MIN).await;
    // The last hour in paddock B.
    let recent = now.timestamp_millis() - 60 * MIN;
    for i in 0..60 {
        app.fix("col_1", recent + i * MIN, at(150.0, 50.0), 4.0).await;
    }

    let rolled = op_analytics::rollup::rollup(&app.ctx, 3, now).await.unwrap();
    assert_eq!(rolled.len(), 2, "{rolled:?}");
    assert_eq!(app.count("fixes").await, 60);
    assert_eq!(app.count("cues").await, 0);
    let file = op_analytics::telemetry::day_path(app.ctx.data_dir(), "fixes", old_day.date_naive());
    assert!(file.is_file());
    assert!(op_analytics::telemetry::day_path(app.ctx.data_dir(), "cues", old_day.date_naive()).is_file());

    // Tracks, SQL and export see both.
    let tracks = app.get(&format!("/api/tracks?from=-7d&max_points=100000")).await;
    assert_eq!(tracks[0]["points"].as_array().unwrap().len(), 204);
    let (status, res) = app.sql("SELECT count(*) AS n, min(t) AS first FROM fixes").await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["columns"], json!(["n", "first"]));
    assert_eq!(res["rows"][0][0], json!(204));
    assert_eq!(res["rows"][0][1], json!(old));
    let (_, res) = app.sql("SELECT count(*) FROM cues").await;
    assert_eq!(res["rows"][0][0], json!(1));
    let (_, _, csv) = app.raw("GET", "/api/export?table=fixes&format=csv&from=-7d", None).await;
    assert_eq!(String::from_utf8(csv).unwrap().lines().count(), 205);

    // Idempotent, and a late fix for a rolled day merges into its file.
    assert!(op_analytics::rollup::rollup(&app.ctx, 3, now).await.unwrap().is_empty());
    app.fix("col_1", old + 5 * MIN, at(50.0, 50.0), 4.0).await;
    let rolled = op_analytics::rollup::rollup(&app.ctx, 3, now).await.unwrap();
    assert_eq!(rolled.len(), 1);
    let (_, res) = app.sql("SELECT count(*) FROM fixes WHERE t < 1000 * 86400 * 365 * 100").await;
    assert_eq!(res["rows"][0][0], json!(205));
    let rows: usize = op_analytics::telemetry::read_parquet(&file).unwrap().iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 145);

    // NDVI comes from the latest land report with ok imagery.
    for (id, as_of, status, mean) in [
        ("lr_1", "2026-01-01T00:00:00.000Z", "ok", 0.41),
        ("lr_2", "2026-02-01T00:00:00.000Z", "ok", 0.62),
        ("lr_3", "2026-03-01T00:00:00.000Z", "unavailable", 0.0),
    ] {
        let report = json!({ "sections": { "imagery": { "status": status, "ndvi_stats": { "mean": mean } } } });
        sqlx::query("INSERT INTO land_reports (id, paddock_id, cache_key, source, as_of, report, created_at) VALUES (?, 'pad_b', ?, 'alexandria', ?, ?, ?)")
            .bind(id)
            .bind(id)
            .bind(as_of)
            .bind(report.to_string())
            .bind(as_of)
            .execute(app.ctx.db())
            .await
            .unwrap();
    }
    // Pasture history for the rolled day comes from the daily summary.
    let pasture = app.get("/api/analytics/pasture?herd_id=herd_1").await;
    let a = pasture.as_array().unwrap().iter().find(|p| p["paddock_id"] == "pad_a").unwrap();
    let b = pasture.as_array().unwrap().iter().find(|p| p["paddock_id"] == "pad_b").unwrap();
    assert_eq!(a["grazing_days"], json!(1));
    let area = a["area_ha"].as_f64().unwrap();
    assert!((area - 1.0).abs() < 0.01, "{area}");
    // 10 cattle = 10 AU, the whole tracked day in A.
    assert!((a["au_days"].as_f64().unwrap() - 10.0).abs() < 0.01, "{a}");
    assert!((a["pressure"].as_f64().unwrap() - 10.0 / area).abs() < 0.05, "{a}");
    let rest = a["rest_days"].as_f64().unwrap();
    assert!((5.0..=6.1).contains(&rest), "{rest}");
    assert!(b["grazing_days"].as_i64().unwrap() >= 1);
    assert!(b["rest_days"].as_f64().unwrap() < 0.1);
    assert_eq!(a["ndvi"], Value::Null);
    assert_eq!(b["ndvi"], json!(0.62));
}

#[tokio::test]
async fn sql_is_read_only_and_json_friendly() {
    let app = App::new().await;
    let t = time::now().timestamp_millis() - 10 * MIN;
    app.fix("col_1", t, at(10.0, 10.0), 3.5).await;
    for bad in
        ["DELETE FROM fixes", "DROP TABLE fixes", "INSERT INTO fixes (collar_id) VALUES ('x')", "SELECT 1; DELETE FROM fixes", "CREATE TABLE x AS SELECT 1"]
    {
        let (status, res) = app.sql(bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {res}");
        assert!(res["error"].is_string());
    }
    assert_eq!(app.count("fixes").await, 1);

    let (status, res) =
        app.sql("SELECT f.collar_id, a.tag, f.lon, f.lat, f.accuracy_m, f.cn0, f.paddock_id FROM fixes f JOIN animals a ON a.collar_id = f.collar_id").await;
    assert_eq!(status, StatusCode::OK, "{res}");
    let row = &res["rows"][0];
    assert_eq!(row[0], "col_1");
    assert_eq!(row[1], "ANI_1");
    assert!((row[2].as_f64().unwrap() - at(10.0, 10.0)[0]).abs() < 1e-9);
    assert_eq!(row[4], json!(3.5));
    assert_eq!(row[6], Value::Null);
    assert!(res["ms"].is_number());
    // Secrets never show.
    let (status, res) = app.sql("SELECT key_hash FROM collars").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{res}");
    // Row limit.
    let (_, res) = app.sql("SELECT * FROM generate_series(1, 20000)").await;
    assert_eq!(res["rows"].as_array().unwrap().len(), 10_000);
    assert_eq!(res["truncated"], json!(true));
}

/// Collar 1 stands in A for 65 minutes, walks 100 m east into B, then stands
/// in B. One fix a minute, 5 m accuracy, 1.5 m jitter.
async fn walk_a_to_b(app: &App, base: i64) {
    for i in 0..130i64 {
        let j = if i % 2 == 0 { 1.5 } else { -1.5 };
        let p = match i {
            0..60 => at(50.0 + j, 50.0 - j),
            60..70 => at(55.0 + 10.0 * (i - 60) as f64, 50.0),
            _ => at(150.0 + j, 50.0 + j),
        };
        app.fix("col_1", base + i * MIN, p, 5.0).await;
    }
}

#[tokio::test]
async fn behaviour_distance_and_time_per_paddock() {
    let app = App::new().await;
    let base = time::now().timestamp_millis() - 5 * 60 * MIN;
    walk_a_to_b(&app, base).await;
    for i in 0..3 {
        app.cue("col_1", base + (10 + i) * MIN).await;
    }
    let from = time::from_unix_ms(base);
    let to = time::from_unix_ms(base + 130 * MIN);
    let rows = app.get(&format!("/api/analytics/behaviour?herd_id=herd_1&from={}&to={}", q(from), q(to))).await;
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let r = &rows[0];
    assert_eq!(r["animal_id"], "ani_1");
    assert_eq!(r["collar_id"], "col_1");
    assert_eq!(r["tag"], "ANI_1");
    let km = r["distance_km"].as_f64().unwrap();
    assert!((0.09..=0.11).contains(&km), "{km}");
    let a = r["paddock_hours"]["pad_a"].as_f64().unwrap();
    let b = r["paddock_hours"]["pad_b"].as_f64().unwrap();
    assert!((a - 65.0 / 60.0).abs() < 0.03, "{a}");
    assert!((b - 65.0 / 60.0).abs() < 0.03, "{b}");
    assert_eq!(r["outside_hours"], json!(0.0));
    assert_eq!(r["cues"], json!(3));
    let per_day: i64 = r["cues_per_day"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).sum();
    assert_eq!(per_day, 3);
    assert_eq!(r["days"].as_array().unwrap().len(), r["cues_per_day"].as_array().unwrap().len());
}

#[tokio::test]
async fn health_fix_rate_accuracy_and_cues() {
    let app = App::new().await;
    let base = time::now().timestamp_millis() - 5 * 60 * MIN;
    walk_a_to_b(&app, base).await;
    // Collar 2 reports for the first hour only.
    for i in 0..65 {
        app.fix("col_2", base + i * MIN, at(20.0, 20.0), if i % 2 == 0 { 3.0 } else { 7.0 }).await;
    }
    app.cue("col_2", base + 5 * MIN).await;
    // Battery comes from op-ingest's health rows, one per report.
    for (i, b) in [(0, 0.9), (4, 0.7), (70, 0.5)] {
        let t = base + i * MIN;
        sqlx::query("INSERT INTO health (collar_id, herd_id, at, t, battery, fixes) VALUES ('col_2', 'herd_1', ?, ?, ?, 2)")
            .bind(time::to_db(&time::from_unix_ms(t)))
            .bind(t)
            .bind(b)
            .execute(app.ctx.db())
            .await
            .unwrap();
    }
    let from = time::from_unix_ms(base);
    let to = time::from_unix_ms(base + 130 * MIN);
    let res = app.get(&format!("/api/analytics/health?herd_id=herd_1&from={}&to={}&bucket=10m", q(from), q(to))).await;
    let res = res.as_array().unwrap();
    assert_eq!(res.len(), 2);
    let c1 = res.iter().find(|c| c["collar_id"] == "col_1").unwrap();
    let c2 = res.iter().find(|c| c["collar_id"] == "col_2").unwrap();
    assert_eq!(c1["cadence_s"], json!(60.0));
    assert_eq!(c1["summary"]["fix_rate"], json!(1.0));
    assert_eq!(c1["summary"]["acc_p50"], json!(5.0));
    assert_eq!(c1["summary"]["sats"], json!(9.0));
    assert_eq!(c1["summary"]["cn0"], json!(38.5));
    assert_eq!(c1["summary"]["battery"], Value::Null);
    let rate = c2["summary"]["fix_rate"].as_f64().unwrap();
    assert!((rate - 0.5).abs() < 0.01, "{rate}");
    assert_eq!(c2["summary"]["cues"], json!(1));
    assert_eq!(c2["summary"]["acc_p50"], json!(3.0));
    assert_eq!(c2["summary"]["acc_p95"], json!(7.0));
    let pts = c2["points"].as_array().unwrap();
    assert!(pts.len() >= 13 && pts.len() <= 14, "{}", pts.len());
    let first = pts.iter().find(|p| p["battery"].is_number()).unwrap()["battery"].as_f64().unwrap();
    assert!([0.9, 0.8].contains(&first), "{first}");
    assert!(pts.iter().any(|p| p["battery"] == json!(0.5)));
    assert_eq!(c2["summary"]["battery"], json!(0.7));
    // After collar 2 stops, its buckets have no fixes and no accuracy.
    let last = pts.last().unwrap();
    assert_eq!(last["fixes"], json!(0));
    assert_eq!(last["fix_rate"], json!(0.0));
    assert_eq!(last["acc_p50"], Value::Null);
    assert!(last.get("cn0").is_none());
    // Unknown collar.
    let (status, _, _) = app.raw("GET", "/api/analytics/health?collar_id=col_x", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn heatmap_bins_fixes_into_cells() {
    let app = App::new().await;
    let base = time::now().timestamp_millis() - 60 * MIN;
    // 6 fixes in one 10 m cell, 2 in the next one east.
    for (i, e) in [1.0, 2.0, 3.0, 4.0, 5.0, 9.0, 12.0, 18.0].iter().enumerate() {
        app.fix("col_1", base + i as i64 * MIN, at(*e, 5.0), 3.0).await;
    }
    let raw = app.get("/api/analytics/heatmap?herd_id=herd_1&cell_m=10&normalize=false").await;
    let cells = raw.as_array().unwrap();
    assert_eq!(cells.len(), 2, "{raw}");
    let weights: Vec<f64> = cells.iter().map(|c| c[2].as_f64().unwrap()).collect();
    assert_eq!(weights, vec![6.0, 2.0]);
    let centre = at(5.0, 5.0);
    assert!((cells[0][0].as_f64().unwrap() - centre[0]).abs() < 1e-6);
    assert!((cells[0][1].as_f64().unwrap() - centre[1]).abs() < 1e-6);
    let norm = app.get("/api/analytics/heatmap?herd_id=herd_1&cell_m=10").await;
    assert_eq!(norm[0][2], json!(1.0));
    assert_eq!(norm[1][2], json!(0.3333));
}

#[tokio::test]
async fn tracks_downsample_per_collar() {
    let app = App::new().await;
    let base = time::now().timestamp_millis() - 5 * 60 * MIN;
    walk_a_to_b(&app, base).await;
    let res = app.get("/api/tracks?collar_id=col_1&max_points=10").await;
    let pts = res[0]["points"].as_array().unwrap();
    assert!(pts.len() <= 11 && pts.len() >= 2, "{}", pts.len());
    // Ends at the latest fix.
    let last = pts.last().unwrap();
    assert_eq!(last[2].as_f64().unwrap(), (base + 129 * MIN) as f64 / 1000.0);
    let all = app.get("/api/tracks?herd_id=herd_1&max_points=100000").await;
    assert_eq!(all[0]["points"].as_array().unwrap().len(), 130);
}

#[tokio::test]
async fn export_formats_parse() {
    let app = App::new().await;
    let base = time::now().timestamp_millis() - 5 * 60 * MIN;
    walk_a_to_b(&app, base).await;
    app.cue("col_1", base + MIN).await;

    let (status, headers, csv) = app.raw("GET", "/api/export?table=fixes&format=csv", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-disposition"].to_str().unwrap().starts_with("attachment; filename=\"fixes-"));
    let csv = String::from_utf8(csv).unwrap();
    let mut lines = csv.lines();
    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    assert!(header.contains(&"lon") && header.contains(&"lat") && header.contains(&"collar_id"));
    assert_eq!(lines.count(), 130);

    let (_, headers, gj) = app.raw("GET", "/api/export?table=fixes&format=geojson", None).await;
    assert_eq!(headers["content-type"], "application/geo+json");
    let gj: Value = serde_json::from_slice(&gj).unwrap();
    assert_eq!(gj["type"], "FeatureCollection");
    assert_eq!(gj["features"].as_array().unwrap().len(), 130);
    assert_eq!(gj["features"][0]["geometry"]["type"], "Point");
    assert_eq!(gj["features"][0]["properties"]["collar_id"], "col_1");

    let (_, _, tr) = app.raw("GET", "/api/export?table=tracks&format=geojson", None).await;
    let tr: Value = serde_json::from_slice(&tr).unwrap();
    assert_eq!(tr["features"].as_array().unwrap().len(), 1);
    assert_eq!(tr["features"][0]["geometry"]["type"], "LineString");
    assert_eq!(tr["features"][0]["geometry"]["coordinates"].as_array().unwrap().len(), 130);

    let (_, headers, pq) = app.raw("GET", "/api/export?table=fixes&format=parquet", None).await;
    assert!(headers["content-disposition"].to_str().unwrap().ends_with(".parquet\""));
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(axum::body::Bytes::from(pq)).unwrap().build().unwrap();
    let rows: usize = reader.map(|b| b.unwrap().num_rows()).sum();
    assert_eq!(rows, 130);

    let (_, _, pads) = app.raw("GET", "/api/export?table=paddocks&format=geojson", None).await;
    let pads: Value = serde_json::from_slice(&pads).unwrap();
    assert_eq!(pads["features"].as_array().unwrap().len(), 2);
    assert_eq!(pads["features"][0]["geometry"]["type"], "Polygon");

    let (_, _, cues) = app.raw("GET", "/api/export?table=cues&format=csv", None).await;
    assert_eq!(String::from_utf8(cues).unwrap().lines().count(), 2);
    let (_, _, collars) = app.raw("GET", "/api/export?table=collars&format=csv", None).await;
    let collars = String::from_utf8(collars).unwrap();
    assert!(!collars.contains("key_hash"));
    assert_eq!(collars.lines().count(), 3);

    let (status, _, _) = app.raw("GET", "/api/export?table=herds&format=geojson", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = app.raw("GET", "/api/export?table=settings", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
