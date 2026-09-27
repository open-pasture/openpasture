//! `GET /api/layers/paddocks`: rest days from every herd's record, NDVI,
//! drought and flood from cached land reports only. The reports here are
//! fixtures in the cache; the endpoint must answer from them without
//! fetching (a fetch would add a row to `land_reports`).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Duration;
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Via, time};
use serde_json::{Value, json};
use tower::ServiceExt;

struct T {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    app: Router,
}

async fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let app = Router::new().merge(op_core::router()).merge(op_ingest::router()).merge(op_engine::router()).with_state(ctx.clone());
    let app = op_core::with_identity(app, Identity::owner(Via::Local));
    T { _dir: dir, ctx, app }
}

impl T {
    async fn req(&self, method: &str, path: &str, body: Option<Value>, bearer: Option<&str>) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path).header("host", "127.0.0.1");
        if let Some(k) = bearer {
            b = b.header("authorization", format!("Bearer {k}"));
        }
        let req = match body {
            Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())),
            None => b.body(Body::empty()),
        }
        .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.req(method, path, body, None).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    async fn cache_report(&self, paddock_id: &str, source: &str, sections: Value) {
        let now = time::now();
        let report_id = op_core::id::new_id("lr");
        let report = json!({ "report_id": report_id, "paddock_id": paddock_id, "source": source, "as_of": time::to_db(&now), "sections": sections });
        sqlx::query("INSERT INTO land_reports (id, paddock_id, cache_key, source, as_of, report, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(&report_id)
            .bind(paddock_id)
            .bind(format!("fixture-{report_id}"))
            .bind(source)
            .bind(time::to_db(&now))
            .bind(report.to_string())
            .bind(time::to_db(&now))
            .execute(self.ctx.db())
            .await
            .unwrap();
    }

    async fn reports(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM land_reports").fetch_one(self.ctx.db()).await.unwrap()
    }

    async fn layers(&self) -> Vec<Value> {
        self.ok("GET", "/api/layers/paddocks", None).await["paddocks"].as_array().unwrap().clone()
    }
}

/// A paddock about 16.5 ha, `n` paddocks east of the §6 P1 (0 = P1).
fn square(n: usize) -> Value {
    let w = -93.625 + 0.005 * n as f64;
    json!({ "type": "Polygon", "coordinates": [[[w, 42.03], [w + 0.005, 42.03], [w + 0.005, 42.0336], [w, 42.0336], [w, 42.03]]] })
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

fn row<'a>(rows: &'a [Value], paddock: &str) -> &'a Value {
    rows.iter().find(|r| r["paddock_id"] == paddock).unwrap()
}

/// An Alexandria-shaped report: NDVI imagery, drought category, floodplain and
/// a forecast with 60 mm of rain over three days.
fn alexandria() -> Value {
    json!({
        "imagery": { "status": "ok", "latest": { "captured_at": "2026-09-21T16:40:00Z" }, "ndvi_stats": { "mean": 0.6418 } },
        "climate": { "status": "ok", "drought": { "category": "d2" } },
        "water": { "status": "ok", "floodplain": { "in_floodplain": true, "zone": "AE" } },
        "weather": { "status": "ok", "current": { "air_temp_c": 18.0 }, "history": [],
            "forecast": [{ "date": "2026-09-28", "precip_mm": 30.0 }, { "date": "2026-09-29", "precip_mm": 20.0 }, { "date": "2026-09-30", "precip_mm": 10.0 }] },
    })
}

/// What open data gives without a land provider key: weather only.
fn open_data() -> Value {
    json!({
        "weather": { "status": "ok", "current": { "air_temp_c": 18.0 }, "history": [], "forecast": [{ "date": "2026-09-28", "precip_mm": 80.0 }] },
        "imagery": { "status": "unavailable", "reason": "Imagery comes through Alexandria. Add a Firecrawl API key to enable it." },
        "climate": { "status": "unavailable", "reason": "Only available through Alexandria." },
        "water": { "status": "unavailable", "reason": "Only available through Alexandria." },
    })
}

