use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::Ctx;
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
        let value =
            if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into())) };
        (status, value)
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.req(method, path, None, body).await
    }

    /// Farm, a 100 x 100 m-ish paddock, and a herd in it. Returns the herd id.
    async fn herd(&self) -> String {
        let (s, _) = self.call("POST", "/api/farm", Some(json!({"name": "Home", "timezone": "UTC", "center": [-92.405, 38.125]}))).await;
        assert_eq!(s, StatusCode::CREATED);
        let (s, pad) = self.call("POST", "/api/paddocks", Some(json!({"name": "North", "geometry": square(0.0)}))).await;
        assert_eq!(s, StatusCode::CREATED);
        let (s, herd) = self.call("POST", "/api/herds", Some(json!({"name": "Cows", "species": "cattle", "count": 0, "paddock_id": pad["id"]}))).await;
        assert_eq!(s, StatusCode::CREATED);
        herd["id"].as_str().unwrap().to_owned()
    }

    /// The boundary a move sent last (its decision's `boundary_id`).
    async fn sent(&self, mv: &Value) -> Value {
        let row = sqlx::query("SELECT b.* FROM boundaries b JOIN decisions d ON d.boundary_id = b.id WHERE d.id = ?")
            .bind(mv["decision_id"].as_str().unwrap())
            .fetch_one(self.ctx.db())
            .await
            .unwrap();
        serde_json::to_value(op_core::store::boundary_from_row(&row).unwrap()).unwrap()
    }

    async fn device(&self, herd: &str) -> (String, String) {
        let (s, v) = self.call("POST", "/api/collars", Some(json!({"herd_id": herd}))).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        (v["collar"]["id"].as_str().unwrap().to_owned(), v["key"].as_str().unwrap().to_owned())
    }
}

