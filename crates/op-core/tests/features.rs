//! Map features over REST (stream D): CRUD per kind, geometry per kind,
//! one farm boundary, paddock or farm scope, active windows, paddock delete,
//! live events, place phrases from named landmarks and the MCP tool.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, SecondsFormat, Utc};
use http_body_util::BodyExt;
use op_core::features;
use op_core::tools::ToolScope;
use op_core::units::{Fmt, Units};
use op_core::*;
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: Router,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let router = op_core::router().with_state(ctx.clone());
        Self { _dir: dir, ctx, router }
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = self.router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.call(method, path, body).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    /// Farm around Ames plus P1 (0..400 m) and P2 (1000..1400 m east). Returns their ids.
    async fn farm(&self) -> (String, String) {
        self.ok("POST", "/api/farm", Some(json!({"name": "Home", "center": [-93.62, 42.03]}))).await;
        let p1 = self.ok("POST", "/api/paddocks", Some(json!({"name": "P1", "geometry": square(0.0, 0.0, 400.0)}))).await;
        let p2 = self.ok("POST", "/api/paddocks", Some(json!({"name": "P2", "geometry": square(1000.0, 0.0, 400.0)}))).await;
        (p1["id"].as_str().unwrap().to_owned(), p2["id"].as_str().unwrap().to_owned())
    }

    async fn create(&self, body: Value) -> Value {
        let (s, f) = self.call("POST", "/api/features", Some(body.clone())).await;
        assert_eq!(s, StatusCode::CREATED, "{body} → {f}");
        f
    }

    async fn ids(&self, query: &str) -> Vec<String> {
        let list = self.ok("GET", &format!("/api/features{query}"), None).await;
        list.as_array().unwrap().iter().map(|f| f["id"].as_str().unwrap().to_owned()).collect()
    }
}

fn proj() -> op_geo::Projection {
    op_geo::Projection::new([-93.62, 42.03])
}

/// A spot `east`/`north` metres from the farm centre.
fn at(east: f64, north: f64) -> LonLat {
    proj().offset(east, north)
}

fn ring(east: f64, north: f64, side: f64) -> Vec<LonLat> {
    vec![at(east, north), at(east + side, north), at(east + side, north + side), at(east, north + side), at(east, north)]
}

/// GeoJSON Polygon, about `side` metres square.
fn square(east: f64, north: f64, side: f64) -> Value {
    json!({"type": "Polygon", "coordinates": [ring(east, north, side)]})
}

fn point(east: f64, north: f64) -> Value {
    json!({"type": "Point", "coordinates": at(east, north)})
}

fn line(from: (f64, f64), to: (f64, f64)) -> Value {
    json!({"type": "LineString", "coordinates": [at(from.0, from.1), at(to.0, to.1)]})
}

