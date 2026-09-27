//! Reports from a fixture farm: herd moves and count changes recorded by the
//! history triggers (dated by rewriting their rows), leases, feed log and
//! report settings through the REST API.

use std::borrow::Cow;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Role, Via, with_identity};
use serde_json::{Value, json};
use tower::ServiceExt;

const AC_PER_HA: f64 = 2.471_053_814_671_653;

fn round(x: f64, d: usize) -> f64 {
    format!("{x:.d$}").parse().unwrap()
}

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        Self::with(dir, ctx)
    }

    fn with(dir: tempfile::TempDir, ctx: Ctx) -> Self {
        op_reports::register_tools(&ctx);
        Self { router: Self::router_as(&ctx, Identity::owner(Via::Local)), ctx, _dir: dir }
    }

    fn router_as(ctx: &Ctx, id: Identity) -> axum::Router {
        with_identity(op_core::router().merge(op_reports::router()), id).with_state(ctx.clone())
    }

    async fn send(router: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, String, String) {
        let mut req = Request::builder().method(method).uri(path);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let ctype = res.headers().get("content-type").map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, ctype, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (s, _, text) = Self::send(&self.router, method, path, body).await;
        (s, if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::String(text)) })
    }

    async fn ok(&self, method: &str, path: &str, body: Value) -> Value {
        let (s, v) = self.call(method, path, Some(body)).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    async fn report(&self, id: &str, q: &str) -> Value {
        let (s, v) = self.call("GET", &format!("/api/reports/{id}?{q}"), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    async fn units(&self, units: &str) {
        self.ctx.update_settings(&json!({ "units": units })).await.unwrap();
    }

    /// Sets the times of a herd's history rows, oldest first.
    async fn date_history(&self, herd: &str, times: &[&str]) {
        let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM herd_history WHERE herd_id = ? ORDER BY id").bind(herd).fetch_all(self.ctx.db()).await.unwrap();
        assert_eq!(ids.len(), times.len(), "history rows for {herd}");
        for (id, t) in ids.iter().zip(times) {
            sqlx::query("UPDATE herd_history SET at = ? WHERE id = ?").bind(t).bind(id).execute(self.ctx.db()).await.unwrap();
        }
    }
}

struct Farm {
    p1: String,
    p2: String,
    p3: String,
    area: f64,
}

fn square(lon: f64, lat: f64) -> Value {
    json!({"type": "Polygon", "coordinates": [[[lon, lat], [lon + 0.005, lat], [lon + 0.005, lat + 0.0036], [lon, lat + 0.0036], [lon, lat]]]})
}

/// The live-check farm: three ~16.5 ha paddocks near Ames, Iowa.
async fn farm(app: &App) -> Farm {
    app.ok("POST", "/api/farm", json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]})).await;
    let p1 = app.ok("POST", "/api/paddocks", json!({"name": "P1", "geometry": square(-93.625, 42.03)})).await;
    let p2 = app.ok("POST", "/api/paddocks", json!({"name": "P2", "geometry": square(-93.62, 42.03)})).await;
    let p3 = app.ok("POST", "/api/paddocks", json!({"name": "P3", "geometry": square(-93.625, 42.0336)})).await;
    let area = p1["area_ha"].as_f64().unwrap();
    assert!((area - p2["area_ha"].as_f64().unwrap()).abs() < 1e-9);
    Farm { p1: p1["id"].as_str().unwrap().into(), p2: p2["id"].as_str().unwrap().into(), p3: p3["id"].as_str().unwrap().into(), area }
}

async fn herd(app: &App, name: &str, count: u32, paddock: Option<&str>) -> String {
    let h = app.ok("POST", "/api/herds", json!({"name": name, "species": "cattle", "count": count, "paddock_id": paddock})).await;
    h["id"].as_str().unwrap().to_owned()
}

async fn patch_herd(app: &App, herd: &str, body: Value) {
    app.ok("PATCH", &format!("/api/herds/{herd}"), body).await;
}

/// Cows (100 head) through September 2025, Chicago time 07:00 = 12:00 UTC:
/// in P1 from Sep 1, P2 Sep 6, 10 sold Sep 8, P3 Sep 11, P1 Sep 15, P2 Sep 21.
async fn grazing(app: &App) -> (Farm, String) {
    let f = farm(app).await;
    let h = herd(app, "Cows", 100, Some(&f.p1)).await;
    patch_herd(app, &h, json!({"paddock_id": f.p2})).await;
    patch_herd(app, &h, json!({"count": 90})).await;
    patch_herd(app, &h, json!({"paddock_id": f.p3})).await;
    patch_herd(app, &h, json!({"paddock_id": f.p1})).await;
    patch_herd(app, &h, json!({"paddock_id": f.p2})).await;
    app.date_history(
        &h,
        &[
            "2025-09-01T12:00:00.000Z",
            "2025-09-06T12:00:00.000Z",
            "2025-09-08T12:00:00.000Z",
            "2025-09-11T12:00:00.000Z",
            "2025-09-15T12:00:00.000Z",
            "2025-09-21T12:00:00.000Z",
        ],
    )
    .await;
    (f, h)
}

const SEPT: &str = "from=2025-09-01&to=2025-09-30";

fn col(section: &Value, key: &str) -> usize {
    section["columns"].as_array().unwrap().iter().position(|c| c["key"] == key).unwrap_or_else(|| panic!("no column {key} in {}", section["columns"]))
}

fn column(section: &Value, key: &str) -> Vec<Value> {
    let i = col(section, key);
    section["rows"].as_array().unwrap().iter().map(|r| r[i].clone()).collect()
}