/// About 88 m x 111 m, shifted east by `dx` degrees.
fn square(dx: f64) -> Value {
    let (w, s, e, n) = (-92.4055 + dx, 38.1245, -92.4045 + dx, 38.1255);
    json!({"type": "Polygon", "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]]})
}

fn report(point: [f64; 2]) -> Value {
    json!({"fixes": [{"at": "2026-09-25T10:40:00Z", "point": point, "accuracy_m": 2.0, "sats": 9}], "battery": 0.8})
}

#[tokio::test]
async fn linking_and_device_auth() {
    let app = App::new().await;
    let herd = app.herd().await;

    let (s, v) = app.call("POST", "/api/collars", Some(json!({"herd_id": herd, "name": "Bench"}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let key = v["key"].as_str().unwrap().to_owned();
    let id = v["collar"]["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("col_"));
    assert_eq!(v["collar"]["name"], "Bench");
    assert_eq!(v["collar"]["state"], "unknown");
    assert!(v["endpoint"].as_str().unwrap().ends_with("/collar/v1"));
    assert_eq!(v["public_key"], app.ctx.public_key_b64());
    // Only the hash is stored.
    let (stored,): (String,) = sqlx::query_as("SELECT key_hash FROM collars WHERE id = ?").bind(&id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(stored, op_core::keys::hash_key(&key));
    assert_ne!(stored, key);

    // Default names, and a key per collar.
    let (_, v) = app.call("POST", "/api/collars", Some(json!({"herd_id": herd}))).await;
    assert_eq!(v["collar"]["name"], "Collar 2");
    assert_ne!(v["key"].as_str().unwrap(), key);
    assert!(v["collar"].get("key_hash").is_none());
    let (s, _) = app.call("POST", "/api/collars", Some(json!({"herd_id": "herd_nope"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let body = report([-92.405, 38.125]);
    let (s, v) = app.req("POST", "/collar/v1/report", None, Some(body.clone())).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert!(v["error"].is_string());
    let (s, _) = app.req("POST", "/collar/v1/report", Some("not-a-key"), Some(body.clone())).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = app.req("GET", "/collar/v1/boundary", Some("not-a-key"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, v) = app.req("POST", "/collar/v1/report", Some(&key), Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({"latest_version": null}));

    let (_, c) = app.call("GET", &format!("/api/collars/{id}"), None).await;
    assert_eq!(c["battery"], 0.8);
    assert!(c["last_seen"].is_string());
    assert_eq!(c["last_fix"]["sats"], 9);
    // No boundary yet.
    assert_eq!(c["state"], "unknown");

    // Deleted collars lose access.
    let (s, _) = app.call("DELETE", &format!("/api/collars/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(report([-92.405, 38.125]))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reports_are_validated() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;

    let mut bad = report([-92.405, 38.125]);
    bad["battery"] = json!(1.5);
    let (s, v) = app.req("POST", "/collar/v1/report", Some(&key), Some(bad)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("battery"));

    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(report([-200.0, 38.125]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let mut neg = report([-92.405, 38.125]);
    neg["fixes"][0]["accuracy_m"] = json!(-1);
    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(neg)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, v) = app.req("POST", "/collar/v1/report", Some(&key), Some(json!({"fixes": "nope"}))).await;
    assert!(s.is_client_error());
    assert!(v["error"].is_string());

    let mut future = report([-92.405, 38.125]);
    future["fixes"][0]["at"] = json!("2099-01-01T00:00:00Z");
    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(future)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let mut other = report([-92.405, 38.125]);
    other["collar_id"] = json!("col_someone_else");
    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(other)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let mut mine = report([-92.405, 38.125]);
    mine["collar_id"] = json!(id);
    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(mine)).await;
    assert_eq!(s, StatusCode::OK);

    // Nothing from the refused reports was stored.
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fixes").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn versions_staging_and_signatures() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (_, key) = app.device(&herd).await;

    let (s, v) = app.call("GET", &format!("/api/herds/{herd}/boundary"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({"acks": []}));
    let (s, _) = app.req("GET", "/collar/v1/boundary?have=0", Some(&key), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Bad shapes are refused before anything is recorded.
    let bowtie = json!({"type": "Polygon", "coordinates": [[[-92.41, 38.12], [-92.40, 38.13], [-92.40, 38.12], [-92.41, 38.13], [-92.41, 38.12]]]});
    let (s, _) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": bowtie}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": square(0.0), "warn_m": -3}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM decisions").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 0);

    let (s, m1) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": square(0.0), "warn_m": 8}))).await;
    assert_eq!(s, StatusCode::CREATED, "{m1}");
    // No tracked animals: the target goes out directly and the move is done.
    assert_eq!(m1["status"], "done");
    assert_eq!(m1["step"], 1);
    assert_eq!(m1["target"], square(0.0));
    let b1 = app.sent(&m1).await;
    assert_eq!(b1["version"], 1);
    assert_eq!(b1["warn_m"], 8.0);
    assert_eq!(b1["hysteresis_m"], 1.0);
    let decision_id = b1["decision_id"].as_str().unwrap();
    let (source, status, boundary_id): (String, String, String) =
        sqlx::query_as("SELECT source, status, boundary_id FROM decisions WHERE id = ?").bind(decision_id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!((source.as_str(), status.as_str(), boundary_id.as_str()), ("farmer", "applied", b1["id"].as_str().unwrap()));

    let later = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let (_, m2) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": square(0.0005), "effective_at": later}))).await;
    assert_eq!(app.sent(&m2).await["version"], 2);

    let (_, st) = app.call("GET", &format!("/api/herds/{herd}/boundary"), None).await;
    assert_eq!(st["active"]["version"], 1);
    assert_eq!(st["pending"]["version"], 2);

    // The collar downloads v1, signed by the server key.
    let (s, cmd) = app.req("GET", "/collar/v1/boundary?have=0", Some(&key), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cmd["version"], 1);
    assert_eq!(cmd["command_id"], b1["id"]);
    assert_eq!(cmd["warn_m"], 8.0);
    assert_eq!(cmd["boundary"].as_array().unwrap().len(), 4, "one unclosed ring");
    assert_eq!(cmd["herd_id"], json!(herd), "signed for this herd");
    op_protocol::verify_json(&cmd, &app.ctx.public_key()).unwrap();
    let parsed: op_protocol::BoundaryCommand = serde_json::from_value(cmd.clone()).unwrap();
    op_protocol::verify_command(&parsed, &app.ctx.public_key()).unwrap();
    parsed.validate(None).unwrap();
    let mut tampered = cmd.clone();
    tampered["version"] = json!(9);
    assert!(op_protocol::verify_json(&tampered, &app.ctx.public_key()).is_err());
    let other = op_protocol::generate_signing_key();
    assert!(op_protocol::verify_json(&cmd, &other.verifying_key()).is_err());

    // Then the staged v2, then nothing.
    let (_, cmd2) = app.req("GET", "/collar/v1/boundary?have=1", Some(&key), None).await;
    assert_eq!(cmd2["version"], 2);
    assert!(cmd2["effective_at"].as_str().unwrap().ends_with('Z'));
    op_protocol::verify_json(&cmd2, &app.ctx.public_key()).unwrap();
    let (s, _) = app.req("GET", "/collar/v1/boundary?have=2", Some(&key), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Versions only go up, also through the library call and across herds.
    let b3 = op_ingest::send_boundary(&app.ctx, &herd, serde_json::from_value(square(0.001)).unwrap(), Default::default(), "dec_x").await.unwrap();
    assert_eq!(b3.version, 3);
    let (_, st) = app.call("GET", &format!("/api/herds/{herd}/boundary"), None).await;
    assert_eq!(st["active"]["version"], 3, "v3 is in effect now, v2 is overtaken");
    assert!(st.get("pending").is_none());
    let (_, cmd) = app.req("GET", "/collar/v1/boundary?have=1", Some(&key), None).await;
    assert_eq!(cmd["version"], 3);

    let (_, h2) = app.call("POST", "/api/herds", Some(json!({"name": "Heifers", "species": "cattle", "count": 0}))).await;
    let other_herd = h2["id"].as_str().unwrap();
    let b = op_ingest::send_boundary(&app.ctx, other_herd, serde_json::from_value(square(0.0)).unwrap(), Default::default(), "dec_y").await.unwrap();
    assert_eq!(b.version, 4, "one sequence across herds");
    let (_, st) = app.call("GET", &format!("/api/herds/{herd}/boundary"), None).await;
    assert_eq!(st["active"]["version"], 3, "each herd keeps its own latest");
    assert!(op_ingest::send_boundary(&app.ctx, "herd_nope", serde_json::from_value(square(0.0)).unwrap(), Default::default(), "dec_z").await.is_err());

    // Report response points at the newest version.
    let (_, v) = app.req("POST", "/collar/v1/report", Some(&key), Some(report([-92.405, 38.125]))).await;
    assert_eq!(v["latest_version"], 3);
}

#[tokio::test]
async fn acks_aggregate_per_collar() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (a_id, a_key) = app.device(&herd).await;
    let (b_id, b_key) = app.device(&herd).await;

    let (_, m1) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": square(0.0)}))).await;
    let b1 = app.sent(&m1).await;
    let (_, m2) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": square(0.0002)}))).await;
    let b2 = app.sent(&m2).await;
    let ack = |cmd: &Value, status: &str| json!({"command_id": cmd["id"], "version": cmd["version"], "status": status, "at": "2026-09-25T12:30:04Z"});

    let (s, _) = app.req("POST", "/collar/v1/ack", Some(&a_key), Some(ack(&b1, "applied"))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = app.req("POST", "/collar/v1/ack", Some(&a_key), Some(ack(&b2, "received"))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    app.req("POST", "/collar/v1/ack", Some(&b_key), Some(ack(&b1, "received"))).await;
    app.req("POST", "/collar/v1/ack", Some(&b_key), Some(ack(&b1, "applied"))).await;
    // A repeat is accepted and not stored twice.
    let (s, _) = app.req("POST", "/collar/v1/ack", Some(&b_key), Some(ack(&b1, "applied"))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM acks WHERE collar_id = ?").bind(&b_id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 2);

    // Wrong version, unknown command, someone else's collar id.
    let mut wrong = ack(&b1, "applied");
    wrong["version"] = json!(7);
    let (s, _) = app.req("POST", "/collar/v1/ack", Some(&a_key), Some(wrong)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app
        .req("POST", "/collar/v1/ack", Some(&a_key), Some(json!({"command_id": "bnd_nope", "version": 1, "status": "applied", "at": "2026-09-25T12:30:04Z"})))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let mut spoof = ack(&b2, "applied");
    spoof["collar_id"] = json!(b_id);
    let (s, _) = app.req("POST", "/collar/v1/ack", Some(&a_key), Some(spoof)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let st = op_ingest::boundary_status(&app.ctx, &herd).await.unwrap();
    assert_eq!(st.active.as_ref().unwrap().version, 2);
    assert_eq!(st.acks.len(), 2);
    let a = st.acks.iter().find(|x| x.collar_id == a_id).unwrap();
    assert_eq!((a.version, a.status), (2, op_protocol::AckStatus::Received));
    let b = st.acks.iter().find(|x| x.collar_id == b_id).unwrap();
    assert_eq!((b.version, b.status), (1, op_protocol::AckStatus::Applied));

    let (_, rejected) = app
        .req(
            "POST",
            "/collar/v1/ack",
            Some(&b_key),
            Some(json!({"command_id": b2["id"], "version": 2, "status": "rejected", "reason": "Too many corners.", "at": "2026-09-25T12:31:00Z"})),
        )
        .await;
    assert_eq!(rejected, Value::Null);
    let st = op_ingest::boundary_status(&app.ctx, &herd).await.unwrap();
    let b = st.acks.iter().find(|x| x.collar_id == b_id).unwrap();
    assert_eq!((b.version, b.status, b.reason.as_deref()), (2, op_protocol::AckStatus::Rejected, Some("Too many corners.")));

    // Applied acks move the collar's boundary_version.
    let (_, ca) = app.call("GET", &format!("/api/collars/{a_id}"), None).await;
    assert_eq!(ca["boundary_version"], 1);
    let (_, cb) = app.call("GET", &format!("/api/collars/{b_id}"), None).await;
    assert_eq!(cb["boundary_version"], 1);

    // Acks of deleted collars drop out.
    app.call("DELETE", &format!("/api/collars/{b_id}"), None).await;
    let st = op_ingest::boundary_status(&app.ctx, &herd).await.unwrap();
    assert_eq!(st.acks.len(), 1);
}

#[tokio::test]
async fn fence_state_positions_and_events() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;
    app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": square(0.0)}))).await;
    let mut rx = app.ctx.subscribe();

    // Middle, 2 m from the south edge, 10 m outside it.
    let body = json!({
        "boundary_version": 1,
        "fixes": [
            {"at": "2026-09-25T10:40:10Z", "point": [-92.405, 38.1245 + 2.0 / 111_195.0], "accuracy_m": 2.0, "sats": 9},
            {"at": "2026-09-25T10:40:00Z", "point": [-92.405, 38.125], "accuracy_m": 2.0, "sats": 9},
            {"at": "2026-09-25T10:40:20Z", "point": [-92.405, 38.1245 - 10.0 / 111_195.0], "accuracy_m": 2.0},
        ],
        "cues": [{"at": "2026-09-25T10:40:20Z", "level": 4, "margin_m": -10.0}],
        "battery": 0.77,
        "health": {"sats": 8, "cn0": 41.0}
    });
    let (s, v) = app.req("POST", "/collar/v1/report", Some(&key), Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    let states: Vec<(String, Option<String>, i64)> =
        sqlx::query_as("SELECT state, paddock_id, sats FROM fixes WHERE collar_id = ? ORDER BY t").bind(&id).fetch_all(app.ctx.db()).await.unwrap();
    let names: Vec<&str> = states.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(names, ["inside", "warning", "outside"]);
    assert!(states[0].1.as_deref().unwrap().starts_with("pad_"));
    assert!(states[2].1.is_none());
    assert_eq!(states[2].2, 8, "sats from health when the fix has none");

    let (_, c) = app.call("GET", &format!("/api/collars/{id}"), None).await;
    assert_eq!(c["state"], "outside");
    assert_eq!(c["boundary_version"], 1);
    assert_eq!(c["last_fix"]["at"], "2026-09-25T10:40:20Z");
    let (_, pos) = app.call("GET", &format!("/api/positions?herd_id={herd}"), None).await;
    assert_eq!(pos.as_array().unwrap().len(), 1);
    assert_eq!(pos[0]["collar_id"], json!(id));
    assert_eq!(pos[0]["state"], "outside");
    let latest = op_ingest::latest_positions(&app.ctx, &herd).await.unwrap();
    assert_eq!(latest.len(), 1);

    let (n, battery): (i64, f64) = sqlx::query_as("SELECT fixes, battery FROM health WHERE collar_id = ?").bind(&id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!((n, battery), (3, 0.77));

    let mut kinds = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        kinds.push(serde_json::to_value(&ev).unwrap()["type"].as_str().unwrap().to_owned());
    }
    // One fix event per report: the newest fix.
    assert_eq!(kinds, ["fix", "cue", "collar"]);

    // Coming back in needs the hysteresis margin, per the firmware.
    let back = json!({"fixes": [{"at": "2026-09-25T10:40:30Z", "point": [-92.405, 38.1245 + 0.5 / 111_195.0], "accuracy_m": 2.0, "sats": 9}]});
    app.req("POST", "/collar/v1/report", Some(&key), Some(back)).await;
    let (_, c) = app.call("GET", &format!("/api/collars/{id}"), None).await;
    assert_eq!(c["state"], "outside");
}

#[tokio::test]
async fn a_new_boundary_starts_the_fence_state_over_as_the_collar_does() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;
    app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": square(0.0)}))).await;
    // 10 m south of it: outside.
    let fix = |dy: f64, at: &str| json!({"fixes": [{"at": at, "point": [-92.405, 38.1245 + dy / 111_195.0], "accuracy_m": 2.0, "sats": 9}]});
    app.req("POST", "/collar/v1/report", Some(&key), Some(fix(-10.0, "2026-09-25T10:40:00Z"))).await;
    assert_eq!(app.call("GET", &format!("/api/collars/{id}"), None).await.1["state"], "outside");
    // The farmer lets the herd 10.5 m further south: the animal is 0.5 m inside the new edge.
    let (w, s, e, n) = (-92.4055, 38.1245 - 10.5 / 111_195.0, -92.4045, 38.1255);
    let bigger = json!({"type": "Polygon", "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]]});
    app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": bigger}))).await;
    // The collar rearms on the new boundary and says warning; so does the server,
    // rather than keeping it outside until it is past the hysteresis margin.
    app.req("POST", "/collar/v1/report", Some(&key), Some(fix(-10.0, "2026-09-25T10:40:30Z"))).await;
    let c = app.call("GET", &format!("/api/collars/{id}"), None).await.1;
    assert_eq!(c["state"], "warning", "{c}");
    assert!(c.get("outside_since").is_none_or(|v| v.is_null()), "{c}");
}

#[tokio::test]
async fn proposed_boundary_comes_from_decisions() {
    let app = App::new().await;
    let herd = app.herd().await;
    let geometry = square(0.001).to_string();
    for (id, status, at) in [
        ("dec_old", "proposed", "2026-09-25T10:00:00.000Z"),
        ("dec_new", "proposed", "2026-09-25T11:00:00.000Z"),
        ("dec_done", "rejected", "2026-09-25T12:00:00.000Z"),
    ] {
        sqlx::query("INSERT INTO decisions (id, herd_id, source, status, action, geometry, created_at) VALUES (?, ?, 'brain', ?, 'MOVE', ?, ?)")
            .bind(id)
            .bind(&herd)
            .bind(status)
            .bind(&geometry)
            .bind(at)
            .execute(app.ctx.db())
            .await
            .unwrap();
    }
    let (_, st) = app.call("GET", &format!("/api/herds/{herd}/boundary"), None).await;
    assert_eq!(st["proposed"]["decision_id"], "dec_new");
    assert_eq!(st["proposed"]["geometry"]["type"], "Polygon");
    let (s, _) = app.call("GET", "/api/herds/herd_nope/boundary", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn collar_crud() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, _) = app.device(&herd).await;
    let (_, animal) = app.call("POST", "/api/animals", Some(json!({"tag": "101", "herd_id": herd}))).await;
    let animal_id = animal["id"].as_str().unwrap();

    // Put it on an animal, rename it; telemetry fields are not writable.
    let (s, c) = app.call("PATCH", &format!("/api/collars/{id}"), Some(json!({"name": "Daisy's", "animal_id": animal_id, "state": "inside"}))).await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["name"], "Daisy's");
    assert_eq!(c["animal_id"], animal_id);
    assert_eq!(c["state"], "unknown");
    let (_, a) = app.call("GET", &format!("/api/animals/{animal_id}"), None).await;
    assert_eq!(a["collar_id"], json!(id));

    // Off again.
    let (_, c) = app.call("PATCH", &format!("/api/collars/{id}"), Some(json!({"animal_id": null}))).await;
    assert!(c.get("animal_id").is_none());
    let (_, a) = app.call("GET", &format!("/api/animals/{animal_id}"), None).await;
    assert!(a.get("collar_id").is_none());

    let (s, _) = app.call("PATCH", &format!("/api/collars/{id}"), Some(json!({"animal_id": "ani_nope"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app.call("PATCH", &format!("/api/collars/{id}"), Some(json!({"herd_id": "herd_nope"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app.call("PATCH", &format!("/api/collars/{id}"), Some(json!({"name": " "}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (_, list) = app.call("GET", &format!("/api/collars?herd_id={herd}"), None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let (_, list) = app.call("GET", "/api/collars?herd_id=herd_other", None).await;
    assert_eq!(list, json!([]));
    let (s, _) = app.call("DELETE", &format!("/api/collars/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = app.call("DELETE", &format!("/api/collars/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app.call("GET", &format!("/api/collars/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn late_fixes_do_not_move_the_collar_back() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;
    let fix = |at: &str, lat: f64| json!({"fixes": [{"at": at, "point": [-92.405, lat], "accuracy_m": 2.0, "sats": 9}]});
    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(fix("2026-09-25T10:40:00Z", 38.125))).await;
    assert_eq!(s, StatusCode::OK);
    let mut rx = app.ctx.subscribe();
    // A backfilled fix from earlier is stored but leaves the position alone.
    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(fix("2026-09-25T10:30:00Z", 38.1251))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, c) = app.call("GET", &format!("/api/collars/{id}"), None).await;
    assert_eq!(c["last_fix"]["at"], "2026-09-25T10:40:00Z");
    assert_eq!(c["last_fix"]["point"][1], 38.125);
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fixes WHERE collar_id = ?").bind(&id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 2);
    let mut kinds = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        kinds.push(serde_json::to_value(&ev).unwrap()["type"].as_str().unwrap().to_owned());
    }
    assert_eq!(kinds, ["collar"], "no fix event for a late fix");
}

#[tokio::test]
async fn a_moved_collar_gets_its_new_herds_boundary() {
    let app = App::new().await;
    let herd_a = app.herd().await;
    let (_, h) = app.call("POST", "/api/herds", Some(json!({"name": "Heifers", "species": "cattle", "count": 0}))).await;
    let herd_b = h["id"].as_str().unwrap().to_owned();
    let (id, key) = app.device(&herd_a).await;
    // B's boundary is older than what the collar will hold in A.
    let (_, mb) = app.call("POST", &format!("/api/herds/{herd_b}/boundary"), Some(json!({"geometry": square(0.0005)}))).await;
    let bb = app.sent(&mb).await;
    assert_eq!(bb["version"], 1);
    app.call("POST", &format!("/api/herds/{herd_a}/boundary"), Some(json!({"geometry": square(0.0)}))).await;
    let (_, ma) = app.call("POST", &format!("/api/herds/{herd_a}/boundary"), Some(json!({"geometry": square(0.0002)}))).await;
    let ba = app.sent(&ma).await;
    assert_eq!(ba["version"], 3);
    let ack = json!({"command_id": ba["id"], "version": 3, "status": "applied", "at": "2026-09-25T12:30:04Z"});
    app.req("POST", "/collar/v1/ack", Some(&key), Some(ack)).await;

    let (s, c) = app.call("PATCH", &format!("/api/collars/{id}"), Some(json!({"herd_id": herd_b}))).await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert!(c.get("boundary_version").is_none());
    // The collar still says it has v3; B's boundary comes again as a newer
    // version, its own copy: the rest of B's collars download nothing.
    let (s, cmd) = app.req("GET", "/collar/v1/boundary?have=3", Some(&key), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cmd["version"], 4);
    assert_eq!(cmd["herd_id"], json!(herd_b));
    op_protocol::verify_json(&cmd, &app.ctx.public_key()).unwrap();
    let (_, st) = app.call("GET", &format!("/api/herds/{herd_b}/boundary"), None).await;
    assert_eq!(st["active"]["version"], 1);
    assert_eq!(st["active"]["geometry"], bb["geometry"]);
    // Moving a collar that holds nothing yet changes nothing: it gets B's own.
    let (id2, key2) = app.device(&herd_a).await;
    app.call("PATCH", &format!("/api/collars/{id2}"), Some(json!({"herd_id": herd_b}))).await;
    let (_, own) = app.req("GET", "/collar/v1/boundary?have=0", Some(&key2), None).await;
    assert_eq!(own["version"], 1);
    assert_eq!(own["boundary"], cmd["boundary"], "the copy is B's shape");
}

#[tokio::test]
async fn farmer_boundary_supersedes_and_moves_the_herd() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (_, east) = app.call("POST", "/api/paddocks", Some(json!({"name": "East", "geometry": square(0.002)}))).await;
    sqlx::query("INSERT INTO decisions (id, herd_id, source, status, action, geometry, created_at, apply_at) VALUES ('dec_p', ?, 'brain', 'proposed', 'MOVE', ?, '2026-09-25T10:00:00.000Z', '2026-09-25T10:05:00.000Z')")
        .bind(&herd)
        .bind(square(0.001).to_string())
        .execute(app.ctx.db())
        .await
        .unwrap();
    let (s, mv) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": square(0.002)}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let b = app.sent(&mv).await;
    let (status, apply_at): (String, Option<String>) =
        sqlx::query_as("SELECT status, apply_at FROM decisions WHERE id = 'dec_p'").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!((status.as_str(), apply_at), ("superseded", None));
    let (_, h) = app.call("GET", &format!("/api/herds/{herd}"), None).await;
    assert_eq!(h["paddock_id"], east["id"]);
    // The farmer's decision and its boundary were written together.
    let (dec_boundary,): (Option<String>,) =
        sqlx::query_as("SELECT boundary_id FROM decisions WHERE id = ?").bind(b["decision_id"].as_str().unwrap()).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(dec_boundary.as_deref(), b["id"].as_str());
}

/// 12 collars reporting while boundaries go out: nothing fails with
/// "database is locked" and versions stay one sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_reports_and_boundaries() {
    let app = std::sync::Arc::new(App::new().await);
    let herd = app.herd().await;
    let mut keys = Vec::new();
    for _ in 0..12 {
        keys.push(app.device(&herd).await.1);
    }
    let mut tasks = Vec::new();
    for (i, key) in keys.into_iter().enumerate() {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            for n in 0..15 {
                let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                let body = json!({"fixes": [{"at": at, "point": [-92.405 + i as f64 * 1e-5, 38.125 + n as f64 * 1e-6], "accuracy_m": 2.0, "sats": 9}], "battery": 0.9});
                let (s, v) = app.req("POST", "/collar/v1/report", Some(&key), Some(body)).await;
                assert_eq!(s, StatusCode::OK, "report: {v}");
            }
        }));
    }
    for n in 0..6 {
        let (app, app2) = (app.clone(), app.clone());
        let (herd, herd2) = (herd.clone(), herd.clone());
        tasks.push(tokio::spawn(async move {
            let (s, v) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": square(n as f64 * 0.0001)}))).await;
            assert_eq!(s, StatusCode::CREATED, "boundary: {v}");
        }));
        tasks.push(tokio::spawn(async move {
            op_ingest::send_boundary(&app2.ctx, &herd2, serde_json::from_value(square(0.0003)).unwrap(), Default::default(), "dec_lib").await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let versions: Vec<(i64,)> = sqlx::query_as("SELECT version FROM boundaries ORDER BY version").fetch_all(app.ctx.db()).await.unwrap();
    assert_eq!(versions.iter().map(|v| v.0).collect::<Vec<_>>(), (1..=12).collect::<Vec<_>>());
    let (fixes,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fixes").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(fixes, 12 * 15);
    let (orphans,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM decisions WHERE source = 'farmer' AND boundary_id IS NULL").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(orphans, 0);
}

// Moves

const MID: [f64; 2] = [-92.405, 38.125];

fn m_at(x: f64, y: f64) -> [f64; 2] {
    op_geo::Projection::new(MID).inverse([x, y])
}

/// A rectangle in metres around MID.
fn m_rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Value {
    let r = [m_at(x0, y0), m_at(x1, y0), m_at(x1, y1), m_at(x0, y1), m_at(x0, y0)];
    json!({"type": "Polygon", "coordinates": [r]})
}

fn poly(v: &Value) -> op_geo::Polygon {
    serde_json::from_value(v.clone()).unwrap()
}

impl App {
    /// A 300 x 200 m paddock with the herd in it, and `n` collars reporting
    /// now from the south-west part. Returns herd id and (collar id, key).
    async fn sweep_herd(&self, n: usize) -> (String, Vec<(String, String)>) {
        let (_, _) = self.call("POST", "/api/farm", Some(json!({"name": "Home", "center": MID}))).await;
        let (_, pad) = self.call("POST", "/api/paddocks", Some(json!({"name": "Big", "geometry": m_rect(0.0, 0.0, 300.0, 200.0)}))).await;
        let (_, herd) = self.call("POST", "/api/herds", Some(json!({"name": "Cows", "species": "cattle", "count": n, "paddock_id": pad["id"]}))).await;
        let herd = herd["id"].as_str().unwrap().to_owned();
        let mut collars = Vec::new();
        for i in 0..n {
            let c = self.device(&herd).await;
            self.fix(&c.1, m_at(30.0 + 17.0 * i as f64, 25.0 + 11.0 * (i % 4) as f64), chrono::Utc::now()).await;
            collars.push(c);
        }
        (herd, collars)
    }

    async fn fix(&self, key: &str, point: [f64; 2], at: chrono::DateTime<chrono::Utc>) {
        let at = at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let body = json!({"fixes": [{"at": at, "point": point, "accuracy_m": 2.0, "sats": 9}]});
        let (s, v) = self.req("POST", "/collar/v1/report", Some(key), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    async fn active(&self, herd: &str) -> Value {
        self.call("GET", &format!("/api/herds/{herd}/boundary"), None).await.1
    }
}

#[tokio::test]
async fn a_target_away_from_the_herd_is_swept_into() {
    let app = App::new().await;
    let (herd, collars) = app.sweep_herd(6).await;
    let mut rx = app.ctx.subscribe();

    // The paddock itself: everyone is inside it, so it goes out directly.
    let (s, m) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(0.0, 0.0, 300.0, 200.0)}))).await;
    assert_eq!(s, StatusCode::CREATED, "{m}");
    assert_eq!((m["status"].as_str(), m["step"].as_i64()), (Some("done"), Some(1)));
    assert!(m["id"].as_str().unwrap().starts_with("mov_"));

    // A small corner target: a sweep.
    let target = m_rect(250.0, 150.0, 300.0, 200.0);
    let (s, m) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": target}))).await;
    assert_eq!(s, StatusCode::CREATED, "{m}");
    assert_eq!(m["status"], "sweeping");
    assert_eq!(m["step"], 1);
    assert_eq!(m["target"], target);
    assert_eq!(m["stragglers"], json!([]));
    assert!(m["remaining_m"].as_f64().unwrap() > 100.0, "{m}");
    let step1 = app.sent(&m).await;
    assert_eq!(step1["version"], 2);
    assert_eq!(step1["decision_id"], m["decision_id"]);
    let g = poly(&step1["geometry"]);
    assert_ne!(step1["geometry"], target);
    assert!(g.area_ha() < 6.0, "the corner behind the herd is cut: {} ha", g.area_ha());
    let st = app.active(&herd).await;
    assert_eq!(st["active"]["version"], 2);
    assert_eq!(st["move"]["id"], m["id"]);
    let mut kinds = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        let v = serde_json::to_value(&ev).unwrap();
        if v["type"] == "move" {
            kinds.push(v["move"]["status"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(kinds, ["done", "sweeping"]);

    // Nobody moved: no new step.
    let now = chrono::Utc::now();
    assert!(op_ingest::moves::drive(&app.ctx, &herd, now + chrono::Duration::seconds(40)).await.unwrap().is_none());
    // Everyone walks 40 m toward the corner, but it's only been 10 s.
    for (i, (_, key)) in collars.iter().enumerate() {
        app.fix(key, m_at(30.0 + 17.0 * i as f64 + 34.0, 25.0 + 11.0 * (i % 4) as f64 + 22.0), now).await;
    }
    assert!(op_ingest::moves::drive(&app.ctx, &herd, now + chrono::Duration::seconds(10)).await.unwrap().is_none());
    let m2 = op_ingest::moves::drive(&app.ctx, &herd, now + chrono::Duration::seconds(35)).await.unwrap().expect("a second step");
    assert_eq!(m2.step, 2);
    assert!(m2.remaining_m < m["remaining_m"].as_f64().unwrap());
    let step2 = app.active(&herd).await["active"].clone();
    assert_eq!(step2["version"], 3);
    assert_eq!(step2["decision_id"], m["decision_id"]);
    // The new step lies inside the old one (or the target).
    let g2 = poly(&step2["geometry"]);
    assert!(g2.area_ha() < g.area_ha());

    // Stop keeps the active boundary.
    let (s, stopped) = app.call("POST", &format!("/api/herds/{herd}/move/stop"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(stopped["status"], "stopped");
    assert!(op_ingest::moves::drive(&app.ctx, &herd, now + chrono::Duration::seconds(120)).await.unwrap().is_none());
    let st = app.active(&herd).await;
    assert_eq!(st["active"]["version"], 3);
    assert_eq!(st["move"]["status"], "stopped");
    let (s, _) = app.call("POST", &format!("/api/herds/{herd}/move/stop"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = app.call("POST", "/api/herds/herd_nope/move/stop", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_new_target_replaces_the_running_move() {
    let app = App::new().await;
    let (herd, _) = app.sweep_herd(4).await;
    app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(0.0, 0.0, 300.0, 200.0)}))).await;
    let (_, a) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(250.0, 150.0, 300.0, 200.0)}))).await;
    assert_eq!(a["status"], "sweeping");
    let (_, b) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(250.0, 0.0, 300.0, 50.0)}))).await;
    assert_eq!(b["status"], "sweeping");
    let (status,): (String,) = sqlx::query_as("SELECT status FROM moves WHERE id = ?").bind(a["id"].as_str().unwrap()).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(status, "stopped");
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM moves WHERE status = 'sweeping'").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 1);
    // The new sweep is clipped to what the herd holds now (the first sweep's step).
    let st = app.active(&herd).await;
    assert_eq!(st["move"]["id"], b["id"]);
    assert_eq!(st["active"]["decision_id"], b["decision_id"]);
}

#[tokio::test]
async fn a_straggler_is_dropped_and_left_behind() {
    let app = App::new().await;
    let (herd, collars) = app.sweep_herd(5).await;
    app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(0.0, 0.0, 300.0, 200.0)}))).await;
    let (_, m) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(250.0, 150.0, 300.0, 200.0)}))).await;
    assert_eq!(m["status"], "sweeping");
    // All but the first walk up; the first stays where it is.
    let now = chrono::Utc::now();
    for (i, (_, key)) in collars.iter().enumerate().skip(1) {
        app.fix(key, m_at(30.0 + 17.0 * i as f64 + 60.0, 25.0 + 11.0 * (i % 4) as f64 + 40.0), now).await;
    }
    // Its fixes keep saying so.
    for s in [0, 40, 120, 240] {
        app.fix(&collars[0].1, m_at(30.0, 25.0), now + chrono::Duration::seconds(s)).await;
        let out = op_ingest::moves::drive(&app.ctx, &herd, now + chrono::Duration::seconds(s.max(40))).await.unwrap();
        assert!(out.is_none(), "held for the straggler at {s} s");
    }
    app.fix(&collars[0].1, m_at(30.0, 25.0), now + chrono::Duration::seconds(320)).await;
    let m2 = op_ingest::moves::drive(&app.ctx, &herd, now + chrono::Duration::seconds(320)).await.unwrap().expect("goes on without it");
    assert_eq!(m2.stragglers, vec![collars[0].0.clone()]);
    assert_eq!(m2.step, 2);
    let st = app.active(&herd).await;
    assert!(!poly(&st["active"]["geometry"]).contains(m_at(30.0, 25.0)));
    assert_eq!(st["move"]["stragglers"], json!([collars[0].0]));
}

#[tokio::test]
async fn a_sweep_waits_out_a_collar_outage() {
    let app = App::new().await;
    let (herd, _) = app.sweep_herd(0).await;
    app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(0.0, 0.0, 300.0, 200.0)}))).await;
    // One clock through the whole outage, moving forward (a fix can't be more
    // than ten minutes ahead of the server's): the collars last reported five
    // minutes ago, just before the farm's link went down.
    let t0 = chrono::Utc::now();
    let min = |m: f64| t0 + chrono::Duration::milliseconds((m * 60_000.0) as i64);
    let spot = |i: usize, up: f64| m_at(30.0 + 17.0 * i as f64 + 1.5 * up, 25.0 + 11.0 * (i % 4) as f64 + up);
    let mut collars = Vec::new();
    for i in 0..4 {
        let c = app.device(&herd).await;
        app.fix(&c.1, spot(i, 0.0), min(-5.0)).await;
        collars.push(c);
    }
    let (_, m) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(250.0, 150.0, 300.0, 200.0)}))).await;
    assert_eq!((m["status"].as_str(), m["step"].as_u64()), (Some("sweeping"), Some(1)));
    let version = app.active(&herd).await["active"]["version"].clone();
    // No collar reports again. Past five minutes nobody is taken as stuck;
    // past ten, with every fix stale, the sweep holds.
    for m in [1.0, 4.0, 5.5, 6.0] {
        let out = op_ingest::moves::drive(&app.ctx, &herd, min(m)).await.unwrap();
        assert!(out.is_none(), "stepped at {m} min with no fixes: {out:?}");
    }
    let st = app.active(&herd).await;
    assert_eq!((st["move"]["status"].as_str(), st["move"]["step"].as_u64()), (Some("sweeping"), Some(1)));
    assert_eq!(st["move"]["stragglers"], json!([]));
    assert_eq!(st["active"]["version"], version, "nothing sent past the animals");
    // Back, walked up. It waits a little for collars still to report, then goes on from where they are.
    for (i, (_, key)) in collars.iter().enumerate() {
        app.fix(key, spot(i, 40.0), min(7.0)).await;
    }
    assert!(op_ingest::moves::drive(&app.ctx, &herd, min(7.1)).await.unwrap().is_none());
    let m2 = op_ingest::moves::drive(&app.ctx, &herd, min(8.1)).await.unwrap().expect("a step");
    assert_eq!((m2.step, m2.stragglers.len()), (2, 0));
}

#[tokio::test]
async fn an_outage_reaching_the_collars_one_report_at_a_time_drops_nobody() {
    let app = App::new().await;
    let (herd, collars) = app.sweep_herd(8).await;
    app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(0.0, 0.0, 300.0, 200.0)}))).await;
    let (_, m) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(250.0, 150.0, 300.0, 200.0)}))).await;
    assert_eq!((m["status"].as_str(), m["step"].as_u64()), (Some("sweeping"), Some(1)));
    let version = app.active(&herd).await["active"]["version"].clone();
    // Each collar's last report lands 15 s after the one before, where it stands; then the link goes.
    let now = chrono::Utc::now();
    for (i, (_, key)) in collars.iter().enumerate() {
        app.fix(key, m_at(30.0 + 17.0 * i as f64, 25.0 + 11.0 * (i % 4) as f64), now + chrono::Duration::seconds(15 * i as i64)).await;
    }
    // Their fixes pass ten minutes old one by one: passes every 5 s see one, two, three … of eight silent.
    for k in 0..40 {
        let at = now + op_ingest::moves::FRESH_FIX + chrono::Duration::seconds(5 * k + 1);
        assert!(op_ingest::moves::drive(&app.ctx, &herd, at).await.unwrap().is_none_or(|m| m.stragglers.is_empty()), "pass {k}");
    }
    let st = app.active(&herd).await;
    assert_eq!(st["move"]["stragglers"], json!([]), "nobody dropped on the way into the outage");
    assert_eq!((st["move"]["step"].as_u64(), st["active"]["version"].clone()), (Some(1), version));
}

#[tokio::test]
async fn a_move_started_while_the_collars_are_silent_sends_nothing_until_they_report() {
    let app = App::new().await;
    let (herd, collars) = app.sweep_herd(0).await;
    app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(0.0, 0.0, 300.0, 200.0)}))).await;
    let version = app.active(&herd).await["active"]["version"].clone();
    assert!(collars.is_empty());
    // The link went down half an hour ago: every collar's last fix is that old.
    let then = chrono::Utc::now() - chrono::Duration::minutes(30);
    let mut collars = Vec::new();
    for i in 0..4 {
        let c = app.device(&herd).await;
        app.fix(&c.1, m_at(30.0 + 17.0 * i as f64, 25.0 + 11.0 * (i % 4) as f64), then).await;
        collars.push(c);
    }
    // The farmer sends the corner, for two hours from now.
    let later = op_protocol::wire_time::trunc_secs(chrono::Utc::now() + chrono::Duration::hours(2));
    let (_, m) = app
        .call(
            "POST",
            &format!("/api/herds/{herd}/boundary"),
            Some(json!({"geometry": m_rect(250.0, 150.0, 300.0, 200.0), "effective_at": op_protocol::wire_time::format(&later)})),
        )
        .await;
    assert_eq!((m["status"].as_str(), m["step"].as_u64()), (Some("sweeping"), Some(0)), "{m}");
    let st = app.active(&herd).await;
    assert_eq!(st["active"]["version"], version, "no target sent blind");
    assert!(st["staged"].as_array().is_none_or(|a| a.is_empty()), "nor staged: {st}");
    // One clock, moving forward (a fix can't be more than ten minutes ahead of the server's).
    let t0 = chrono::Utc::now();
    for s in [30, 60] {
        assert!(op_ingest::moves::drive(&app.ctx, &herd, t0 + chrono::Duration::seconds(s)).await.unwrap().is_none());
    }
    assert!(app.active(&herd).await["staged"].as_array().is_none_or(|a| a.is_empty()));
    // They report: after a little wait for the rest, the first step is planned from where they are, for the farmer's time.
    let back = t0 + chrono::Duration::seconds(90);
    for (i, (_, key)) in collars.iter().enumerate() {
        app.fix(key, m_at(30.0 + 17.0 * i as f64, 25.0 + 11.0 * (i % 4) as f64), back).await;
    }
    assert!(op_ingest::moves::drive(&app.ctx, &herd, back + chrono::Duration::seconds(1)).await.unwrap().is_none());
    let resume = t0 + chrono::Duration::seconds(60) + op_ingest::moves::RESUME_AFTER + chrono::Duration::seconds(1);
    let m1 = op_ingest::moves::drive(&app.ctx, &herd, resume).await.unwrap().expect("the first step");
    assert_eq!(m1.step, 1);
    let st = app.active(&herd).await;
    assert_eq!(st["active"]["version"], version);
    let staged = st["staged"].as_array().unwrap();
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0]["effective_at"], json!(op_protocol::wire_time::format(&later)));
    let step = poly(&staged[0]["geometry"]);
    for (i, _) in collars.iter().enumerate() {
        assert!(step.contains(m_at(30.0 + 17.0 * i as f64, 25.0 + 11.0 * (i % 4) as f64)), "collar {i} inside the first step");
    }
}

