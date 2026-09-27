//! Welfare record and training mode through the real routes (field-ready H):
//! a herd's training `warn_m` goes on the boundaries sent while training is
//! on, and off again after; and the same walk reported over
//! `/collar/v1/report` by a firmware 0.2 collar (with its episodes) and a
//! firmware 0.1 collar (without) gives the same episodes and ledger once the
//! server has rebuilt the second's from its own fence states.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, time};
use op_geo::{CollarLimits, Cue, CueConfig, Geofence, GeofenceConfig, LonLat, Polygon, Projection};
use serde_json::{Value, json};
use tower::ServiceExt;

const SW: LonLat = [-93.625, 42.03];

fn p1() -> Value {
    json!({"type": "Polygon", "coordinates": [[[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]]]})
}

struct T {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    app: Router,
}

impl T {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let app = Router::new().merge(op_core::router()).merge(op_ingest::router()).merge(op_analytics::router()).with_state(ctx.clone());
        let app = op_core::with_identity(app, op_core::Identity::owner(op_core::Via::Local));
        Self { _dir: dir, ctx, app }
    }

    async fn req(&self, method: &str, path: &str, body: Option<Value>, key: Option<&str>) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path).header("host", "127.0.0.1");
        if let Some(k) = key {
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

    async fn report(&self, key: &str, body: Value) {
        let (s, v) = self.req("POST", "/collar/v1/report", Some(body), Some(key)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    /// Farm, P1, herd Cows in P1 (no collars yet).
    async fn farm(&self) -> String {
        self.ok("POST", "/api/farm", Some(json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]}))).await;
        let p = self.ok("POST", "/api/paddocks", Some(json!({"name": "P1", "geometry": p1()}))).await;
        let h = self.ok("POST", "/api/herds", Some(json!({"name": "Cows", "species": "cattle", "count": 2, "paddock_id": p["id"]}))).await;
        h["id"].as_str().unwrap().to_owned()
    }

    async fn warn_after_send(&self, herd: &str, body: Value) -> f64 {
        self.ok("POST", &format!("/api/herds/{herd}/boundary"), Some(body)).await;
        self.ok("GET", &format!("/api/herds/{herd}/boundary"), None).await["active"]["warn_m"].as_f64().unwrap()
    }
}

#[tokio::test]
async fn training_warn_m_goes_on_boundaries_sent_while_it_is_on() {
    let t = T::new().await;
    let herd = t.farm().await;
    assert_eq!(t.warn_after_send(&herd, json!({"geometry": p1()})).await, 5.0, "the firmware default");
    t.ok("PUT", &format!("/api/welfare/training/{herd}"), Some(json!({"enabled": true, "warn_m": 12.0}))).await;
    assert_eq!(op_ingest::default_margins(&t.ctx, &herd).await.unwrap(), (12.0, 1.0));
    assert_eq!(t.warn_after_send(&herd, json!({"geometry": p1()})).await, 12.0, "training on");
    assert_eq!(t.warn_after_send(&herd, json!({"geometry": p1(), "warn_m": 8.0})).await, 8.0, "a margin the send names wins");
    t.ok("PUT", &format!("/api/welfare/training/{herd}"), Some(json!({"enabled": false}))).await;
    assert_eq!(t.warn_after_send(&herd, json!({"geometry": p1()})).await, 5.0, "training off");
    // The training warn is kept for next time, and other herds were never affected.
    assert_eq!(t.ok("GET", &format!("/api/welfare/training/{herd}"), None).await["warn_m"], 12.0);
    assert_eq!(op_ingest::default_margins(&t.ctx, "herd_other").await.unwrap(), (5.0, 1.0));
}

/// Metres from P1's south-west corner (P1 is about 413 × 400 m): up to the
/// north edge and back three times, a minute in the warning zone, across the
/// east edge and back, then five more times up and back.
fn walk() -> Vec<[f64; 2]> {
    let mut out = Vec::new();
    let mut at = [200.0, 200.0];
    let mut go = |to: [f64; 2], steps: usize, hold: usize| {
        let from = at;
        for k in 1..=steps {
            let f = k as f64 / steps as f64;
            at = [from[0] + (to[0] - from[0]) * f, from[1] + (to[1] - from[1]) * f];
            out.push(at);
        }
        out.extend(std::iter::repeat_n(at, hold));
    };
    let north = 400.3 - 3.5;
    for _ in 0..3 {
        go([200.0, north], 20, 3);
        go([200.0, 385.0], 4, 3);
    }
    go([200.0, 397.0], 6, 60);
    go([200.0, 385.0], 4, 3);
    go([420.0, 380.0], 30, 15);
    go([380.0, 300.0], 8, 3);
    for _ in 0..5 {
        go([200.0, north], 20, 2);
        go([200.0, 385.0], 4, 2);
    }
    out
}

