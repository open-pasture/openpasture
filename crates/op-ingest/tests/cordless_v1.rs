//! Cordless protocol-v1 field-claim lifecycle (`prebind_v1`): USB provision
//! mints the bearer before the farmer scans the OC2 card; pre-claim reports
//! keep alive with 200; claim reuses the key; re-scan while in_service is
//! 409 (no key rotate); undo restores the card code and the same v1 key.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Duration;
use http_body_util::BodyExt;
use op_core::settings::CheckedCollarUrl;
use op_core::*;
use op_protocol::SigningKey;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Site {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    herd: String,
}

async fn farm() -> Site {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    ctx.set_collar_url(&CheckedCollarUrl::passed("https://farm.example.com".into())).await.unwrap();
    let now = time::now();
    let s = ctx.store();
    s.insert_farm(&Farm { id: "farm_1".into(), name: "Test".into(), timezone: "America/Chicago".into(), center: [-93.62, 42.03], created_at: now })
        .await
        .unwrap();
    s.insert_herd(&Herd {
        id: "herd_1".into(),
        name: "Cows".into(),
        species: Species::Cattle,
        count: 10,
        paddock_id: None,
        autonomy: Autonomy::Propose,
        timer_minutes: 60,
        created_at: now,
    })
    .await
    .unwrap();
    Site { _dir: dir, ctx, herd: "herd_1".into() }
}

impl Site {
    fn app(&self) -> axum::Router {
        op_core::with_identity(op_core::router().merge(op_ingest::router()).merge(op_fleet::router()), Identity::owner(Via::Local)).with_state(self.ctx.clone())
    }

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder().method(method).uri(uri);
        let req = match body {
            Some(b) => req.header("content-type", "application/json").body(Body::from(b.to_string())).unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = self.app().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())))
    }

    async fn collar(&self, method: &str, uri: &str, key: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri).header("authorization", format!("Bearer {key}"));
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = self.app().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) })
    }
}

fn device_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn new_unit(serial: &str, seed: u8) -> Value {
    json!({
        "serial": serial, "device_class": "node", "hw_rev": "N1.0", "fw_version": "1.0.0",
        "device_pubkey": op_protocol::encode_public_key(&device_key(seed).verifying_key()),
        "radios": { "lora_eui": "AC1F09FFFE0A1B2C", "ble_addr": "D4:12:AB:00:01:FF" },
        "station": "bench-1",
    })
}

fn run(rail: f64, minute: i64) -> Value {
    let end = time::now() - Duration::minutes(60 - minute);
    let steps: Vec<Value> = op_fleet::plans::node_steps()
        .into_iter()
        .map(|p| {
            let value = match (p.name.as_str(), p.limit_lo, p.limit_hi) {
                ("rail_3v3", _, _) => Some(rail),
                (_, Some(lo), Some(hi)) => Some((lo + hi) / 2.0),
                (_, Some(lo), None) => Some(lo),
                (_, None, Some(hi)) => Some(hi),
                _ => None,
            };
            json!({ "name": p.name, "status": "pass", "value": value, "unit": p.unit })
        })
        .collect();
    json!({
        "plan_id": "node-fct", "plan_version": 1, "station": "bench-1", "fixture_id": "fx-01",
        "started_at": time::to_db(&(end - Duration::seconds(55))), "finished_at": time::to_db(&end), "steps": steps,
    })
}

