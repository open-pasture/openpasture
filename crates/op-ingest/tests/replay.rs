//! A report sent twice (the reply was lost and the collar retried) changes
//! nothing the second time: no new fixes, cues, episodes or cue events, and
//! the report counts in `health` add up to each fix and cue once.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, SecondsFormat, Utc};
use http_body_util::BodyExt;
use op_core::{Ctx, Event};
use serde_json::{Value, json};
use tower::ServiceExt;

const MID: [f64; 2] = [-92.405, 38.125];

fn at(dx: f64, dy: f64) -> [f64; 2] {
    op_geo::Projection::new(MID).inverse([dx, dy])
}

async fn call(router: &axum::Router, method: &str, path: &str, key: Option<&str>, body: Value) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json");
    if let Some(k) = key {
        req = req.header("authorization", format!("Bearer {k}"));
    }
    let res = router.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn count(ctx: &Ctx, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(ctx.db()).await.unwrap()
}

#[tokio::test]
async fn a_resent_report_stores_and_announces_nothing_new() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let router = op_core::router().merge(op_ingest::router()).with_state(ctx.clone());
    let square = json!({"type": "Polygon", "coordinates": [[at(0.0, 0.0), at(300.0, 0.0), at(300.0, 200.0), at(0.0, 200.0), at(0.0, 0.0)]]});
    let (s, _) = call(&router, "POST", "/api/farm", None, json!({"name": "Home", "timezone": "America/Chicago", "center": MID})).await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, pad) = call(&router, "POST", "/api/paddocks", None, json!({"name": "North", "geometry": square})).await;
    let (_, herd) = call(&router, "POST", "/api/herds", None, json!({"name": "Cows", "species": "cattle", "count": 1, "paddock_id": pad["id"]})).await;
    let (s, linked) = call(&router, "POST", "/api/collars", None, json!({"herd_id": herd["id"]})).await;
    assert_eq!(s, StatusCode::CREATED, "{linked}");
    let key = linked["key"].as_str().unwrap().to_owned();

    let now = Utc::now();
    let ts = |d: i64| (now - Duration::seconds(d)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let report = json!({
        "fixes": [
            {"at": ts(20), "point": at(150.0, 50.0), "accuracy_m": 2.0, "sats": 9},
            {"at": ts(10), "point": at(150.0, 60.0), "accuracy_m": 2.0, "sats": 9}
        ],
        // A warn and an outside cue at the same instant are two cues.
        "cues": [
            {"at": ts(10), "kind": "warn", "level": 1, "margin_m": 1.0, "ring": 1},
            {"at": ts(10), "kind": "outside", "level": 3, "margin_m": -1.0}
        ],
        "episodes": [{"start": ts(30), "end": ts(10), "ring": 1, "cues": 2, "max_level": 3, "min_margin_m": -1.0, "outcome": "crossed"}],
        "battery": 0.8
    });

    let mut rx = ctx.subscribe();
    let (s, v) = call(&router, "POST", "/collar/v1/report", Some(&key), report.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let cue_events = |rx: &mut tokio::sync::broadcast::Receiver<Event>| {
        let mut n = 0;
        while let Ok(e) = rx.try_recv() {
            n += matches!(e, Event::Cue { .. }) as usize;
        }
        n
    };
    assert_eq!(cue_events(&mut rx), 2);
    let collar_after_first: String = sqlx::query_scalar("SELECT last_fix FROM collars").fetch_one(ctx.db()).await.unwrap();

    // The same report again, and once more with one new fix.
    let (s, v) = call(&router, "POST", "/collar/v1/report", Some(&key), report.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(cue_events(&mut rx), 0, "a resent cue is not announced again");
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM fixes").await, 2);
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM cues").await, 2);
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM episodes").await, 1);
    let collar_now: String = sqlx::query_scalar("SELECT last_fix FROM collars").fetch_one(ctx.db()).await.unwrap();
    assert_eq!(collar_now, collar_after_first);

    let mut more = report.clone();
    more["fixes"].as_array_mut().unwrap().push(json!({"at": ts(0), "point": at(150.0, 70.0), "accuracy_m": 2.0, "sats": 9}));
    let (s, _) = call(&router, "POST", "/collar/v1/report", Some(&key), more).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM fixes").await, 3);
    assert_eq!(cue_events(&mut rx), 0);

    // Three reports arrived; together they hold three fixes and two cues.
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM health").await, 3);
    assert_eq!(count(&ctx, "SELECT SUM(fixes) FROM health").await, 3);
    assert_eq!(count(&ctx, "SELECT SUM(cues) FROM health").await, 2);
}

/// A collar linked to a herd in a paddock: its data dir, Ctx, router and key.
async fn linked_collar() -> (tempfile::TempDir, Ctx, axum::Router, String) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let router = op_core::router().merge(op_ingest::router()).with_state(ctx.clone());
    let square = json!({"type": "Polygon", "coordinates": [[at(0.0, 0.0), at(300.0, 0.0), at(300.0, 200.0), at(0.0, 200.0), at(0.0, 0.0)]]});
    call(&router, "POST", "/api/farm", None, json!({"name": "Home", "timezone": "America/Chicago", "center": MID})).await;
    let (_, pad) = call(&router, "POST", "/api/paddocks", None, json!({"name": "North", "geometry": square})).await;
    let (_, herd) = call(&router, "POST", "/api/herds", None, json!({"name": "Cows", "species": "cattle", "count": 1, "paddock_id": pad["id"]})).await;
    let (s, linked) = call(&router, "POST", "/api/collars", None, json!({"herd_id": herd["id"]})).await;
    assert_eq!(s, StatusCode::CREATED, "{linked}");
    let key = linked["key"].as_str().unwrap().to_owned();
    (dir, ctx, router, key)
}

#[tokio::test]
async fn a_different_record_at_an_instant_already_held_is_kept_and_counted() {
    let (_dir, ctx, router, key) = linked_collar().await;
    let now = Utc::now();
    let ts = |d: i64| (now - Duration::seconds(d)).to_rfc3339_opts(SecondsFormat::Millis, true);
    let first = json!({
        "fixes": [{"at": ts(10), "point": at(150.0, 50.0), "accuracy_m": 2.0, "sats": 9}],
        "cues": [{"at": ts(10), "kind": "warn", "level": 1, "margin_m": 1.0}],
    });
    let (s, v) = call(&router, "POST", "/collar/v1/report", Some(&key), first).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // The same instants with other contents (a clock step, a truncated time):
    // both kept, both counted, the new cue announced.
    let mut rx = ctx.subscribe();
    let second = json!({
        "fixes": [{"at": ts(10), "point": at(150.0, 80.0), "accuracy_m": 2.0, "sats": 9}],
        "cues": [{"at": ts(10), "kind": "warn", "level": 2, "margin_m": 0.5}],
    });
    let (s, v) = call(&router, "POST", "/collar/v1/report", Some(&key), second.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM fixes").await, 2);
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM cues").await, 2);
    let mut levels = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let Event::Cue { level, .. } = e {
            levels.push(level);
        }
    }
    assert_eq!(levels, [2]);
    assert_eq!(count(&ctx, "SELECT IFNULL(SUM(conflicts), 0) FROM health").await, 2);

    // That report resent is a retransmission: nothing new, no conflict.
    let (s, _) = call(&router, "POST", "/collar/v1/report", Some(&key), second).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM fixes").await, 2);
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM cues").await, 2);
    assert_eq!(count(&ctx, "SELECT IFNULL(SUM(conflicts), 0) FROM health").await, 2);
}
