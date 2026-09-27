//! The morning brief after someone stopped a move before its target: it says
//! how far short the herd stopped, not "Sent, all confirmed", through the real
//! routes (a farmer boundary that sweeps, a collar applying the step, the
//! hand's stop), and still the next morning, when the boundary status no
//! longer shows the ended move.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::units::{Fmt, Units};
use op_core::{Ctx, Identity, Via, time};
use op_engine::brief;
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

    async fn brief(&self, herd_id: &str) -> brief::Brief {
        let herd = self.ctx.store().get_herd(herd_id).await.unwrap().unwrap();
        brief::brief(&self.ctx, &herd, time::now()).await.unwrap()
    }
}

fn square(lon: f64, lat: f64) -> Value {
    json!({ "type": "Polygon", "coordinates": [[[lon, lat], [lon + 0.005, lat], [lon + 0.005, lat + 0.0036], [lon, lat + 0.0036], [lon, lat]]] })
}

/// Real protocol reports: `n` fixes at `point` over the last `n` minutes.
async fn report(t: &T, key: &str, point: [f64; 2], n: usize) {
    let now = time::now();
    let fixes: Vec<Value> = (0..n)
        .map(|i| json!({ "at": (now - chrono::Duration::minutes((n - i) as i64)).format("%Y-%m-%dT%H:%M:%SZ").to_string(), "point": point, "accuracy_m": 2.0, "sats": 9 }))
        .collect();
    let (s, v) = t.req("POST", "/collar/v1/report", Some(json!({ "fixes": fixes, "cues": [], "battery": 0.8 })), Some(key)).await;
    assert!(s.is_success(), "report: {s} {v}");
}

#[tokio::test]
async fn a_stopped_move_says_how_far_short_it_stopped() {
    let t = setup().await;
    t.ok("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03] }))).await;
    let p1 = t.ok("POST", "/api/paddocks", Some(json!({ "name": "P1", "geometry": square(-93.625, 42.03) }))).await;
    let p2 = t.ok("POST", "/api/paddocks", Some(json!({ "name": "P2", "geometry": square(-93.62, 42.03) }))).await;
    let herd = t.ok("POST", "/api/herds", Some(json!({ "name": "Cows", "species": "cattle", "count": 2, "paddock_id": p1["id"] }))).await;
    let herd = herd["id"].as_str().unwrap().to_owned();
    let mut keys = vec![];
    for _ in 0..2 {
        keys.push(t.ok("POST", "/api/collars", Some(json!({ "herd_id": herd }))).await["key"].as_str().unwrap().to_owned());
    }
    // The herd is in P1 by its collars, so a farmer boundary on P2 sweeps.
    for k in &keys {
        report(&t, k, [-93.624, 42.0318], 5).await;
    }
    t.ok("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({ "geometry": p2["geometry"] }))).await;
    let status = op_ingest::boundary_status(&t.ctx, &herd).await.unwrap();
    assert_eq!(status.r#move.as_ref().map(|m| m.status), Some(op_core::MoveStatus::Sweeping), "{status:?}");
    let active = status.active.clone().unwrap();
    // Both collars hold the step.
    for k in &keys {
        let ack =
            json!({ "command_id": active.id, "version": active.version, "status": "applied", "at": time::now().format("%Y-%m-%dT%H:%M:%SZ").to_string() });
        let (s, v) = t.req("POST", "/collar/v1/ack", Some(ack), Some(k)).await;
        assert!(s.is_success(), "ack: {s} {v}");
    }
    t.ctx.update_settings(&json!({ "units": "imperial" })).await.unwrap();
    let sweeping = t.brief(&herd).await;
    assert!(sweeping.lines[1].starts_with("Sent, 2/2 collars confirmed, ") && sweeping.lines[1].ends_with(" to go."), "{:?}", sweeping.lines);

    // A hand stops it partway.
    let stopped = t.ok("POST", &format!("/api/herds/{herd}/move/stop"), None).await;
    assert_eq!(stopped["status"], "stopped");
    let short = stopped["remaining_m"].as_f64().unwrap();
    assert!(short >= 1.0, "stopped short of the target: {short} m");
    let b = t.brief(&herd).await;
    let area = Fmt::new(Units::Imperial).area(p2["area_ha"].as_f64().unwrap());
    let feet = Fmt::new(Units::Imperial).len(short);
    assert_eq!(b.lines[0], format!("Cows: MOVE to P2 ({area})."));
    assert_eq!(b.lines[1], format!("Stopped {feet} short, 2/2 collars confirmed."));
    assert!(!b.lines.iter().any(|l| l.starts_with("Sent")), "{:?}", b.lines);
    assert!(b.text.contains(&format!("Stopped {feet} short")), "the text keeps it: {}", b.text);

    // The next morning the boundary status no longer shows the ended move; the brief still knows.
    let hours_ago = time::to_db(&(time::now() - chrono::Duration::hours(10)));
    sqlx::query("UPDATE moves SET updated_at = ? WHERE herd_id = ?").bind(&hours_ago).bind(&herd).execute(t.ctx.db()).await.unwrap();
    assert!(op_ingest::boundary_status(&t.ctx, &herd).await.unwrap().r#move.is_none());
    t.ctx.update_settings(&json!({ "units": "metric" })).await.unwrap();
    let metres = Fmt::new(Units::Metric).len(short);
    assert_eq!(t.brief(&herd).await.lines[1], format!("Stopped {metres} short, 2/2 collars confirmed."));
}

#[tokio::test]
async fn a_finished_move_is_sent_and_confirmed() {
    let t = setup().await;
    t.ok("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03] }))).await;
    let p1 = t.ok("POST", "/api/paddocks", Some(json!({ "name": "P1", "geometry": square(-93.625, 42.03) }))).await;
    let herd = t.ok("POST", "/api/herds", Some(json!({ "name": "Cows", "species": "cattle", "count": 1, "paddock_id": p1["id"] }))).await;
    let herd = herd["id"].as_str().unwrap().to_owned();
    let key = t.ok("POST", "/api/collars", Some(json!({ "herd_id": herd }))).await["key"].as_str().unwrap().to_owned();
    report(&t, &key, [-93.624, 42.0318], 3).await;
    // Already inside: sent directly, nothing to stop.
    t.ok("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({ "geometry": p1["geometry"] }))).await;
    let active = op_ingest::boundary_status(&t.ctx, &herd).await.unwrap().active.unwrap();
    let ack = json!({ "command_id": active.id, "version": active.version, "status": "applied", "at": time::now().format("%Y-%m-%dT%H:%M:%SZ").to_string() });
    assert!(t.req("POST", "/collar/v1/ack", Some(ack), Some(&key)).await.0.is_success());
    assert_eq!(t.brief(&herd).await.lines[1], "Sent, 1/1 collars confirmed.");
}
