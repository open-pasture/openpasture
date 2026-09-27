//! Saved strip layouts and paddock copies through the real routes.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Polygon, Via, time};
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
    let who = Identity { name: Some("Cody".into()), user_id: Some("usr_cody".into()), ..Identity::owner(Via::AppToken) };
    T { _dir: dir, ctx, app: op_core::with_identity(app, who) }
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
}

fn p1() -> Value {
    json!({ "type": "Polygon", "coordinates": [[[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]]] })
}

async fn farm(t: &T) -> (String, String) {
    t.ok("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03] }))).await;
    let p = t.ok("POST", "/api/paddocks", Some(json!({ "name": "P3", "geometry": p1() }))).await;
    let h = t.ok("POST", "/api/herds", Some(json!({ "name": "Cows", "species": "cattle", "count": 250, "paddock_id": p["id"] }))).await;
    (p["id"].as_str().unwrap().into(), h["id"].as_str().unwrap().into())
}

/// A cached land report with this mean NDVI, `later_s` seconds after now (the newest report counts).
async fn ndvi_report(ctx: &Ctx, paddock_id: &str, ndvi: f64, later_s: i64) {
    let now = time::to_db(&(time::now() + chrono::Duration::seconds(later_s)));
    let id = op_core::id::new_id("lr");
    let report = json!({ "report_id": id, "paddock_id": paddock_id, "source": "alexandria", "as_of": now, "geometry": p1(),
        "sections": { "imagery": { "status": "ok", "ndvi_stats": { "mean": ndvi }, "sources": [] } } });
    sqlx::query("INSERT INTO land_reports (id, paddock_id, cache_key, source, as_of, report, created_at) VALUES (?, ?, 'test', 'alexandria', ?, ?, ?)")
        .bind(&id)
        .bind(paddock_id)
        .bind(&now)
        .bind(report.to_string())
        .bind(&now)
        .execute(ctx.db())
        .await
        .unwrap();
}

fn area(g: &Value) -> f64 {
    serde_json::from_value::<Polygon>(g.clone()).unwrap().area_ha()
}

#[tokio::test]
async fn layouts_are_saved_listed_renamed_and_deleted() {
    let t = setup().await;
    let (pad, herd) = farm(&t).await;
    let preview = t.ok("POST", "/api/strips/preview", Some(json!({ "paddock_id": pad, "herd_id": herd, "orientation_deg": 0, "count": 12 }))).await;
    let (s, l) = t.req("POST", "/api/layouts", Some(json!({ "paddock_id": pad, "herd_id": herd, "orientation_deg": 0, "count": 12 }))).await;
    assert_eq!(s, StatusCode::CREATED, "{l}");
    assert!(l["id"].as_str().unwrap().starts_with("lay_"));
    assert_eq!(l["name"], "12 strips");
    assert_eq!(l["paddock_id"], pad.as_str());
    assert_eq!(l["params"], json!({ "orientation_deg": 0.0, "count": 12 }));
    assert_eq!(l["created_by"], json!({ "via": "app_token", "user_id": "usr_cody", "name": "Cody" }));
    // The stored strips are the preview's strips.
    let previewed: Vec<Value> = preview["strips"].as_array().unwrap().iter().map(|s| s["geometry"].clone()).collect();
    assert_eq!(l["strips"].as_array().unwrap(), &previewed);

    // A second one of the same shape gets a name of its own; a named one keeps its name.
    let l2 = t.ok("POST", "/api/layouts", Some(json!({ "paddock_id": pad, "orientation_deg": 90, "count": 12 }))).await;
    assert_eq!(l2["name"], "12 strips 2");
    let l3 = t.ok("POST", "/api/layouts", Some(json!({ "paddock_id": pad, "orientation_deg": 0, "width_m": 50, "name": "  Spring  " }))).await;
    assert_eq!(l3["name"], "Spring");

    let list = t.ok("GET", &format!("/api/layouts?paddock_id={pad}"), None).await;
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["12 strips", "12 strips 2", "Spring"]);
    let id = l["id"].as_str().unwrap();
    assert_eq!(t.ok("GET", &format!("/api/layouts/{id}"), None).await, l);

    let renamed = t.ok("PATCH", &format!("/api/layouts/{id}"), Some(json!({ "name": "North to south" }))).await;
    assert_eq!(renamed["name"], "North to south");
    assert_eq!(t.ok("GET", &format!("/api/layouts/{id}"), None).await["name"], "North to south");
    let (s, _) = t.req("PATCH", &format!("/api/layouts/{id}"), Some(json!({ "name": "   " }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = t.req("DELETE", &format!("/api/layouts/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = t.req("GET", &format!("/api/layouts/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = t.req("DELETE", &format!("/api/layouts/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(t.ok("GET", "/api/layouts", None).await.as_array().unwrap().len(), 2);

    // Bad settings are refused as a preview would refuse them.
    let (s, _) = t.req("POST", "/api/layouts", Some(json!({ "paddock_id": pad, "orientation_deg": 0 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = t.req("POST", "/api/layouts", Some(json!({ "paddock_id": "pad_nope", "orientation_deg": 0, "count": 2 }))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // A deleted paddock takes its layouts with it.
    t.ok("DELETE", &format!("/api/paddocks/{pad}"), None).await;
    assert!(t.ok("GET", "/api/layouts", None).await.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn applying_a_layout_gives_its_strips_with_todays_days() {
    let t = setup().await;
    let (pad, herd) = farm(&t).await;
    let l = t.ok("POST", "/api/layouts", Some(json!({ "paddock_id": pad, "orientation_deg": 0, "count": 4 }))).await;
    let id = l["id"].as_str().unwrap();
    // No forage yet: the strips, no days.
    let a = t.ok("POST", &format!("/api/layouts/{id}/apply"), Some(json!({ "herd_id": herd }))).await;
    assert_eq!(a["layout"]["strips"], l["strips"]);
    let strips = a["strips"].as_array().unwrap();
    assert_eq!(strips.len(), 4);
    assert!(strips.iter().all(|s| s.get("days").is_none()));
    for (s, g) in strips.iter().zip(l["strips"].as_array().unwrap()) {
        assert_eq!(&s["geometry"], g);
    }
    assert_eq!(a["head"], 250);
    // Forage came in: the same strips, now with days.
    ndvi_report(&t.ctx, &pad, 0.5, 0).await;
    let a = t.ok("POST", &format!("/api/layouts/{id}/apply"), Some(json!({ "herd_id": herd }))).await;
    assert_eq!(a["layout"]["strips"], l["strips"]);
    assert_eq!(a["forage_kg_dm_per_ha"], 672.0);
    for s in a["strips"].as_array().unwrap() {
        // 60 % of the strip's forage at 11.8 kg DM per AU a day, as every grazing-days figure.
        assert_eq!(s["days"].as_f64().unwrap(), op_engine::calc::round(672.0 * s["grazeable_ha"].as_f64().unwrap() * 0.6 / 2950.0, 1));
    }
    // For fewer head: a quarter of 16.556 ha at 672 kg, 60 % eaten by 25 head (295 kg a day), is 5.7 days.
    let a = t.ok("POST", &format!("/api/layouts/{id}/apply"), Some(json!({ "herd_id": herd, "head": 25 }))).await;
    assert_eq!(a["head"], 25);
    assert!((a["strips"][0]["days"].as_f64().unwrap() - 5.7).abs() < 0.15, "{}", a["strips"][0]);
    let (s, _) = t.req("POST", "/api/layouts/lay_nope/apply", Some(json!({}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_layout_follows_its_paddock_when_the_shape_changes() {
    let t = setup().await;
    let (pad, herd) = farm(&t).await;
    let l = t.ok("POST", "/api/layouts", Some(json!({ "paddock_id": pad, "orientation_deg": 0, "width_m": 100 }))).await;
    let id = l["id"].as_str().unwrap();
    assert_eq!(l["strips"].as_array().unwrap().len(), 4);
    // The paddock grows 200 m north.
    let bigger = json!({ "type": "Polygon", "coordinates": [[[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0354], [-93.625, 42.0354], [-93.625, 42.03]]] });
    t.ok("PATCH", &format!("/api/paddocks/{pad}"), Some(json!({ "geometry": bigger }))).await;
    let a = t.ok("POST", &format!("/api/layouts/{id}/apply"), Some(json!({ "herd_id": herd }))).await;
    let strips = a["layout"]["strips"].as_array().unwrap();
    assert_eq!(strips.len(), 6, "the same 100 m strips over 600 m");
    assert_eq!(a["layout"]["params"], l["params"]);
    let total: f64 = strips.iter().map(area).sum();
    assert!((total - area(&bigger)).abs() < 0.01, "{total}");
    // Stored: the next read has the new strips.
    let stored = t.ok("GET", &format!("/api/layouts/{id}"), None).await;
    assert_eq!(stored["strips"].as_array().unwrap().len(), 6);
}

#[tokio::test]
async fn a_layout_sized_by_days_keeps_its_width_when_forage_changes() {
    let t = setup().await;
    let (pad, herd) = farm(&t).await;
    ndvi_report(&t.ctx, &pad, 0.5, 0).await;
    let l = t.ok("POST", "/api/layouts", Some(json!({ "paddock_id": pad, "herd_id": herd, "orientation_deg": 0, "days": 0.5 }))).await;
    // Half a day of 250 head eats 1,475 kg DM, 60 % of 2,458 kg standing: 3.658 ha of 672 kg, 88 m of the 400 m.
    let width = l["params"]["width_m"].as_f64().unwrap();
    assert!((width - 88.45).abs() < 0.1, "{width}");
    assert_eq!(l["params"]["days"], 0.5);
    // The grass grew: the same strips, more days each.
    ndvi_report(&t.ctx, &pad, 0.8, 1).await;
    let a = t.ok("POST", &format!("/api/layouts/{id}/apply", id = l["id"].as_str().unwrap()), Some(json!({ "herd_id": herd }))).await;
    assert_eq!(a["layout"]["strips"], l["strips"]);
    assert_eq!(a["forage_kg_dm_per_ha"], 2352.0);
    assert!((a["strips"][0]["days"].as_f64().unwrap() - 1.75).abs() < 0.06, "{}", a["strips"][0]);
    // Reshaped, it is cut again at the width it kept, not from today's forage.
    let bigger = json!({ "type": "Polygon", "coordinates": [[[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0354], [-93.625, 42.0354], [-93.625, 42.03]]] });
    t.ok("PATCH", &format!("/api/paddocks/{pad}"), Some(json!({ "geometry": bigger }))).await;
    let a = t.ok("POST", &format!("/api/layouts/{id}/apply", id = l["id"].as_str().unwrap()), Some(json!({ "herd_id": herd }))).await;
    assert!((a["width_m"].as_f64().unwrap() - width).abs() < 0.01, "{}", a["width_m"]);
}

#[tokio::test]
async fn a_paddock_copy_has_the_same_shape_and_a_name_of_its_own() {
    let t = setup().await;
    let (pad, _) = farm(&t).await;
    t.ok("PATCH", &format!("/api/paddocks/{pad}"), Some(json!({ "notes": "Wet in spring", "props": { "fsa_field": "4" } }))).await;
    let (s, c) = t.req("POST", &format!("/api/paddocks/{pad}/copy"), Some(json!({}))).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    assert_eq!(c["name"], "P3 copy");
    assert_eq!(c["geometry"], p1());
    assert_eq!(c["area_ha"], 16.556);
    assert_ne!(c["id"], pad.as_str());
    assert!(c.get("notes").is_none() && c.get("props").is_none(), "{c}");
    let c2 = t.ok("POST", &format!("/api/paddocks/{pad}/copy"), Some(json!({}))).await;
    assert_eq!(c2["name"], "P3 copy 2");
    // Moved 500 m east, it keeps its size.
    let c3 = t.ok("POST", &format!("/api/paddocks/{pad}/copy"), Some(json!({ "offset_m": [500, 0], "name": "P9" }))).await;
    assert_eq!(c3["name"], "P9");
    let g: Polygon = serde_json::from_value(c3["geometry"].clone()).unwrap();
    let o: Polygon = serde_json::from_value(p1()).unwrap();
    let moved = op_geo::projection::distance_m(o.centroid().unwrap(), g.centroid().unwrap());
    assert!((moved - 500.0).abs() < 0.5, "{moved}");
    assert!((g.centroid().unwrap()[1] - o.centroid().unwrap()[1]).abs() < 1e-6);
    assert!((c3["area_ha"].as_f64().unwrap() - 16.556).abs() < 0.01);
    // Every paddock is there once.
    let names: Vec<String> = t.ok("GET", "/api/paddocks", None).await.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap().to_owned()).collect();
    assert_eq!(names.len(), 4, "{names:?}");
    let (s, _) = t.req("POST", "/api/paddocks/pad_nope/copy", Some(json!({}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = t.req("POST", &format!("/api/paddocks/{pad}/copy"), Some(json!({ "offset_m": [20000, 0] }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
