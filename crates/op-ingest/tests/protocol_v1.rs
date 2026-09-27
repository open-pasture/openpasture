//! Protocol v1 on the server (field-ready §2.12, §3): caps, slots, the
//! download order, holes and legacy collars, acks with codes, cues and
//! episodes, health, slot counts, escapes restaged per collar, configs, and
//! the query plans on the report path.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use http_body_util::BodyExt;
use op_core::Ctx;
use op_geo::{CollarLimits, Polygon, Projection};
use op_protocol::{BoundaryCommand, ConfigCommand};
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
}

const MID: [f64; 2] = [-92.405, 38.125];

fn m_at(x: f64, y: f64) -> [f64; 2] {
    Projection::new(MID).inverse([x, y])
}

fn ring(pts: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut r: Vec<[f64; 2]> = pts.iter().map(|p| m_at(p[0], p[1])).collect();
    r.push(r[0]);
    r
}

fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<[f64; 2]> {
    ring(&[[x0, y0], [x1, y0], [x1, y1], [x0, y1]])
}

fn circle(cx: f64, cy: f64, r: f64, n: usize) -> Vec<[f64; 2]> {
    let pts: Vec<[f64; 2]> = (0..n)
        .map(|i| {
            let a = std::f64::consts::TAU * i as f64 / n as f64;
            [cx + r * a.cos(), cy + r * a.sin()]
        })
        .collect();
    ring(&pts)
}

