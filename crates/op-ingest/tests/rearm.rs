//! Collar re-arm and welfare reporting.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Role, Via, with_identity};
use op_protocol::{REARM_TTL_HOURS, caps, verify_rearm};
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    role: Role,
}

impl App {
    async fn new() -> Self {
        Self::with_role(Role::Owner).await
    }

    async fn with_role(role: Role) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        op_ingest::register_tools(&ctx);
        Self { _dir: dir, ctx, role }
    }

    fn router(&self) -> axum::Router {
        let id = Identity { role: self.role, user_id: Some("usr_test".into()), name: Some("Tester".into()), via: Via::UserToken };
        with_identity(op_core::router().merge(op_ingest::router()), id).with_state(self.ctx.clone())
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
        let res = self.router().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let value =
            if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into())) };
        (status, value)
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.req(method, path, None, body).await
    }

    async fn herd(&self) -> String {
        let (s, _) = self.call("POST", "/api/farm", Some(json!({"name": "Home", "timezone": "UTC", "center": [-92.405, 38.125]}))).await;
        assert_eq!(s, StatusCode::CREATED);
        let (s, pad) = self
            .call(
                "POST",
                "/api/paddocks",
                Some(json!({"name": "North", "geometry": {
                    "type": "Polygon",
                    "coordinates": [[[-92.4055, 38.1245], [-92.4045, 38.1245], [-92.4045, 38.1255], [-92.4055, 38.1255], [-92.4055, 38.1245]]]
                }})),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED);
        let (s, herd) = self.call("POST", "/api/herds", Some(json!({"name": "Cows", "species": "cattle", "count": 0, "paddock_id": pad["id"]}))).await;
        assert_eq!(s, StatusCode::CREATED);
        herd["id"].as_str().unwrap().to_owned()
    }

    async fn device(&self, herd: &str) -> (String, String) {
        let (s, v) = self.call("POST", "/api/collars", Some(json!({"herd_id": herd}))).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        (v["collar"]["id"].as_str().unwrap().to_owned(), v["key"].as_str().unwrap().to_owned())
    }

    async fn report_shutoff(&self, key: &str, shutoff_at: &str, rearm_seq: u32) {
        let body = json!({
            "device": { "fw": "0.3", "caps": [caps::REARM, caps::CONFIG, "holes", "slots"] },
            "fixes": [{"at": "2026-11-04T05:00:00Z", "point": [-92.405, 38.125], "accuracy_m": 2.0, "sats": 9}],
            "battery": 0.8,
            "welfare": {
                "enforcement": "off",
                "shutoff_at": shutoff_at,
                "escape_cycles": 2,
                "rearm_seq": rearm_seq
            },
            "welfare_events": [{ "kind": 3, "at": shutoff_at, "extra_note": "auto" }]
        });
        let (s, v) = self.req("POST", "/collar/v1/report", Some(key), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn role_gate_requires_manager() {
    let owner = App::new().await;
    let herd = owner.herd().await;
    let (id, key) = owner.device(&herd).await;
    owner.report_shutoff(&key, "2026-11-04T04:55:00Z", 0).await;

    for role in [Role::Viewer, Role::Hand] {
        let _app = App { role, ..App::with_role(role).await };
        // Rebuild against the same DB by copying ctx is hard; use owner's ctx with a different identity.
        let id_low = Identity { role, user_id: Some("usr_low".into()), name: None, via: Via::UserToken };
        let router = with_identity(op_core::router().merge(op_ingest::router()), id_low).with_state(owner.ctx.clone());
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/collars/{id}/rearm"))
            .header("content-type", "application/json")
            .body(Body::from(json!({"reason": "back in"}).to_string()))
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{role:?}");
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["error"].as_str().unwrap().contains("Manager"), "{v}");
    }

    let (s, v) = owner.call("POST", &format!("/api/collars/{id}/rearm"), Some(json!({"reason": "back in"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn reason_required_and_pending_conflicts() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;
    app.report_shutoff(&key, "2026-11-04T04:55:00Z", 1).await;

    let (s, v) = app.call("POST", &format!("/api/collars/{id}/rearm"), Some(json!({"reason": "  "}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("reason"), "{v}");

    let (s, v) = app.call("POST", &format!("/api/collars/{id}/rearm"), Some(json!({"reason": "fence fixed", "reason_code": "fence_fixed"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["command"]["status"], "pending");
    assert_eq!(v["command"]["reason_code"], "fence_fixed");
    assert_eq!(v["collar"]["rearm"]["status"], "pending");

    let (s, v) = app.call("POST", &format!("/api/collars/{id}/rearm"), Some(json!({"reason": "again"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "rearm_pending");
    assert!(v["pending"].is_object(), "{v}");
}

#[tokio::test(flavor = "current_thread")]
async fn not_supported_and_not_shut_off() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;

    let (s, v) = app.call("POST", &format!("/api/collars/{id}/rearm"), Some(json!({"reason": "x"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "rearm_not_supported");

    // Caps without a shut-off.
    let body = json!({
        "device": { "fw": "0.3", "caps": [caps::REARM] },
        "fixes": [{"at": "2026-11-04T05:00:00Z", "point": [-92.405, 38.125], "accuracy_m": 2.0, "sats": 9}],
        "battery": 0.8
    });
    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(body)).await;
    assert_eq!(s, StatusCode::OK);
    let (_, c) = app.call("GET", &format!("/api/collars/{id}"), None).await;
    assert_eq!(c["rearm_available"], true);

    let (s, v) = app.call("POST", &format!("/api/collars/{id}/rearm"), Some(json!({"reason": "x"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "not_shut_off");
}

#[tokio::test(flavor = "current_thread")]
async fn deliver_ack_and_audit() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;
    app.report_shutoff(&key, "2026-11-04T04:55:00Z", 4).await;

    let (s, v) = app.call("POST", &format!("/api/collars/{id}/rearm"), Some(json!({"reason": "animal back", "reason_code": "animal_back_in"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["command"]["rearm_seq"], 5); // max(0, 4)+1
    let cmd_id = v["command"]["id"].as_str().unwrap().to_owned();

    let body = json!({
        "device": { "fw": "0.3", "caps": [caps::REARM] },
        "fixes": [{"at": "2026-11-04T05:05:00Z", "point": [-92.405, 38.125], "accuracy_m": 2.0, "sats": 9}],
        "battery": 0.8,
        "welfare": { "enforcement": "off", "shutoff_at": "2026-11-04T04:55:00Z", "rearm_seq": 4 }
    });
    let (s, rep) = app.req("POST", "/collar/v1/report", Some(&key), Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{rep}");
    let rearm = &rep["rearm"];
    assert_eq!(rearm["command_id"], cmd_id);
    assert_eq!(rearm["rearm_seq"], 5);
    assert!(rearm["sig"].is_string());
    assert!(rearm.get("reason").is_none(), "free-text reason must not be on the wire");
    let cmd: op_protocol::RearmCommand = serde_json::from_value(rearm.clone()).unwrap();
    verify_rearm(&cmd, &app.ctx.public_key()).unwrap();

    let ack = json!({
        "command_id": cmd_id,
        "version": 5,
        "status": "applied",
        "at": "2026-11-04T05:05:30Z"
    });
    let (s, _) = app.req("POST", "/collar/v1/ack", Some(&key), Some(ack)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (_, c) = app.call("GET", &format!("/api/collars/{id}"), None).await;
    assert_eq!(c["rearm"]["status"], "acked");
    assert_eq!(c["rearm"]["ack_status"], "applied");
    assert_eq!(c["welfare_state"], "on");
    assert!(c.get("shutoff_at").is_none() || c["shutoff_at"].is_null());

    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM collar_rearm_audit WHERE collar_id = ? AND event IN ('issued', 'acked')")
        .bind(&id)
        .fetch_one(app.ctx.db())
        .await
        .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test(flavor = "current_thread")]
async fn expiry_with_pinned_clock() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;
    app.report_shutoff(&key, "2026-11-04T04:55:00Z", 0).await;

    let issued = chrono::DateTime::parse_from_rfc3339("2026-11-04T05:00:00Z").unwrap().with_timezone(&chrono::Utc);
    op_core::time::pin(Some(issued));
    let (s, v) = app.call("POST", &format!("/api/collars/{id}/rearm"), Some(json!({"reason": "will expire"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let cmd_id = v["command"]["id"].as_str().unwrap().to_owned();

    // Past TTL: sweep on report must not deliver.
    op_core::time::pin(Some(issued + chrono::Duration::hours(REARM_TTL_HOURS) + chrono::Duration::seconds(1)));
    let body = json!({
        "device": { "fw": "0.3", "caps": [caps::REARM] },
        "fixes": [{"at": "2026-11-05T05:00:00Z", "point": [-92.405, 38.125], "accuracy_m": 2.0, "sats": 9}],
        "battery": 0.7
    });
    let (s, rep) = app.req("POST", "/collar/v1/report", Some(&key), Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{rep}");
    assert!(rep.get("rearm").is_none(), "{rep}");

    let status: String = sqlx::query_scalar("SELECT status FROM collar_rearms WHERE id = ?").bind(&cmd_id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(status, "expired");
    let expired: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM collar_rearm_audit WHERE rearm_id = ? AND event = 'expired'")
        .bind(&cmd_id)
        .fetch_one(app.ctx.db())
        .await
        .unwrap();
    assert_eq!(expired, 1);
    op_core::time::pin(None);
}

#[tokio::test(flavor = "current_thread")]
async fn welfare_parsing_and_auto_shutoff_alert() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;

    // Alias `cycles`, unknown fields/kinds, bool enforcement.
    let body = json!({
        "device": { "fw": "0.3", "caps": [caps::REARM] },
        "fixes": [{"at": "2026-11-04T05:00:00Z", "point": [-92.405, 38.125], "accuracy_m": 2.0, "sats": 9}],
        "battery": 0.8,
        "welfare": {
            "enforcement": false,
            "shutoff_at": "2026-11-04T04:50:00Z",
            "cycles": 9,
            "rearm_seq": 2,
            "future": true
        },
        "welfare_events": [
            { "kind": 3, "at": "2026-11-04T04:50:00Z" },
            { "kind": 99, "at": "2026-11-04T04:51:00Z", "note": "unknown" }
        ]
    });
    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(body)).await;
    assert_eq!(s, StatusCode::OK);

    let (_, c) = app.call("GET", &format!("/api/collars/{id}"), None).await;
    assert_eq!(c["welfare_state"], "off");
    assert_eq!(c["shutoff_at"], "2026-11-04T04:50:00Z");
    assert_eq!(c["rearm_available"], true);

    let kinds: Vec<(i64,)> =
        sqlx::query_as("SELECT kind FROM collar_welfare_events WHERE collar_id = ? ORDER BY kind").bind(&id).fetch_all(app.ctx.db()).await.unwrap();
    assert_eq!(kinds, vec![(3,), (99,)]);

    let open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM alerts WHERE kind = 'auto_shutoff' AND status = 'open' AND key = ?")
        .bind(format!("auto_shutoff:{id}"))
        .fetch_one(app.ctx.db())
        .await
        .unwrap();
    assert_eq!(open, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn mcp_tool_and_optional_guards() {
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;
    app.report_shutoff(&key, "2026-11-04T04:55:00Z", 0).await;

    let spec = app.ctx.tools().get("rearm_collar").unwrap();
    assert_eq!(spec.min_role, Role::Manager);
    assert!(!spec.read);

    let identity = Identity { role: Role::Manager, user_id: Some("usr_agent".into()), name: Some("Bot".into()), via: Via::AppToken };
    let scope = op_core::tools::ToolScope::Full;
    let v = app
        .ctx
        .tools()
        .call(&app.ctx, "rearm_collar", json!({ "collar_id": id, "reason": "via mcp" }), Some("test".into()), identity.clone(), &scope)
        .await
        .unwrap();
    assert_eq!(v["collar"]["rearm"]["status"], "pending");
    assert_eq!(v["collar"]["rearm"]["by"]["kind"], "agent");

    // Clear pending; new shut-off so the prior agent rearm does not count against the limit.
    sqlx::query("UPDATE collar_rearms SET status = 'expired' WHERE collar_id = ?").bind(&id).execute(app.ctx.db()).await.unwrap();
    sqlx::query("UPDATE collars SET shutoff_at = ?, welfare_state = 'off' WHERE id = ?")
        .bind("2026-11-04T05:40:00.000Z")
        .bind(&id)
        .execute(app.ctx.db())
        .await
        .unwrap();
    app.ctx
        .store()
        .set_setting(op_ingest::rearm::GUARDS_KEY, &json!({ "agent_rate_limit": true, "refuse_outside": false, "notify_on_agent": true }))
        .await
        .unwrap();

    // First agent rearm for this shut-off succeeds; second is refused.
    let _ = app.ctx.tools().call(&app.ctx, "rearm_collar", json!({ "collar_id": id, "reason": "first" }), None, identity.clone(), &scope).await.unwrap();
    sqlx::query("UPDATE collar_rearms SET status = 'acked' WHERE collar_id = ? AND status = 'pending'").bind(&id).execute(app.ctx.db()).await.unwrap();
    let err =
        app.ctx.tools().call(&app.ctx, "rearm_collar", json!({ "collar_id": id, "reason": "second same shutoff" }), None, identity, &scope).await.unwrap_err();
    assert_eq!(err.status, StatusCode::CONFLICT);
    assert_eq!(err.details.as_ref().and_then(|d| d.get("code")).and_then(|c| c.as_str()), Some("agent_rearm_limit"));

    let notified: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM alerts WHERE kind = 'agent_rearm'").fetch_one(app.ctx.db()).await.unwrap();
    assert!(notified >= 1);

    // Guards off: human can still rearm; refuse_outside when enabled.
    app.ctx
        .store()
        .set_setting(op_ingest::rearm::GUARDS_KEY, &json!({ "agent_rate_limit": false, "refuse_outside": true, "notify_on_agent": false }))
        .await
        .unwrap();
    sqlx::query("UPDATE collars SET state = 'outside', shutoff_at = ?, welfare_state = 'off' WHERE id = ?")
        .bind("2026-11-04T05:30:00Z")
        .bind(&id)
        .execute(app.ctx.db())
        .await
        .unwrap();
    let (s, v) = app.call("POST", &format!("/api/collars/{id}/rearm"), Some(json!({"reason": "outside"}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "outside_boundary");
}

#[tokio::test(flavor = "current_thread")]
async fn stale_when_seq_already_held_counts_as_applied() {
    // Re-signed redelivery of an already-applied seq: collar returns stale (6)
    // but welfare.rearm_seq >= that seq → mark done, do not refresh.
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;
    app.report_shutoff(&key, "2026-11-04T04:55:00Z", 0).await;
    let (s, v) = app.call("POST", &format!("/api/collars/{id}/rearm"), Some(json!({"reason": "try"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let cmd_id = v["command"]["id"].as_str().unwrap().to_owned();
    let seq = v["command"]["rearm_seq"].as_u64().unwrap();
    assert!(seq >= 1);

    // Collar has already applied this seq (held >= command seq).
    let body = json!({
        "device": { "fw": "0.3", "caps": [caps::REARM] },
        "fixes": [{"at": "2026-11-04T05:08:00Z", "point": [-92.405, 38.125], "accuracy_m": 2.0, "sats": 9}],
        "battery": 0.8,
        "welfare": { "enforcement": "on", "rearm_seq": seq }
    });
    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(body)).await;
    assert_eq!(s, StatusCode::OK);

    let ack = json!({
        "command_id": cmd_id,
        "version": seq,
        "status": "rejected",
        "code": 6,
        "at": "2026-11-04T05:10:00Z"
    });
    let (s, _) = app.req("POST", "/collar/v1/ack", Some(&key), Some(ack)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (status, ack_status, ack_code): (String, String, Option<String>) =
        sqlx::query_as("SELECT status, ack_status, ack_code FROM collar_rearms WHERE id = ?").bind(&cmd_id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(status, "acked");
    assert_eq!(ack_status, "applied");
    assert_eq!(ack_code.as_deref(), Some("stale")); // collar said stale; treated as applied

    let pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM collar_rearms WHERE collar_id = ? AND status = 'pending'").bind(&id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(pending, 0, "already-held stale must not queue a refresh");

    let welfare_state: Option<String> = sqlx::query_scalar("SELECT welfare_state FROM collars WHERE id = ?").bind(&id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(welfare_state.as_deref(), Some("on"));
}

#[tokio::test(flavor = "current_thread")]
async fn stale_below_held_or_shutoff_mismatch_refreshes_once() {
    // Real stale: welfare.rearm_seq still below the command (e.g. shutoff_at
    // moved). Refresh once; a second stale on the refresh does not retry.
    let app = App::new().await;
    let herd = app.herd().await;
    let (id, key) = app.device(&herd).await;
    app.report_shutoff(&key, "2026-11-04T04:55:00Z", 0).await;
    let (s, v) = app.call("POST", &format!("/api/collars/{id}/rearm"), Some(json!({"reason": "try"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let cmd_id = v["command"]["id"].as_str().unwrap().to_owned();
    let seq = v["command"]["rearm_seq"].as_u64().unwrap();
    assert!(seq >= 1);

    // Newer shut-off; held seq still below the command → real stale.
    let body = json!({
        "device": { "fw": "0.3", "caps": [caps::REARM] },
        "fixes": [{"at": "2026-11-04T05:08:00Z", "point": [-92.405, 38.125], "accuracy_m": 2.0, "sats": 9}],
        "battery": 0.8,
        "welfare": { "enforcement": "off", "shutoff_at": "2026-11-04T05:07:00Z", "rearm_seq": 0 }
    });
    let (s, _) = app.req("POST", "/collar/v1/report", Some(&key), Some(body)).await;
    assert_eq!(s, StatusCode::OK);

    let ack = json!({
        "command_id": cmd_id,
        "version": seq,
        "status": "rejected",
        "code": 6,
        "at": "2026-11-04T05:10:00Z"
    });
    let (s, _) = app.req("POST", "/collar/v1/ack", Some(&key), Some(ack)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let prior: String = sqlx::query_scalar("SELECT status FROM collar_rearms WHERE id = ?").bind(&cmd_id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(prior, "rejected");
    let ack_code: String = sqlx::query_scalar("SELECT ack_code FROM collar_rearms WHERE id = ?").bind(&cmd_id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(ack_code, "stale");

    let (new_id, new_seq, shut, refresh_of): (String, i64, String, String) =
        sqlx::query_as("SELECT id, rearm_seq, shutoff_at, refresh_of FROM collar_rearms WHERE collar_id = ? AND status = 'pending'")
            .bind(&id)
            .fetch_one(app.ctx.db())
            .await
            .unwrap();
    assert_eq!(refresh_of, cmd_id);
    assert!(new_seq as u64 > seq);
    assert_eq!(shut, "2026-11-04T05:07:00.000Z");
    assert_ne!(new_id, cmd_id);

    // Second stale on the refresh (held still below) does not create another.
    let ack2 = json!({
        "command_id": new_id,
        "version": new_seq,
        "status": "rejected",
        "code": "stale",
        "at": "2026-11-04T05:11:00Z"
    });
    let (s, _) = app.req("POST", "/collar/v1/ack", Some(&key), Some(ack2)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM collar_rearms WHERE collar_id = ? AND status = 'pending'").bind(&id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(pending, 0);

    assert_eq!(op_protocol::RejectCode::Stale.code(), 6);
    assert_eq!(op_protocol::RejectCode::Expired.code(), 22);
    assert_eq!(op_protocol::RejectCode::Unsupported.code(), 24);
    assert_eq!(op_protocol::RejectCode::NotNow.code(), 28);
    assert_eq!(op_protocol::RejectCode::BadSig.code(), 1);
    assert_eq!(op_protocol::RejectCode::WrongCollar.code(), 3);
}
