//! Regressions for the safety review of 2026-10-26 (Astra 02 F01–F06, F17):
//! the exact command a collar downloads, copies, escape pens, staged
//! boundaries after the map changes, the check and the insert racing a map
//! edit, and who may confirm.

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
        let dir = tempfile::tempdir_in("/tmp").unwrap();
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

/// F01: a collar linked after a holed boundary went out would get the outer
/// ring alone (the no-go zone grazable): the download refuses it.
#[tokio::test]
async fn a_legacy_collar_is_refused_a_boundary_whose_no_go_hole_it_cant_keep() {
    let app = App::new().await;
    let (_, key) = app.collar().await;
    app.report(&key, json!({"device": v0()})).await;
    app.feature("no_go", "Creek", json!(poly(120.0, 60.0, 180.0, 140.0)), json!({})).await;
    assert_eq!(app.send(&poly(0.0, 0.0, 300.0, 200.0), &[]).await.0, StatusCode::CREATED);
    // The hole-holding collar gets it, signed, with the creek a hole.
    let (s, v) = app.req("GET", "/collar/v1/boundary?have=0", Some(&key), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let cmd: op_protocol::BoundaryCommand = serde_json::from_value(v).unwrap();
    assert!(!cmd.polygon().contains(at(150.0, 100.0)));
    // A legacy collar (no caps) linked afterwards: refused, logged, said.
    let (legacy, legacy_key) = app.collar().await;
    let (s, v) = app.req("GET", "/collar/v1/boundary?have=0", Some(&legacy_key), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["never"], json!(["enters_no_go"]));
    let log = app.log().await;
    assert_eq!((log[0]["stage"].as_str(), log[0]["allowed"].as_bool(), log[0]["collar_id"].as_str()), (Some("delivery"), Some(false), Some(legacy.as_str())));
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events WHERE kind = 'boundary.refused'").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 1);
}

/// F01: copies for one collar (out of step, handed over) are judged afresh:
/// a no-go zone drawn over the herd's boundary since stops them.
#[tokio::test]
async fn copies_for_a_collar_are_judged_on_the_map_as_it_is_now() {
    let app = App::new().await;
    let (collar, key) = app.collar().await;
    app.report(&key, json!({"device": v0()})).await;
    assert_eq!(app.send(&poly(0.0, 0.0, 300.0, 200.0), &[]).await.0, StatusCode::CREATED);
    app.feature("no_go", "New closure", json!(poly(-20.0, -20.0, 320.0, 220.0)), json!({})).await;
    app.report(&key, json!({"device": v0(), "slots": [{"version": 99, "status": "applied"}]})).await;
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM boundaries WHERE collar_id IS NOT NULL").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 0, "no copy into the closure");
    let log = app.log().await;
    let copy = log.iter().find(|e| e["stage"] == "copy").expect("the copy is in the gate log");
    assert_eq!((copy["allowed"].as_bool(), copy["collar_id"].as_str()), (Some(false), Some(collar.as_str())));
    assert_eq!(copy["codes"], json!(["enters_no_go"]));
}

/// F01/F17: every stored boundary is linked to the gate row that let it
/// through, with the revision and the exact shape's hash.
#[tokio::test]
async fn every_stored_boundary_names_its_gate_row() {
    let app = App::new().await;
    let (_, key) = app.collar().await;
    app.report(&key, json!({"device": v0()})).await;
    assert_eq!(app.send(&poly(0.0, 0.0, 300.0, 200.0), &[]).await.0, StatusCode::CREATED);
    let rows: Vec<(String, u32, Option<String>, Option<i64>, String)> =
        sqlx::query_as("SELECT id, version, gate_id, policy_revision, geometry FROM boundaries").fetch_all(app.ctx.db()).await.unwrap();
    assert!(!rows.is_empty());
    for (id, version, gate_id, revision, geometry) in rows {
        let gate_id = gate_id.expect("stored through the gate");
        let (bid, v, rev, hash, allowed): (Option<String>, Option<i64>, Option<i64>, Option<String>, bool) =
            sqlx::query_as("SELECT boundary_id, version, revision, geometry_hash, allowed FROM gate_log WHERE id = ?")
                .bind(&gate_id)
                .fetch_one(app.ctx.db())
                .await
                .unwrap();
        assert!(allowed);
        assert_eq!((bid.as_deref(), v, rev), (Some(id.as_str()), Some(i64::from(version)), revision));
        let g: Polygon = serde_json::from_str(&geometry).unwrap();
        assert_eq!(hash.unwrap(), op_ingest::gate::geometry_hash(&g));
    }
}

/// F02: a pen for an animal outside is judged whole: a no-go zone over the
/// herd's boundary and the animal is no excuse.
#[tokio::test]
async fn an_escape_pen_never_goes_into_a_no_go_zone_the_herd_is_in() {
    let app = App::new().await;
    let (_, key) = app.collar().await;
    app.report(&key, json!({"device": v0()})).await;
    let t = Utc::now();
    app.fix(&key, 150.0, 100.0, t).await;
    assert_eq!(app.send(&poly(0.0, 0.0, 300.0, 200.0), &[]).await.0, StatusCode::CREATED);
    app.feature("no_go", "Flooded ground", json!(poly(-200.0, -200.0, 500.0, 500.0)), json!({})).await;
    app.fix(&key, 150.0, 230.0, t + Duration::seconds(120)).await;
    app.fix(&key, 150.0, 231.0, t + Duration::seconds(150)).await;
    op_ingest::escapes::scan(&app.ctx, t + Duration::seconds(210)).await.unwrap();
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM escapes").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 0, "no pen in the flooded ground");
    assert!(app.log().await.iter().all(|e| !(e["sender"] == "escape" && e["allowed"] == true)), "no pen allowed");
}

/// F04: a staged boundary the map has since made unsafe is never handed to
/// a collar, and a check after the change finds it (nothing safe can
/// replace the herd's ground here: said in the activity).
#[tokio::test]
async fn a_staged_boundary_made_unsafe_is_not_delivered_and_is_reported() {
    let app = App::new().await;
    let (_, key) = app.collar().await;
    app.report(&key, json!({"device": v0()})).await;
    let when = op_protocol::wire_time::trunc_secs(Utc::now() + Duration::minutes(10));
    let (s, v) = app
        .req("POST", &format!("/api/herds/{}/boundary", app.herd), None, Some(json!({"geometry": poly(0.0, 0.0, 300.0, 200.0), "effective_at": ts(when)})))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    app.feature("no_go", "Washed out", json!(poly(-20.0, -20.0, 320.0, 220.0)), json!({})).await;
    let (s, v) = app.req("GET", "/collar/v1/boundary?have=0&free=4", Some(&key), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "not delivered: {v}");
    let out = op_ingest::withdraw::recheck_staged(&app.ctx).await.unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].superseded_by, None);
    // As it takes effect, it is checked again and said.
    let announced = op_ingest::announce_activations(&app.ctx, when - Duration::seconds(1), when + Duration::seconds(1)).await.unwrap();
    assert_eq!(announced.len(), 1);
    let log = app.log().await;
    assert!(log.iter().any(|e| e["stage"] == "activation" && e["allowed"] == false));
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events WHERE kind = 'boundary.unsafe'").fetch_one(app.ctx.db()).await.unwrap();
    assert!(n >= 1);
}