#[tokio::test]
async fn sweeping_moves_resume_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (herd, collars) = {
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let router = op_core::router().merge(op_ingest::router()).with_state(ctx.clone());
        let app = App { _dir: tempfile::tempdir().unwrap(), ctx, router };
        let (herd, collars) = app.sweep_herd(4).await;
        app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(0.0, 0.0, 300.0, 200.0)}))).await;
        let (_, m) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(250.0, 150.0, 300.0, 200.0)}))).await;
        assert_eq!(m["status"], "sweeping");
        // The last step went out a minute ago, and the herd has walked up since.
        sqlx::query("UPDATE moves SET sweep = json_set(sweep, '$.last_step_at', ?)")
            .bind(op_core::time::to_db(&(op_core::time::now() - chrono::Duration::seconds(60))))
            .execute(app.ctx.db())
            .await
            .unwrap();
        for (i, (_, key)) in collars.iter().enumerate() {
            app.fix(key, m_at(30.0 + 17.0 * i as f64 + 60.0, 25.0 + 11.0 * (i % 4) as f64 + 40.0), chrono::Utc::now()).await;
        }
        app.ctx.shutdown();
        (herd, collars)
    };
    assert_eq!(collars.len(), 4);
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let mut rx = ctx.subscribe();
    op_ingest::start(ctx.clone()).await.unwrap();
    let step = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(op_core::Event::Move { r#move }) = rx.recv().await
                && r#move.herd_id == herd
            {
                return r#move;
            }
        }
    })
    .await
    .expect("the driver steps the resumed move");
    assert_eq!(step.step, 2);
    ctx.shutdown();
}

