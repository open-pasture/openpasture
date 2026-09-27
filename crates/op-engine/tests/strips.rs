//! Strip previews through the real routes: geometry, grazeable ground less
//! exclusions in effect, and days from the paddock's forage (a cached NDVI
//! land report, or a measured height handed to the same sizing).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Duration;
use http_body_util::BodyExt;
use op_core::features::{FeatureGeometry, FeatureKind, NewFeature};
use op_core::{Ctx, Polygon, time};
use op_engine::calc;
use op_engine::strips::{self, Forage, StripParams};
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
    let app = op_core::with_identity(app, op_core::Identity::owner(op_core::Via::Local));
    T { _dir: dir, ctx, app }
}

impl T {
    async fn req(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path).header("host", "127.0.0.1");
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
        let (s, v) = self.req(method, path, body).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    async fn preview(&self, body: Value) -> Value {
        self.ok("POST", "/api/strips/preview", Some(body)).await
    }
}

/// P1 of the live check near Ames: about 413 m east-west by 400 m north-south, 16.56 ha.
fn p1() -> Value {
    json!({ "type": "Polygon", "coordinates": [[[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]]] })
}

struct Farm {
    paddock: String,
    herd: String,
}

async fn farm(t: &T) -> Farm {
    t.ok("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03] }))).await;
    let p = t.ok("POST", "/api/paddocks", Some(json!({ "name": "P1", "geometry": p1() }))).await;
    let h = t.ok("POST", "/api/herds", Some(json!({ "name": "Cows", "species": "cattle", "count": 250, "paddock_id": p["id"] }))).await;
    Farm { paddock: p["id"].as_str().unwrap().into(), herd: h["id"].as_str().unwrap().into() }
}

/// A cached land report for the paddock with this mean NDVI, as Alexandria would have saved it.
async fn ndvi_report(ctx: &Ctx, paddock_id: &str, ndvi: f64) {
    let now = time::to_db(&time::now());
    let report = json!({
        "report_id": format!("lr_{paddock_id}"), "paddock_id": paddock_id, "source": "alexandria", "as_of": now, "geometry": p1(),
        "sections": { "imagery": { "status": "ok", "ndvi_stats": { "mean": ndvi }, "latest": { "captured_at": "2026-09-20" }, "sources": [] } },
    });
    sqlx::query("INSERT INTO land_reports (id, paddock_id, cache_key, source, as_of, report, created_at) VALUES (?, ?, 'test', 'alexandria', ?, ?, ?)")
        .bind(format!("lr_{paddock_id}"))
        .bind(paddock_id)
        .bind(&now)
        .bind(report.to_string())
        .bind(&now)
        .execute(ctx.db())
        .await
        .unwrap();
}

fn strips_of(v: &Value) -> &Vec<Value> {
    v["strips"].as_array().unwrap()
}

fn sum(v: &Value, key: &str) -> f64 {
    strips_of(v).iter().map(|s| s[key].as_f64().unwrap()).sum()
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

#[tokio::test]
async fn a_count_cuts_that_many_strips_that_cover_the_paddock() {
    let t = setup().await;
    let f = farm(&t).await;
    let v = t.preview(json!({ "paddock_id": f.paddock, "herd_id": f.herd, "orientation_deg": 0, "count": 12 })).await;
    let ss = strips_of(&v);
    assert_eq!(ss.len(), 12);
    assert!(close(sum(&v, "area_ha"), 16.556, 0.01), "{}", sum(&v, "area_ha"));
    for s in ss {
        assert_eq!(s["grazeable_ha"], s["area_ha"], "no exclusions: all of it is grazeable");
        assert!(s.get("days").is_none(), "no forage estimate, no days");
        let g: Polygon = serde_json::from_value(s["geometry"].clone()).unwrap();
        assert!(g.validated().is_ok());
    }
    // Strip 1 is the south end; the next strips lie north of it.
    let lat = |s: &Value| serde_json::from_value::<Polygon>(s["geometry"].clone()).unwrap().centroid().unwrap()[1];
    assert!(lat(&ss[0]) < lat(&ss[1]) && lat(&ss[10]) < lat(&ss[11]));
    assert_eq!(v["head"], 250);
    assert_eq!(v["animal_units"], 250.0);
    assert!(close(v["width_m"].as_f64().unwrap(), 400.3 / 12.0, 0.1), "{}", v["width_m"]);
    assert!(v.get("forage_kg_dm_per_ha").is_none());
    assert_eq!(v["warn_m"], 5.0);
}

#[tokio::test]
async fn a_width_cuts_from_the_start_and_a_thin_rest_joins_the_last_strip() {
    let t = setup().await;
    let f = farm(&t).await;
    // 400.3 m deep: 30 m strips leave 10.3 m, under the 12.5 m least strip at warn 5 m.
    let v = t.preview(json!({ "paddock_id": f.paddock, "orientation_deg": 0, "width_m": 30 })).await;
    assert_eq!(strips_of(&v).len(), 13);
    let last = strips_of(&v)[12]["area_ha"].as_f64().unwrap();
    let first = strips_of(&v)[0]["area_ha"].as_f64().unwrap();
    assert!(close(last / first, 40.3 / 30.0, 0.01), "{last} / {first}");
    // A wider warning zone needs wider strips: warn 10 m makes 22.5 m the least, so the
    // 10.3 m rest still joins, and 20 m strips go in pairs.
    let v = t.preview(json!({ "paddock_id": f.paddock, "orientation_deg": 0, "width_m": 20, "warn_m": 10 })).await;
    assert_eq!(strips_of(&v).len(), 10);
    // Turned 90 degrees the strips run north-south and step east.
    let v = t.preview(json!({ "paddock_id": f.paddock, "orientation_deg": 90, "count": 3 })).await;
    let lon = |s: &Value| serde_json::from_value::<Polygon>(s["geometry"].clone()).unwrap().centroid().unwrap()[0];
    let ss = strips_of(&v);
    assert!(lon(&ss[0]) < lon(&ss[1]) && lon(&ss[1]) < lon(&ss[2]));
    assert!(close(v["depth_m"].as_f64().unwrap(), 413.0, 0.5), "{}", v["depth_m"]);
}

#[tokio::test]
async fn grazeable_ground_leaves_out_exclusions_in_effect_now() {
    let t = setup().await;
    let f = farm(&t).await;
    let now = time::now();
    // A wet spot 100 m x ~110 m in the south-west corner of P1 (strip 1 of 4 is the south 100 m).
    let wet = vec![vec![[-93.625, 42.03], [-93.6238, 42.03], [-93.6238, 42.0309], [-93.625, 42.0309], [-93.625, 42.03]]];
    let excl = |geometry: Vec<Vec<[f64; 2]>>, paddock: Option<&str>, from, until| NewFeature {
        kind: FeatureKind::Exclusion,
        name: None,
        geometry: FeatureGeometry::Polygon(geometry),
        paddock_id: paddock.map(str::to_owned),
        notes: None,
        props: json!({}),
        active_from: from,
        active_until: until,
    };
    op_core::features::insert_feature(&t.ctx, excl(wet.clone(), Some(&f.paddock), None, Some(now + Duration::days(3)))).await.unwrap();
    // Not in effect: one that ended, and one that starts tomorrow, both over strip 4.
    let north = vec![vec![[-93.625, 42.0330], [-93.62, 42.0330], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.0330]]];
    op_core::features::insert_feature(&t.ctx, excl(north.clone(), Some(&f.paddock), Some(now - Duration::days(9)), Some(now - Duration::days(2))))
        .await
        .unwrap();
    op_core::features::insert_feature(&t.ctx, excl(north, None, Some(now + Duration::days(1)), None)).await.unwrap();
    // A farm-wide one that also counts, over strip 2's east end.
    let barn = vec![vec![[-93.6205, 42.0312], [-93.62, 42.0312], [-93.62, 42.0316], [-93.6205, 42.0316], [-93.6205, 42.0312]]];
    op_core::features::insert_feature(&t.ctx, excl(barn.clone(), None, None, None)).await.unwrap();

    let v = t.preview(json!({ "paddock_id": f.paddock, "orientation_deg": 0, "count": 4 })).await;
    let ss = strips_of(&v);
    let area = |r: &Vec<Vec<[f64; 2]>>| Polygon::from_ring(r[0].clone()).area_ha();
    let s1 = &ss[0];
    let wet_in_s1 = area(&wet); // the wet spot lies wholly inside strip 1 (0-100 m)
    assert!(close(s1["grazeable_ha"].as_f64().unwrap(), s1["area_ha"].as_f64().unwrap() - wet_in_s1, 0.01), "{s1}");
    // The barn straddles strips 1-2 (100 m ≈ 42.0309) and 2 only: 42.0312-42.0316 is all in strip 2 (100-200 m).
    let s2 = &ss[1];
    assert!(close(s2["grazeable_ha"].as_f64().unwrap(), s2["area_ha"].as_f64().unwrap() - area(&barn), 0.01), "{s2}");
    // Strip 4: the ended and the future exclusions take nothing.
    assert_eq!(ss[3]["grazeable_ha"], ss[3]["area_ha"]);
    assert_eq!(ss[2]["grazeable_ha"], ss[2]["area_ha"]);
}

#[tokio::test]
async fn days_come_from_the_paddock_forage_estimate() {
    let t = setup().await;
    let f = farm(&t).await;
    // NDVI 0.5 maps to 5 in; 2 in above the 3 in residual is 672 kg DM/ha.
    ndvi_report(&t.ctx, &f.paddock, 0.5).await;
    let v = t.preview(json!({ "paddock_id": f.paddock, "herd_id": f.herd, "orientation_deg": 0, "count": 4 })).await;
    assert_eq!(v["forage_kg_dm_per_ha"], 672.0);
    assert_eq!(v["forage_source"], "imagery");
    for s in strips_of(&v) {
        let want = calc::round(672.0 * s["grazeable_ha"].as_f64().unwrap() / (250.0 * 11.8), 1);
        assert_eq!(s["days"].as_f64().unwrap(), want, "{s}");
    }
    // 16.556 ha x 672 kg / 2,950 kg a day is 3.77 days over the paddock (each strip to 0.1 d).
    assert!(close(sum(&v, "days"), 3.77, 0.2), "{}", sum(&v, "days"));
    // Fewer head, more days: 50 head of the same herd.
    let v = t.preview(json!({ "paddock_id": f.paddock, "herd_id": f.herd, "orientation_deg": 0, "count": 4, "head": 50 })).await;
    assert_eq!(v["head"], 50);
    assert!(close(sum(&v, "days"), 18.86, 0.2), "{}", sum(&v, "days"));
}

#[tokio::test]
async fn days_pick_the_width_that_gives_that_many_days_per_strip() {
    let t = setup().await;
    let f = farm(&t).await;
    ndvi_report(&t.ctx, &f.paddock, 0.5).await;
    // Half a day of 250 head is 1,475 kg DM, 2.195 ha of 672 kg: 53 m of the 400 m.
    let v = t.preview(json!({ "paddock_id": f.paddock, "herd_id": f.herd, "orientation_deg": 0, "days": 0.5 })).await;
    assert!(close(v["width_m"].as_f64().unwrap(), 53.07, 0.1), "{}", v["width_m"]);
    let ss = strips_of(&v);
    assert_eq!(ss.len(), 8, "7 of 53 m and a 29 m rest");
    for s in &ss[..7] {
        assert_eq!(s["days"].as_f64().unwrap(), 0.5, "{s}");
    }
    assert!(ss[7]["days"].as_f64().unwrap() < 0.5);
    // Without a forage estimate days can't size anything.
    let p2 = t.ok("POST", "/api/paddocks", Some(json!({ "name": "P2", "geometry": p1() }))).await;
    let (s, e) = t.req("POST", "/api/strips/preview", Some(json!({ "paddock_id": p2["id"], "herd_id": f.herd, "orientation_deg": 0, "days": 1 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(e["error"].as_str().unwrap().contains("forage"), "{e}");
    // Nor without animals to feed.
    let (s, e) = t.req("POST", "/api/strips/preview", Some(json!({ "paddock_id": f.paddock, "orientation_deg": 0, "days": 1, "head": 0 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
}

#[tokio::test]
async fn a_measured_height_sizes_strips_the_same_way() {
    let t = setup().await;
    let f = farm(&t).await;
    let paddock = t.ctx.store().get_paddock(&f.paddock).await.unwrap().unwrap();
    let herd = t.ctx.store().get_herd(&f.herd).await.unwrap().unwrap();
    // A farmer's 6 in stick reading: 3 in above the residual, 1,008 kg DM/ha.
    let est = calc::forage_estimate(None, Some(6.0), calc::DEFAULT_RESIDUAL_INCHES);
    let forage = Forage { kg_dm_per_ha: est["available_kg_dm_per_ha"].as_f64().unwrap(), source: est["source"].as_str().map(str::to_owned) };
    assert_eq!(forage.kg_dm_per_ha, 1008.0);
    let by_count = StripParams { orientation_deg: 0.0, count: Some(4), ..Default::default() };
    let v = strips::preview_with(&t.ctx, &paddock, Some(&herd), &by_count, Some(forage.clone())).await.unwrap();
    for s in &v.strips {
        assert_eq!(s.days, Some(calc::round(1008.0 * s.grazeable_ha / 2950.0, 1)));
    }
    assert_eq!(v.forage_source.as_deref(), Some("farmer"));
    // Sized by days: 1 day for 250 head is 2,950 kg DM, 2.93 ha of 1,008 kg: about 71 m of the 400 m.
    let by_days = StripParams { orientation_deg: 0.0, days: Some(1.0), ..Default::default() };
    let v = strips::preview_with(&t.ctx, &paddock, Some(&herd), &by_days, Some(forage)).await.unwrap();
    let want = v.depth_m * (2950.0 / 1008.0) / paddock.geometry.area_ha();
    assert!(close(v.width_m, want, 0.01) && close(want, 70.76, 0.1), "{} vs {want}", v.width_m);
    assert_eq!(v.strips.len(), 6, "5 of 71 m and a 46 m rest");
    assert!(v.strips[..5].iter().all(|s| s.days == Some(1.0)), "{:?}", v.strips.iter().map(|s| s.days).collect::<Vec<_>>());
    // The pure rule: forage × grazeable ÷ (AU × 11.8 kg DM a day), none without animals.
    assert_eq!(strips::strip_days(1008.0, 2.95, 250.0), Some(1.0));
    assert_eq!(strips::strip_days(1008.0, 2.95, 0.0), None);
}

#[tokio::test]
async fn bad_requests_say_what_is_wrong() {
    let t = setup().await;
    let f = farm(&t).await;
    for (body, code) in [
        (json!({ "paddock_id": f.paddock, "orientation_deg": 0 }), StatusCode::BAD_REQUEST),
        (json!({ "paddock_id": f.paddock, "orientation_deg": 0, "count": 3, "width_m": 30 }), StatusCode::BAD_REQUEST),
        (json!({ "paddock_id": f.paddock, "orientation_deg": 0, "count": 0 }), StatusCode::BAD_REQUEST),
        (json!({ "paddock_id": f.paddock, "orientation_deg": 0, "count": 201 }), StatusCode::BAD_REQUEST),
        (json!({ "paddock_id": f.paddock, "orientation_deg": 0, "width_m": -5 }), StatusCode::BAD_REQUEST),
        (json!({ "paddock_id": f.paddock, "orientation_deg": 0, "width_m": 30, "warn_m": 2000 }), StatusCode::BAD_REQUEST),
        (json!({ "paddock_id": "pad_nope", "orientation_deg": 0, "count": 3 }), StatusCode::NOT_FOUND),
        (json!({ "paddock_id": f.paddock, "herd_id": "herd_nope", "orientation_deg": 0, "count": 3 }), StatusCode::NOT_FOUND),
        (json!({ "paddock_id": f.paddock, "count": 3 }), StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let (s, v) = t.req("POST", "/api/strips/preview", Some(body.clone())).await;
        assert_eq!(s, code, "{body}: {v}");
        assert!(v["error"].is_string(), "{v}");
    }
    // 1 m strips merge up to the 12.5 m least strip: 30 of 13 m, the 10.3 m rest joining the last.
    let v = t.preview(json!({ "paddock_id": f.paddock, "orientation_deg": 0, "width_m": 1 })).await;
    assert_eq!(strips_of(&v).len(), 30);
    // A paddock 3 km deep in 13 m strips would make 230: more than a layout holds.
    let long = json!({ "type": "Polygon", "coordinates": [[[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.057], [-93.625, 42.057], [-93.625, 42.03]]] });
    let lp = t.ok("POST", "/api/paddocks", Some(json!({ "name": "Long", "geometry": long }))).await;
    let (s, e) = t.req("POST", "/api/strips/preview", Some(json!({ "paddock_id": lp["id"], "orientation_deg": 0, "width_m": 13 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(e["error"].as_str().unwrap().contains("200"), "{e}");
}