/// F04: when the ground the herd is on is still safe, a staged boundary the
/// map made unsafe is withdrawn: the boundary in effect goes again as a
/// newer immediate version, and a collar that picks it up drops the staged one.
#[tokio::test]
async fn a_staged_boundary_made_unsafe_is_superseded_and_collars_drop_it() {
    let app = App::new().await;
    let (_, key) = app.collar().await;
    app.report(&key, json!({"device": v0()})).await;
    assert_eq!(app.send(&poly(0.0, 0.0, 150.0, 200.0), &[]).await.0, StatusCode::CREATED);
    let when = op_protocol::wire_time::trunc_secs(Utc::now() + Duration::minutes(10));
    let (s, v) = app
        .req("POST", &format!("/api/herds/{}/boundary", app.herd), None, Some(json!({"geometry": poly(150.0, 0.0, 300.0, 200.0), "effective_at": ts(when)})))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    // The collar holds both.
    let mut slots = op_protocol::SlotStore::new(op_geo::CollarLimits::V0, Some(app.herd.clone()), None);
    for _ in 0..2 {
        let (s, v) = app.req("GET", &format!("/collar/v1/boundary?have={}&free={}", slots.have(), slots.free()), Some(&key), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        slots.insert(serde_json::from_value(v).unwrap(), Some(Utc::now()));
    }
    let staged_version = slots.staged()[0].cmd.version;
    // A no-go zone drawn inside tomorrow's ground.
    app.feature("no_go", "Sinkhole", json!(poly(200.0, 60.0, 260.0, 140.0)), json!({})).await;
    let out = op_ingest::withdraw::recheck_staged(&app.ctx).await.unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].version, staged_version);
    let newer = out[0].superseded_by.expect("superseded");
    assert!(newer > staged_version);
    let st = app.req("GET", &format!("/api/herds/{}/boundary", app.herd), None, None).await.1;
    assert_eq!(st["active"]["version"].as_u64(), Some(u64::from(newer)));
    assert!(st["staged"].as_array().is_none_or(|a| a.is_empty()), "{st}");
    // The collar picks it up and drops the staged one: at its time, the sinkhole isn't grazable.
    let (s, v) = app.req("GET", &format!("/collar/v1/boundary?have={}&free={}", slots.have(), slots.free()), Some(&key), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    slots.insert(serde_json::from_value(v).unwrap(), Some(Utc::now()));
    slots.tick(when + Duration::seconds(1));
    assert_eq!(slots.active().unwrap().cmd.version, newer);
    assert!(!slots.active().unwrap().cmd.polygon().contains(at(230.0, 100.0)));
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events WHERE kind = 'boundary.withdrawn'").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 1);
}

