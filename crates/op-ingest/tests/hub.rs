//! HUB changes to the collar endpoints: `collars.outside_since`, parked
//! collars, `collar_boundary_state`, and the collar PATCH leaving the new
//! reported fields alone.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, FenceState, Polygon};
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let router = op_core::router().merge(op_ingest::router()).with_state(ctx.clone());
        Self { _dir: dir, ctx, router }
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

    /// Farm, paddock, herd with the paddock as its boundary, one collar.
    /// Returns (herd id, collar id, collar key).
    async fn setup(&self) -> (String, String, String) {
        let (s, _) = self.req("POST", "/api/farm", None, Some(json!({"name": "Home", "timezone": "UTC", "center": [-92.405, 38.125]}))).await;
        assert_eq!(s, StatusCode::CREATED);
        let (_, pad) = self.req("POST", "/api/paddocks", None, Some(json!({"name": "North", "geometry": square()}))).await;
        let (_, herd) = self.req("POST", "/api/herds", None, Some(json!({"name": "Cows", "species": "cattle", "count": 1, "paddock_id": pad["id"]}))).await;
        let herd = herd["id"].as_str().unwrap().to_owned();
        let geometry: Polygon = serde_json::from_value(square()).unwrap();
        op_ingest::send_boundary(&self.ctx, &herd, geometry, Default::default(), "dec_test").await.unwrap();
        let (_, c) = self.req("POST", "/api/collars", None, Some(json!({"herd_id": herd}))).await;
        (herd, c["collar"]["id"].as_str().unwrap().to_owned(), c["key"].as_str().unwrap().to_owned())
    }

    async fn collar(&self, id: &str) -> op_core::Collar {
        let (s, v) = self.req("GET", &format!("/api/collars/{id}"), None, None).await;
        assert_eq!(s, StatusCode::OK);
        serde_json::from_value(v).unwrap()
    }

    async fn report(&self, key: &str, fixes: &[(&str, [f64; 2])]) -> Value {
        let fixes: Vec<Value> = fixes.iter().map(|(at, p)| json!({"at": at, "point": p, "accuracy_m": 2.0, "sats": 9})).collect();
        let (s, v) = self.req("POST", "/collar/v1/report", Some(key), Some(json!({"fixes": fixes, "battery": 0.7}))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    async fn count(&self, table: &str, collar: &str) -> i64 {
        let (n,): (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table} WHERE collar_id = ?")).bind(collar).fetch_one(self.ctx.db()).await.unwrap();
        n
    }
}

/// About 88 m x 111 m.
fn square() -> Value {
    let (w, s, e, n) = (-92.4055, 38.1245, -92.4045, 38.1255);
    json!({"type": "Polygon", "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]]})
}

const IN: [f64; 2] = [-92.405, 38.125];
const OUT: [f64; 2] = [-92.4030, 38.125];

fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
    op_core::time::from_db(s).unwrap()
}

#[tokio::test]
async fn outside_since_follows_the_fixes() {
    let app = App::new().await;
    let (_, collar, key) = app.setup().await;

    app.report(&key, &[("2026-09-25T10:00:00Z", IN)]).await;
    let c = app.collar(&collar).await;
    assert_eq!((c.state, c.outside_since), (FenceState::Inside, None));

    // Out at the second fix of a report: since that fix.
    app.report(&key, &[("2026-09-25T10:01:00Z", IN), ("2026-09-25T10:02:00Z", OUT), ("2026-09-25T10:03:00Z", OUT)]).await;
    let c = app.collar(&collar).await;
    assert_eq!(c.state, FenceState::Outside);
    assert_eq!(c.outside_since, Some(at("2026-09-25T10:02:00Z")));
    // Still out: unchanged. A late fix changes nothing either.
    app.report(&key, &[("2026-09-25T10:05:00Z", OUT)]).await;
    app.report(&key, &[("2026-09-25T09:59:00Z", IN)]).await;
    assert_eq!(app.collar(&collar).await.outside_since, Some(at("2026-09-25T10:02:00Z")));
    // A report without fixes keeps it.
    app.report(&key, &[]).await;
    assert_eq!(app.collar(&collar).await.outside_since, Some(at("2026-09-25T10:02:00Z")));

    // Back in (warning or inside): cleared.
    app.report(&key, &[("2026-09-25T10:06:00Z", IN)]).await;
    let c = app.collar(&collar).await;
    assert_eq!((c.state, c.outside_since), (FenceState::Inside, None));
    // Out and back within one report: cleared too.
    app.report(&key, &[("2026-09-25T10:07:00Z", OUT), ("2026-09-25T10:08:00Z", IN)]).await;
    assert_eq!(app.collar(&collar).await.outside_since, None);
    app.report(&key, &[("2026-09-25T10:09:00Z", OUT)]).await;
    assert_eq!(app.collar(&collar).await.outside_since, Some(at("2026-09-25T10:09:00Z")));

    // Moving the collar to another herd starts it over.
    let (_, other) = app.req("POST", "/api/herds", None, Some(json!({"name": "Heifers", "species": "cattle", "count": 1}))).await;
    let (s, v) = app.req("PATCH", &format!("/api/collars/{collar}"), None, Some(json!({"herd_id": other["id"]}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v.get("outside_since").is_none());
}

#[tokio::test]
async fn a_parked_collar_report_keeps_only_battery_and_health() {
    let app = App::new().await;
    let (_, collar, key) = app.setup().await;
    app.report(&key, &[("2026-09-25T10:00:00Z", IN)]).await;
    let before = app.collar(&collar).await;
    let (fixes, health) = (app.count("fixes", &collar).await, app.count("health", &collar).await);

    sqlx::query("UPDATE collars SET parked_at = ?, parked_reason = 'shelf' WHERE id = ?")
        .bind(op_core::time::to_db(&op_core::time::now()))
        .bind(&collar)
        .execute(app.ctx.db())
        .await
        .unwrap();
    let body = json!({
        "fixes": [{"at": "2026-09-25T11:00:00Z", "point": OUT, "accuracy_m": 2.0, "sats": 9}],
        "cues": [{"at": "2026-09-25T11:00:00Z", "level": 3, "margin_m": -4.0}],
        "battery": 0.42
    });
    let (s, v) = app.req("POST", "/collar/v1/report", Some(&key), Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["latest_version"].is_u64(), "{v}");

    let after = app.collar(&collar).await;
    assert_eq!(app.count("fixes", &collar).await, fixes, "no fixes stored");
    assert_eq!(app.count("cues", &collar).await, 0, "no cues stored");
    assert_eq!(app.count("health", &collar).await, health + 1);
    assert_eq!(after.battery, Some(0.42));
    assert!(after.last_seen > before.last_seen);
    assert_eq!((after.state, after.last_fix, after.outside_since), (before.state, before.last_fix, None), "no fence work");
    assert_eq!(after.parked_reason, Some(op_core::ParkReason::Shelf));
    let escapes: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM escapes").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(escapes.0, 0);
}

#[tokio::test]
async fn acks_keep_collar_boundary_state_current() {
    let app = App::new().await;
    let (herd, collar, key) = app.setup().await;
    let geometry: Polygon = serde_json::from_value(square()).unwrap();
    let v2 = op_ingest::send_boundary(&app.ctx, &herd, geometry, Default::default(), "dec_test2").await.unwrap();
    let v1_id: (String,) = sqlx::query_as("SELECT id FROM boundaries WHERE version = ?").bind(v2.version as i64 - 1).fetch_one(app.ctx.db()).await.unwrap();

    let ack = |id: &str, version: u32, status: &str, at: &str| json!({"command_id": id, "version": version, "status": status, "at": at});
    let state = || async {
        sqlx::query_as::<_, (String, Option<String>, i64, String, String, String)>(
            "SELECT collar_id, herd_id, version, status, command_id, at FROM collar_boundary_state WHERE collar_id = ?",
        )
        .bind(&collar)
        .fetch_optional(app.ctx.db())
        .await
        .unwrap()
    };
    assert!(state().await.is_none());

    let (s, _) = app.req("POST", "/collar/v1/ack", Some(&key), Some(ack(&v1_id.0, v2.version - 1, "applied", "2026-09-25T10:00:00Z"))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let st = state().await.unwrap();
    assert_eq!((st.1.as_deref(), st.2, st.3.as_str(), st.4.as_str()), (Some(herd.as_str()), v2.version as i64 - 1, "applied", v1_id.0.as_str()));

    app.req("POST", "/collar/v1/ack", Some(&key), Some(ack(&v2.id, v2.version, "received", "2026-09-25T10:01:00Z"))).await;
    assert_eq!(state().await.unwrap().3, "received");
    app.req("POST", "/collar/v1/ack", Some(&key), Some(ack(&v2.id, v2.version, "applied", "2026-09-25T10:02:00Z"))).await;
    let st = state().await.unwrap();
    assert_eq!((st.2, st.3.as_str(), st.5.as_str()), (v2.version as i64, "applied", "2026-09-25T10:02:00.000Z"));

    // A late ack for an older version doesn't take it back.
    app.req("POST", "/collar/v1/ack", Some(&key), Some(ack(&v1_id.0, v2.version - 1, "rejected", "2026-09-25T10:03:00Z"))).await;
    let st = state().await.unwrap();
    assert_eq!((st.2, st.3.as_str()), (v2.version as i64, "applied"));
    assert_eq!(app.collar(&collar).await.boundary_version, Some(v2.version));
}

#[tokio::test]
async fn collar_patch_leaves_reported_fields_alone() {
    let app = App::new().await;
    let (_, collar, _) = app.setup().await;
    sqlx::query("UPDATE collars SET fw = '0.2.0', caps = '[\"holes\"]' WHERE id = ?").bind(&collar).execute(app.ctx.db()).await.unwrap();
    let (s, v) = app
        .req(
            "PATCH",
            &format!("/api/collars/{collar}"),
            None,
            Some(json!({"name": "Bench", "fw": "9.9", "caps": [], "parked_at": "2026-09-25T10:00:00Z", "parked_reason": "repair", "outside_since": "2026-09-25T10:00:00Z"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "Bench");
    assert_eq!(v["fw"], "0.2.0");
    assert_eq!(v["caps"], json!(["holes"]));
    assert!(v.get("parked_at").is_none() && v.get("parked_reason").is_none() && v.get("outside_since").is_none());
}