fn polygon(outer: Vec<[f64; 2]>, holes: Vec<Vec<[f64; 2]>>) -> Value {
    let mut coords = vec![outer];
    coords.extend(holes);
    json!({"type": "Polygon", "coordinates": coords})
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn v0_device() -> Value {
    json!({"fw": "0.2.0", "caps": ["holes", "slots", "collar_id", "cue_mode", "episodes", "config"], "limits": CollarLimits::V0})
}

fn fix_at(point: [f64; 2], at: DateTime<Utc>) -> Value {
    json!({"at": ts(at), "point": point, "accuracy_m": 2.0, "sats": 9})
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let router = op_core::router().merge(op_ingest::router()).with_state(ctx.clone());
        Self { _dir: dir, ctx, router }
    }

    async fn raw(&self, method: &str, path: &str, key: Option<&str>, body: Option<Value>) -> (StatusCode, Vec<u8>) {
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
        (status, res.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    async fn req(&self, method: &str, path: &str, key: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let (s, bytes) = self.raw(method, path, key, body).await;
        (s, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into())) })
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.req(method, path, None, body).await
    }

    /// Farm, a 300 x 200 m paddock around MID and a herd in it.
    async fn herd(&self, name: &str) -> String {
        if self.call("GET", "/api/farm", None).await.0 != StatusCode::OK {
            let (s, v) = self.call("POST", "/api/farm", Some(json!({"name": "Home", "timezone": "America/Chicago", "center": MID}))).await;
            assert_eq!(s, StatusCode::CREATED, "{v}");
        }
        let (s, pad) = self
            .call("POST", "/api/paddocks", Some(json!({"name": format!("{name} paddock"), "geometry": polygon(rect(0.0, 0.0, 300.0, 200.0), vec![])})))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{pad}");
        let (s, h) = self.call("POST", "/api/herds", Some(json!({"name": name, "species": "cattle", "count": 0, "paddock_id": pad["id"]}))).await;
        assert_eq!(s, StatusCode::CREATED, "{h}");
        h["id"].as_str().unwrap().to_owned()
    }

    async fn device(&self, herd: &str) -> (String, String) {
        let (s, v) = self.call("POST", "/api/collars", Some(json!({"herd_id": herd}))).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        (v["collar"]["id"].as_str().unwrap().to_owned(), v["key"].as_str().unwrap().to_owned())
    }

    /// A collar that has said it is firmware 0.2 (V0 limits, every cap).
    async fn v0(&self, herd: &str) -> (String, String) {
        let c = self.device(herd).await;
        self.report(&c.1, json!({"device": v0_device()})).await;
        c
    }

    async fn report(&self, key: &str, body: Value) -> Value {
        let (s, v) = self.req("POST", "/collar/v1/report", Some(key), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    /// The command served for `query`, checked on its raw bytes as a collar checks it.
    async fn download(&self, key: &str, query: &str) -> Option<BoundaryCommand> {
        let (s, bytes) = self.raw("GET", &format!("/collar/v1/boundary?{query}"), Some(key), None).await;
        match s {
            StatusCode::NO_CONTENT => None,
            StatusCode::OK => Some(op_protocol::verify_wire(&bytes, &self.ctx.public_key()).expect("signed, canonical wire")),
            other => panic!("{other}: {}", String::from_utf8_lossy(&bytes)),
        }
    }

    async fn send(&self, herd: &str, g: Value, extra: Value) -> (StatusCode, Value) {
        let mut body = json!({"geometry": g});
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        self.call("POST", &format!("/api/herds/{herd}/boundary"), Some(body)).await
    }

    async fn status(&self, herd: &str) -> Value {
        self.call("GET", &format!("/api/herds/{herd}/boundary"), None).await.1
    }

    async fn ack(&self, key: &str, cmd: &BoundaryCommand, status: &str, code: Option<&str>) {
        let mut body = json!({"command_id": cmd.command_id, "version": cmd.version, "status": status, "at": op_protocol::wire_time::format(&Utc::now())});
        if let Some(c) = code {
            body["code"] = json!(c);
        }
        let (s, v) = self.req("POST", "/collar/v1/ack", Some(key), Some(body)).await;
        assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    }

    async fn slots(&self, collar: &str) -> Value {
        let (s, v) = self.call("GET", &format!("/api/collars/{collar}/slots"), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    async fn collar(&self, id: &str) -> Value {
        self.call("GET", &format!("/api/collars/{id}"), None).await.1
    }

    async fn scalar(&self, q: &str, bind: &str) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(q).bind(bind).fetch_one(self.ctx.db()).await.unwrap()
    }
}

#[tokio::test]
async fn caps_limits_and_firmware_are_stored() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let (id, key) = app.device(&herd).await;
    let (_, c) = app.call("GET", &format!("/api/collars/{id}"), None).await;
    assert!(c.get("caps").is_none() && c.get("fw").is_none(), "nothing reported yet: {c}");
    assert_eq!(app.slots(&id).await["limits"], json!(CollarLimits::LEGACY));

    let v1 = CollarLimits::V1;
    app.report(&key, json!({"device": {"fw": "0.2.1", "caps": ["holes", "slots"], "limits": v1}})).await;
    let (_, c) = app.call("GET", &format!("/api/collars/{id}"), None).await;
    assert_eq!((c["fw"].as_str(), c["caps"].clone()), (Some("0.2.1"), json!(["holes", "slots"])));
    let s = app.slots(&id).await;
    assert_eq!(s["limits"], json!(v1));
    assert_eq!(s["fw"], "0.2.1");
    // Caps without limits: V0.
    app.report(&key, json!({"device": {"fw": "0.2.0", "caps": ["holes"]}})).await;
    assert_eq!(app.slots(&id).await["limits"], json!(CollarLimits::V0));
    // A report without `device` keeps what was said.
    app.report(&key, json!({"battery": 0.5})).await;
    assert_eq!(app.slots(&id).await["fw"], "0.2.0");
    // The PATCH route can't write them.
    let (_, c) = app.call("PATCH", &format!("/api/collars/{id}"), Some(json!({"fw": "9", "caps": []}))).await;
    assert_eq!(c["fw"], "0.2.0");
}

#[tokio::test]
async fn download_order_free_slots_and_bytes() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let (_, key) = app.v0(&herd).await;
    let g = polygon(rect(0.0, 0.0, 300.0, 200.0), vec![]);
    let (s, _) = app.send(&herd, g.clone(), json!({})).await;
    assert_eq!(s, StatusCode::CREATED);
    let later = |m: i64| op_protocol::wire_time::format(&(Utc::now() + Duration::minutes(m)));
    app.send(&herd, polygon(rect(0.0, 0.0, 280.0, 200.0), vec![]), json!({"effective_at": later(60)})).await;
    app.send(&herd, polygon(rect(0.0, 0.0, 260.0, 200.0), vec![]), json!({"effective_at": later(120)})).await;

    // The active one first, then the staged ones in order, then nothing.
    assert_eq!(app.download(&key, "have=0&free=16&free_bytes=24576").await.unwrap().version, 1);
    let v2 = app.download(&key, "have=1&free=15&free_bytes=24000").await.unwrap();
    assert_eq!((v2.version, v2.effective_at.is_some()), (2, true));
    assert_eq!(app.download(&key, "have=2&free=14&free_bytes=24000").await.unwrap().version, 3);
    assert!(app.download(&key, "have=3&free=13&free_bytes=24000").await.is_none());
    // No free slot, or too few bytes for the record: staged ones are held back.
    assert!(app.download(&key, "have=1&free=0&free_bytes=24000").await.is_none());
    let bytes = v2.record_bytes();
    assert_eq!(bytes, 192 + 8 * 4);
    assert!(app.download(&key, &format!("have=1&free=3&free_bytes={}", bytes - 1)).await.is_none());
    assert_eq!(app.download(&key, &format!("have=1&free=3&free_bytes={bytes}")).await.unwrap().version, 2);
    // A collar that doesn't say (legacy) gets what its slots hold less what its acks say it holds.
    assert_eq!(app.download(&key, "have=1").await.unwrap().version, 2);
    // The active one is never held back.
    assert_eq!(app.download(&key, "have=0&free=0&free_bytes=0").await.unwrap().version, 1);
}