/// F05: a map edit landing between the check and the insert is seen: the
/// insert judges again under the write lock and refuses.
#[tokio::test]
async fn a_no_go_zone_drawn_while_a_send_waits_refuses_it() {
    let app = App::new().await;
    let mut tx = op_core::store::begin_immediate(app.ctx.db()).await.unwrap();
    let (ctx, herd) = (app.ctx.clone(), app.herd.clone());
    let task = tokio::spawn(async move {
        op_ingest::send_boundary(&ctx, &herd, poly(0.0, 0.0, 300.0, 200.0), SendOpts { sender: Sender::of(SenderKind::Auto), ..Default::default() }, "dec_race")
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(!task.is_finished());
    let time = op_core::time::to_db(&Utc::now());
    sqlx::query("INSERT INTO features (id, kind, name, geometry, props, created_at, updated_at) VALUES ('fea_race', 'no_go', 'Creek', ?, '{}', ?, ?)")
        .bind(json!(poly(-20.0, -20.0, 320.0, 220.0)).to_string())
        .bind(&time)
        .bind(&time)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let result = task.await.unwrap();
    assert!(result.is_err(), "stored on a stale check");
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM boundaries").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 0);
}

/// F06: the app token is a program: it can't confirm a hard finding, token or not.
#[tokio::test]
async fn the_app_token_cant_confirm_a_road() {
    let app = App::new().await;
    app.road_across(100.0).await;
    let api = op_core::with_identity(op_core::router().merge(op_ingest::router()), Identity::owner(Via::AppToken)).with_state(app.ctx.clone());
    let send = |token: Option<String>| {
        let api = api.clone();
        let body = json!({"geometry": poly(0.0, 0.0, 300.0, 200.0), "confirm": ["crosses_road"], "confirm_token": token});
        let path = format!("/api/herds/{}/boundary", app.herd);
        async move {
            let req = Request::builder().method("POST").uri(path).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
            let res = api.oneshot(req).await.unwrap();
            let s = res.status();
            (s, serde_json::from_slice::<Value>(&res.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null))
        }
    };
    let (s, v) = send(None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"].as_str().unwrap().contains("Only a person"), "{v}");
    // Even with a token a person's refusal gave.
    let (_, person) = app.send(&poly(0.0, 0.0, 300.0, 200.0), &[]).await;
    let (s, _) = send(person["confirm_token"].as_str().map(str::to_owned)).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(app.log().await.iter().all(|e| e["allowed"] == false));
}