fn rfc(t: chrono::DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// One of each kind, in each geometry the kind allows.
fn one_of_each() -> Vec<Value> {
    vec![
        json!({"kind": "exclusion", "name": "wet spot", "geometry": square(100.0, 100.0, 40.0)}),
        json!({"kind": "water", "name": "trough", "geometry": point(200.0, 200.0), "props": {"source": "trough"}}),
        json!({"kind": "water", "name": "north pond", "geometry": square(250.0, 300.0, 50.0)}),
        json!({"kind": "gate", "name": "east gate", "geometry": point(400.0, 200.0)}),
        json!({"kind": "shade", "geometry": point(50.0, 350.0)}),
        json!({"kind": "shade", "name": "tree line", "geometry": square(20.0, 20.0, 30.0)}),
        json!({"kind": "hazard", "name": "old well", "geometry": point(300.0, 50.0), "props": {"radius_m": 15.0}}),
        json!({"kind": "hazard", "geometry": square(320.0, 120.0, 30.0)}),
        json!({"kind": "road", "name": "county road", "geometry": line((-50.0, -20.0), (1500.0, -20.0))}),
        json!({"kind": "neighbour_line", "geometry": line((-30.0, 500.0), (1500.0, 500.0))}),
        json!({"kind": "farm_boundary", "geometry": square(-60.0, -60.0, 1600.0)}),
    ]
}

#[tokio::test]
async fn each_kind_is_created_read_changed_and_deleted() {
    let app = App::new().await;
    app.farm().await;
    let mut ids = vec![];
    for body in one_of_each() {
        let f = app.create(body.clone()).await;
        let id = f["id"].as_str().unwrap().to_owned();
        assert!(id.starts_with("fea_"), "{f}");
        assert_eq!(f["kind"], body["kind"]);
        assert_eq!(f["geometry"]["type"], body["geometry"]["type"]);
        assert!(f.get("paddock_id").is_none(), "farm-wide unless a paddock is given");
        assert_eq!(app.ok("GET", &format!("/api/features/{id}"), None).await, f);
        ids.push(id);
    }
    // Listed in the order they were drawn.
    assert_eq!(app.ids("").await, ids);
    assert_eq!(app.ids("?kind=water").await, ids[1..3]);
    assert_eq!(app.ids("?kind=neighbour_line").await, ids[9..10]);

    // Change: name, notes, props, geometry.
    let gate = &ids[3];
    let changed = app
        .ok("PATCH", &format!("/api/features/{gate}"), Some(json!({"name": "  south gate ", "notes": "chain, no lock", "geometry": point(200.0, 0.0)})))
        .await;
    assert_eq!(changed["name"], "south gate");
    assert_eq!(changed["notes"], "chain, no lock");
    // Stored to 7 decimals, as a collar reads it.
    let r7 = |p: LonLat| [op_geo::projection::round7(p[0]), op_geo::projection::round7(p[1])];
    assert_eq!(changed["geometry"]["coordinates"], json!(r7(at(200.0, 0.0))));
    assert_eq!(changed["kind"], "gate");
    assert!(changed["updated_at"].as_str().unwrap() >= changed["created_at"].as_str().unwrap());
    // kind, id and created_at don't change; null clears an optional field.
    let same = app
        .ok("PATCH", &format!("/api/features/{gate}"), Some(json!({"kind": "water", "id": "fea_x", "notes": null, "created_at": "2020-01-01T00:00:00Z"})))
        .await;
    assert_eq!((same["kind"].as_str(), same["id"].as_str()), (Some("gate"), Some(gate.as_str())));
    assert_eq!(same["created_at"], changed["created_at"]);
    assert!(same.get("notes").is_none());
    assert_eq!(app.ok("GET", &format!("/api/features/{gate}"), None).await, same);
    let trough = app.ok("PATCH", &format!("/api/features/{}", ids[1]), Some(json!({"props": {"source": "tank"}}))).await;
    assert_eq!(trough["props"], json!({"source": "tank"}));

    // Delete.
    for id in &ids {
        let (s, _) = app.call("DELETE", &format!("/api/features/{id}"), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, e) = app.call("GET", &format!("/api/features/{id}"), None).await;
        assert_eq!((s, e["error"].as_str()), (StatusCode::NOT_FOUND, Some("No such feature.")));
    }
    assert!(app.ids("").await.is_empty());
    for (m, body) in [("PATCH", Some(json!({"name": "x"}))), ("DELETE", None)] {
        assert_eq!(app.call(m, "/api/features/fea_nope", body).await.0, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn the_wrong_geometry_for_a_kind_is_400() {
    let app = App::new().await;
    app.farm().await;
    for (kind, g, msg) in [
        ("exclusion", point(10.0, 10.0), "An exclusion is drawn as a polygon."),
        ("exclusion", line((0.0, 0.0), (10.0, 0.0)), "An exclusion is drawn as a polygon."),
        ("gate", square(0.0, 0.0, 10.0), "A gate is drawn as a point."),
        ("water", line((0.0, 0.0), (10.0, 0.0)), "Water is drawn as a point or polygon."),
        ("shade", line((0.0, 0.0), (10.0, 0.0)), "Shade is drawn as a point or polygon."),
        ("road", point(1.0, 1.0), "A road is drawn as a linestring."),
        ("neighbour_line", square(0.0, 0.0, 10.0), "A neighbour line is drawn as a linestring."),
        ("farm_boundary", point(1.0, 1.0), "The farm boundary is drawn as a polygon."),
    ] {
        let (s, e) = app.call("POST", "/api/features", Some(json!({"kind": kind, "geometry": g}))).await;
        assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some(msg)), "{kind}");
    }
    let (s, e) = app.call("POST", "/api/features", Some(json!({"kind": "hazard", "geometry": point(5.0, 5.0)}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("A hazard point needs radius_m.")));
    let holed = json!({"type": "Polygon", "coordinates": [ring(0.0, 0.0, 100.0), ring(40.0, 40.0, 10.0)]});
    let (s, e) = app.call("POST", "/api/features", Some(json!({"kind": "exclusion", "geometry": holed}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("An exclusion is one ring, without holes.")));
    let bowtie = json!({"type": "Polygon", "coordinates": [[at(0.0, 0.0), at(50.0, 50.0), at(50.0, 0.0), at(0.0, 50.0), at(0.0, 0.0)]]});
    assert_eq!(app.call("POST", "/api/features", Some(json!({"kind": "exclusion", "geometry": bowtie}))).await.0, StatusCode::BAD_REQUEST);
    let (s, _) = app.call("POST", "/api/features", Some(json!({"kind": "pond", "geometry": point(1.0, 1.0)}))).await;
    assert!(s.is_client_error(), "unknown kind: {s}");
    assert_eq!(app.call("POST", "/api/features", Some(json!({"kind": "gate", "geometry": point(1.0, 1.0), "props": [1]}))).await.0, StatusCode::BAD_REQUEST);

    // A change is checked the same way.
    let gate = app.create(json!({"kind": "gate", "geometry": point(1.0, 1.0)})).await;
    let hazard = app.create(json!({"kind": "hazard", "geometry": point(5.0, 5.0), "props": {"radius_m": 8}})).await;
    let url = |f: &Value| format!("/api/features/{}", f["id"].as_str().unwrap());
    let (s, e) = app.call("PATCH", &url(&gate), Some(json!({"geometry": square(0.0, 0.0, 10.0)}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("A gate is drawn as a point.")));
    let (s, e) = app.call("PATCH", &url(&hazard), Some(json!({"props": {"radius_m": null}}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("A hazard point needs radius_m.")));
    let (s, _) = app.call("PATCH", &url(&hazard), Some(json!({"props": {"radius_m": -3}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(app.call("PATCH", &url(&gate), Some(json!({"name": "x".repeat(201)}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(app.call("PATCH", &url(&gate), Some(json!([1, 2]))).await.0, StatusCode::BAD_REQUEST);
    // Nothing changed.
    assert_eq!(app.ok("GET", &url(&gate), None).await, gate);
    assert_eq!(app.ok("GET", &url(&hazard), None).await, hazard);
}

#[tokio::test]
async fn a_farm_has_one_boundary() {
    let app = App::new().await;
    app.farm().await;
    let first = app.create(json!({"kind": "farm_boundary", "geometry": square(-60.0, -60.0, 1600.0)})).await;
    let (s, e) = app.call("POST", "/api/features", Some(json!({"kind": "farm_boundary", "geometry": square(-80.0, -80.0, 1700.0)}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::CONFLICT, Some("The farm already has a boundary. Edit that one instead.")));
    // Editing the one there is fine; after deleting it, a new one can be drawn.
    let url = format!("/api/features/{}", first["id"].as_str().unwrap());
    app.ok("PATCH", &url, Some(json!({"geometry": square(-80.0, -80.0, 1700.0)}))).await;
    assert_eq!(app.call("DELETE", &url, None).await.0, StatusCode::NO_CONTENT);
    app.create(json!({"kind": "farm_boundary", "geometry": square(-80.0, -80.0, 1700.0)})).await;
    assert_eq!(app.ids("?kind=farm_boundary").await.len(), 1);
}

#[tokio::test]
async fn a_feature_belongs_to_a_paddock_or_the_whole_farm() {
    let app = App::new().await;
    let (p1, p2) = app.farm().await;
    let wet = app.create(json!({"kind": "exclusion", "name": "wet spot", "geometry": square(100.0, 100.0, 40.0), "paddock_id": p1})).await;
    let wet_id = wet["id"].as_str().unwrap().to_owned();
    let url = format!("/api/features/{wet_id}");
    assert_eq!(wet["paddock_id"], p1.as_str());
    let farm_wide = app.create(json!({"kind": "exclusion", "geometry": square(1100.0, 100.0, 40.0), "paddock_id": ""})).await;
    assert!(farm_wide.get("paddock_id").is_none(), "an empty paddock id is farm-wide");
    let fw_id = farm_wide["id"].as_str().unwrap().to_owned();
    assert_eq!(app.ids(&format!("?paddock_id={p1}")).await, [wet_id.clone()]);
    assert!(app.ids(&format!("?paddock_id={p2}")).await.is_empty());

    // A boundary in P2 is kept off P1's exclusions but not the farm-wide one.
    let now = time::now();
    let in_p2 = op_core::Polygon::from_ring(ring(1010.0, 10.0, 300.0));
    let got = |fs: Vec<features::MapFeature>| fs.into_iter().map(|f| f.id).collect::<Vec<_>>();
    assert_eq!(got(features::exclusions_for(&app.ctx, &in_p2, now).await.unwrap()), [fw_id.clone()]);

    // Whole farm, then P2.
    let moved = app.ok("PATCH", &url, Some(json!({"paddock_id": null}))).await;
    assert!(moved.get("paddock_id").is_none());
    assert!(app.ids(&format!("?paddock_id={p1}")).await.is_empty());
    assert_eq!(got(features::exclusions_for(&app.ctx, &in_p2, now).await.unwrap()), [wet_id.clone(), fw_id.clone()]);
    let moved = app.ok("PATCH", &url, Some(json!({"paddock_id": p2}))).await;
    assert_eq!(moved["paddock_id"], p2.as_str());
    assert_eq!(app.ids(&format!("?paddock_id={p2}")).await, [wet_id.clone()]);
    let (s, e) = app.call("PATCH", &url, Some(json!({"paddock_id": "pad_nope"}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("No such paddock.")));
    let (s, e) = app.call("POST", "/api/features", Some(json!({"kind": "gate", "geometry": point(1.0, 1.0), "paddock_id": "pad_nope"}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("No such paddock.")));
}

#[tokio::test]
async fn active_windows_filter_lists_and_exclusions() {
    let app = App::new().await;
    let (p1, _) = app.farm().await;
    let now = Utc::now();
    let h = |n: i64| rfc(now + Duration::hours(n));
    let mk = |name: &str, from: Option<String>, until: Option<String>| {
        let mut b = json!({"kind": "exclusion", "name": name, "geometry": square(100.0, 100.0, 40.0), "paddock_id": p1});
        if let Some(f) = from {
            b["active_from"] = json!(f);
        }
        if let Some(u) = until {
            b["active_until"] = json!(u);
        }
        b
    };
    let lasting = app.create(mk("lasting", None, None)).await["id"].as_str().unwrap().to_owned();
    let current = app.create(mk("reseeding", Some(h(-1)), Some(h(1)))).await["id"].as_str().unwrap().to_owned();
    let future = app.create(mk("calving pen", Some(h(2)), None)).await["id"].as_str().unwrap().to_owned();
    let expired = app.create(mk("wet spot", None, Some(h(-1)))).await["id"].as_str().unwrap().to_owned();
    let (s, e) = app.call("POST", "/api/features", Some(mk("backwards", Some(h(2)), Some(h(1))))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("active_until must be after active_from.")));

    assert_eq!(app.ids("").await, [lasting.clone(), current.clone(), future.clone(), expired.clone()]);
    assert_eq!(app.ids("?active=true").await, [lasting.clone(), current.clone()]);
    assert_eq!(app.ids(&format!("?active={}", h(3).replace('+', "%2B"))).await, [lasting.clone(), future.clone()]);
    assert_eq!(app.ids(&format!("?active={}&kind=exclusion&paddock_id={p1}", h(-2))).await, [lasting.clone(), expired.clone()]);
    let (s, e) = app.call("GET", "/api/features?active=soon", None).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("active is true or an RFC 3339 time.")));
    assert_eq!(app.call("GET", "/api/features?kind=pond", None).await.0, StatusCode::BAD_REQUEST);

    // What a boundary in P1 sent now, or in three hours, is kept off.
    let area = op_core::Polygon::from_ring(ring(10.0, 10.0, 300.0));
    let got = |fs: Vec<features::MapFeature>| fs.into_iter().map(|f| f.id).collect::<Vec<_>>();
    let t = op_core::time::now();
    assert_eq!(got(features::exclusions_for(&app.ctx, &area, t).await.unwrap()), [lasting.clone(), current.clone()]);
    assert_eq!(got(features::exclusions_for(&app.ctx, &area, t + Duration::hours(3)).await.unwrap()), [lasting.clone(), future.clone()]);
    assert_eq!(got(features::list_features(&app.ctx, None, None, Some(t)).await.unwrap()), [lasting.clone(), current.clone()]);

    // Ending the reseeding early takes it out now; clearing the end makes it lasting again.
    let url = format!("/api/features/{current}");
    let (s, _) = app.call("PATCH", &url, Some(json!({"active_until": h(-2)}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "until before from");
    app.ok("PATCH", &url, Some(json!({"active_until": rfc(Utc::now())}))).await;
    assert_eq!(app.ids("?active=true").await, [lasting.clone()]);
    let f = app.ok("PATCH", &url, Some(json!({"active_until": null}))).await;
    assert!(f.get("active_until").is_none());
    assert_eq!(app.ids("?active=true").await, [lasting.clone(), current.clone()]);
}

#[tokio::test]
async fn deleting_a_paddock_deletes_its_features() {
    let app = App::new().await;
    let (p1, p2) = app.farm().await;
    let a = app.create(json!({"kind": "exclusion", "geometry": square(100.0, 100.0, 40.0), "paddock_id": p1})).await;
    let b = app.create(json!({"kind": "water", "geometry": point(200.0, 200.0), "paddock_id": p1})).await;
    let c = app.create(json!({"kind": "gate", "geometry": point(1100.0, 0.0), "paddock_id": p2})).await;
    let d = app.create(json!({"kind": "road", "geometry": line((0.0, -20.0), (900.0, -20.0))})).await;
    assert_eq!(app.call("DELETE", &format!("/api/paddocks/{p1}"), None).await.0, StatusCode::NO_CONTENT);
    let id = |f: &Value| f["id"].as_str().unwrap().to_owned();
    assert_eq!(app.ids("").await, [id(&c), id(&d)]);
    for f in [a, b] {
        assert_eq!(app.call("GET", &format!("/api/features/{}", id(&f)), None).await.0, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn every_change_publishes_a_feature_event() {
    let app = App::new().await;
    app.farm().await;
    let mut rx = app.ctx.subscribe();
    let f = app.create(json!({"kind": "gate", "name": "east gate", "geometry": point(400.0, 200.0)})).await;
    let url = format!("/api/features/{}", f["id"].as_str().unwrap());
    let next = |rx: &mut tokio::sync::broadcast::Receiver<Event>| match rx.try_recv() {
        Ok(Event::Feature { feature, deleted }) => (serde_json::to_value(feature).unwrap(), deleted),
        other => panic!("expected a feature event, got {other:?}"),
    };
    assert_eq!(next(&mut rx), (f.clone(), false));
    let changed = app.ok("PATCH", &url, Some(json!({"name": "south gate"}))).await;
    assert_eq!(next(&mut rx), (changed.clone(), false));
    // A refused change or create says nothing.
    assert_eq!(app.call("PATCH", &url, Some(json!({"geometry": square(0.0, 0.0, 10.0)}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(app.call("POST", "/api/features", Some(json!({"kind": "gate", "geometry": square(0.0, 0.0, 10.0)}))).await.0, StatusCode::BAD_REQUEST);
    assert!(rx.try_recv().is_err());
    assert_eq!(app.call("DELETE", &url, None).await.0, StatusCode::NO_CONTENT);
    let (gone, deleted) = next(&mut rx);
    assert!(deleted);
    assert_eq!(gone, changed);
    // On the wire: `deleted` only when true.
    let v = serde_json::to_value(Event::Feature { feature: serde_json::from_value(gone).unwrap(), deleted: true }).unwrap();
    assert_eq!((v["type"].as_str(), v["deleted"].as_bool()), (Some("feature"), Some(true)));
}

#[tokio::test]
async fn place_phrases_name_the_nearest_named_gate_water_or_shade() {
    let app = App::new().await;
    app.farm().await;
    let ctx = &app.ctx;
    // A farm at Ames starts imperial; the phrases are checked in metric first.
    ctx.update_settings(&json!({"units": "metric"})).await.unwrap();
    let describe = |p: LonLat| async move { place::describe(ctx, p).await.unwrap() };
    // With no features: by paddock, as before.
    assert_eq!(describe(at(350.0, 260.0)).await.as_deref(), Some("in P1"));

    app.create(json!({"kind": "gate", "name": "east gate", "geometry": point(400.0, 200.0)})).await;
    // Closer, but unnamed, or a kind that doesn't name places.
    app.create(json!({"kind": "gate", "geometry": point(400.0, 250.0)})).await;
    app.create(json!({"kind": "hazard", "name": "old well", "geometry": point(400.0, 262.0), "props": {"radius_m": 5}})).await;
    app.create(json!({"kind": "road", "name": "county road", "geometry": line((380.0, 270.0), (420.0, 270.0))})).await;
    // Named but no longer in effect.
    let ended = rfc(Utc::now() - Duration::hours(1));
    app.create(json!({"kind": "shade", "name": "old tree", "geometry": point(400.0, 255.0), "active_until": ended})).await;
    // A pond and a far tree line.
    app.create(json!({"kind": "water", "name": "north pond", "geometry": square(100.0, 300.0, 50.0)})).await;
    app.create(json!({"kind": "shade", "name": "tree line", "geometry": point(-400.0, 0.0)})).await;

    assert_eq!(describe(at(400.0, 260.0)).await.as_deref(), Some("60 m N of east gate"));
    assert_eq!(describe(at(460.0, 200.0)).await.as_deref(), Some("60 m E of east gate"));
    assert_eq!(describe(at(404.0, 197.0)).await.as_deref(), Some("at east gate"));
    assert_eq!(describe(at(120.0, 320.0)).await.as_deref(), Some("in north pond"));
    assert_eq!(describe(at(125.0, 250.0)).await.as_deref(), Some("50 m S of north pond"));
    // Further than 200 m from every landmark: by paddock again.
    assert_eq!(describe(at(1200.0, 200.0)).await.as_deref(), Some("in P2"));
    assert_eq!(describe(at(-170.0, 0.0)).await.as_deref(), Some("170 m W of P1"), "tree line is 230 m off");
    assert_eq!(describe(at(-250.0, 0.0)).await.as_deref(), Some("150 m E of tree line"));
    ctx.update_settings(&json!({"units": "imperial"})).await.unwrap();
    assert_eq!(describe(at(400.0, 260.0)).await.as_deref(), Some("200 ft N of east gate"));

    // The pure form over a given list, with the same rules.
    let list = features::list_features(ctx, None, None, Some(time::now())).await.unwrap();
    let metric = Fmt::new(Units::Metric);
    assert_eq!(place::describe_near(&list, at(400.0, 260.0), &metric).as_deref(), Some("60 m N of east gate"));
    assert_eq!(place::describe_near(&list, at(400.0, 800.0), &metric), None);
    assert_eq!(place::describe_near(&[], at(400.0, 260.0), &metric), None);
}

#[tokio::test]
async fn list_features_is_a_read_tool_in_the_registry() {
    let app = App::new().await;
    let (p1, _) = app.farm().await;
    op_core::register_tools(&app.ctx);
    op_core::register_tools(&app.ctx); // idempotent
    let tools = app.ctx.tools();
    let spec = tools.get("list_features").expect("registered");
    assert!(spec.read && !spec.brain);
    assert_eq!(spec.min_role, Role::Viewer);
    assert_eq!(spec.input_schema["additionalProperties"], false);
    assert!(!tools.brain_tools().contains(&"list_features".to_owned()));
    let viewer = Identity { role: Role::Viewer, user_id: None, name: None, via: Via::Local };
    assert!(tools.listed_for(&viewer, &ToolScope::Full).iter().any(|t| t.name == "list_features"));

    let gate = app.create(json!({"kind": "gate", "name": "east gate", "geometry": point(400.0, 200.0), "paddock_id": p1})).await;
    let ended = rfc(Utc::now() - Duration::hours(1));
    app.create(json!({"kind": "exclusion", "geometry": square(100.0, 100.0, 40.0), "active_until": ended})).await;
    let ctx = &app.ctx;
    let call = |args: Value| async move { ctx.tools().call(ctx, "list_features", args, None, Identity::brain(), &ToolScope::Full).await };
    assert_eq!(call(json!({})).await.unwrap().as_array().unwrap().len(), 2);
    assert_eq!(call(json!({"kind": "gate"})).await.unwrap(), json!([gate.clone()]));
    assert_eq!(call(json!({"paddock_id": p1})).await.unwrap(), json!([gate.clone()]));
    assert_eq!(call(json!({"active": true})).await.unwrap(), json!([gate]));
    assert_eq!(call(json!({"kind": "pond"})).await.unwrap_err().status, StatusCode::BAD_REQUEST);
    // A brain scope that lists it may call it too.
    let only = ToolScope::Only(vec!["list_features".into()]);
    assert!(app.ctx.tools().call(&app.ctx, "list_features", json!({}), None, Identity::brain(), &only).await.is_ok());
}