#[tokio::test]
async fn refused_versions_are_skipped_and_slots_full_is_retried() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let (id, key) = app.v0(&herd).await;
    app.send(&herd, polygon(rect(0.0, 0.0, 300.0, 200.0), vec![]), json!({})).await;
    let later = |m: i64| op_protocol::wire_time::format(&(Utc::now() + Duration::minutes(m)));
    app.send(&herd, polygon(rect(0.0, 0.0, 280.0, 200.0), vec![]), json!({"effective_at": later(60)})).await;
    let v1 = app.download(&key, "have=0&free=16").await.unwrap();
    app.ack(&key, &v1, "applied", None).await;
    let v2 = app.download(&key, "have=1&free=15").await.unwrap();
    app.ack(&key, &v2, "rejected", Some("hole_too_close")).await;
    // Refused for good: not offered again, and not the latest for it.
    assert!(app.download(&key, "have=1&free=15").await.is_none());
    assert_eq!(app.report(&key, json!({})).await["latest_version"], 1);
    app.send(&herd, polygon(rect(0.0, 0.0, 260.0, 200.0), vec![]), json!({"effective_at": later(90)})).await;
    let v3 = app.download(&key, "have=1&free=15").await.unwrap();
    assert_eq!(v3.version, 3);
    // Full: tried again once the collar has room.
    app.ack(&key, &v3, "rejected", Some("slots_full")).await;
    assert!(app.download(&key, "have=1&free=0").await.is_none());
    assert_eq!(app.download(&key, "have=1&free=1").await.unwrap().version, 3);
    // The codes are kept.
    let codes: Vec<(i64, Option<String>)> = sqlx::query_as("SELECT version, code FROM acks WHERE collar_id = ? AND status = 'rejected' ORDER BY id")
        .bind(&id)
        .fetch_all(app.ctx.db())
        .await
        .unwrap();
    assert_eq!(codes, vec![(2, Some("hole_too_close".into())), (3, Some("slots_full".into()))]);
    assert_eq!(app.scalar("SELECT code FROM collar_boundary_state WHERE collar_id = ?", &id).await.as_deref(), Some("slots_full"));
    let st = app.status(&herd).await;
    assert_eq!((st["acks"][0]["version"].as_u64(), st["acks"][0]["code"].as_str()), (Some(3), Some("slots_full")));
    // A rejection with a code this server doesn't know counts as permanent.
    app.send(&herd, polygon(rect(0.0, 0.0, 250.0, 200.0), vec![]), json!({"effective_at": later(100)})).await;
    let v4 = app.download(&key, "have=3&free=15").await.unwrap();
    app.ack(&key, &v4, "rejected", Some("from_the_future")).await;
    assert!(app.download(&key, "have=3&free=15").await.is_none());
}

#[tokio::test]
async fn a_boundary_with_a_hole_goes_whole_to_v0_and_as_one_ring_to_legacy() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let (v0_id, v0_key) = app.v0(&herd).await;
    let (legacy_id, legacy_key) = app.device(&herd).await;
    // A round 100-corner field with a pond in the middle.
    let pond = rect(130.0, 80.0, 170.0, 120.0);
    let g = polygon(circle(150.0, 100.0, 95.0, 100), vec![pond.clone()]);
    let (s, m) = app.send(&herd, g, json!({})).await;
    assert_eq!(s, StatusCode::CREATED, "{m}");
    let st = app.status(&herd).await;
    let stored: Polygon = serde_json::from_value(st["active"]["geometry"].clone()).unwrap();
    assert_eq!(stored.coordinates.len(), 2, "the hole is stored");

    let v0 = app.download(&v0_key, "have=0&free=16&free_bytes=24576").await.unwrap();
    assert_eq!((v0.boundary.len(), v0.holes.len()), (100, 1));
    v0.check(Some(&herd), Some(&v0_id), &CollarLimits::V0, &Default::default()).unwrap();
    let legacy = app.download(&legacy_key, "have=0").await.unwrap();
    assert!(legacy.holes.is_empty() && legacy.boundary.len() <= 64, "{} corners, {} holes", legacy.boundary.len(), legacy.holes.len());
    assert!(legacy.collar_id.is_none());
    legacy.validate(None).expect("a firmware 0.1 collar takes it");
    // Same version and signer; only the shape differs per collar.
    assert_eq!((legacy.version, &legacy.command_id), (v0.version, &v0.command_id));

    // The server fence follows what each collar holds: in the pond is outside
    // for the v0 collar and inside for the legacy one.
    let now = Utc::now();
    for key in [&v0_key, &legacy_key] {
        app.report(key, json!({"boundary_version": 1, "fixes": [fix_at(m_at(150.0, 100.0), now)]})).await;
    }
    assert_eq!(app.collar(&v0_id).await["state"], "outside");
    assert_eq!(app.collar(&legacy_id).await["state"], "inside");
    // And at a point inside the legacy ring, both agree.
    let edge = op_ingest::fence_geometry(
        &serde_json::from_value(st["active"].clone()).unwrap(),
        &op_ingest::CollarCaps { fw: None, caps: vec![], limits: CollarLimits::LEGACY },
    );
    assert_eq!(edge, legacy.polygon(), "the server fence is the collar's ring");
}