fn keys(section: &Value) -> Vec<String> {
    section["columns"].as_array().unwrap().iter().map(|c| c["key"].as_str().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn triggers_write_herd_and_paddock_history_on_real_changes_only() {
    let app = App::new().await;
    let f = farm(&app).await;
    let db = app.ctx.db();
    let h = herd(&app, "Cows", 250, Some(&f.p1)).await;
    let rows = |sql: &'static str| async move { sqlx::query_as::<_, (i64, Option<String>, String)>(sql).fetch_all(db).await.unwrap() };
    let herd_rows = "SELECT count, paddock_id, source FROM herd_history ORDER BY id";
    assert_eq!(rows(herd_rows).await, vec![(250, Some(f.p1.clone()), "created".into())]);

    patch_herd(&app, &h, json!({"name": "Cow herd"})).await;
    patch_herd(&app, &h, json!({"autonomy": "timer"})).await;
    assert_eq!(rows(herd_rows).await.len(), 1, "renames and autonomy leave no history");
    patch_herd(&app, &h, json!({"count": 247})).await;
    patch_herd(&app, &h, json!({"paddock_id": f.p2})).await;
    let r = rows(herd_rows).await;
    assert_eq!(r[1..], [(247, Some(f.p1.clone()), "changed".into()), (247, Some(f.p2.clone()), "changed".into())]);
    let name: String = sqlx::query_scalar("SELECT name FROM herd_history ORDER BY id DESC LIMIT 1").fetch_one(db).await.unwrap();
    assert_eq!(name, "Cow herd");

    // Paddocks: the shape and the name, not the status or notes.
    let pad_rows = "SELECT CAST(area_ha * 1000 AS INTEGER), name, source FROM paddock_geometry_history WHERE paddock_id = ? ORDER BY id";
    let pad = |id: String| async move { sqlx::query_as::<_, (i64, String, String)>(pad_rows).bind(id).fetch_all(db).await.unwrap() };
    assert_eq!(pad(f.p3.clone()).await.len(), 1);
    app.ok("PATCH", &format!("/api/paddocks/{}", f.p3), json!({"notes": "wet corner", "status": "planned"})).await;
    assert_eq!(pad(f.p3.clone()).await.len(), 1);
    let half =
        json!({"type": "Polygon", "coordinates": [[[-93.625, 42.0336], [-93.6225, 42.0336], [-93.6225, 42.0372], [-93.625, 42.0372], [-93.625, 42.0336]]]});
    app.ok("PATCH", &format!("/api/paddocks/{}", f.p3), json!({"geometry": half})).await;
    app.ok("PATCH", &format!("/api/paddocks/{}", f.p3), json!({"name": "P3 north"})).await;
    let p = pad(f.p3.clone()).await;
    assert_eq!(p.iter().map(|x| x.2.as_str()).collect::<Vec<_>>(), ["created", "changed", "changed"]);
    assert!((p[1].0 as f64 - p[0].0 as f64 / 2.0).abs() < 50.0, "half the area: {p:?}");
    assert_eq!(p[2].1, "P3 north");

    // Deleting the paddock the herd is in takes the herd out of it on the record.
    let (s, _) = app.call("DELETE", &format!("/api/paddocks/{}", f.p2), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(pad(f.p2.clone()).await.last().unwrap().2, "deleted");
    assert_eq!(rows(herd_rows).await.last().unwrap(), &(247, None, "changed".into()));

    let (s, _) = app.call("DELETE", &format!("/api/herds/{h}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(rows(herd_rows).await.last().unwrap(), &(0, None, "deleted".into()));
}

#[tokio::test]
async fn backfill_rebuilds_moves_from_applied_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(op_core::store::DB_FILE);
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&file).create_if_missing(true).foreign_keys(true);
    let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
    let before = sqlx::migrate::Migrator {
        migrations: Cow::Owned(op_core::store::MIGRATOR.migrations.iter().filter(|m| m.version < 600).cloned().collect()),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    before.run(&pool).await.unwrap();
    let sql = [
        "INSERT INTO farm VALUES ('farm_1', 'Old farm', 'America/Chicago', -93.62, 42.03, '2025-01-01T00:00:00.000Z')",
        "INSERT INTO paddocks (id, name, geometry, area_ha, created_at) VALUES
            ('pad_1', 'P1', '{\"type\":\"Polygon\",\"coordinates\":[[[-93.625,42.03],[-93.62,42.03],[-93.62,42.0336],[-93.625,42.0336],[-93.625,42.03]]]}', 16.5, '2025-01-01T00:00:00.000Z'),
            ('pad_2', 'P2', '{\"type\":\"Polygon\",\"coordinates\":[[[-93.62,42.03],[-93.615,42.03],[-93.615,42.0336],[-93.62,42.0336],[-93.62,42.03]]]}', 16.5, '2025-01-01T00:00:00.000Z'),
            ('pad_3', 'P3', '{\"type\":\"Polygon\",\"coordinates\":[[[-93.625,42.0336],[-93.62,42.0336],[-93.62,42.0372],[-93.625,42.0372],[-93.625,42.0336]]]}', 16.5, '2025-01-01T00:00:00.000Z')",
        "INSERT INTO herds (id, name, species, count, paddock_id, created_at) VALUES ('herd_1', 'Cows', 'cattle', 250, 'pad_3', '2025-06-01T12:00:00.000Z')",
        // A brain move P1 → P2 approved by the farmer.
        "INSERT INTO decisions (id, herd_id, source, status, action, to_paddock_id, inputs, created_at, responded_at)
            VALUES ('dec_1', 'herd_1', 'brain', 'applied', 'MOVE', 'pad_2', '{\"from_paddock_id\":\"pad_1\"}', '2025-06-10T11:00:00.000Z', '2025-06-10T12:00:00.000Z')",
        // A timer move P2 → P3: applied when the activity log says, not at creation.
        "INSERT INTO decisions (id, herd_id, source, status, action, to_paddock_id, inputs, created_at)
            VALUES ('dec_2', 'herd_1', 'brain', 'applied', 'MOVE', 'pad_3', '{\"from_paddock_id\":\"pad_2\"}', '2025-06-20T11:00:00.000Z')",
        "INSERT INTO events (id, kind, source, occurred_at, recorded_at, title) VALUES ('evt_1', 'decision.applied', 'system', '2025-06-20T12:00:00.000Z', '2025-06-20T12:00:00.000Z', 'Move started')",
        "INSERT INTO event_targets VALUES ('evt_1', 'decision', 'dec_2')",
        // Not moves: a rejected proposal and a STAY.
        "INSERT INTO decisions (id, herd_id, source, status, action, to_paddock_id, created_at) VALUES ('dec_3', 'herd_1', 'brain', 'rejected', 'MOVE', 'pad_1', '2025-06-25T12:00:00.000Z')",
        "INSERT INTO decisions (id, herd_id, source, status, action, created_at) VALUES ('dec_4', 'herd_1', 'brain', 'applied', 'STAY', '2025-06-26T12:00:00.000Z')",
    ];
    for s in sql {
        sqlx::query(s).execute(&pool).await.unwrap();
    }
    pool.close().await;

    let ctx = Ctx::open(dir.path()).await.unwrap();
    let rows: Vec<(String, i64, Option<String>, String)> =
        sqlx::query_as("SELECT at, count, paddock_id, source FROM herd_history ORDER BY id").fetch_all(ctx.db()).await.unwrap();
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(rows[0], ("2025-06-01T12:00:00.000Z".into(), 250, Some("pad_1".into()), "backfill".into()));
    assert_eq!(rows[1], ("2025-06-10T12:00:00.000Z".into(), 250, Some("pad_2".into()), "backfill".into()));
    assert_eq!(rows[2], ("2025-06-20T12:00:00.000Z".into(), 250, Some("pad_3".into()), "backfill".into()));
    assert_eq!((rows[3].1, rows[3].2.as_deref(), rows[3].3.as_str()), (250, Some("pad_3"), "install"));
    let shapes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paddock_geometry_history WHERE source = 'backfill'").fetch_one(ctx.db()).await.unwrap();
    assert_eq!(shapes, 3);

    let app = App::with(dir, ctx);
    let doc = app.report("paddock_record", "from=2025-06-01&to=2025-06-30").await;
    let events = &doc["sections"][0];
    assert_eq!(column(events, "paddock"), [json!("P1"), json!("P2"), json!("P3")]);
    assert_eq!(column(events, "days"), [json!(9.0), json!(10.0), json!(10.7)]);
    assert_eq!(column(events, "head"), [json!(250), json!(250), json!(250)]);
    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::Chicago).date_naive();
    let note = format!("Head counts before {today} are the count on that day; herd history starts then.");
    assert!(doc["notes"].as_array().unwrap().contains(&json!(note)), "{}", doc["notes"]);
}

#[tokio::test]
async fn paddock_record_counts_head_days_au_days_density_and_rest() {
    let app = App::new().await;
    let (f, _) = grazing(&app).await;
    let doc = app.report("paddock_record", SEPT).await;
    assert_eq!(doc["title"], "Paddock grazing record");
    assert_eq!(doc["header"], json!([["Farm", "Test farm"], ["Dates", "2025-09-01 – 2025-09-30"]]));
    let ev = &doc["sections"][0];
    assert_eq!(ev["title"], "Grazing events");
    assert_eq!(keys(ev), ["paddock", "herd", "in", "out", "days", "head", "au", "head_days", "au_days", "density", "rest_days"]);
    assert_eq!(ev["columns"][col(ev, "density")]["unit"], "AU/ha");
    assert_eq!(column(ev, "paddock"), [json!("P1"), json!("P2"), json!("P3"), json!("P1"), json!("P2")]);
    assert_eq!(
        column(ev, "in"),
        [json!("2025-09-01 07:00"), json!("2025-09-06 07:00"), json!("2025-09-11 07:00"), json!("2025-09-15 07:00"), json!("2025-09-21 07:00")]
    );
    // The last stay runs past the report: cut at the end of Sep 30.
    assert_eq!(
        column(ev, "out"),
        [json!("2025-09-06 07:00"), json!("2025-09-11 07:00"), json!("2025-09-15 07:00"), json!("2025-09-21 07:00"), json!("2025-10-01 00:00")]
    );
    assert_eq!(column(ev, "days"), [json!(5.0), json!(5.0), json!(4.0), json!(6.0), json!(9.7)]);
    assert_eq!(column(ev, "head"), [json!(100), json!(100), json!(90), json!(90), json!(90)]);
    assert_eq!(column(ev, "au"), [json!(100.0), json!(100.0), json!(90.0), json!(90.0), json!(90.0)]);
    // P2 first stay: 100 head for 2 days, 90 for 3.
    assert_eq!(column(ev, "head_days"), [json!(500.0), json!(470.0), json!(360.0), json!(540.0), json!(873.8)]);
    assert_eq!(column(ev, "au_days"), column(ev, "head_days"));
    let d = |au: f64| json!(round(au / f.area, 1));
    assert_eq!(column(ev, "density"), [d(100.0), d(100.0), d(90.0), d(90.0), d(90.0)]);
    // Rest: P1 left Sep 6, back Sep 15; P2 left Sep 11, back Sep 21.
    assert_eq!(column(ev, "rest_days"), [Value::Null, Value::Null, Value::Null, json!(9.0), json!(10.0)]);
    let totals = ev["totals"].as_array().unwrap();
    assert_eq!(totals[0], "Total");
    assert_eq!(totals[col(ev, "days")], json!(29.7));
    assert_eq!(totals[col(ev, "head_days")], json!(2743.8));

    let by = &doc["sections"][1];
    assert_eq!(column(by, "paddock"), [json!("P1"), json!("P2"), json!("P3")]);
    assert_eq!(column(by, "events"), [json!(2), json!(2), json!(1)]);
    assert_eq!(column(by, "days"), [json!(11.0), json!(14.7), json!(4.0)]);
    assert_eq!(column(by, "head_days"), [json!(1040.0), json!(1343.8), json!(360.0)]);
    assert_eq!(column(by, "area")[0], json!(round(f.area, 1)));

    let notes = doc["notes"].as_array().unwrap();
    assert!(notes.contains(&json!("Animal units per head: cattle 1.0.")), "{notes:?}");
    assert!(notes.contains(&json!("Head is the count on the day in; head-days follow every change in the count.")));
    assert!(notes.contains(&json!("Events are cut at 2025-09-01 and the end of 2025-09-30.")));
    assert!(!notes.iter().any(|n| n.as_str().unwrap().starts_with("Head counts before")), "no backfill on a fresh farm");

    // Imperial: acres and AU per acre, same head-days.
    app.units("imperial").await;
    let doc = app.report("paddock_record", SEPT).await;
    let ev = &doc["sections"][0];
    assert_eq!(ev["columns"][col(ev, "density")]["unit"], "AU/ac");
    assert_eq!(column(ev, "density")[0], json!(round(100.0 / f.area / AC_PER_HA, 1)));
    assert_eq!(column(ev, "head_days")[4], json!(873.8));
    let by = &doc["sections"][1];
    assert_eq!(by["columns"][col(by, "area")]["unit"], "ac");
    assert_eq!(column(by, "area")[0], json!(round(f.area * AC_PER_HA, 1)));
    assert_eq!(column(by, "area")[0], json!(40.9));

    // One herd, part of the month: the stay in P1 from Sep 15 is cut at Sep 18.
    let doc = app.report("paddock_record", "from=2025-09-12&to=2025-09-17").await;
    let ev = &doc["sections"][0];
    assert_eq!(column(ev, "paddock"), [json!("P3"), json!("P1")]);
    assert_eq!(column(ev, "in"), [json!("2025-09-12 00:00"), json!("2025-09-15 07:00")]);
    // P3 stay started Sep 11 07:00: 3 days 7 hours before Sep 15 07:00 inside the cut from Sep 12 00:00.
    assert_eq!(column(ev, "days"), [json!(round(3.0 + 7.0 / 24.0, 1)), json!(round(2.0 + 17.0 / 24.0, 1))]);
}

#[tokio::test]
async fn a_mix_sets_animal_units_and_a_stay_running_now_has_no_out() {
    let app = App::new().await;
    let f = farm(&app).await;
    let h = herd(&app, "Pairs", 100, Some(&f.p1)).await;
    let (s, v) =
        app.call("PUT", "/api/reports/settings", Some(json!({"herds": {h.clone(): {"mix": {"cows": 90, "bulls": 10, "calves": 90, "pairs": true}}}}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::Chicago).date_naive();
    let doc = app.report("paddock_record", &format!("from={today}&to={today}")).await;
    let ev = &doc["sections"][0];
    // 90 pairs × 1.3 + 10 bulls × 1.35 over 100 head.
    assert_eq!(column(ev, "au"), [json!(130.5)]);
    assert_eq!(column(ev, "out"), [Value::Null]);
    let notes = doc["notes"].as_array().unwrap();
    assert!(notes.contains(&json!("Animal units per head: cow 1.0, bull 1.35, pair 1.3, weaned calf 0.5 (Pairs 90 pairs, 10 bulls).")), "{notes:?}");
    assert!(notes.contains(&json!("A blank out date means the herd is still there; its days run to now.")));
}

#[tokio::test]
async fn nrcs_528_has_fsa_numbers_header_and_signatures() {
    let app = App::new().await;
    let (f, _) = grazing(&app).await;
    app.units("imperial").await;

    // Without FSA numbers or an operator: those columns and lines are left out.
    let doc = app.report("nrcs_528", SEPT).await;
    let rec = &doc["sections"][0];
    assert_eq!(keys(rec), ["field", "area", "date_in", "date_out", "kind", "number", "au", "days", "aud", "rest"]);
    assert_eq!(doc["header"], json!([["Farm", "Test farm"], ["Dates", "2025-09-01 – 2025-09-30"]]));
    assert_eq!(doc["signatures"], json!(["Operator", "NRCS planner"]));

    for (p, tract, field) in [(&f.p1, "1234", "7"), (&f.p2, "1234", "8"), (&f.p3, "1235", "2")] {
        app.ok("PATCH", &format!("/api/paddocks/{p}"), json!({"props": {"fsa_farm": "4471", "fsa_tract": tract, "fsa_field": field}})).await;
    }
    app.ok("PUT", "/api/reports/settings", json!({"operator": "Cody Menefee"})).await;
    let doc = app.report("nrcs_528", SEPT).await;
    assert_eq!(doc["header"], json!([["Farm", "Test farm"], ["Operator", "Cody Menefee"], ["FSA farm", "4471"], ["Dates", "2025-09-01 – 2025-09-30"]]));
    let rec = &doc["sections"][0];
    assert_eq!(rec["title"], "Grazing record");
    assert_eq!(keys(rec), ["field", "fsa_tract", "fsa_field", "area", "date_in", "date_out", "kind", "number", "au", "days", "aud", "rest"]);
    assert_eq!(rec["columns"][col(rec, "area")]["unit"], "ac");
    assert_eq!(column(rec, "field"), [json!("P1"), json!("P2"), json!("P3"), json!("P1"), json!("P2")]);
    assert_eq!(column(rec, "fsa_tract"), [json!("1234"), json!("1234"), json!("1235"), json!("1234"), json!("1234")]);
    assert_eq!(column(rec, "fsa_field"), [json!("7"), json!("8"), json!("2"), json!("7"), json!("8")]);
    assert_eq!(column(rec, "date_in")[1], json!("2025-09-06"));
    assert_eq!(column(rec, "date_out")[1], json!("2025-09-11"));
    assert_eq!(column(rec, "kind")[0], json!("Cattle"));
    assert_eq!(column(rec, "number"), [json!(100), json!(100), json!(90), json!(90), json!(90)]);
    assert_eq!(column(rec, "aud"), [json!(500.0), json!(470.0), json!(360.0), json!(540.0), json!(873.8)]);
    assert_eq!(column(rec, "rest"), [Value::Null, Value::Null, Value::Null, json!(9.0), json!(10.0)]);
    assert_eq!(column(rec, "area")[0], json!(40.9));

    // Fields on different FSA farms get the column instead of the header line.
    app.ok("PATCH", &format!("/api/paddocks/{}", f.p3), json!({"props": {"fsa_farm": "5000", "fsa_tract": "9", "fsa_field": "1"}})).await;
    let doc = app.report("nrcs_528", SEPT).await;
    assert!(!doc["header"].as_array().unwrap().iter().any(|h| h[0] == "FSA farm"));
    assert_eq!(column(&doc["sections"][0], "fsa_farm")[2], json!("5000"));
}

#[tokio::test]
async fn organic_checks_120_days_and_hides_dry_matter_without_inputs() {
    let app = App::new().await;
    let f = farm(&app).await;
    // On pasture Jun 1 – Jul 31 (61 days), in the drylot before and after.
    let dry = herd(&app, "Drylot", 50, None).await;
    patch_herd(&app, &dry, json!({"paddock_id": f.p1})).await;
    patch_herd(&app, &dry, json!({"paddock_id": null})).await;
    app.date_history(&dry, &["2025-04-01T05:00:00.000Z", "2025-06-01T12:00:00.000Z", "2025-08-01T03:00:00.000Z"]).await;
    // On pasture all season.
    let grass = herd(&app, "Grass", 40, Some(&f.p2)).await;
    app.date_history(&grass, &["2025-04-01T05:00:00.000Z"]).await;

    let doc = app.report("organic_season", "from=2025-04-01&to=2025-10-31").await;
    assert_eq!(doc["sections"].as_array().unwrap().len(), 1, "no dry matter section without weights");
    let days = &doc["sections"][0];
    assert_eq!(days["title"], "Days on pasture");
    let mut rows = days["rows"].as_array().unwrap().clone();
    rows.sort_by_key(|r| r[0].as_str().unwrap().to_owned());
    // Jun 1 07:00 to Jul 31 22:00 Chicago time: 61 farm days.
    assert_eq!(rows, [json!(["Drylot", 214, 61, "No"]), json!(["Grass", 214, 214, "Yes"])]);
    assert!(doc["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().starts_with("Dry matter from pasture shows for a herd once")));

    // A weight alone isn't enough: the feed log must cover the season too.
    app.ok("PUT", "/api/reports/settings", json!({"herds": {grass.clone(): {"mean_weight_kg": 500.0}}})).await;
    app.ok("POST", "/api/feed-log", json!({"herd_id": grass, "date": "2025-04-03", "kg_dm": 800.0})).await;
    let doc = app.report("organic_season", "from=2025-04-01&to=2025-10-31").await;
    assert_eq!(doc["sections"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn organic_dry_matter_share_from_weight_intake_and_feed_log() {
    let app = App::new().await;
    let f = farm(&app).await;
    let grass = herd(&app, "Grass", 40, Some(&f.p2)).await;
    app.date_history(&grass, &["2025-04-01T05:00:00.000Z"]).await;
    app.ok("PUT", "/api/reports/settings", json!({"herds": {grass.clone(): {"mean_weight_kg": 500.0, "intake_pct": 2.5}}})).await;
    for (date, kg) in [("2025-04-03", 30_000.0), ("2025-07-01", 20_000.0), ("2025-10-29", 0.0)] {
        app.ok("POST", "/api/feed-log", json!({"herd_id": grass, "date": date, "kg_dm": kg, "kind": "hay"})).await;
    }
    let doc = app.report("organic_season", "from=2025-04-01&to=2025-10-31").await;
    let dm = &doc["sections"][1];
    assert_eq!(dm["title"], "Dry matter from pasture");
    assert_eq!(keys(dm), ["herd", "head_days", "weight", "intake", "demand", "supplement", "pasture", "share", "min_share"]);
    // 40 head × 214 days × 500 kg × 2.5 % = 107,000 kg DM needed; 50,000 fed.
    assert_eq!(dm["rows"][0], json!(["Grass", 8560.0, 500.0, 2.5, 107000.0, 50000.0, 57000.0, 53.3, "Yes"]));
    assert_eq!(dm["columns"][col(dm, "demand")]["unit"], "kg");

    app.units("imperial").await;
    let doc = app.report("organic_season", "from=2025-04-01&to=2025-10-31").await;
    let dm = &doc["sections"][1];
    assert_eq!(dm["columns"][col(dm, "weight")]["unit"], "lb");
    assert_eq!(dm["rows"][0][2], json!(round(500.0 * 2.204_622_621_848_776, 0)));
    assert_eq!(dm["rows"][0][7], json!(53.3));

    // Feeding more than 70 % fails the 30 % check.
    app.ok("POST", "/api/feed-log", json!({"herd_id": grass, "date": "2025-08-01", "kg_dm": 30_000.0})).await;
    let doc = app.report("organic_season", "from=2025-04-01&to=2025-10-31").await;
    assert_eq!(doc["sections"][1]["rows"][0][7], json!(round(27_000.0 / 107_000.0 * 100.0, 1)));
    assert_eq!(doc["sections"][1]["rows"][0][8], json!("No"));
}

#[tokio::test]
async fn lease_amounts_follow_each_rate_and_the_season() {
    let app = App::new().await;
    let (f, h) = grazing(&app).await;
    // P2 head-days in September: 470 + 873.75 = 1,343.75.
    let hd: f64 = 1343.75;
    let cases = [("head_day", 0.5, hd * 0.5), ("au_day", 0.4, hd * 0.4), ("aum", 20.0, hd / 30.4 * 20.0), ("acre_season", 100.0, 100.0 * f.area)];
    for (rate_per, rate, amount) in cases {
        app.ok("PUT", &format!("/api/leases/{}", f.p2), json!({"landowner": "Jane Doe", "rate_per": rate_per, "rate_amount": rate})).await;
        let doc = app.report("lease_head_days", SEPT).await;
        let s = &doc["sections"][0];
        assert_eq!(s["title"], "Jane Doe");
        assert_eq!(column(s, "paddock"), [json!("P2")]);
        assert_eq!(column(s, "head_days"), [json!(1343.8)]);
        assert_eq!(column(s, "au_days"), [json!(1343.8)]);
        assert_eq!(column(s, "aum"), [json!(round(hd / 30.4, 1))]);
        assert_eq!(column(s, "amount"), [json!(round(amount, 2))], "{rate_per}");
        assert_eq!(s["columns"][col(s, "amount")]["unit"], "USD");
        assert_eq!(s["totals"][col(s, "amount")], json!(round(amount, 2)));
        assert_eq!(doc["signatures"], json!(["Operator", "Jane Doe"]));
        assert!(!keys(s).contains(&"pair_months".to_owned()));
    }

    // Per-acre rent reads per acre on an imperial farm, stored per hectare.
    app.units("imperial").await;
    let doc = app.report("lease_head_days", SEPT).await;
    assert_eq!(column(&doc["sections"][0], "rate"), [json!(format!("{:.2} per ac, season", 100.0 / AC_PER_HA))]);

    // A cornstalk-style season from Sep 10: only the grazing after it counts.
    app.ok(
        "PUT",
        &format!("/api/leases/{}", f.p2),
        json!({"landowner": "Jane Doe", "rate_per": "head_day", "rate_amount": 1.0, "season_from": "2025-09-10", "season_to": "2026-03-31"}),
    )
    .await;
    let doc = app.report("lease_head_days", SEPT).await;
    let s = &doc["sections"][0];
    // Sep 10 00:00 (05:00 UTC) to Sep 11 12:00 UTC at 90 head, then 873.75.
    let expect = 90.0 * (31.0 / 24.0) + 873.75;
    assert_eq!(column(s, "head_days"), [json!(round(expect, 1))]);
    assert_eq!(column(s, "dates"), [json!("2025-09-10 – 2025-09-30")]);

    // Pairs known: pair-months and a pair-month rate.
    app.ok("PUT", "/api/reports/settings", json!({"herds": {h.clone(): {"mix": {"cows": 80, "bulls": 10, "pairs": true}}}})).await;
    app.ok("PUT", &format!("/api/leases/{}", f.p2), json!({"landowner": "Jane Doe", "rate_per": "pair_month", "rate_amount": 30.0})).await;
    let doc = app.report("lease_head_days", SEPT).await;
    let s = &doc["sections"][0];
    let pair_months = hd * 80.0 / 90.0 / 30.4;
    assert_eq!(column(s, "pair_months"), [json!(round(pair_months, 1))]);
    assert_eq!(column(s, "amount"), [json!(round(pair_months * 30.0, 2))]);
    // AU from the mix: (80 × 1.3 + 10 × 1.35) / 90 per head.
    assert_eq!(column(s, "au_days"), [json!(round(hd * (80.0 * 1.3 + 13.5) / 90.0, 1))]);

    // Two landowners: a section and a signature line each.
    app.ok("PUT", &format!("/api/leases/{}", f.p1), json!({"landowner": "Bob Ames", "rate_per": "head_day", "rate_amount": 0.25})).await;
    let doc = app.report("lease_head_days", SEPT).await;
    let titles: Vec<&str> = doc["sections"].as_array().unwrap().iter().map(|s| s["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Bob Ames", "Jane Doe"]);
    assert_eq!(doc["signatures"], json!(["Operator", "Bob Ames", "Jane Doe"]));
    assert_eq!(column(&doc["sections"][0], "head_days"), [json!(1040.0)]);
}

#[tokio::test]
async fn csv_is_one_file_that_parses_back() {
    let app = App::new().await;
    grazing(&app).await;
    app.units("imperial").await;
    app.ok("PUT", "/api/reports/settings", json!({"operator": "Cody, Menefee"})).await;
    let doc = app.report("paddock_record", SEPT).await;
    let (s, ctype, text) = App::send(&app.router, "GET", &format!("/api/reports/paddock_record?{SEPT}&format=csv"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ctype, "text/csv; charset=utf-8");

    let mut r = csv::ReaderBuilder::new().has_headers(false).flexible(true).from_reader(text.as_bytes());
    let records: Vec<Vec<String>> = r.records().map(|x| x.unwrap().iter().map(str::to_owned).collect()).collect();
    let mut it = records.into_iter();
    assert_eq!(it.next().unwrap(), ["Paddock grazing record"]);
    let mut header = Vec::new();
    for rec in it.by_ref() {
        if rec.iter().all(String::is_empty) {
            break;
        }
        header.push(json!([rec[0], rec[1]]));
    }
    assert_eq!(Value::Array(header), doc["header"]);
    for section in doc["sections"].as_array().unwrap() {
        assert_eq!(it.next().unwrap(), [section["title"].as_str().unwrap()]);
        let heads: Vec<String> = section["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| match c["unit"].as_str() {
                Some(u) => format!("{} ({u})", c["label"].as_str().unwrap()),
                None => c["label"].as_str().unwrap().to_owned(),
            })
            .collect();
        assert_eq!(it.next().unwrap(), heads);
        let want: Vec<&Value> = section["rows"].as_array().unwrap().iter().chain(section.get("totals").filter(|t| !t.is_null())).collect();
        for row in want {
            let got = it.next().unwrap();
            for (cell, v) in got.iter().zip(row.as_array().unwrap()) {
                match v {
                    Value::Null => assert_eq!(cell, ""),
                    Value::String(s) => assert_eq!(cell, s),
                    Value::Number(n) => assert_eq!(cell.parse::<f64>().unwrap(), n.as_f64().unwrap()),
                    other => panic!("unexpected {other}"),
                }
            }
        }
        assert!(it.next().unwrap().iter().all(String::is_empty), "blank line after a section");
    }
    assert_eq!(it.next().unwrap(), ["Notes"]);
    // Number columns keep their places: AU 100.0, head 100, area 40.9.
    let first = text.lines().find(|l| l.starts_with("P1,")).unwrap();
    assert_eq!(first, "P1,Cows,2025-09-01 07:00,2025-09-06 07:00,5.0,100,100.0,500.0,500.0,2.4,");
    let notes: Vec<Value> = it.map(|r| json!(r[0])).collect();
    assert_eq!(Value::Array(notes), doc["notes"]);
}

#[tokio::test]
async fn rest_api_lists_builds_and_refuses_bad_input() {
    let app = App::new().await;
    grazing(&app).await;
    let (s, list) = app.call("GET", "/api/reports", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        list,
        json!([
            {"id": "paddock_record", "title": "Paddock grazing record"},
            {"id": "nrcs_528", "title": "NRCS 528 grazing record"},
            {"id": "organic_season", "title": "Organic grazing season"},
            {"id": "lease_head_days", "title": "Lease head-days"}
        ])
    );
    for id in ["paddock_record", "nrcs_528", "organic_season", "lease_head_days"] {
        let doc = app.report(id, SEPT).await;
        assert_eq!(doc["id"], id);
        assert_eq!((doc["from"].as_str(), doc["to"].as_str()), (Some("2025-09-01"), Some("2025-09-30")));
    }
    // Default: this year to today, farm time.
    let doc = app.report("paddock_record", "").await;
    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::Chicago).date_naive();
    assert_eq!(doc["to"], json!(today.to_string()));
    assert!(doc["from"].as_str().unwrap().ends_with("-01-01"));

    assert_eq!(app.call("GET", "/api/reports/nope", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(app.call("GET", "/api/reports/paddock_record?from=2025-13-01", None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(app.call("GET", "/api/reports/paddock_record?from=2025-09-30&to=2025-09-01", None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(app.call("GET", "/api/reports/paddock_record?format=pdf", None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(app.call("GET", "/api/reports/paddock_record?herd_id=herd_nope", None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn report_settings_merge_and_validate() {
    let app = App::new().await;
    let (s, v) = app.call("GET", "/api/reports/settings", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({"au": {"cow": 1.0, "bull": 1.35, "pair": 1.3, "weaned_calf": 0.5}, "herds": {}}));
    app.ok("PUT", "/api/reports/settings", json!({"operator": "  Cody  ", "fsa_farm": "4471", "au": {"bull": 1.5}})).await;
    let v = app.ok("PUT", "/api/reports/settings", json!({"herds": {"herd_a": {"mean_weight_kg": 540.0}}})).await;
    assert_eq!(v["operator"], "Cody");
    assert_eq!(v["au"], json!({"cow": 1.0, "bull": 1.5, "pair": 1.3, "weaned_calf": 0.5}));
    assert_eq!(v["herds"]["herd_a"], json!({"mean_weight_kg": 540.0, "intake_pct": 2.5}));
    let v = app.ok("PUT", "/api/reports/settings", json!({"fsa_farm": null, "herds": {"herd_a": null}})).await;
    assert_eq!(v, json!({"operator": "Cody", "au": {"cow": 1.0, "bull": 1.5, "pair": 1.3, "weaned_calf": 0.5}, "herds": {}}));
    for bad in [json!({"au": {"cow": 0}}), json!({"herds": {"h": {"intake_pct": 40}}}), json!({"herds": {"h": {"mean_weight_kg": -1}}}), json!([1])] {
        assert_eq!(app.call("PUT", "/api/reports/settings", Some(bad.clone())).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (_, v) = app.call("GET", "/api/reports/settings", None).await;
    assert_eq!(v["operator"], "Cody");
}

#[tokio::test]
async fn feed_log_entries_keep_who_logged_them() {
    let app = App::new().await;
    let f = farm(&app).await;
    let h = herd(&app, "Cows", 20, Some(&f.p1)).await;
    let hand = Identity { role: Role::Hand, user_id: Some("usr_sam".into()), name: Some("Sam".into()), via: Via::UserToken };
    let as_hand = App::router_as(&app.ctx, hand);
    let (s, _, text) =
        App::send(&as_hand, "POST", "/api/feed-log", Some(json!({"herd_id": h, "date": "2025-09-02", "kg_dm": 450.0, "note": " round bales "}))).await;
    assert_eq!(s, StatusCode::CREATED, "{text}");
    let e: Value = serde_json::from_str(&text).unwrap();
    assert!(e["id"].as_str().unwrap().starts_with("fed_"));
    assert_eq!((e["kind"].as_str(), e["note"].as_str()), (Some("hay"), Some("round bales")));
    assert_eq!(e["created_by"], json!({"via": "user_token", "user_id": "usr_sam", "name": "Sam"}));

    app.ok("POST", "/api/feed-log", json!({"herd_id": h, "date": "2025-09-05", "kg_dm": 0.0, "kind": "none"})).await;
    let (_, list) = app.call("GET", &format!("/api/feed-log?herd_id={h}"), None).await;
    assert_eq!(list.as_array().unwrap().iter().map(|e| e["date"].as_str().unwrap()).collect::<Vec<_>>(), ["2025-09-05", "2025-09-02"]);
    let (_, list) = app.call("GET", "/api/feed-log?from=2025-09-03", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    let id = e["id"].as_str().unwrap();
    let v = app.ok("PATCH", &format!("/api/feed-log/{id}"), json!({"kg_dm": 500.0, "note": null})).await;
    assert_eq!((v["kg_dm"].as_f64(), v.get("note")), (Some(500.0), None));
    assert_eq!(v["created_by"]["name"], "Sam");
    for bad in [
        json!({"herd_id": h, "date": "2025-09-02", "kg_dm": -1}),
        json!({"herd_id": "herd_x", "date": "2025-09-02", "kg_dm": 1}),
        json!({"herd_id": h, "date": "Sep 2", "kg_dm": 1}),
    ] {
        assert!(app.call("POST", "/api/feed-log", Some(bad.clone())).await.0.is_client_error(), "{bad}");
    }
    assert_eq!(app.call("DELETE", &format!("/api/feed-log/{id}"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(app.call("DELETE", &format!("/api/feed-log/{id}"), None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn leases_are_kept_per_paddock() {
    let app = App::new().await;
    let f = farm(&app).await;
    assert_eq!(app.call("GET", &format!("/api/leases/{}", f.p1), None).await.0, StatusCode::NOT_FOUND);
    let l =
        app.ok("PUT", &format!("/api/leases/{}", f.p1), json!({"landowner": " Jane Doe ", "rate_per": "aum", "rate_amount": 28.5, "currency": "usd"})).await;
    assert_eq!((l["landowner"].as_str(), l["currency"].as_str(), l["rate_per"].as_str()), (Some("Jane Doe"), Some("USD"), Some("aum")));
    let (_, list) = app.call("GET", "/api/leases", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    for bad in [
        json!({"landowner": "", "rate_per": "aum", "rate_amount": 1}),
        json!({"landowner": "J", "rate_per": "per_acre", "rate_amount": 1}),
        json!({"landowner": "J", "rate_per": "aum", "rate_amount": -1}),
        json!({"landowner": "J", "rate_per": "aum", "rate_amount": 1, "currency": "dollars"}),
        json!({"landowner": "J", "rate_per": "aum", "rate_amount": 1, "season_from": "2025-11-01", "season_to": "2025-03-31"}),
    ] {
        assert!(app.call("PUT", &format!("/api/leases/{}", f.p1), Some(bad.clone())).await.0.is_client_error(), "{bad}");
    }
    assert_eq!(app.call("PUT", "/api/leases/pad_nope", Some(json!({"landowner": "J", "rate_per": "aum", "rate_amount": 1}))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(app.call("DELETE", &format!("/api/leases/{}", f.p1), None).await.0, StatusCode::NO_CONTENT);
    let (_, list) = app.call("GET", "/api/leases", None).await;
    assert_eq!(list, json!([]));
}

#[tokio::test]
async fn get_report_tool_answers_through_the_registry() {
    let app = App::new().await;
    grazing(&app).await;
    let tool = app.ctx.tools().get("get_report").expect("registered");
    assert!(tool.read && !tool.brain && tool.min_role == Role::Viewer);
    let scope = op_core::tools::ToolScope::Full;
    let doc = app
        .ctx
        .tools()
        .call(&app.ctx, "get_report", json!({"id": "nrcs_528", "from": "2025-09-01", "to": "2025-09-30"}), None, Identity::brain(), &scope)
        .await
        .unwrap();
    assert_eq!(doc["id"], "nrcs_528");
    assert_eq!(doc["sections"][0]["rows"].as_array().unwrap().len(), 5);
    let err = app.ctx.tools().call(&app.ctx, "get_report", json!({"id": "nope"}), None, Identity::brain(), &scope).await.unwrap_err();
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_applied_move_on_the_record_writes_history() {
    let app = App::new().await;
    let f = farm(&app).await;
    let h = herd(&app, "Cows", 250, Some(&f.p1)).await;
    // The engine's apply path: the herd goes to the decision's paddock, the old one rests.
    let d: op_core::Decision = serde_json::from_value(json!({
        "id": "dec_1", "herd_id": h, "source": "heuristic", "status": "applied", "action": "MOVE",
        "to_paddock_id": f.p2, "inputs": {"from_paddock_id": f.p1}, "created_at": "2026-09-01T12:00:00.000Z"
    }))
    .unwrap();
    op_engine::cycle::move_herd(&app.ctx, &d).await.unwrap();
    let rows: Vec<(i64, Option<String>, String)> =
        sqlx::query_as("SELECT count, paddock_id, source FROM herd_history WHERE herd_id = ? ORDER BY id").bind(&h).fetch_all(app.ctx.db()).await.unwrap();
    assert_eq!(rows, [(250, Some(f.p1.clone()), "created".into()), (250, Some(f.p2.clone()), "changed".into())]);
    // Marking P1 resting and P2 grazing changes status only: no new paddock history.
    let shapes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paddock_geometry_history").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(shapes, 3);

    // The herd came an hour ago (the two rows can share a millisecond otherwise).
    let hour_ago = op_core::time::to_db(&(op_core::time::now() - chrono::Duration::hours(1)));
    sqlx::query("UPDATE herd_history SET at = ? WHERE source = 'created'").bind(hour_ago).execute(app.ctx.db()).await.unwrap();
    let today = chrono::Utc::now().with_timezone(&chrono_tz::America::Chicago).date_naive();
    let doc = app.report("paddock_record", &format!("from={}&to={today}", today - chrono::Duration::days(1))).await;
    assert_eq!(column(&doc["sections"][0], "paddock"), [json!("P1"), json!("P2")]);
    assert_eq!(column(&doc["sections"][0], "days")[0], json!(0.0));
    assert_eq!(column(&doc["sections"][0], "head"), [json!(250), json!(250)]);
}