#[tokio::test]
async fn cordless_v1_prebind_lifecycle() {
    let f = farm().await;
    let serial = "OCN-2611-ROOK1";

    assert_eq!(f.call("POST", "/api/fleet/units", Some(new_unit(serial, 9))).await.0, StatusCode::CREATED);
    let (s, r) = f.call("POST", &format!("/api/fleet/units/{serial}/tests"), Some(run(3.31, 1))).await;
    assert_eq!(s, StatusCode::CREATED, "{r}");

    // USB provision with prebind_v1: mint collar id + bearer before claim.
    let (s, p) = f.call("POST", &format!("/api/fleet/units/{serial}/provision"), Some(json!({ "prebind_v1": true, "herd_id": f.herd }))).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let card = p["card"].as_str().unwrap().to_owned();
    let claim_code = p["claim_code"].as_str().unwrap().to_owned();
    let v1 = &p["v1"];
    let collar_id = {
        let line = v1["line"].as_str().unwrap();
        assert!(line.starts_with("provision {"));
        let json_part = line.strip_prefix("provision ").unwrap();
        let parsed: Value = serde_json::from_str(json_part).unwrap();
        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["h"], f.herd);
        assert_eq!(parsed["k"], v1["key"]);
        parsed["c"].as_str().unwrap().to_owned()
    };
    let bearer = v1["key"].as_str().unwrap().to_owned();
    assert_eq!(bearer.len(), 64);

    // Pre-claim keepalive: report/boundary/ack succeed without storing fixes.
    let (s, rep) = f
        .collar(
            "POST",
            "/collar/v1/report",
            &bearer,
            Some(json!({ "collar_id": collar_id, "fixes": [{ "at": time::to_db(&time::now()), "point": [-93.62, 42.03], "accuracy_m": 2.0, "sats": 8 }] })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{rep}");
    assert_eq!(rep, json!({ "latest_version": null }));
    let fixes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fixes").fetch_one(f.ctx.db()).await.unwrap();
    assert_eq!(fixes, 0, "pre-claim reports must not store grazing fixes");
    assert_eq!(f.collar("GET", "/collar/v1/boundary?have=0", &bearer, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(
        f.collar("POST", "/collar/v1/ack", &bearer, Some(json!({ "command_id": "bnd_x", "version": 1, "status": "applied", "at": time::to_db(&time::now()) })))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (s, e) = f.collar("POST", "/collar/v1/report", &bearer, Some(json!({ "collar_id": "col_wrong" }))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{e}");

    // Wrong herd on claim: Rook's provisioned "h" must match (transfer later).
    let (s, e) = f.call("POST", "/api/herds", Some(json!({ "name": "Other", "species": "cattle", "count": 0 }))).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    let other = e["id"].as_str().unwrap().to_owned();
    let (s, e) = f.call("POST", "/api/fleet/claim", Some(json!({ "card": card, "herd_id": other }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert_eq!(e["code"], "v1_herd_mismatch");
    assert_eq!(e["v1_herd_id"], f.herd);

    // Claim via OC2 card into the USB-bound herd: reuses the prebound key.
    let (s, c) = f.call("POST", "/api/fleet/claim", Some(json!({ "card": card, "herd_id": f.herd }))).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    assert_eq!(c["collar"]["id"], collar_id);
    assert_eq!(c["v1"]["key"], "");
    assert!(c["v1"]["why"].as_str().unwrap().contains("pre-bound"));
    let undo_id = c["undo_id"].as_str().unwrap().to_owned();
    let hash: String = sqlx::query_scalar("SELECT key_hash FROM collars WHERE id = ?").bind(&collar_id).fetch_one(f.ctx.db()).await.unwrap();
    assert_eq!(hash, op_core::keys::hash_key(&bearer));

    // Post-claim report + boundary push + ack with the original USB bearer.
    let (s, rep) = f
        .collar(
            "POST",
            "/collar/v1/report",
            &bearer,
            Some(json!({
                "collar_id": collar_id,
                "device": { "fw": "rook-0.1", "caps": ["holes", "slots", "collar_id", "cue_mode", "config"] },
                "fixes": [{ "at": time::to_db(&time::now()), "point": [-93.62, 42.03], "accuracy_m": 2.0, "sats": 9 }],
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{rep}");
    assert!(op_fleet::units::mark_in_service(&f.ctx, &collar_id).await.unwrap());
    assert_eq!(f.call("GET", &format!("/api/fleet/units/{serial}"), None).await.1["unit"]["state"], "in_service");

    let ring = json!({
        "type": "Polygon",
        "coordinates": [[
            [-93.621, 42.029], [-93.619, 42.029], [-93.619, 42.031], [-93.621, 42.031], [-93.621, 42.029]
        ]]
    });
    let (s, mv) = f.call("POST", &format!("/api/herds/{}/boundary", f.herd), Some(json!({ "geometry": ring }))).await;
    assert_eq!(s, StatusCode::CREATED, "{mv}");
    let (s, rep) = f.collar("POST", "/collar/v1/report", &bearer, Some(json!({ "collar_id": collar_id }))).await;
    assert_eq!(s, StatusCode::OK, "{rep}");
    let latest = rep["latest_version"].as_u64().expect("boundary version after push");
    let (s, bytes_status) = {
        let (s, v) = f.collar("GET", &format!("/collar/v1/boundary?have=0&free=1"), &bearer, None).await;
        (s, v)
    };
    assert_eq!(s, StatusCode::OK, "{bytes_status}");
    let cmd_id = bytes_status["command_id"].as_str().unwrap().to_owned();
    let version = bytes_status["version"].as_u64().unwrap();
    assert_eq!(version, latest);
    assert_eq!(
        f.collar(
            "POST",
            "/collar/v1/ack",
            &bearer,
            Some(json!({ "command_id": cmd_id, "version": version, "status": "applied", "at": time::to_db(&time::now()) })),
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );

    // Accidental OC2 re-scan while in_service: 409, key unchanged.
    let (s, e) = f.call("POST", "/api/fleet/claim", Some(json!({ "card": card, "herd_id": f.herd }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert_eq!(e["code"], "already_claimed");
    assert_eq!(e["collar_id"], collar_id);
    let hash2: String = sqlx::query_scalar("SELECT key_hash FROM collars WHERE id = ?").bind(&collar_id).fetch_one(f.ctx.db()).await.unwrap();
    assert_eq!(hash2, op_core::keys::hash_key(&bearer), "re-scan must not rotate the USB key");

    // Undo restores provisioned + same card code + same v1 key/herd.
    let (s, u) = f.call("POST", &format!("/api/collars/{collar_id}/undo"), None).await;
    assert_eq!(s, StatusCode::OK, "{u}");
    assert_eq!(u["claim_code"], claim_code);
    assert_eq!(u["undone"], "claim");
    let unit = f.call("GET", &format!("/api/fleet/units/{serial}"), None).await.1["unit"].clone();
    assert_eq!(unit["state"], "provisioned");
    assert_eq!(unit["claim_code"], claim_code);
    let (v1_hash, v1_herd, v1_cid): (String, String, String) =
        sqlx::query_as("SELECT v1_key_hash, v1_herd_id, v1_collar_id FROM fleet_units WHERE serial = ?").bind(serial).fetch_one(f.ctx.db()).await.unwrap();
    assert_eq!(v1_hash, op_core::keys::hash_key(&bearer));
    assert_eq!(v1_herd, f.herd);
    assert_eq!(v1_cid, collar_id);
    let _ = undo_id;

    // Pre-claim keepalive works again with the same bearer.
    let (s, rep) = f.collar("POST", "/collar/v1/report", &bearer, Some(json!({ "collar_id": collar_id }))).await;
    assert_eq!(s, StatusCode::OK, "{rep}");

    // Re-claim by the same QR card; same bearer still works.
    let (s, c2) = f.call("POST", "/api/fleet/claim", Some(json!({ "card": card, "herd_id": f.herd }))).await;
    assert_eq!(s, StatusCode::CREATED, "{c2}");
    assert_eq!(c2["collar"]["id"], collar_id);
    let (s, rep) = f.collar("POST", "/collar/v1/report", &bearer, Some(json!({ "collar_id": collar_id }))).await;
    assert_eq!(s, StatusCode::OK, "{rep}");
}
