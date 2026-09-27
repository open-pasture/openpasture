//! Measured forage heights: stored per paddock through the real routes, and
//! the latest one from the last 21 days replaces the imagery estimate in the
//! grazing signals. Snow and dormant grass withhold the imagery estimate.
//! Land reports are fixtures written to the cache, so nothing is fetched.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Duration;
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Role, Via, time};
use serde_json::{Value, json};
use tower::ServiceExt;

struct T {
    _dir: tempfile::TempDir,
    ctx: Ctx,
}

async fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    T { _dir: dir, ctx }
}

impl T {
    fn app(&self, id: Identity) -> Router {
        let app = Router::new().merge(op_core::router()).merge(op_ingest::router()).merge(op_engine::router()).with_state(self.ctx.clone());
        op_core::with_identity(app, id)
    }

    async fn call(&self, id: Identity, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path).header("host", "127.0.0.1");
        let req = match body {
            Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())),
            None => b.body(Body::empty()),
        }
        .unwrap();
        let res = self.app(id).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.call(Identity::owner(Via::Local), method, path, body).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    /// The §6 farm near Ames: P1 and P2 (about 16.5 ha each), a herd of 250 in P1.
    async fn farm(&self) -> (String, String, String) {
        self.ok("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03] }))).await;
        let p1 = self.ok("POST", "/api/paddocks", Some(json!({ "name": "P1", "geometry": square(-93.625) }))).await;
        let p2 = self.ok("POST", "/api/paddocks", Some(json!({ "name": "P2", "geometry": square(-93.62) }))).await;
        let h = self.ok("POST", "/api/herds", Some(json!({ "name": "Cows", "species": "cattle", "count": 250, "paddock_id": p1["id"] }))).await;
        (id(&p1), id(&p2), id(&h))
    }

    /// Put a land report for the paddock in the cache, as a fetch would have.
    async fn cache_report(&self, paddock_id: &str, sections: Value) {
        let now = time::now();
        let report_id = op_core::id::new_id("lr");
        let report = json!({ "report_id": report_id, "paddock_id": paddock_id, "source": "alexandria", "as_of": time::to_db(&now), "sections": sections });
        sqlx::query("INSERT INTO land_reports (id, paddock_id, cache_key, source, as_of, report, created_at) VALUES (?, ?, ?, 'alexandria', ?, ?, ?)")
            .bind(&report_id)
            .bind(paddock_id)
            .bind(format!("fixture-{report_id}"))
            .bind(time::to_db(&now))
            .bind(report.to_string())
            .bind(time::to_db(&now))
            .execute(self.ctx.db())
            .await
            .unwrap();
    }

    /// The forage row `GET /api/signals` gives for a paddock.
    async fn paddock_signals(&self, herd: &str, paddock: &str) -> Value {
        let s = self.ok("GET", &format!("/api/signals?herd_id={herd}"), None).await;
        s["paddocks"].as_array().unwrap().iter().find(|p| p["paddock_id"] == paddock).cloned().unwrap()
    }
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

fn square(west: f64) -> Value {
    json!({ "type": "Polygon", "coordinates": [[[west, 42.03], [west + 0.005, 42.03], [west + 0.005, 42.0336], [west, 42.0336], [west, 42.03]]] })
}

fn imagery(ndvi: f64) -> Value {
    json!({ "status": "ok", "latest": { "captured_at": "2026-09-20T17:00:00Z" }, "ndvi_stats": { "mean": ndvi }, "history": [] })
}

/// A weather section: snow depth now, and one history day per mean temperature (oldest first).
fn weather(snow_cm: Option<f64>, means: &[f64]) -> Value {
    let history: Vec<Value> = means
        .iter()
        .enumerate()
        .map(|(i, m)| json!({ "date": format!("2026-01-{:02}", i + 1), "precip_mm": 0.0, "temp_max_c": m + 4.0, "temp_min_c": m - 4.0, "temp_mean_c": m }))
        .collect();
    json!({ "status": "ok", "current": { "air_temp_c": 1.0, "precip_mm_24h": 0.0, "snow_depth_cm": snow_cm }, "history": history, "forecast": [] })
}

fn owner() -> Identity {
    Identity::owner(Via::Local)
}

fn person(role: Role, name: &str) -> Identity {
    Identity { role, user_id: Some(format!("usr_{name}")), name: Some(name.into()), via: Via::UserToken }
}

#[tokio::test]
async fn a_height_is_stored_with_who_measured_it_and_listed_newest_first() {
    let t = setup().await;
    let (p1, _, _) = t.farm().await;
    let path = format!("/api/paddocks/{p1}/heights");
    let week_ago = time::to_db(&(time::now() - Duration::days(7)));
    let (s, old) = t.call(person(Role::Hand, "Ana"), "POST", &path, Some(json!({ "height_cm": 14.0, "at": week_ago }))).await;
    assert_eq!(s, StatusCode::CREATED, "{old}");
    assert!(old["id"].as_str().unwrap().starts_with("hgt_"));
    assert_eq!(old["by"], json!({ "via": "user_token", "user_id": "usr_Ana", "name": "Ana" }));
    let (s, new) = t.call(owner(), "POST", &path, Some(json!({ "height_cm": 9.5, "residual_cm": 7.5 }))).await;
    assert_eq!(s, StatusCode::CREATED, "{new}");
    assert_eq!(new["by"], json!({ "via": "local" }));
    assert_eq!(new["residual_cm"], 7.5);

    let list = t.ok("GET", &path, None).await;
    let heights: Vec<f64> = list.as_array().unwrap().iter().map(|h| h["height_cm"].as_f64().unwrap()).collect();
    assert_eq!(heights, vec![9.5, 14.0]);
    assert!(list[1].get("residual_cm").is_none());
    assert_eq!(t.ok("GET", &format!("{path}?limit=1"), None).await.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn heights_are_checked_and_need_a_hand_or_better() {
    let t = setup().await;
    let (p1, _, _) = t.farm().await;
    let path = format!("/api/paddocks/{p1}/heights");
    for bad in [json!({ "height_cm": 0 }), json!({ "height_cm": -3 }), json!({ "height_cm": 900 }), json!({ "height_cm": 10, "residual_cm": 12 })] {
        let (s, v) = t.call(owner(), "POST", &path, Some(bad.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad} {v}");
    }
    let tomorrow = time::to_db(&(time::now() + Duration::days(1)));
    assert_eq!(t.call(owner(), "POST", &path, Some(json!({ "height_cm": 10, "at": tomorrow }))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.call(owner(), "POST", "/api/paddocks/pad_nope/heights", Some(json!({ "height_cm": 10 }))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(t.call(owner(), "GET", "/api/paddocks/pad_nope/heights", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(t.call(person(Role::Viewer, "Jo"), "POST", &path, Some(json!({ "height_cm": 10 }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(t.call(Identity::anonymous(), "POST", &path, Some(json!({ "height_cm": 10 }))).await.0, StatusCode::UNAUTHORIZED);
    // A viewer reads them.
    assert_eq!(t.call(person(Role::Viewer, "Jo"), "GET", &path, None).await.0, StatusCode::OK);
    assert_eq!(t.ok("GET", &path, None).await, json!([]));
}

#[tokio::test]
async fn a_measured_height_overrides_ndvi() {
    let t = setup().await;
    let (p1, p2, herd) = t.farm().await;
    t.cache_report(&p1, json!({ "imagery": imagery(0.62) })).await;
    t.cache_report(&p2, json!({ "imagery": imagery(0.62) })).await;

    let before = t.paddock_signals(&herd, &p1).await;
    assert_eq!(before["forage"]["source"], "imagery");
    assert_eq!(before["forage"]["height_inches"], 7.0);

    t.ok("POST", &format!("/api/paddocks/{p1}/heights"), Some(json!({ "height_cm": 15.24 }))).await; // 6 in
    let after = t.paddock_signals(&herd, &p1).await;
    let f = &after["forage"];
    assert_eq!(f["source"], "measured");
    assert_eq!(f["height_cm"], 15.24);
    assert_eq!(f["height_inches"], 6.0);
    assert_eq!(f["available_kg_dm_per_ha"], 1008.0); // (6 - 3) in × 336
    assert!(f["measured_at"].as_str().is_some());
    // Grazing days for this herd: 60 % of the paddock's forage at 11.8 kg DM per AU a day.
    let area = after["area_ha"].as_f64().unwrap();
    let want = ((1008.0 * area * 0.6 / (250.0 * 11.8)) * 10.0_f64).round() / 10.0;
    assert_eq!(after["grazing_days"].as_f64().unwrap(), want);
    // The other paddock still reads imagery.
    assert_eq!(t.paddock_signals(&herd, &p2).await["forage"]["source"], "imagery");
}

#[tokio::test]
async fn a_measured_height_expires_after_21_days() {
    let t = setup().await;
    let (p1, p2, herd) = t.farm().await;
    t.cache_report(&p1, json!({ "imagery": imagery(0.5) })).await;
    let at = |days: i64| time::to_db(&(time::now() - Duration::days(days)));
    t.ok("POST", &format!("/api/paddocks/{p1}/heights"), Some(json!({ "height_cm": 20.0, "at": at(22) }))).await;
    assert_eq!(t.paddock_signals(&herd, &p1).await["forage"]["source"], "imagery");
    t.ok("POST", &format!("/api/paddocks/{p1}/heights"), Some(json!({ "height_cm": 12.0, "at": at(20) }))).await;
    let f = t.paddock_signals(&herd, &p1).await["forage"].clone();
    assert_eq!(f["source"], "measured");
    assert_eq!(f["height_cm"], 12.0);
    // A paddock with only an old height and no imagery has no forage at all.
    t.ok("POST", &format!("/api/paddocks/{p2}/heights"), Some(json!({ "height_cm": 20.0, "at": at(30) }))).await;
    let f = t.paddock_signals(&herd, &p2).await["forage"].clone();
    assert!(f["source"].is_null() && f["available_kg_dm_per_ha"].is_null(), "{f}");
}

#[tokio::test]
async fn snow_withholds_ndvi_forage() {
    let t = setup().await;
    let (p1, _, herd) = t.farm().await;
    t.cache_report(&p1, json!({ "imagery": imagery(0.6), "weather": weather(Some(8.0), &[6.0; 7]) })).await;
    let row = t.paddock_signals(&herd, &p1).await;
    let f = &row["forage"];
    assert_eq!(f["reason"], "snow");
    assert!(f["available_kg_dm_per_ha"].is_null() && f["height_inches"].is_null() && f["source"].is_null(), "{f}");
    assert!(row["grazing_days"].is_null());
    // The farm's current paddock gives no feed budget either.
    let s = t.ok("GET", &format!("/api/signals?herd_id={herd}"), None).await;
    assert!(s["feed_budget_days_current"].is_null());
    // Forage then comes only from a measured height.
    t.ok("POST", &format!("/api/paddocks/{p1}/heights"), Some(json!({ "height_cm": 12.7 }))).await;
    let f = t.paddock_signals(&herd, &p1).await["forage"].clone();
    assert_eq!(f["source"], "measured");
    assert!(f.get("reason").is_none());
    assert_eq!(f["height_inches"], 5.0);
}

#[tokio::test]
async fn shallow_snow_leaves_ndvi_forage() {
    let t = setup().await;
    let (p1, _, herd) = t.farm().await;
    t.cache_report(&p1, json!({ "imagery": imagery(0.6), "weather": weather(Some(2.0), &[8.0; 7]) })).await;
    let f = t.paddock_signals(&herd, &p1).await["forage"].clone();
    assert_eq!(f["source"], "imagery");
    assert!(f.get("reason").is_none());
}

#[tokio::test]
async fn a_cold_week_withholds_ndvi_forage_as_dormant() {
    let t = setup().await;
    let (p1, p2, herd) = t.farm().await;
    // Warm a month ago, the last 7 days average 3 °C.
    let mut means = vec![15.0; 20];
    means.extend([4.0, 2.0, 5.0, 1.0, 3.0, 4.0, 2.0]);
    t.cache_report(&p1, json!({ "imagery": imagery(0.6), "weather": weather(None, &means) })).await;
    let f = t.paddock_signals(&herd, &p1).await["forage"].clone();
    assert_eq!(f["reason"], "dormant");
    assert!(f["available_kg_dm_per_ha"].is_null(), "{f}");

    // A mild last week (mean 5 °C) is growing grass.
    let mut mild = vec![0.0; 20];
    mild.extend([5.0; 7]);
    t.cache_report(&p2, json!({ "imagery": imagery(0.6), "weather": weather(None, &mild) })).await;
    assert_eq!(t.paddock_signals(&herd, &p2).await["forage"]["source"], "imagery");
}

#[tokio::test]
async fn fewer_than_seven_days_of_history_never_count_as_dormant() {
    let t = setup().await;
    let (p1, _, herd) = t.farm().await;
    t.cache_report(&p1, json!({ "imagery": imagery(0.6), "weather": weather(None, &[0.0; 6]) })).await;
    assert_eq!(t.paddock_signals(&herd, &p1).await["forage"]["source"], "imagery");
}

#[tokio::test]
async fn dormancy_reads_the_midpoint_when_a_day_has_no_mean() {
    // Reports shaped like the kit's (highs and lows only) still count.
    let history: Vec<Value> = (1..=7).map(|d| json!({ "date": format!("2026-01-{d:02}"), "temp_max_c": 6.0, "temp_min_c": -2.0 })).collect();
    let report = json!({ "sections": { "weather": { "status": "ok", "current": {}, "history": history } } });
    assert_eq!(op_engine::land::forage_withheld(&report), Some("dormant"));
    let warm: Vec<Value> = (1..=7).map(|d| json!({ "date": format!("2026-01-{d:02}"), "temp_max_c": 14.0, "temp_min_c": 2.0 })).collect();
    let report = json!({ "sections": { "weather": { "status": "ok", "current": {}, "history": warm } } });
    assert_eq!(op_engine::land::forage_withheld(&report), None);
    // No weather, nothing withheld.
    assert_eq!(op_engine::land::forage_withheld(&json!({ "sections": { "weather": { "status": "unavailable", "reason": "x" } } })), None);
}

#[tokio::test]
async fn a_deleted_paddock_takes_its_heights_with_it() {
    let t = setup().await;
    let (p1, _, _) = t.farm().await;
    t.ok("POST", &format!("/api/paddocks/{p1}/heights"), Some(json!({ "height_cm": 10.0 }))).await;
    let (s, _) = t.call(owner(), "DELETE", &format!("/api/paddocks/{p1}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paddock_heights").fetch_one(t.ctx.db()).await.unwrap();
    assert_eq!(n, 0);
}