#[tokio::test]
async fn a_firmware_0_1_collar_gets_the_episodes_a_0_2_collar_reports() {
    let t = T::new().await;
    let herd = t.farm().await;
    let mut collars = Vec::new();
    for tag in ["214", "031"] {
        let c = t.ok("POST", "/api/collars", Some(json!({"herd_id": herd, "name": tag}))).await;
        let a = t.ok("POST", "/api/animals", Some(json!({"tag": tag, "herd_id": herd, "collar_id": c["collar"]["id"]}))).await;
        collars.push((c["key"].as_str().unwrap().to_owned(), a["id"].as_str().unwrap().to_owned()));
    }
    let v02 = json!({"fw": "0.2.0", "caps": ["holes", "slots", "collar_id", "cue_mode", "episodes", "config"], "limits": CollarLimits::V0});
    t.report(&collars[0].0, json!({"device": v02})).await;
    t.ok("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": p1()}))).await;
    let status = t.ok("GET", &format!("/api/herds/{herd}/boundary"), None).await;
    let version = status["active"]["version"].as_u64().unwrap() as u32;
    let geometry: Polygon = serde_json::from_value(status["active"]["geometry"].clone()).unwrap();

    // The collar's own fence and cue policy over the walk, a fix a second.
    let mut fence = Geofence::from_polygon(GeofenceConfig::default(), &geometry, version, &CollarLimits::V0).unwrap();
    let mut cue = Cue::new(CueConfig::default());
    let proj = Projection::new(SW);
    let t0 = (time::now() - chrono::Duration::minutes(20)).timestamp() * 1000;
    let (mut fixes, mut cues_02, mut cues_01, mut eps) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (i, xy) in walk().into_iter().enumerate() {
        let ms = t0 + i as i64 * 1000;
        let at = time::to_db(&time::from_unix_ms(ms));
        let point = proj.offset(xy[0], xy[1]);
        let r = fence.update(point, 2.0);
        let c = cue.update(&r, 5.0, ms);
        fixes.push((ms, json!({"at": at, "point": point, "accuracy_m": 2.0, "sats": 9})));
        if c.active {
            let margin = (r.margin_m * 10.0).round() / 10.0;
            let kind = c.kind.unwrap().as_str();
            cues_02.push((
                ms,
                json!({"at": at, "kind": kind, "level": c.volume, "dur_ms": 300, "margin_m": margin, "ring": 0, "boundary_version": version, "point": point}),
            ));
            cues_01.push((ms, json!({"at": at, "level": c.volume, "margin_m": margin, "point": point})));
        }
        for e in cue.take_episodes() {
            let at = |ms: i64| time::to_db(&time::from_unix_ms(ms));
            eps.push((
                e.end,
                json!({"start": at(e.start), "end": at(e.end), "boundary_version": version, "ring": e.ring, "cues": e.cues, "max_level": e.max_level,
                       "min_margin_m": e.min_margin_m, "outcome": e.outcome.as_str()}),
            ));
        }
    }
    let outcomes: Vec<&str> = eps.iter().map(|e| e.1["outcome"].as_str().unwrap()).collect();
    for o in ["turned_back", "crossed", "rest"] {
        assert!(outcomes.contains(&o), "{outcomes:?}");
    }
    // Reports of a minute each, as the collars send them.
    let n = fixes.len();
    let pick = |v: &[(i64, Value)], a: i64, z: i64| v.iter().filter(|x| x.0 >= a && x.0 <= z).map(|x| x.1.clone()).collect::<Vec<_>>();
    for start in (0..n).step_by(60) {
        let end = (start + 60).min(n);
        let (a, z) = (fixes[start].0, fixes[end - 1].0);
        let part = pick(&fixes, a, z);
        let body = json!({"boundary_version": version, "fixes": part, "cues": pick(&cues_02, a, z), "episodes": pick(&eps, a, z), "battery": 0.9});
        t.report(&collars[0].0, body).await;
        t.report(&collars[1].0, json!({"boundary_version": version, "fixes": part, "cues": pick(&cues_01, a, z), "battery": 0.9})).await;
    }
    let pass = op_analytics::welfare::derive(&t.ctx, time::now().timestamp_millis()).await.unwrap();
    assert_eq!((pass.collars, pass.episodes, pass.waiting), (1, eps.len(), 0), "{pass:?}");

    let a = t.ok("GET", &format!("/api/welfare/animals/{}/cues", collars[0].1), None).await;
    let b = t.ok("GET", &format!("/api/welfare/animals/{}/cues", collars[1].1), None).await;
    let (ea, eb) = (a["episodes"].as_array().unwrap(), b["episodes"].as_array().unwrap());
    assert_eq!((ea.len(), eb.len()), (eps.len(), eps.len()));
    for (x, y) in ea.iter().zip(eb) {
        for k in ["start", "end", "outcome", "cues", "max_level"] {
            assert_eq!(x[k], y[k], "{k}: {x} vs {y}");
        }
        assert!((x["min_margin_m"].as_f64().unwrap() - y["min_margin_m"].as_f64().unwrap()).abs() < 0.01, "{x} vs {y}");
    }
    let ledger = |v: &Value| v["cues"].as_array().unwrap().iter().map(|c| (c["at"].clone(), c["kind"].clone(), c["outcome"].clone())).collect::<Vec<_>>();
    assert_eq!(ledger(&a), ledger(&b));
    assert_eq!(ledger(&a).len(), cues_02.len());
    assert_eq!(a["learning"]["status"], "trained");
    assert_eq!(a["learning"]["since"], b["learning"]["since"]);
    assert_eq!(b["learning"]["derived"], true);
    let herd_view = t.ok("GET", &format!("/api/welfare/animals?herd_id={herd}"), None).await;
    assert_eq!((herd_view["trained"].as_u64(), herd_view["head"].as_u64()), (Some(2), Some(2)));
}