// Escapes

#[tokio::test]
async fn an_escaped_animal_gets_its_own_boundary_and_is_handed_back() {
    let app = App::new().await;
    let (herd, collars) = app.sweep_herd(2).await;
    let (a, a_key) = &collars[0];
    let (_, b_key) = &collars[1];
    let (s, _) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(0.0, 0.0, 300.0, 200.0)}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let herd_v = app.active(&herd).await["active"]["version"].as_u64().unwrap() as u32;
    let now = chrono::Utc::now();

    // Out for 30 s: the collar's own cue is still the thing to wait on.
    app.fix(a_key, m_at(150.0, 230.0), now - chrono::Duration::seconds(30)).await;
    op_ingest::escapes::scan(&app.ctx, now).await.unwrap();
    assert!(app.active(&herd).await.get("escapes").is_none());

    // Out for 90 s: its own boundary.
    app.fix(a_key, m_at(150.0, 231.0), now).await;
    op_ingest::escapes::scan(&app.ctx, now + chrono::Duration::seconds(60)).await.unwrap();
    let st = app.active(&herd).await;
    let esc = &st["escapes"][0];
    assert_eq!((esc["collar_id"].as_str(), esc["status"].as_str()), (Some(a.as_str()), Some("returning")));
    assert_eq!(st["active"]["version"].as_u64(), Some(herd_v as u64), "the herd's boundary is unchanged");

    // The escaped collar is served its pen; the rest of the herd is not.
    let (s, cmd) = app.req("GET", &format!("/collar/v1/boundary?have={herd_v}"), Some(a_key), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cmd["version"], esc["version"]);
    let pen_v = cmd["version"].as_u64().unwrap() as u32;
    assert!(pen_v > herd_v);
    assert!(poly(&esc["geometry"]).contains(m_at(150.0, 231.0)));
    assert!(poly(&esc["geometry"]).contains(m_at(1.0, 1.0)));
    let (s, _) = app.req("GET", &format!("/collar/v1/boundary?have={herd_v}"), Some(b_key), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // Reports point it at its pen, not the herd's boundary.
    let body = json!({"fixes": [{"at": now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), "point": m_at(150.0, 231.0), "accuracy_m": 2.0, "sats": 9}]});
    let (_, r) = app.req("POST", "/collar/v1/report", Some(a_key), Some(body)).await;
    assert_eq!(r["latest_version"].as_u64(), Some(pen_v as u64));
    let ack = |cmd: &Value| json!({"command_id": cmd["command_id"], "version": cmd["version"], "status": "applied", "at": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)});
    let (s, _) = app.req("POST", "/collar/v1/ack", Some(a_key), Some(ack(&cmd))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // Another collar can't ack it.
    let (s, _) = app.req("POST", "/collar/v1/ack", Some(b_key), Some(ack(&cmd))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Back inside: a copy of the herd's boundary, which counts as holding it.
    app.fix(a_key, m_at(150.0, 150.0), now + chrono::Duration::seconds(90)).await;
    op_ingest::escapes::scan(&app.ctx, now + chrono::Duration::seconds(95)).await.unwrap();
    let st = app.active(&herd).await;
    assert_eq!(st["escapes"][0]["status"].as_str(), Some("back"));
    let (s, copy) = app.req("GET", &format!("/collar/v1/boundary?have={pen_v}"), Some(a_key), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(copy["version"].as_u64().unwrap() > pen_v as u64);
    let herd_cmd = app.req("GET", "/collar/v1/boundary?have=0", Some(b_key), None).await.1;
    assert_eq!(herd_cmd["version"].as_u64(), Some(herd_v as u64));
    assert_eq!(copy["boundary"], herd_cmd["boundary"]);
    app.req("POST", "/collar/v1/ack", Some(a_key), Some(ack(&copy))).await;
    let st = op_ingest::boundary_status(&app.ctx, &herd).await.unwrap();
    assert_eq!(st.acks.iter().find(|x| x.collar_id == *a).unwrap().version, herd_v);
    let (s, _) = app.req("GET", &format!("/collar/v1/boundary?have={}", copy["version"]), Some(a_key), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_farmer_can_let_an_escaped_animal_go() {
    let app = App::new().await;
    let (herd, collars) = app.sweep_herd(1).await;
    let (a, a_key) = &collars[0];
    app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": m_rect(0.0, 0.0, 300.0, 200.0)}))).await;
    let now = chrono::Utc::now();
    app.fix(a_key, m_at(150.0, 230.0), now - chrono::Duration::seconds(90)).await;
    app.fix(a_key, m_at(150.0, 231.0), now).await;
    // Out since its first new fix outside (the backfilled one doesn't start the clock).
    op_ingest::escapes::scan(&app.ctx, now + chrono::Duration::seconds(60)).await.unwrap();

    let (s, e) = app.call("POST", &format!("/api/collars/{a}/escape/stop"), None).await;
    assert_eq!(s, StatusCode::OK, "{e}");
    assert_eq!(e["status"].as_str(), Some("stopped"));
    let (s, _) = app.call("POST", &format!("/api/collars/{a}/escape/stop"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Still out, but not given another until it has been back in.
    app.fix(a_key, m_at(150.0, 232.0), now + chrono::Duration::seconds(5)).await;
    op_ingest::escapes::scan(&app.ctx, now + chrono::Duration::seconds(10)).await.unwrap();
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM escapes").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 1);
    // In, then out again for a minute: a new escape.
    app.fix(a_key, m_at(150.0, 150.0), now + chrono::Duration::seconds(20)).await;
    app.fix(a_key, m_at(150.0, 230.0), now + chrono::Duration::seconds(30)).await;
    op_ingest::escapes::scan(&app.ctx, now + chrono::Duration::seconds(100)).await.unwrap();
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM escapes WHERE status = 'returning'").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 1);
}