#[tokio::test]
async fn layers_come_from_cached_land_reports_only() {
    let t = setup().await;
    t.ok("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03] }))).await;
    let p1 = id(&t.ok("POST", "/api/paddocks", Some(json!({ "name": "P1", "geometry": square(0) }))).await);
    let p2 = id(&t.ok("POST", "/api/paddocks", Some(json!({ "name": "P2", "geometry": square(1) }))).await);
    let p3 = id(&t.ok("POST", "/api/paddocks", Some(json!({ "name": "P3", "geometry": square(2) }))).await);
    let p4 = id(&t.ok("POST", "/api/paddocks", Some(json!({ "name": "P4", "geometry": square(3) }))).await);
    // A land provider key is set, so any fetch would go out and be cached.
    t.ctx.secrets().set("firecrawl_api_key", "fc-test-not-used").unwrap();
    t.cache_report(&p1, "alexandria", alexandria()).await;
    t.cache_report(&p2, "open_data", open_data()).await;
    // P3: not in drought, not in a floodplain; both are still facts.
    t.cache_report(
        &p3,
        "alexandria",
        json!({
            "climate": { "status": "ok", "drought": { "category": null } },
            "water": { "status": "ok", "floodplain": { "in_floodplain": false } },
        }),
    )
    .await;
    let before = t.reports().await;

    let rows = t.layers().await;
    assert_eq!(t.reports().await, before, "the layers endpoint fetched a report");
    assert_eq!(rows.len(), 4);

    let a = row(&rows, &p1);
    assert_eq!(a["ndvi"], 0.642);
    assert_eq!(a["ndvi_at"], "2026-09-21");
    assert_eq!(a["drought"], json!({ "category": "D2" }));
    assert_eq!(a["flood"], json!({ "in_floodplain": true, "zone": "AE", "risk": "high" }));

    // Open data: weather only, so no NDVI, drought or flood, even with a wet forecast.
    let b = row(&rows, &p2);
    for k in ["ndvi", "ndvi_at", "drought", "flood"] {
        assert!(b.get(k).is_none(), "{k} in {b}");
    }

    let c = row(&rows, &p3);
    assert_eq!(c["drought"], json!({ "category": null }));
    assert_eq!(c["flood"], json!({ "in_floodplain": false }));
    assert!(c.get("ndvi").is_none());

    // No report at all.
    let d = row(&rows, &p4);
    assert_eq!(d.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["paddock_id"]);
}

#[tokio::test]
async fn rest_days_follow_every_herd_and_its_history() {
    let t = setup().await;
    t.ok("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03] }))).await;
    let pads: Vec<String> = paddocks(&t, 5).await;
    let (p1, p2, p3, p4, p5) = (&pads[0], &pads[1], &pads[2], &pads[3], &pads[4]);
    // Cows are in P1 by their collars.
    let cows = id(&t.ok("POST", "/api/herds", Some(json!({ "name": "Cows", "species": "cattle", "count": 250, "paddock_id": p2 }))).await);
    let key = t.ok("POST", "/api/collars", Some(json!({ "herd_id": cows }))).await["key"].as_str().unwrap().to_owned();
    let now = time::now();
    let fixes: Vec<Value> = (0..10)
        .map(|i| json!({ "at": (now - Duration::minutes(10 - i)).format("%Y-%m-%dT%H:%M:%SZ").to_string(), "point": [-93.6225, 42.0318], "accuracy_m": 2.0, "sats": 9 }))
        .collect();
    let (s, v) = t.req("POST", "/collar/v1/report", Some(json!({ "fixes": fixes, "battery": 0.9 })), Some(&key)).await;
    assert!(s.is_success(), "{s} {v}");
    // Heifers have no collars: the farm record puts them in P4.
    t.ok("POST", "/api/herds", Some(json!({ "name": "Heifers", "species": "cattle", "count": 20, "paddock_id": p4 }))).await;
    // P2: the farmer says it was grazed until three days ago.
    t.ok("PATCH", &format!("/api/paddocks/{p2}"), Some(json!({ "grazed_until": time::to_db(&(now - Duration::days(3))) }))).await;
    // P3: imported position history puts the cows there ten days ago.
    let ten = now - Duration::days(10);
    sqlx::query("INSERT INTO imported_paddock_days (date, herd_id, collar_id, paddock_id, fixes, dwell_s, last_t, import_id) VALUES (?, ?, 'ani_1', ?, 40, 20000, ?, 'imp_1')")
        .bind(ten.format("%Y-%m-%d").to_string())
        .bind(&cows)
        .bind(p3)
        .bind(time::unix_ms(&ten))
        .execute(t.ctx.db())
        .await
        .unwrap();

    let rows = t.layers().await;
    let r1 = row(&rows, p1);
    assert_eq!(r1["grazing"], true);
    assert_eq!(r1["rest_days"], 0.0);
    assert_eq!(row(&rows, p2)["rest_days"], 3.0);
    assert!(row(&rows, p2).get("grazing").is_none());
    assert_eq!(row(&rows, p3)["rest_days"], 10.0);
    let last = row(&rows, p3)["last_grazed"].as_str().unwrap();
    assert_eq!(time::from_db(last).unwrap(), time::from_unix_ms(time::unix_ms(&ten)));
    assert_eq!(row(&rows, p4)["grazing"], true);
    assert_eq!(row(&rows, p4)["rest_days"], 0.0);
    // Never grazed on the record: nothing.
    assert!(row(&rows, p5).get("rest_days").is_none());
    assert!(row(&rows, p5).get("last_grazed").is_none());

    // The decision engine's signals see the imported history too.
    let s = t.ok("GET", &format!("/api/signals?herd_id={cows}"), None).await;
    let p3sig = s["paddocks"].as_array().unwrap().iter().find(|p| p["paddock_id"] == *p3).unwrap().clone();
    assert_eq!(p3sig["rest_days"], 10.0);
}

async fn paddocks(t: &T, n: usize) -> Vec<String> {
    let mut out = vec![];
    for i in 0..n {
        out.push(id(&t.ok("POST", "/api/paddocks", Some(json!({ "name": format!("P{}", i + 1), "geometry": square(i) }))).await));
    }
    out
}

#[tokio::test]
async fn a_farm_without_herds_still_has_rest_from_grazed_until() {
    let t = setup().await;
    t.ok("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03] }))).await;
    let p1 = id(&t.ok("POST", "/api/paddocks", Some(json!({ "name": "P1", "geometry": square(0) }))).await);
    t.ok("PATCH", &format!("/api/paddocks/{p1}"), Some(json!({ "grazed_until": time::to_db(&(time::now() - Duration::days(12))) }))).await;
    let rows = t.layers().await;
    assert_eq!(row(&rows, &p1)["rest_days"], 12.0);
    assert!(row(&rows, &p1).get("grazing").is_none());
}
