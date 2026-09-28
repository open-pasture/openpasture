//! Ingest and the sweep driver at 250 collars (stream L): a report's fixes
//! and cues go in with multi-row INSERTs, backlogs of any size included; the
//! driver places each animal where its last few fixes agree, not where one
//! fix wandered; the WAL is cut back once a checkpoint catches up.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Via};
use op_geo::Projection;
use serde_json::{Value, json};
use tower::ServiceExt;

const MID: [f64; 2] = [-92.405, 38.125];

fn at(x: f64, y: f64) -> [f64; 2] {
    Projection::new(MID).inverse([x, y])
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
    herd: String,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let router = op_core::with_identity(op_core::router().merge(op_ingest::router()).with_state(ctx.clone()), Identity::owner(Via::Local));
        let mut app = Self { _dir: dir, ctx, router, herd: String::new() };
        app.ok("POST", "/api/farm", json!({"name": "Home", "timezone": "America/Chicago", "center": MID})).await;
        let ring = vec![at(0.0, 0.0), at(300.0, 0.0), at(300.0, 200.0), at(0.0, 200.0), at(0.0, 0.0)];
        let p = app.ok("POST", "/api/paddocks", json!({"name": "P1", "geometry": {"type": "Polygon", "coordinates": [ring]}})).await;
        let h = app.ok("POST", "/api/herds", json!({"name": "Cows", "species": "cattle", "count": 0, "paddock_id": p["id"]})).await;
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

    async fn collar(&self) -> (String, String) {
        let v = self.ok("POST", "/api/collars", json!({"herd_id": self.herd})).await;
        (v["collar"]["id"].as_str().unwrap().to_owned(), v["key"].as_str().unwrap().to_owned())
    }

    async fn report(&self, key: &str, body: Value) {
        let (s, v) = self.req("POST", "/collar/v1/report", Some(key), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
}

#[tokio::test]
async fn a_backlog_of_fixes_and_cues_is_stored_whole_and_in_order() {
    let app = App::new().await;
    let (collar, key) = app.collar().await;
    // A collar back after hours out of coverage: more fixes and cues than one INSERT takes.
    let t0 = Utc::now() - Duration::hours(3);
    let fixes: Vec<Value> = (0..150)
        .rev()
        .map(|i| json!({"at": ts(t0 + Duration::seconds(5 * i)), "point": at(10.0 + i as f64 * 0.5, 50.0), "accuracy_m": 2.5, "sats": 9}))
        .collect();
    let cues: Vec<Value> =
        (0..70).map(|i| json!({"at": ts(t0 + Duration::seconds(10 * i)), "kind": "warn", "level": 2, "margin_m": 1.5, "point": at(10.0, 50.0)})).collect();
    app.report(&key, json!({"fixes": fixes, "cues": cues})).await;
    let rows: Vec<(i64, f64)> =
        sqlx::query_as("SELECT t, accuracy_m FROM fixes WHERE collar_id = ? ORDER BY id").bind(&collar).fetch_all(app.ctx.db()).await.unwrap();
    assert_eq!(rows.len(), 150);
    assert!(rows.windows(2).all(|w| w[0].0 < w[1].0), "stored in time order");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cues WHERE collar_id = ?").bind(&collar).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 70);
    let paddocks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM fixes WHERE collar_id = ? AND paddock_id IS NOT NULL").bind(&collar).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(paddocks, 150, "each fix placed in its paddock");
    // The collar's latest position is the newest fix, whatever order they came in.
    let last = app.req("GET", &format!("/api/positions?herd_id={}", app.herd), None, None).await.1;
    let p: [f64; 2] = serde_json::from_value(last[0]["fix"]["point"].clone()).unwrap();
    assert!((Projection::new(MID).forward(p)[0] - (10.0 + 149.0 * 0.5)).abs() < 0.01);
}

#[tokio::test]
async fn the_driver_places_an_animal_where_its_last_fixes_agree() {
    let app = App::new().await;
    let (still, still_key) = app.collar().await;
    let (walker, walker_key) = app.collar().await;
    let now = Utc::now();
    // One collar's fixes scatter around (100, 100); the sharper ones count for more.
    app.report(
        &still_key,
        json!({"fixes": [
            {"at": ts(now - Duration::seconds(10)), "point": at(102.0, 100.0), "accuracy_m": 4.0, "sats": 7},
            {"at": ts(now - Duration::seconds(5)), "point": at(99.0, 101.0), "accuracy_m": 2.0, "sats": 10},
            {"at": ts(now), "point": at(100.0, 98.0), "accuracy_m": 2.0, "sats": 10}
        ]}),
    )
    .await;
    // The other walked 60 m between two fixes: its newest fix is where it is.
    app.report(
        &walker_key,
        json!({"fixes": [
            {"at": ts(now - Duration::seconds(5)), "point": at(20.0, 20.0), "accuracy_m": 2.0, "sats": 10},
            {"at": ts(now), "point": at(80.0, 20.0), "accuracy_m": 2.0, "sats": 10}
        ]}),
    )
    .await;
    let sit = op_ingest::prepare::situation(&app.ctx, &app.herd, now + Duration::seconds(1)).await.unwrap();
    let pos = |c: &str| Projection::new(MID).forward(sit.positions.iter().find(|(id, _)| id == c).unwrap().1);
    let p = pos(&still);
    // Weights 1/16, 1/4, 1/4: x = (102/16 + 99/4 + 100/4) / (9/16) = 99.78, y = (100/16 + 101/4 + 98/4) / (9/16) = 99.56.
    assert!((p[0] - 99.78).abs() < 0.05 && (p[1] - 99.56).abs() < 0.05, "{p:?}");
    assert!(sit.spread[&still] < 2.0, "three fixes are sharper than one: {}", sit.spread[&still]);
    let w = pos(&walker);
    assert!((w[0] - 80.0).abs() < 0.01 && (w[1] - 20.0).abs() < 0.01, "{w:?}");
    assert_eq!(sit.spread[&walker], 2.0);
}

#[tokio::test]
async fn the_wal_is_cut_back_after_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let limit: i64 = sqlx::query_scalar("PRAGMA journal_size_limit").fetch_one(ctx.db()).await.unwrap();
    assert_eq!(limit, op_core::store::JOURNAL_SIZE_LIMIT);
}
