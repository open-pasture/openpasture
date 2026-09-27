//! Every herd boundary through `prepare` (field-ready §2.12 F): exclusions in
//! effect when it takes effect become holes or cuts, preparing twice changes
//! nothing, sweep steps keep the holes and end on the prepared target, and
//! the pre-send findings from the map and the collars. Hazards, roads,
//! neighbour lines, water and the farm boundary never change the shape.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use http_body_util::BodyExt;
use op_core::check::Finding;
use op_core::{Ctx, Identity, Severity, Via};
use op_geo::{CollarLimits, Polygon, Projection};
use op_ingest::{CollarCaps, SendOpts};
use serde_json::{Value, json};
use tower::ServiceExt;

const MID: [f64; 2] = [-92.405, 38.125];

fn at(x: f64, y: f64) -> [f64; 2] {
    Projection::new(MID).inverse([x, y])
}

fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<[f64; 2]> {
    vec![at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1), at(x0, y0)]
}

fn poly(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon {
    Polygon::from_ring(rect(x0, y0, x1, y1))
}

fn circle(cx: f64, cy: f64, r: f64, n: usize) -> Vec<[f64; 2]> {
    let mut v: Vec<[f64; 2]> = (0..n)
        .map(|i| {
            let a = std::f64::consts::TAU * i as f64 / n as f64;
            at(cx + r * a.cos(), cy + r * a.sin())
        })
        .collect();
    v.push(v[0]);
    v
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn codes(f: &[Finding]) -> Vec<&str> {
    f.iter().map(|f| f.code.as_str()).collect()
}

fn find<'a>(f: &'a [Finding], code: &str) -> &'a Finding {
    f.iter().find(|f| f.code == code).unwrap_or_else(|| panic!("no {code} in {:?}", codes(f)))
}

fn local(p: [f64; 2]) -> [f64; 2] {
    Projection::new(MID).forward(p)
}

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
    herd: String,
    paddock: String,
}