#[tokio::test]
async fn prepare_validates_fits_and_reports() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    // A hole too close to the edge is refused, in the farm's units.
    let near = polygon(rect(0.0, 0.0, 300.0, 200.0), vec![rect(5.0, 50.0, 40.0, 90.0)]);
    let (s, v) = app.send(&herd, near, json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let gap = op_core::units::Fmt::of(&app.ctx).await.unwrap().len(12.5);
    assert_eq!(v["error"], json!(format!("Holes need {gap} between them and from the edge.")));
    // Crossing rings, too.
    let (s, _) = app.send(&herd, polygon(rect(0.0, 0.0, 300.0, 200.0), vec![rect(250.0, 50.0, 350.0, 90.0)]), json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM boundaries").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 0);

    // 200 corners fit V0 (the default for a herd with no v1 collars): simplified.
    let big: Polygon = serde_json::from_value(polygon(circle(150.0, 100.0, 95.0, 200), vec![rect(130.0, 80.0, 170.0, 120.0)])).unwrap();
    let p = op_ingest::prepare(&app.ctx, &herd, &big, &Default::default()).await.unwrap();
    assert!(p.geometry.outer_ring().len() <= 128);
    assert_eq!((p.warn_m, p.hysteresis_m), (5.0, 1.0));
    let codes: Vec<&str> = p.findings.iter().map(|f| f.code.as_str()).collect();
    assert_eq!(codes, ["simplified"]);
    // Collars that can't hold holes are named.
    let (legacy, _) = app.device(&herd).await;
    let p = op_ingest::prepare(&app.ctx, &herd, &big, &Default::default()).await.unwrap();
    let f = p.findings.iter().find(|f| f.code == "collars_no_holes").unwrap();
    assert_eq!((f.text.as_str(), f.targets.clone()), ("1 collar can't hold holes", vec![("collar".to_owned(), legacy)]));
    // A herd whose v1 collars hold fewer corners gets fewer.
    let (_, key) = app.device(&herd).await;
    app.report(&key, json!({"device": {"fw": "0.2.0", "caps": ["holes"], "limits": {"outer": 40, "holes": 4, "hole_vertices": 16, "total": 80, "slots": 8, "slot_bytes": 8000}}}))
        .await;
    let p = op_ingest::prepare(&app.ctx, &herd, &big, &Default::default()).await.unwrap();
    assert!(p.geometry.outer_ring().len() <= 40, "{}", p.geometry.outer_ring().len());
    assert_eq!(op_ingest::herd_limits(&app.ctx, &herd).await.unwrap().outer, 40);
}

#[tokio::test]
async fn sweep_steps_carry_the_targets_holes() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let mut keys = Vec::new();
    for i in 0..4 {
        let (_, key) = app.v0(&herd).await;
        app.report(&key, json!({"fixes": [fix_at(m_at(30.0 + 15.0 * i as f64, 30.0), Utc::now())]})).await;
        keys.push(key);
    }
    app.send(&herd, polygon(rect(0.0, 0.0, 300.0, 200.0), vec![]), json!({})).await;
    let target = polygon(rect(200.0, 100.0, 300.0, 200.0), vec![rect(240.0, 140.0, 260.0, 160.0)]);
    let (s, m) = app.send(&herd, target, json!({})).await;
    assert_eq!((s, m["status"].as_str()), (StatusCode::CREATED, Some("sweeping")), "{m}");
    let step: Polygon = serde_json::from_value(app.status(&herd).await["active"]["geometry"].clone()).unwrap();
    assert_eq!(step.coordinates.len(), 2, "the target's hole is in the step");
    assert!(!step.contains(m_at(250.0, 150.0)));
    assert!(step.contains(m_at(210.0, 110.0)));
}

