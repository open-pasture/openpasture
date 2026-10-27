//! The safety gate on every path a boundary takes to the collars: a person
//! confirms a hard finding by code, nobody sends into a no-go zone, every
//! other sender is refused, escape pens never cross a road, and every
//! decision is logged.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Polygon, Via};
use op_geo::Projection;
use op_ingest::SendOpts;
use op_ingest::gate::{Sender, SenderKind};
use serde_json::{Value, json};
use tower::ServiceExt;

const MID: [f64; 2] = [-93.6225, 42.0318];

fn at(x: f64, y: f64) -> [f64; 2] {
    Projection::new(MID).inverse([x, y])
}

fn ring(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<[f64; 2]> {
    vec![at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1), at(x0, y0)]
}

fn poly(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon {
    Polygon::from_ring(ring(x0, y0, x1, y1))
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
    herd: String,
}

impl App {
    /// Farm, a 300 x 200 m paddock around MID, herd Cows in it.
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let router = op_core::with_identity(op_core::router().merge(op_ingest::router()), Identity::owner(Via::Local)).with_state(ctx.clone());
        let mut app = Self { _dir: dir, ctx, router, herd: String::new() };
        app.ok("POST", "/api/farm", json!({"name": "Home", "timezone": "America/Chicago", "center": MID})).await;
        let p = app.ok("POST", "/api/paddocks", json!({"name": "P1", "geometry": poly(0.0, 0.0, 300.0, 200.0)})).await;
        let h = app.ok("POST", "/api/herds", json!({"name": "Cows", "species": "cattle", "count": 0, "paddock_id": p["id"]})).await;
        app.herd = h["id"].as_str().unwrap().into();
        app
    }

    async fn req(&self, method: &str, path: &str, key: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
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

    async fn feature(&self, kind: &str, name: &str, geometry: Value, props: Value) -> Value {
        self.ok("POST", "/api/features", json!({"kind": kind, "name": name, "geometry": geometry, "props": props})).await
    }

    async fn road_across(&self, y: f64) {
        self.feature("road", "County Road", json!({"type": "LineString", "coordinates": [at(-50.0, y), at(350.0, y)]}), json!({})).await;
    }

    async fn send(&self, g: &Polygon, confirm: &[&str]) -> (StatusCode, Value) {
        self.send_with(g, confirm, None).await
    }

    async fn send_with(&self, g: &Polygon, confirm: &[&str], token: Option<&str>) -> (StatusCode, Value) {
        self.req("POST", &format!("/api/herds/{}/boundary", self.herd), None, Some(json!({"geometry": g, "confirm": confirm, "confirm_token": token}))).await
    }

    /// Send, and when the gate asks, confirm `codes` with the token it gave.
    async fn send_confirmed(&self, g: &Polygon, codes: &[&str]) -> (StatusCode, Value) {
        let (s, v) = self.send(g, &[]).await;
        if s != StatusCode::CONFLICT {
            return (s, v);
        }
        self.send_with(g, codes, v["confirm_token"].as_str()).await
    }

    async fn log(&self) -> Vec<Value> {
        let (s, v) = self.req("GET", &format!("/api/gate/log?herd_id={}", self.herd), None, None).await;
        assert_eq!(s, StatusCode::OK);
        v.as_array().unwrap().clone()
    }

    async fn collar(&self) -> (String, String) {
        let v = self.ok("POST", "/api/collars", json!({"herd_id": self.herd})).await;
        (v["collar"]["id"].as_str().unwrap().to_owned(), v["key"].as_str().unwrap().to_owned())
    }

    async fn report(&self, key: &str, body: Value) {
        let (s, v) = self.req("POST", "/collar/v1/report", Some(key), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    async fn fix(&self, key: &str, x: f64, y: f64, t: DateTime<Utc>) {
        self.report(key, json!({"fixes": [{"at": ts(t), "point": at(x, y), "accuracy_m": 2.0, "sats": 9}]})).await;
    }
}

fn v0() -> Value {
    json!({"fw": "0.2.0", "caps": ["holes", "slots", "collar_id", "cue_mode", "episodes", "config"], "limits": op_geo::CollarLimits::V0})
}

fn codes(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|f| f["code"].as_str().unwrap_or_else(|| f.as_str().unwrap()).to_owned()).collect()
}

#[tokio::test]
async fn a_person_confirms_a_road_by_its_code_and_the_log_keeps_both_tries() {
    let app = App::new().await;
    app.road_across(100.0).await;
    let (s, v) = app.send(&poly(0.0, 0.0, 300.0, 200.0), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(codes(&v["findings"]), ["crosses_road"]);
    assert_eq!(v["confirm"], json!(["crosses_road"]));
    assert!(v["error"].as_str().unwrap().starts_with("Crosses County Road"), "{v}");
    assert_eq!(v["findings"][0]["geometry"]["type"], "LineString", "drawn on the map");
    let token = v["confirm_token"].as_str().unwrap().to_owned();
    // The wrong code doesn't do, nor the right one without the token for this shape and map.
    assert_eq!(app.send_with(&poly(0.0, 0.0, 300.0, 200.0), &["crosses_farm_boundary"], Some(&token)).await.0, StatusCode::CONFLICT);
    assert_eq!(app.send(&poly(0.0, 0.0, 300.0, 200.0), &["crosses_road"]).await.0, StatusCode::CONFLICT);
    let (s, mv) = app.send_with(&poly(0.0, 0.0, 300.0, 200.0), &["crosses_road"], Some(&token)).await;
    assert_eq!(s, StatusCode::CREATED, "{mv}");

    let log = app.log().await;
    assert!(log.len() >= 3);
    let sent = &log[0];
    assert_eq!((sent["allowed"].as_bool(), sent["sender"].as_str()), (Some(true), Some("farmer")));
    assert_eq!(sent["confirmed"], json!(["crosses_road"]));
    assert_eq!(sent["by"]["via"], "local");
    assert_eq!(sent["decision_id"], mv["decision_id"]);
    assert!(log[1..].iter().all(|e| e["allowed"] == false && e["codes"] == json!(["crosses_road"])));
    // The move's sweep steps carry it.
    assert_eq!(op_ingest::gate::confirmed_for(&app.ctx, mv["decision_id"].as_str().unwrap()).await.unwrap(), ["crosses_road"]);
    let refused = app.req("GET", &format!("/api/gate/log?herd_id={}&refused=true", app.herd), None, None).await.1;
    assert_eq!(refused.as_array().unwrap().len(), log.len() - 1);
}

#[tokio::test]
async fn nobody_sends_a_herd_into_a_no_go_zone() {
    let app = App::new().await;
    let creek = app.feature("no_go", "Creek", json!({"type": "Polygon", "coordinates": [ring(120.0, 60.0, 180.0, 140.0)]}), json!({})).await;
    assert_eq!(creek["kind"], "no_go");
    // A no-go zone has no dates.
    let dated = json!({"kind": "no_go", "geometry": {"type": "Polygon", "coordinates": [ring(0.0, 0.0, 10.0, 10.0)]}, "active_until": ts(Utc::now() + Duration::days(30))});
    assert_eq!(app.req("POST", "/api/features", None, Some(dated)).await.0, StatusCode::BAD_REQUEST);

    // Kept out like an exclusion when the collars hold holes.
    let (_, key) = app.collar().await;
    app.report(&key, json!({"device": v0()})).await;
    let (s, v) = app.send(&poly(0.0, 0.0, 300.0, 200.0), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let st = app.req("GET", &format!("/api/herds/{}/boundary", app.herd), None, None).await.1;
    assert_eq!(st["active"]["geometry"]["coordinates"].as_array().unwrap().len(), 2, "the zone is a hole");

    // A collar that can't hold holes would take it in: refused, confirmed or not.
    app.collar().await;
    for confirm in [&[][..], &["enters_no_go"][..]] {
        let (s, v) = app.send(&poly(0.0, 0.0, 300.0, 200.0), confirm).await;
        assert_eq!(s, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["never"], json!(["enters_no_go"]));
        assert_eq!(v["confirm"], json!([]));
        assert!(v["error"].as_str().unwrap().contains("no-go zone"), "{v}");
    }
    // All of it in the zone: nothing to keep out.
    let (s, v) = app.send(&poly(125.0, 65.0, 175.0, 135.0), &["enters_no_go"]).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(codes(&v["findings"]), ["enters_no_go"]);
}

#[tokio::test]
async fn every_sender_but_a_person_is_refused_on_a_hard_finding() {
    let app = App::new().await;
    app.feature("farm_boundary", "Home", json!({"type": "Polygon", "coordinates": [ring(-10.0, -10.0, 250.0, 210.0)]}), json!({})).await;
    let past = poly(0.0, 0.0, 300.0, 200.0);
    // The farm's own boundary is hard now.
    let p = op_ingest::prepare(&app.ctx, &app.herd, &past, &SendOpts::default()).await.unwrap();
    let f = p.findings.iter().find(|f| f.code == "crosses_farm_boundary").unwrap();
    assert_eq!(f.severity, op_core::Severity::Critical);

    for kind in [SenderKind::Api, SenderKind::Brain, SenderKind::Mcp, SenderKind::Timer, SenderKind::Auto, SenderKind::Schedule, SenderKind::System] {
        let opts = SendOpts { sender: Sender::of(kind), confirm: vec!["crosses_farm_boundary".into()], ..Default::default() };
        let e = op_ingest::start_move_gated(&app.ctx, &app.herd, past.clone(), opts.clone(), &format!("dec_{}", op_core::DbEnum::as_db(&kind))).await;
        let e = e.err().unwrap_or_else(|| panic!("{kind:?} sent it"));
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert!(e.message.contains("Only a person"), "{}", e.message);
        assert!(op_ingest::send_boundary(&app.ctx, &app.herd, past.clone(), opts, "dec_lib").await.is_err());
    }
    // A signed-in person, confirming it by code with the token the refusal gave.
    let person = Sender::person(&Identity::owner(Via::Local), vec!["crosses_farm_boundary".into()]);
    let opts = SendOpts { sender: person.clone(), ..Default::default() };
    let e = op_ingest::start_move_gated(&app.ctx, &app.herd, past.clone(), opts, "dec_person").await.err().expect("no token: refused");
    let token = e.details.as_ref().and_then(|d| d["confirm_token"].as_str()).map(str::to_owned);
    let opts = SendOpts { sender: person.with_token(token), ..Default::default() };
    op_ingest::start_move_gated(&app.ctx, &app.herd, past.clone(), opts, "dec_person").await.unwrap();
    // Inside the farm: anyone.
    let opts = SendOpts { sender: Sender::of(SenderKind::Auto), ..Default::default() };
    op_ingest::start_move_gated(&app.ctx, &app.herd, poly(0.0, 0.0, 200.0, 200.0), opts, "dec_auto").await.unwrap();

    let log = app.log().await;
    let senders: Vec<(&str, bool)> = log.iter().map(|e| (e["sender"].as_str().unwrap(), e["allowed"].as_bool().unwrap())).collect();
    assert_eq!(senders[0], ("auto", true));
    assert_eq!(senders[1], ("farmer", true));
    for kind in ["api", "brain", "mcp", "timer", "auto", "schedule", "system"] {
        assert!(senders.contains(&(kind, false)), "{kind} refused and logged");
    }
}

#[tokio::test]
async fn water_is_hard_when_the_farm_requires_it() {
    let app = App::new().await;
    app.feature("water", "Trough", json!({"type": "Point", "coordinates": at(500.0, 500.0)}), json!({})).await;
    assert_eq!(app.send(&poly(0.0, 0.0, 300.0, 200.0), &[]).await.0, StatusCode::CREATED, "a warning only");
    let v = app.ok("PUT", "/api/gate/settings", json!({"require_water": true})).await;
    assert_eq!(v["require_water"], true);
    let (s, v) = app.send(&poly(0.0, 0.0, 300.0, 200.0), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["confirm"], json!(["no_water"]));
    assert_eq!(app.send_confirmed(&poly(0.0, 0.0, 300.0, 200.0), &["no_water"]).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn an_escape_pen_never_crosses_a_road() {
    let app = App::new().await;
    let (collar, key) = app.collar().await;
    app.fix(&key, 150.0, 100.0, Utc::now()).await;
    assert_eq!(app.send(&poly(0.0, 0.0, 300.0, 200.0), &[]).await.0, StatusCode::CREATED);
    // A road just past the herd's boundary, between the herd and where the animal went.
    app.road_across(215.0).await;
    let now = Utc::now() + Duration::seconds(120);
    app.fix(&key, 150.0, 230.0, now - Duration::seconds(30)).await;
    app.fix(&key, 150.0, 231.0, now).await;
    op_ingest::escapes::scan(&app.ctx, now + Duration::seconds(60)).await.unwrap();
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM escapes").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 0, "no pen across the road");
    let log = app.log().await;
    let pen = &log[0];
    assert_eq!((pen["sender"].as_str(), pen["allowed"].as_bool(), pen["collar_id"].as_str()), (Some("escape"), Some(false), Some(collar.as_str())));
    assert_eq!(pen["codes"], json!(["crosses_road"]));
    // Looked at again a minute later, not every pass.
    op_ingest::escapes::scan(&app.ctx, now + Duration::seconds(62)).await.unwrap();
    assert_eq!(app.log().await.len(), log.len());
}

#[tokio::test]
async fn a_jump_gets_no_pen() {
    let app = App::new().await;
    let (_, key) = app.collar().await;
    let t0 = Utc::now();
    app.fix(&key, 150.0, 100.0, t0).await;
    assert_eq!(app.send(&poly(0.0, 0.0, 300.0, 200.0), &[]).await.0, StatusCode::CREATED);
    // Inside, then 150 m out a second later.
    app.fix(&key, 150.0, 250.0, t0 + Duration::seconds(1)).await;
    app.fix(&key, 150.0, 251.0, t0 + Duration::seconds(30)).await;
    op_ingest::escapes::scan(&app.ctx, t0 + Duration::seconds(90)).await.unwrap();
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM escapes").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 0);
    let (kind,): (String,) = sqlx::query_as("SELECT kind FROM events ORDER BY rowid DESC LIMIT 1").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(kind, "escape.implausible");
}