impl App {
    /// Farm, a 300 x 200 m paddock P1 around MID, herd Cows in it.
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let router = op_core::with_identity(op_core::router().merge(op_ingest::router()).with_state(ctx.clone()), Identity::owner(Via::Local));
        let mut app = Self { _dir: dir, ctx, router, herd: String::new(), paddock: String::new() };
        app.ok("POST", "/api/farm", json!({"name": "Home", "timezone": "America/Chicago", "center": MID})).await;
        app.ctx.update_settings(&json!({"units": "metric"})).await.unwrap();
        let p = app.ok("POST", "/api/paddocks", json!({"name": "P1", "geometry": poly(0.0, 0.0, 300.0, 200.0)})).await;
        let h = app.ok("POST", "/api/herds", json!({"name": "Cows", "species": "cattle", "count": 0, "paddock_id": p["id"]})).await;
        app.paddock = p["id"].as_str().unwrap().into();
        app.herd = h["id"].as_str().unwrap().into();
        app
    }

    async fn req(&self, method: &str, path: &str, key: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path).header("host", "127.0.0.1");
        if let Some(k) = key {
            req = req.header("authorization", format!("Bearer {k}"));
        }
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
        let (s, v) = self.req(method, path, None, Some(body)).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    async fn feature(&self, body: Value) -> Value {
        self.ok("POST", "/api/features", body).await
    }

    async fn exclusion(&self, name: &str, ring: Vec<[f64; 2]>) -> String {
        let f = self.feature(json!({"kind": "exclusion", "name": name, "geometry": {"type": "Polygon", "coordinates": [ring]}})).await;
        f["id"].as_str().unwrap().into()
    }

    async fn prepare(&self, g: &Polygon, effective_at: Option<DateTime<Utc>>) -> op_ingest::Prepared {
        op_ingest::prepare(&self.ctx, &self.herd, g, &SendOpts { effective_at, ..Default::default() }).await.unwrap()
    }

    async fn send(&self, g: &Polygon, extra: Value) -> Value {
        let mut body = json!({"geometry": g});
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        self.ok("POST", &format!("/api/herds/{}/boundary", self.herd), body).await
    }

    async fn status(&self) -> Value {
        self.req("GET", &format!("/api/herds/{}/boundary", self.herd), None, None).await.1
    }

    /// A collar in the herd; `device` makes it firmware 0.2 (holes).
    async fn collar(&self, device: Option<Value>) -> (String, String) {
        let v = self.ok("POST", "/api/collars", json!({"herd_id": self.herd})).await;
        let c = (v["collar"]["id"].as_str().unwrap().to_owned(), v["key"].as_str().unwrap().to_owned());
        if let Some(d) = device {
            self.report(&c.1, json!({"device": d})).await;
        }
        c
    }

    async fn report(&self, key: &str, body: Value) {
        let (s, v) = self.req("POST", "/collar/v1/report", Some(key), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    async fn fix(&self, key: &str, x: f64, y: f64) {
        self.report(key, json!({"fixes": [{"at": ts(Utc::now()), "point": at(x, y), "accuracy_m": 2.0, "sats": 9}]})).await;
    }
}

fn v0() -> Value {
    json!({"fw": "0.2.0", "caps": ["holes", "slots", "collar_id", "cue_mode", "episodes", "config"], "limits": CollarLimits::V0})
}

fn geometry_of(v: &Value) -> Polygon {
    serde_json::from_value(v["geometry"].clone()).unwrap()
}

#[tokio::test]
async fn exclusions_become_holes_and_cuts_and_the_send_stores_them() {
    let app = App::new().await;
    app.exclusion("pond", rect(100.0, 80.0, 140.0, 120.0)).await;
    let wet = app.exclusion("", rect(280.0, -20.0, 330.0, 40.0)).await;
    let target = poly(0.0, 0.0, 300.0, 200.0);
    let p = app.prepare(&target, None).await;
    assert_eq!(p.geometry.coordinates.len(), 2, "the pond is a hole: {:?}", p.geometry);
    assert!(!p.geometry.contains(at(120.0, 100.0)), "the pond is kept out");
    assert!(!p.geometry.contains(at(295.0, 10.0)), "the wet corner is cut off");
    assert!(p.geometry.contains(at(50.0, 50.0)) && p.geometry.contains(at(250.0, 150.0)));
    let over: Vec<&Finding> = p.findings.iter().filter(|f| f.code == "overlaps_exclusion").collect();
    assert_eq!(
        over.iter().map(|f| (f.severity, f.text.as_str())).collect::<Vec<_>>(),
        [(Severity::Info, "Pond kept out"), (Severity::Info, "Exclusion kept out")]
    );
    assert_eq!(over[1].targets, [("feature".to_owned(), wet)]);
    assert_eq!(over[0].geometry.as_ref().unwrap()["type"], "Polygon", "the overlap to draw");
    assert!(!codes(&p.findings).contains(&"simplified"));

    // No collars report, so the move sends the target itself: exactly what prepare made.
    let m = app.send(&target, json!({})).await;
    assert_eq!(m["status"], "done");
    let s = app.status().await;
    assert_eq!(geometry_of(&s["active"]), p.geometry);
}

#[tokio::test]
async fn a_temporary_exclusion_counts_only_when_the_boundary_takes_effect() {
    let app = App::new().await;
    let now = Utc::now();
    app.feature(json!({
        "kind": "exclusion", "name": "calving pen", "geometry": {"type": "Polygon", "coordinates": [rect(150.0, 50.0, 190.0, 90.0)]},
        "active_from": ts(now + Duration::hours(1)), "active_until": ts(now + Duration::hours(3)),
    }))
    .await;
    let target = poly(0.0, 0.0, 300.0, 200.0);
    assert_eq!(app.prepare(&target, None).await.geometry.coordinates.len(), 1, "not yet in effect");
    let staged = now + Duration::hours(2);
    let p = app.prepare(&target, Some(staged)).await;
    assert_eq!(p.geometry.coordinates.len(), 2, "in effect when this one takes effect");
    assert!(!p.geometry.contains(at(170.0, 70.0)));
    assert_eq!(app.prepare(&target, Some(now + Duration::hours(4))).await.geometry.coordinates.len(), 1, "over by then");

    app.send(&target, json!({"effective_at": ts(staged)})).await;
    let s = app.status().await;
    assert_eq!(geometry_of(&s["pending"]), p.geometry, "the staged boundary holds the pen");
    assert_eq!(geometry_of(&s["pending"]), app.prepare(&target, Some(staged)).await.geometry);
}

#[tokio::test]
async fn preparing_a_prepared_boundary_changes_nothing() {
    let app = App::new().await;
    app.exclusion("pond", rect(100.0, 80.0, 140.0, 120.0)).await;
    app.exclusion("wet corner", rect(280.0, -20.0, 330.0, 40.0)).await;
    // Inside, but closer to the west edge than a collar's gap: joined to the edge.
    app.exclusion("trough", rect(5.0, 150.0, 20.0, 165.0)).await;
    // Two close together: one hole.
    app.exclusion("rocks", rect(200.0, 120.0, 215.0, 135.0)).await;
    app.exclusion("more rocks", rect(220.0, 120.0, 235.0, 135.0)).await;
    let target = poly(0.0, 0.0, 300.0, 200.0);
    let once = app.prepare(&target, None).await;
    assert!(!once.geometry.contains(at(10.0, 157.0)) && !once.geometry.contains(at(12.0, 157.0)), "the trough is kept out");
    assert_eq!(once.geometry.coordinates.len(), 3, "pond and one hole for both rocks: {:?}", once.geometry);
    let twice = app.prepare(&once.geometry, None).await;
    assert_eq!(twice.geometry, once.geometry);
    assert!(!codes(&twice.findings).contains(&"overlaps_exclusion"), "already kept out: {:?}", codes(&twice.findings));
    assert!(!codes(&twice.findings).contains(&"simplified"));
}

#[tokio::test]
async fn sweep_steps_keep_the_exclusion_holes_and_end_on_the_prepared_target() {
    let app = App::new().await;
    // The herd holds the whole paddock first.
    app.send(&poly(0.0, 0.0, 300.0, 200.0), json!({})).await;
    let mut keys = Vec::new();
    for y in [40.0, 100.0, 160.0] {
        let (_, k) = app.collar(Some(v0())).await;
        app.fix(&k, 30.0, y).await;
        keys.push(k);
    }
    app.exclusion("pond", rect(230.0, 80.0, 270.0, 120.0)).await;
    let target = poly(200.0, 0.0, 300.0, 200.0);
    let p = app.prepare(&target, None).await;
    assert_eq!(p.geometry.coordinates.len(), 2);

    let m = app.send(&target, json!({})).await;
    assert_eq!(m["status"], "sweeping");
    assert_eq!(serde_json::from_value::<Polygon>(m["target"].clone()).unwrap(), p.geometry, "the move's target is the prepared one");
    let step = geometry_of(&app.status().await["active"]);
    assert!(step.coordinates.len() >= 2, "the first step keeps the pond out: {step:?}");
    assert!(!step.contains(at(250.0, 100.0)));
    assert!(step.contains(at(30.0, 100.0)), "and holds the herd");

    // Everyone walks in; the next pass sends the target: the prepared shape, as the check said.
    for (k, y) in keys.iter().zip([30.0, 150.0, 170.0]) {
        app.fix(k, 250.0, y).await;
    }
    op_ingest::moves::drive(&app.ctx, &app.herd, Utc::now() + Duration::seconds(31)).await.unwrap();
    let s = app.status().await;
    assert_eq!(s["move"]["status"], "done");
    assert_eq!(geometry_of(&s["active"]), p.geometry);
}

#[tokio::test]
async fn hazards_roads_lines_water_and_the_farm_boundary_are_found_never_enforced() {
    let app = App::new().await;
    let target = poly(0.0, 0.0, 300.0, 200.0);
    let before = app.prepare(&target, None).await;
    assert!(before.findings.is_empty(), "{:?}", codes(&before.findings));
    app.feature(json!({"kind": "water", "name": "north trough", "geometry": {"type": "Point", "coordinates": at(150.0, 400.0)}})).await;
    app.feature(json!({"kind": "hazard", "name": "old well", "geometry": {"type": "Point", "coordinates": at(60.0, 60.0)}, "props": {"radius_m": 8.0}})).await;
    app.feature(json!({"kind": "road", "name": "county road", "geometry": {"type": "LineString", "coordinates": [at(-50.0, 190.0), at(350.0, 190.0)]}})).await;
    app.feature(json!({"kind": "neighbour_line", "geometry": {"type": "LineString", "coordinates": [at(280.0, -50.0), at(280.0, 250.0)]}})).await;
    app.feature(json!({"kind": "farm_boundary", "geometry": {"type": "Polygon", "coordinates": [rect(-100.0, -100.0, 290.0, 300.0)]}})).await;
    let p = app.prepare(&target, None).await;
    assert_eq!(p.geometry, before.geometry, "nothing but exclusions changes the shape");
    assert_eq!(codes(&p.findings), ["crosses_road", "overlaps_hazard", "crosses_neighbour_line", "crosses_farm_boundary", "no_water"], "critical first");
    assert_eq!(find(&p.findings, "crosses_road").severity, Severity::Critical);
    assert_eq!(find(&p.findings, "crosses_road").text, "Crosses county road");
    assert_eq!(find(&p.findings, "overlaps_hazard").text, "Takes in old well");
    assert_eq!(find(&p.findings, "crosses_neighbour_line").text, "Crosses the neighbour line");
    assert_eq!(find(&p.findings, "no_water").text, "No water inside");
    // The part past the farm boundary is a 10 m strip down the east side.
    let past: Polygon = serde_json::from_value(find(&p.findings, "crosses_farm_boundary").geometry.clone().unwrap()).unwrap();
    assert!((past.area_ha() - 0.2).abs() < 0.005, "{}", past.area_ha());
    // The road inside: 300 m of it.
    let road = find(&p.findings, "crosses_road").geometry.clone().unwrap();
    assert_eq!(road["type"], "LineString");
    let ends: Vec<[f64; 2]> = serde_json::from_value(road["coordinates"].clone()).unwrap();
    assert!((local(ends[0])[0] - local(ends[1])[0]).abs() > 299.0);

    // Water inside: ringed on the map, and no "no water".
    app.feature(json!({"kind": "water", "name": "tank", "geometry": {"type": "Point", "coordinates": at(150.0, 100.0)}})).await;
    let p = app.prepare(&target, None).await;
    assert!(!codes(&p.findings).contains(&"no_water"));
    let w = find(&p.findings, "water_inside");
    assert_eq!((w.severity, w.text.as_str(), w.geometry.as_ref().unwrap()["type"].as_str()), (Severity::Info, "Tank inside", Some("Point")));
    // A hazard that has ended isn't checked.
    let ended = Utc::now() - Duration::hours(1);
    app.feature(json!({
        "kind": "hazard", "name": "flood debris", "geometry": {"type": "Polygon", "coordinates": [rect(150.0, 150.0, 170.0, 170.0)]},
        "active_from": ts(ended - Duration::days(2)), "active_until": ts(ended),
    }))
    .await;
    let p = app.prepare(&target, None).await;
    assert_eq!(p.findings.iter().filter(|f| f.code == "overlaps_hazard").count(), 1, "only the well");
    assert_eq!(p.geometry, before.geometry);
}

#[tokio::test]
async fn findings_from_the_collars() {
    let app = App::new().await;
    app.send(&poly(0.0, 0.0, 300.0, 200.0), json!({})).await;
    let (in_pond, k1) = app.collar(Some(v0())).await;
    app.fix(&k1, 120.0, 100.0).await;
    let (east, k2) = app.collar(Some(v0())).await;
    app.fix(&k2, 250.0, 100.0).await;
    let (silent, _) = app.collar(None).await;
    let (legacy, k4) = app.collar(None).await;
    app.fix(&k4, 50.0, 50.0).await;
    let (parked, _) = app.collar(None).await;
    app.ok("POST", &format!("/api/collars/{parked}/park"), json!({"reason": "shelf"})).await;
    app.exclusion("pond", rect(100.0, 80.0, 140.0, 120.0)).await;

    let p = app.prepare(&poly(0.0, 0.0, 200.0, 200.0), None).await;
    let f = |c: &str| find(&p.findings, c);
    assert_eq!(f("animals_in_new_holes").targets, [("collar".to_owned(), in_pond.clone())]);
    assert_eq!(f("animals_in_new_holes").text, "1 inside a new hole");
    assert_eq!(f("animals_outside").targets, [("collar".to_owned(), east)]);
    assert_eq!(f("animals_outside").text, "1 outside it");
    assert_eq!(f("collars_offline").targets, [("collar".to_owned(), silent.clone())], "the parked one isn't offline");
    // Neither has said it is firmware 0.2; the parked one isn't counted.
    let mut no_holes = f("collars_no_holes").targets.clone();
    no_holes.sort();
    let mut expected = vec![("collar".to_owned(), silent), ("collar".to_owned(), legacy)];
    expected.sort();
    assert_eq!(no_holes, expected);
    assert!(f("animals_in_new_holes").geometry.as_ref().unwrap()["type"] == "MultiPoint");

    // Once the pond is a hole of the active boundary, an animal in it is outside, not in a new hole.
    app.fix(&k1, 50.0, 100.0).await;
    app.fix(&k2, 50.0, 150.0).await;
    let m = app.send(&poly(0.0, 0.0, 200.0, 200.0), json!({})).await;
    assert_eq!(m["status"], "done", "everyone inside: the target at once");
    app.fix(&k1, 120.0, 100.0).await;
    let p = app.prepare(&poly(0.0, 0.0, 200.0, 200.0), None).await;
    assert!(!codes(&p.findings).contains(&"animals_in_new_holes"));
    assert!(find(&p.findings, "animals_outside").targets.contains(&("collar".to_owned(), in_pond)));
}

#[tokio::test]
async fn a_staged_boundary_finds_collars_with_no_room() {
    let app = App::new().await;
    let now = Utc::now();
    app.send(&poly(0.0, 0.0, 300.0, 200.0), json!({})).await;
    app.send(&poly(0.0, 0.0, 250.0, 200.0), json!({"effective_at": ts(now + Duration::hours(1))})).await;
    let s = app.status().await;
    let (active, staged) = (s["active"]["version"].as_u64().unwrap(), s["pending"]["version"].as_u64().unwrap());
    // Two slots, both taken: the one in effect and the one staged for an hour from now.
    let two = json!({"fw": "0.2.0", "caps": ["holes", "slots"], "limits": CollarLimits { slots: 2, ..CollarLimits::V0 }});
    let (small, k) = app.collar(Some(two)).await;
    app.report(&k, json!({"slots": [{"version": active, "status": "applied"}, {"version": staged, "status": "received", "effective_at": op_protocol::wire_time::format(&(now + Duration::hours(1)))}]})).await;
    let (_, k2) = app.collar(Some(v0())).await;
    app.report(&k2, json!({"slots": [{"version": active, "status": "applied"}]})).await;

    let later = app.prepare(&poly(0.0, 0.0, 200.0, 200.0), Some(now + Duration::hours(2))).await;
    let full = find(&later.findings, "slots_full");
    assert_eq!(full.targets, [("collar".to_owned(), small)], "sixteen slots have room");
    assert_eq!(full.text, "1 collar has no room for it yet");
    // Before the staged one takes effect, that one dies when this one does: room.
    let sooner = app.prepare(&poly(0.0, 0.0, 200.0, 200.0), Some(now + Duration::minutes(30))).await;
    assert!(!codes(&sooner.findings).contains(&"slots_full"), "{:?}", codes(&sooner.findings));
    // Sent now, nothing is staged.
    assert!(!codes(&app.prepare(&poly(0.0, 0.0, 200.0, 200.0), None).await.findings).contains(&"slots_full"));
}

#[tokio::test]
async fn the_legacy_fence_is_what_a_legacy_collar_downloads() {
    let app = App::new().await;
    let (_, key) = app.collar(None).await;
    app.exclusion("pond", rect(100.0, 80.0, 140.0, 120.0)).await;
    let target = Polygon::from_rings(circle(150.0, 100.0, 95.0, 100), []);
    let p = app.prepare(&target, None).await;
    assert_eq!(p.geometry.coordinates.len(), 2);
    app.send(&target, json!({})).await;
    let (s, bytes) = {
        let req =
            Request::builder().method("GET").uri("/collar/v1/boundary?have=0").header("authorization", format!("Bearer {key}")).body(Body::empty()).unwrap();
        let res = app.router.clone().oneshot(req).await.unwrap();
        (res.status(), res.into_body().collect().await.unwrap().to_bytes())
    };
    assert_eq!(s, StatusCode::OK);
    let cmd: op_protocol::BoundaryCommand = serde_json::from_slice(&bytes).unwrap();
    let fence = op_ingest::prepare::legacy_fence(&p.geometry, p.warn_m).unwrap();
    assert_eq!(cmd.polygon(), fence);
    assert!(fence.coordinates.len() == 1 && fence.outer_ring().len() <= 64);
    let stored: op_core::Boundary = serde_json::from_value(app.status().await["active"].clone()).unwrap();
    assert_eq!(op_ingest::fence_geometry(&stored, &CollarCaps { fw: None, caps: vec![], limits: CollarLimits::LEGACY }), fence);
}

#[tokio::test]
async fn simplified_when_the_collars_hold_fewer_corners() {
    let app = App::new().await;
    let many = Polygon::from_rings(circle(150.0, 100.0, 95.0, 200), []);
    let p = app.prepare(&many, None).await;
    assert_eq!(find(&p.findings, "simplified").severity, Severity::Info);
    assert!(p.geometry.outer_ring().len() <= CollarLimits::V0.outer);
    // With an exclusion hole too: still simplified; a plain rectangle with a hole isn't.
    app.exclusion("pond", rect(130.0, 80.0, 170.0, 120.0)).await;
    assert!(codes(&app.prepare(&many, None).await.findings).contains(&"simplified"));
    assert!(!codes(&app.prepare(&poly(0.0, 0.0, 300.0, 200.0), None).await.findings).contains(&"simplified"));
}