#[tokio::test]
async fn cues_episodes_and_health_are_stored() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let (id, key) = app.v0(&herd).await;
    app.send(&herd, polygon(rect(0.0, 0.0, 300.0, 200.0), vec![rect(100.0, 60.0, 160.0, 120.0)]), json!({})).await;
    let mut rx = app.ctx.subscribe();
    let now = Utc::now();
    let body = json!({
        "boundary_version": 1,
        "fixes": [
            {"at": ts(now - Duration::seconds(10)), "point": m_at(150.0, 50.0), "accuracy_m": 1.8, "sats": 9, "hdop": 0.9},
            {"at": ts(now), "point": m_at(150.0, 57.0), "accuracy_m": 1.8, "sats": 9, "hdop": 1.1, "boundary_version": 2}
        ],
        "cues": [
            {"at": ts(now - Duration::seconds(5)), "kind": "warn", "level": 2, "dur_ms": 300, "margin_m": 1.4, "ring": 1, "boundary_version": 1},
            {"at": ts(now), "level": 4, "margin_m": -2.0}
        ],
        "episodes": [{"start": ts(now - Duration::seconds(30)), "end": ts(now - Duration::seconds(5)), "boundary_version": 1, "ring": 1,
                      "cues": 4, "max_level": 3, "min_margin_m": 0.8, "outcome": "turned_back"}],
        "battery": 0.81,
        "health": {"fix_attempts": 120, "fix_ok": 118,
                   "cell": {"rsrp_dbm": -104, "rsrq_db": -11.5, "snr_db": 6, "mode": "ltem", "band": 12, "cell_id": "1A2B3C", "tac": 1234},
                   "still_s": 40, "tilt_deg": 12, "temp_c": 21.5, "battery_v": 3.31, "charging": true, "uptime_s": 86400, "reset": "power_on"}
    });
    app.report(&key, body.clone()).await;
    let cues: Vec<(Option<String>, Option<i64>, Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT kind, ring, dur_ms, boundary_version FROM cues WHERE collar_id = ? ORDER BY t").bind(&id).fetch_all(app.ctx.db()).await.unwrap();
    assert_eq!(cues, vec![(Some("warn".into()), Some(1), Some(300), Some(1)), (None, None, None, Some(1))]);
    let fixes: Vec<(Option<f64>, Option<i64>)> =
        sqlx::query_as("SELECT hdop, boundary_version FROM fixes WHERE collar_id = ? ORDER BY t").bind(&id).fetch_all(app.ctx.db()).await.unwrap();
    assert_eq!(fixes, vec![(Some(0.9), Some(1)), (Some(1.1), Some(2))]);
    let ep: (String, i64, i64, i64, f64, String, Option<i64>) =
        sqlx::query_as("SELECT id, ring, cues, max_level, min_margin_m, outcome, boundary_version FROM episodes WHERE collar_id = ?")
            .bind(&id)
            .fetch_one(app.ctx.db())
            .await
            .unwrap();
    assert!(ep.0.starts_with("epi_"));
    assert_eq!((ep.1, ep.2, ep.3, ep.4, ep.5.as_str(), ep.6), (1, 4, 3, 0.8, "turned_back", Some(1)));
    let cell: (i64, i64, f64, f64, f64, String, i64, String, i64) = sqlx::query_as(
        "SELECT fix_attempts, fix_ok, rsrp_dbm, rsrq_db, snr_db, cell_mode, band, cell_id, tac FROM health WHERE collar_id = ? AND fix_attempts IS NOT NULL",
    )
    .bind(&id)
    .fetch_one(app.ctx.db())
    .await
    .unwrap();
    assert_eq!(cell, (120, 118, -104.0, -11.5, 6.0, "ltem".into(), 12, "1A2B3C".into(), 1234));
    let power: (f64, f64, f64, f64, bool, i64, String) =
        sqlx::query_as("SELECT still_s, tilt_deg, temp_c, battery_v, charging, uptime_s, reset FROM health WHERE collar_id = ? AND fix_attempts IS NOT NULL")
            .bind(&id)
            .fetch_one(app.ctx.db())
            .await
            .unwrap();
    assert_eq!(power, (40.0, 12.0, 21.5, 3.31, true, 86400, "power_on".into()));
    // Live cue events carry the kind (derived for firmware 0.1's) and ring.
    let mut kinds = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let op_core::Event::Cue { kind, ring, .. } = ev {
            kinds.push((kind, ring));
        }
    }
    assert_eq!(kinds, vec![(Some("warn".into()), Some(1)), (Some("outside".into()), None)]);
    // A resent episode is stored once.
    let resend = json!({"episodes": body["episodes"].clone()});
    app.report(&key, resend).await;
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM episodes").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn slots_follow_reports_and_acks_and_counts_leave_out_parked_collars() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let mut collars = Vec::new();
    for _ in 0..3 {
        collars.push(app.v0(&herd).await);
    }
    let (legacy, legacy_key) = app.device(&herd).await;
    app.send(&herd, polygon(rect(0.0, 0.0, 300.0, 200.0), vec![]), json!({})).await;
    let at = Utc::now() + Duration::minutes(30);
    app.send(&herd, polygon(rect(0.0, 0.0, 280.0, 200.0), vec![]), json!({"effective_at": op_protocol::wire_time::format(&at)})).await;
    let at = op_protocol::wire_time::trunc_secs(at);
    for (_, key) in &collars {
        app.report(
            key,
            json!({"slots": [{"version": 1, "status": "applied"}, {"version": 2, "status": "received", "effective_at": op_protocol::wire_time::format(&at)}]}),
        )
        .await;
    }
    // The legacy collar never lists slots; its acks stand in.
    let v1 = app.download(&legacy_key, "have=0").await.unwrap();
    app.ack(&legacy_key, &v1, "applied", None).await;
    let v2 = app.download(&legacy_key, "have=1").await.unwrap();
    app.ack(&legacy_key, &v2, "received", None).await;
    let s = app.slots(&collars[0].0).await;
    let held: Vec<(u64, &str)> = s["slots"].as_array().unwrap().iter().map(|x| (x["version"].as_u64().unwrap(), x["status"].as_str().unwrap())).collect();
    assert_eq!(held, [(1, "applied"), (2, "received")]);
    assert_eq!(app.slots(&legacy).await["slots"].as_array().unwrap().len(), 2);

    // Park one collar: it drops out of the counts.
    sqlx::query("UPDATE collars SET parked_at = ?, parked_reason = 'charging' WHERE id = ?")
        .bind(op_core::time::to_db(&Utc::now()))
        .bind(&collars[2].0)
        .execute(app.ctx.db())
        .await
        .unwrap();
    let st = app.status(&herd).await;
    assert_eq!(st["staged"].as_array().unwrap().len(), 1);
    let counts: Vec<(u64, u64, u64, u64, u64)> = st["slots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["version"].as_u64().unwrap(),
                c["applied"].as_u64().unwrap(),
                c["stored"].as_u64().unwrap(),
                c["rejected"].as_u64().unwrap(),
                c["collars"].as_u64().unwrap(),
            )
        })
        .collect();
    // Two v0 collars and the legacy one; the parked one isn't counted.
    assert_eq!(counts, [(1, 3, 0, 0, 3), (2, 0, 3, 0, 3)]);
    assert_eq!(st["slots"][1]["effective_at"], st["staged"][0]["effective_at"]);
    assert!(st["slots"][0].get("effective_at").is_none());
    let (_, hs) = app.call("GET", &format!("/api/herds/{herd}/slots"), None).await;
    assert_eq!(hs["counts"], st["slots"]);
    assert_eq!(hs["collars"].as_array().unwrap().len(), 4);
    assert!(hs["collars"].as_array().unwrap().iter().any(|c| c["parked"] == true));

    // The next report replaces the list: v2 applied offline, v1 gone.
    app.report(&collars[0].1, json!({"slots": [{"version": 2, "status": "applied"}]})).await;
    let s = app.slots(&collars[0].0).await;
    assert_eq!(s["slots"].as_array().unwrap().len(), 1);
    assert_eq!((s["slots"][0]["version"].as_u64(), s["slots"][0]["status"].as_str()), (Some(2), Some("applied")));
    // An applied ack drops everything lower, as the collar does.
    app.ack(&legacy_key, &v2, "applied", None).await;
    let s = app.slots(&legacy).await;
    assert_eq!(s["slots"].as_array().unwrap().len(), 1);
    let (s, _) = app.call("GET", "/api/collars/col_nope/slots", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app.call("GET", "/api/herds/herd_nope/slots", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_escape_ends_with_copies_for_that_collar_alone() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let (a, a_key) = app.v0(&herd).await;
    let (_, b_key) = app.v0(&herd).await;
    let now = Utc::now();
    for key in [&a_key, &b_key] {
        app.report(key, json!({"fixes": [fix_at(m_at(150.0, 100.0), now)]})).await;
    }
    app.send(&herd, polygon(rect(0.0, 0.0, 300.0, 200.0), vec![rect(100.0, 60.0, 130.0, 90.0)]), json!({})).await;
    let at = Utc::now() + Duration::hours(1);
    app.send(&herd, polygon(rect(0.0, 0.0, 280.0, 200.0), vec![]), json!({"effective_at": op_protocol::wire_time::format(&at)})).await;
    let v2 = app.download(&b_key, "have=1&free=15").await.unwrap();

    // A walks out and stays out: its own pen, which keeps the herd's hole.
    let t = now + Duration::seconds(5);
    app.report(&a_key, json!({"fixes": [fix_at(m_at(150.0, 230.0), t)]})).await;
    op_ingest::escapes::scan(&app.ctx, t + Duration::seconds(60)).await.unwrap();
    let pen = app.download(&a_key, "have=2&free=14").await.unwrap();
    assert_eq!((pen.version, pen.collar_id.as_deref(), pen.holes.len()), (3, Some(a.as_str()), 1));
    // Its config went fast while it is walked back.
    let cfg = app.slots(&a).await["config"].clone();
    assert_eq!(cfg["fast_report_s"], 10, "{cfg}");

    // Back: a copy of the active boundary and of the staged one, for A alone.
    app.report(&a_key, json!({"fixes": [fix_at(m_at(150.0, 150.0), t + Duration::seconds(90))]})).await;
    op_ingest::escapes::scan(&app.ctx, t + Duration::seconds(95)).await.unwrap();
    let copy = app.download(&a_key, "have=3&free=15").await.unwrap();
    assert_eq!((copy.version, copy.collar_id.as_deref(), copy.effective_at), (4, Some(a.as_str()), None));
    let staged = app.download(&a_key, "have=4&free=14").await.unwrap();
    assert_eq!((staged.version, staged.collar_id.as_deref(), staged.effective_at), (5, Some(a.as_str()), v2.effective_at));
    assert_eq!(staged.boundary, v2.boundary);
    assert!(app.download(&a_key, "have=5&free=13").await.is_none());
    // Nobody else downloads anything.
    assert!(app.download(&b_key, "have=2&free=14").await.is_none());
    let st = app.status(&herd).await;
    assert_eq!((st["active"]["version"].as_u64(), st["pending"]["version"].as_u64()), (Some(1), Some(2)));
    // A holding the copies counts for the herd versions they copy.
    app.ack(&a_key, &copy, "applied", None).await;
    app.ack(&a_key, &staged, "received", None).await;
    app.report(&b_key, json!({"slots": [{"version": 1, "status": "applied"}, {"version": 2, "status": "received"}]})).await;
    let st = app.status(&herd).await;
    let slot = |v: u64| st["slots"].as_array().unwrap().iter().find(|s| s["version"] == v).unwrap().clone();
    assert_eq!((slot(1)["applied"].as_u64(), slot(2)["stored"].as_u64(), slot(2)["collars"].as_u64()), (Some(2), Some(2), Some(2)));
    assert_eq!(st["acks"].as_array().unwrap().iter().find(|x| x["collar_id"] == json!(a)).unwrap()["version"], 2);
    // When the staged time comes, A's copy is what it enforces.
    let split = op_ingest::escapes::collar_boundaries(
        app.ctx.db(),
        &serde_json::from_value(app.call("GET", &format!("/api/collars/{a}"), None).await.1).unwrap(),
        at + Duration::seconds(1),
    )
    .await
    .unwrap();
    assert_eq!(split.active.unwrap().version, 5);
}

#[tokio::test]
async fn configs_follow_herd_url_moves_and_escapes() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let (id, key) = app.device(&herd).await;
    let device = |version: Option<u32>| {
        let mut d = v0_device();
        if let Some(v) = version {
            d["config_version"] = json!(v);
        }
        json!({"device": d})
    };
    let config_of = |v: &Value| -> Option<ConfigCommand> {
        let c = v.get("config")?;
        let cmd: ConfigCommand = serde_json::from_value(c.clone()).unwrap();
        // Signed and canonical, as the collar checks the bytes.
        op_protocol::verify_config_wire(c.to_string().as_bytes(), &app.ctx.public_key()).unwrap();
        Some(cmd)
    };
    // A legacy collar never gets one.
    assert!(app.report(&key, json!({})).await.get("config").is_none());
    let c1 = config_of(&app.report(&key, device(None)).await).expect("first config");
    assert_eq!((c1.version, c1.collar_id.as_str(), c1.herd_id.as_deref(), c1.endpoint.as_deref()), (1, id.as_str(), Some(herd.as_str()), None));
    assert_eq!((c1.report_s, c1.poll_s, c1.fast_until), (60, 60, None));
    assert!(c1.command_id.starts_with("cfg_"));
    assert!(config_of(&app.report(&key, device(Some(1))).await).is_none(), "current");

    // The public URL changes: a new endpoint, on the next report.
    app.ctx.update_settings(&json!({"server": {"public_url": "https://farm.example.com"}})).await.unwrap();
    let c2 = config_of(&app.report(&key, device(Some(1))).await).unwrap();
    assert_eq!((c2.version, c2.endpoint.as_deref()), (2, Some("https://farm.example.com/collar/v1")));

    // Moved to another herd: its config names that herd, at once.
    let herd_b = app.herd("Heifers").await;
    app.call("PATCH", &format!("/api/collars/{id}"), Some(json!({"herd_id": herd_b}))).await;
    assert_eq!(app.slots(&id).await["config"]["version"], 3);
    let c3 = config_of(&app.report(&key, device(Some(2))).await).unwrap();
    assert_eq!((c3.version, c3.herd_id.as_deref()), (3, Some(herd_b.as_str())));
    // …and it takes that herd's boundary, which a collar checks against the config's herd.
    app.send(&herd_b, polygon(rect(0.0, 0.0, 300.0, 200.0), vec![]), json!({})).await;
    let b = app.download(&key, "have=0&free=16").await.unwrap();
    let mut store = op_protocol::SlotStore::new(CollarLimits::V0, Some(herd.clone()), Some(id.clone()));
    store.set_herd(c3.herd_id.clone());
    assert_eq!(store.insert(b, Some(Utc::now())).status, op_protocol::AckStatus::Applied);

    // A move starts: fast until the estimated end plus 10 minutes.
    app.report(&key, json!({"device": v0_device(), "fixes": [fix_at(m_at(40.0, 30.0), Utc::now())]})).await;
    let (_, m) = app.send(&herd_b, polygon(rect(250.0, 150.0, 300.0, 200.0), vec![]), json!({})).await;
    assert_eq!(m["status"], "sweeping", "{m}");
    let c4 = config_of(&app.report(&key, device(Some(3))).await).unwrap();
    assert_eq!((c4.version, c4.fast_report_s, c4.fast_poll_s), (4, Some(10), Some(10)));
    let left = c4.fast_until.unwrap() - Utc::now();
    assert!(left > Duration::minutes(14) && left <= Duration::minutes(40), "{left}");
    assert_eq!(c4.cadence(Utc::now()), (10, 10));
    assert_eq!(c4.cadence(c4.fast_until.unwrap()), (60, 60), "back to base on its own");

    // A refused config is not sent again; the next change is.
    let mut refused = device(Some(3));
    refused["device"]["config_reject"] = json!({"version": 4, "code": "bad_config"});
    assert!(config_of(&app.report(&key, refused.clone()).await).is_none());
    assert!(config_of(&app.report(&key, device(Some(3))).await).is_none());
    assert_eq!(app.slots(&id).await["config"]["refused"], true);
    let (s, saved) = app.call("PUT", "/api/collars/config", Some(json!({"report_s": 120, "poll_s": 90, "fast_report_s": 15, "fast_poll_s": 15}))).await;
    assert_eq!(s, StatusCode::OK, "{saved}");
    let c5 = config_of(&app.report(&key, device(Some(3))).await).unwrap();
    assert_eq!((c5.version, c5.report_s, c5.poll_s, c5.fast_report_s), (5, 120, 90, Some(15)));
    assert_eq!(app.call("GET", "/api/collars/config", None).await.1, saved);
    let (s, _) = app.call("PUT", "/api/collars/config", Some(json!({"report_s": 5}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // A collar ahead of this server (a restored database) is overtaken.
    let c6 = config_of(&app.report(&key, device(Some(9))).await).unwrap();
    assert_eq!(c6.version, 10);
}

#[tokio::test]
async fn an_escape_alone_opens_a_fast_window() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let (id, key) = app.v0(&herd).await;
    let now = Utc::now();
    app.report(&key, json!({"fixes": [fix_at(m_at(150.0, 100.0), now)]})).await;
    app.send(&herd, polygon(rect(0.0, 0.0, 300.0, 200.0), vec![]), json!({})).await;
    let before = app.slots(&id).await["config"].clone();
    assert!(before.get("fast_until").is_none(), "{before}");
    app.report(&key, json!({"fixes": [fix_at(m_at(150.0, 230.0), now + Duration::seconds(5))]})).await;
    op_ingest::escapes::scan(&app.ctx, now + Duration::seconds(70)).await.unwrap();
    let after = app.slots(&id).await["config"].clone();
    assert_eq!(after["version"].as_u64(), Some(before["version"].as_u64().unwrap() + 1));
    assert!(after["fast_until"].is_string());
}

#[tokio::test]
async fn staged_boundaries_announce_only_when_they_take_effect() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let t0 = op_protocol::wire_time::trunc_secs(Utc::now());
    let at = |s: i64| op_protocol::wire_time::format(&(t0 + Duration::seconds(s)));
    app.send(&herd, polygon(rect(0.0, 0.0, 300.0, 200.0), vec![]), json!({"effective_at": at(60)})).await;
    // An immediate one overtakes it: v1 is dead before its time.
    app.send(&herd, polygon(rect(0.0, 0.0, 290.0, 200.0), vec![]), json!({})).await;
    app.send(&herd, polygon(rect(0.0, 0.0, 280.0, 200.0), vec![]), json!({"effective_at": at(120)})).await;
    let none = op_ingest::announce_activations(&app.ctx, t0, t0 + Duration::seconds(61)).await.unwrap();
    assert!(none.is_empty(), "{none:?}");
    let v3 = op_ingest::announce_activations(&app.ctx, t0 + Duration::seconds(100), t0 + Duration::seconds(121)).await.unwrap();
    assert_eq!(v3.iter().map(|b| b.version).collect::<Vec<_>>(), [3]);
    let split = op_ingest::herd_boundaries(app.ctx.db(), &herd, t0).await.unwrap();
    assert_eq!((split.active.map(|b| b.version), split.staged.iter().map(|b| b.version).collect::<Vec<_>>()), (Some(2), vec![3]));
}

#[tokio::test]
async fn report_path_queries_use_indexes() {
    let app = App::new().await;
    // (statement, binds, may sort its few rows in memory)
    let plans = [
        (op_ingest::sql::HERD_ACTIVE, 2, false),
        (op_ingest::sql::HERD_ABOVE, 2, false),
        (op_ingest::sql::OWN_COPIES, 3, false),
        (op_ingest::sql::OPEN_PEN, 2, false),
        (op_ingest::sql::REJECTED, 1, false),
        (op_ingest::sql::HELD, 1, false),
        (op_ingest::sql::NEXT_VERSION, 0, false),
        (op_ingest::sql::CONFIG, 1, false),
        // The activation watcher: a 2-second window of effective_at, then sorted.
        (op_ingest::sql::TAKING_EFFECT, 2, true),
    ];
    for (q, binds, may_sort) in plans {
        let sql = format!("EXPLAIN QUERY PLAN {q}");
        let mut query = sqlx::query(&sql);
        for _ in 0..binds {
            query = query.bind("x");
        }
        let rows = query.fetch_all(app.ctx.db()).await.unwrap();
        let details: Vec<String> = rows.iter().map(|r| sqlx::Row::get::<String, _>(r, "detail")).collect();
        for d in &details {
            assert!(!d.starts_with("SCAN ") || d.contains("USING"), "full scan in {q}: {details:?}");
            assert!(may_sort || !d.contains("TEMP B-TREE"), "sort without an index in {q}: {details:?}");
        }
        assert!(details.iter().any(|d| d.contains("USING")), "{q}: {details:?}");
    }
}

#[tokio::test]
async fn a_legacy_collar_is_sent_a_staged_boundary_only_when_it_has_room() {
    let app = App::new().await;
    let herd = app.herd("Cows").await;
    let (id, key) = app.device(&herd).await;
    app.send(&herd, polygon(rect(0.0, 0.0, 300.0, 200.0), vec![]), json!({})).await;
    let later = |m: i64| op_protocol::wire_time::format(&(Utc::now() + Duration::minutes(m)));
    app.send(&herd, polygon(rect(0.0, 0.0, 280.0, 200.0), vec![]), json!({"effective_at": later(2)})).await;
    app.send(&herd, polygon(rect(0.0, 0.0, 260.0, 200.0), vec![]), json!({"effective_at": later(4)})).await;
    let v1 = app.download(&key, "have=0").await.unwrap();
    app.ack(&key, &v1, "applied", None).await;
    let v2 = app.download(&key, "have=1").await.unwrap();
    app.ack(&key, &v2, "received", None).await;
    // Two slots, both held: v3 waits instead of being refused (firmware 0.1 would give no code,
    // and a refusal without a code is for good).
    assert!(app.download(&key, "have=2").await.is_none());
    // v2 took effect: room again.
    app.ack(&key, &v2, "applied", None).await;
    assert_eq!(app.download(&key, "have=2").await.unwrap().version, 3);
    assert_eq!(app.slots(&id).await["slots"].as_array().unwrap().len(), 1);
}
