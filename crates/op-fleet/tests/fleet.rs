//! op-fleet end to end through its router: a unit from registration through
//! factory tests, provisioning, its label and claim into a herd, to service;
//! batch yield; a card claimed on another server; firmware releases and the
//! rollout's gates; the herd network; the collar console; station tokens.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Duration;
use http_body_util::BodyExt;
use op_core::*;
use op_protocol::SigningKey;
use op_protocol::v2::firmware::Manifest;
use op_protocol::v2::registry::DeviceClass;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

struct Site {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    herd: String,
}

async fn farm() -> Site {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let now = time::now();
    let s = ctx.store();
    s.insert_farm(&Farm { id: "farm_1".into(), name: "Test".into(), timezone: "America/Chicago".into(), center: [-93.62, 42.03], created_at: now }).await.unwrap();
    s.insert_herd(&Herd { id: "herd_1".into(), name: "Cows".into(), species: Species::Cattle, count: 10, paddock_id: None, autonomy: Autonomy::Propose, timer_minutes: 60, created_at: now })
        .await
        .unwrap();
    s.insert_animal(&Animal { id: "ani_1".into(), tag: "101".into(), herd_id: "herd_1".into(), ..Default::default() }).await.unwrap();
    Site { _dir: dir, ctx, herd: "herd_1".into() }
}

fn owner() -> Identity {
    Identity::owner(Via::Local)
}

fn manager() -> Identity {
    Identity { role: Role::Manager, user_id: Some("usr_m".into()), name: Some("Sam".into()), via: Via::UserToken }
}

impl Site {
    fn app(&self, who: Identity) -> Router {
        op_core::with_identity(op_fleet::router(), who).with_state(self.ctx.clone())
    }

    async fn call_as(&self, who: Identity, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder().method(method).uri(uri);
        let req = match body {
            Some(b) => req.header("content-type", "application/json").body(Body::from(b.to_string())).unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = self.app(who).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())))
    }

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call_as(owner(), method, uri, body).await
    }

    async fn raw(&self, method: &str, uri: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
        let req = Request::builder().method(method).uri(uri).header("content-type", "application/octet-stream").body(Body::from(body)).unwrap();
        let res = self.app(owner()).oneshot(req).await.unwrap();
        let status = res.status();
        (status, res.into_body().collect().await.unwrap().to_bytes().to_vec())
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

/// A run of the shipped node plan: every step passing, `rail_3v3` at `rail` volts.
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

/// Register, fail once, pass, provision: a unit ready to claim.
async fn provisioned(f: &Site, serial: &str, seed: u8) -> Value {
    assert_eq!(f.call("POST", "/api/fleet/units", Some(new_unit(serial, seed))).await.0, StatusCode::CREATED);
    let (s, r) = f.call("POST", &format!("/api/fleet/units/{serial}/tests"), Some(run(3.31, 1))).await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    let (s, p) = f.call("POST", &format!("/api/fleet/units/{serial}/provision"), None).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    p
}

#[tokio::test]
async fn a_unit_from_registration_to_service() {
    let f = farm().await;
    let serial = "OCN-2611-00001";

    // Registered: idempotent on serial, the key can't change without the owner.
    let (s, u) = f.call_as(manager(), "POST", "/api/fleet/units", Some(new_unit(serial, 1))).await;
    assert_eq!(s, StatusCode::CREATED, "{u}");
    assert_eq!(u["state"], "built");
    let kid = op_fleet::keys::kid(&device_key(1).verifying_key());
    assert_eq!((u["device_kid"].as_str(), u["device_id"].as_str()), (Some(kid.as_str()), Some(kid.as_str())));
    assert_eq!(f.call_as(manager(), "POST", "/api/fleet/units", Some(new_unit(serial, 1))).await.0, StatusCode::OK);
    let (s, e) = f.call_as(manager(), "POST", "/api/fleet/units", Some(new_unit(serial, 2))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    let mut forced = new_unit(serial, 2);
    forced["force"] = json!(true);
    assert_eq!(f.call_as(manager(), "POST", "/api/fleet/units", Some(forced.clone())).await.0, StatusCode::FORBIDDEN);
    let mut bad = new_unit("ocn-lower", 3);
    bad["serial"] = json!("ocn-lower");
    assert_eq!(f.call("POST", "/api/fleet/units", Some(bad)).await.0, StatusCode::BAD_REQUEST);
    let (s, e) = f.call("POST", "/api/fleet/units", Some(new_unit("OCN-2611-00099", 1))).await;
    assert_eq!(s, StatusCode::CONFLICT, "two units can't share a key: {e}");

    // The shipped plan, a failing run, then a passing one.
    let (s, plan) = f.call("GET", "/api/fleet/test-plans/node", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((plan["id"].as_str(), plan["version"].as_i64(), plan["shipped"].as_bool()), (Some("node-fct"), Some(1), Some(true)));
    assert_eq!(f.call("GET", "/api/fleet/test-plans/trough", None).await.0, StatusCode::NOT_FOUND);
    let (s, r) = f.call("POST", &format!("/api/fleet/units/{serial}/tests"), Some(run(2.1, 0))).await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    assert_eq!(r["result"], "fail");
    assert_eq!(r["failed"], json!(["rail_3v3"]));
    let (s, e) = f.call("POST", &format!("/api/fleet/units/{serial}/provision"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "a failed unit isn't provisioned: {e}");
    let (_, r) = f.call("POST", &format!("/api/fleet/units/{serial}/tests"), Some(run(3.3, 5))).await;
    assert_eq!(r["result"], "pass");
    assert_eq!(f.call("GET", &format!("/api/fleet/units/{serial}"), None).await.1["unit"]["state"], "tested");

    // Provisioned: a claim code, its card, a factory record this server signed.
    let (s, p) = f.call("POST", &format!("/api/fleet/units/{serial}/provision"), None).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let code = p["claim_code"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 16);
    let card = op_protocol::v2::identity::Card::parse(p["card"].as_str().unwrap()).unwrap();
    assert_eq!((card.serial.as_str(), card.claim_code.as_str()), (serial, code.as_str()));
    assert_eq!(p["link"]["transport"], "lora");
    assert_eq!(p["link"]["lora_region"], "US915");
    assert!(p.get("endpoint").is_none(), "no collar address on this server");
    let rec = &p["factory_record"];
    assert_eq!(rec["verified"], true);
    assert_eq!(rec["record"]["serial"], serial);
    assert_eq!(rec["record"]["maker"], "Test", "the farm's name by default");
    assert_eq!(rec["record"]["test"]["passed"], true);
    let bytes = op_fleet::keys::unhex(rec["cose"].as_str().unwrap()).unwrap();
    let (_, read) = op_protocol::v2::identity::FactoryRecord::read(&bytes).unwrap();
    let check = op_protocol::v2::identity::claim_check(&op_protocol::v2::identity::claim_key(&code, &read.device_id()));
    assert_eq!(read.claim_check, Some(check), "the record's claim check is the code's");
    let (_, again) = f.call("POST", &format!("/api/fleet/units/{serial}/provision"), None).await;
    assert_eq!(again["claim_code"], code.as_str(), "the label stays good");

    // The label.
    let (s, svg) = f.call("GET", &format!("/api/fleet/units/{serial}/label.svg"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(svg.as_str().unwrap().contains(&op_fleet::provision::grouped(&code)));

    // Claimed into the herd, on an animal: a collar, and the v1 key for today's firmware.
    let wrong = json!({ "claim_code": "0000000000000000", "herd_id": f.herd });
    assert_eq!(f.call("POST", &format!("/api/fleet/units/{serial}/claim"), Some(wrong)).await.0, StatusCode::NOT_FOUND);
    let typed = op_fleet::provision::grouped(&code).to_lowercase();
    let (s, c) = f.call("POST", &format!("/api/fleet/units/{serial}/claim"), Some(json!({ "claim_code": typed, "herd_id": f.herd, "animal_id": "ani_1" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    let collar_id = c["collar"]["id"].as_str().unwrap().to_owned();
    assert_eq!(c["collar"]["name"], serial);
    assert_eq!(c["unit"]["state"], "claimed");
    assert_eq!(c["unit"]["collar_id"], collar_id.as_str());
    assert_eq!(c["unit"]["herd_id"], "herd_1");
    assert_eq!(c["v1"]["key"].as_str().unwrap().len(), 64);
    assert!(c["v1"]["why"].as_str().unwrap().contains("collar address"));
    assert_eq!(f.ctx.store().get_animal("ani_1").await.unwrap().unwrap().collar_id.as_deref(), Some(collar_id.as_str()));
    let (s, e) = f.call("POST", "/api/fleet/claim", Some(json!({ "claim_code": code, "herd_id": f.herd }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert!(e["error"].as_str().unwrap().contains("Cows"));
    assert_eq!(f.call("POST", &format!("/api/fleet/units/{serial}/tests"), Some(run(3.3, 9))).await.0, StatusCode::CONFLICT);

    // In service from the collar's first report.
    sqlx::query("UPDATE collars SET last_seen = ? WHERE id = ?").bind(time::to_db(&time::now())).bind(&collar_id).execute(f.ctx.db()).await.unwrap();
    assert!(op_fleet::units::mark_in_service(&f.ctx, &collar_id).await.unwrap());
    let (_, d) = f.call("GET", &format!("/api/fleet/units/{serial}"), None).await;
    assert_eq!(d["unit"]["state"], "in_service");
    assert!(d["unit"].get("claim_code").is_none(), "kept only as long as a claim takes");
    let kinds: Vec<&str> = d["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["registered", "tested", "tested", "provisioned", "provisioned", "claimed", "in_service"]);
    assert_eq!(d["runs"].as_array().unwrap().len(), 2);
    assert_eq!(d["factory_record"]["verified"], true);

    // Back for repair: RMA, retested, re-provisioned returns it to its collar.
    let (_, u) = f.call("POST", &format!("/api/fleet/units/{serial}/state"), Some(json!({ "state": "rma", "note": "buzzer" }))).await;
    assert_eq!(u["state"], "rma");
    f.call("POST", &format!("/api/fleet/units/{serial}/tests"), Some(run(3.3, 20))).await;
    let (_, p) = f.call("POST", &format!("/api/fleet/units/{serial}/provision"), None).await;
    let code2 = p["claim_code"].as_str().unwrap();
    assert!(code2.len() == 16 && code2 != code, "the old code was forgotten once in service");
    let (_, d) = f.call("GET", &format!("/api/fleet/units/{serial}"), None).await;
    assert_eq!((d["unit"]["state"].as_str(), d["unit"]["collar_id"].as_str()), (Some("claimed"), Some(collar_id.as_str())));

    // Batch views.
    let (s, y) = f.call("GET", "/api/fleet/yield?plan_id=node-fct", None).await;
    assert_eq!(s, StatusCode::OK, "{y}");
    assert_eq!((y["runs"].as_i64(), y["units"].as_i64()), (Some(3), Some(1)));
    assert_eq!(y["first_pass"]["passed"], 0);
    assert_eq!(y["final"]["passed"], 1);
    assert_eq!(y["failures"], json!([{ "name": "rail_3v3", "fail": 1 }]));
    let rail = y["steps"].as_array().unwrap().iter().find(|s| s["name"] == "rail_3v3").unwrap().clone();
    assert_eq!((rail["pass"].as_i64(), rail["fail"].as_i64()), (Some(2), Some(1)));
    let (_, t) = f.call("GET", "/api/fleet/throughput?bucket=hour", None).await;
    assert_eq!(t["stations"][0]["station"], "bench-1");
    assert_eq!(t["stations"][0]["runs"], 3);
    let (_, sum) = f.call("GET", "/api/fleet/summary", None).await;
    assert_eq!(sum["by_state"]["claimed"], 1);
}

#[tokio::test]
async fn the_owner_can_replace_a_key_and_edit_plans() {
    let f = farm().await;
    let serial = "OCN-2611-00002";
    f.call("POST", "/api/fleet/units", Some(new_unit(serial, 4))).await;
    let mut forced = new_unit(serial, 5);
    forced["force"] = json!(true);
    let (s, u) = f.call("POST", "/api/fleet/units", Some(forced)).await;
    assert_eq!(s, StatusCode::OK, "{u}");
    assert_eq!(u["device_kid"], op_fleet::keys::kid(&device_key(5).verifying_key()).as_str());
    let (_, d) = f.call("GET", &format!("/api/fleet/units/{serial}"), None).await;
    let keys = d["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys[0]["to"].is_string() && keys[1].get("to").is_none());

    let steps = json!([{ "name": "rail_3v3", "kind": "measure", "limit_lo": 3.25, "limit_hi": 3.35, "unit": "V", "required": true }]);
    let (s, p) = f.call("PUT", "/api/fleet/test-plans/node", Some(json!({ "steps": steps }))).await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    assert_eq!((p["id"].as_str(), p["version"].as_i64(), p["shipped"].as_bool()), (Some("node-fct"), Some(2), Some(false)));
    assert_eq!(f.call("GET", "/api/fleet/test-plans/node?version=1", None).await.1["version"], 1);
    let (s, p) = f.call("PUT", "/api/fleet/test-plans/trough", Some(json!({ "steps": steps }))).await;
    assert_eq!((s, p["id"].as_str()), (StatusCode::CREATED, Some("trough-fct")));
    let bad = json!({ "steps": [{ "name": "imu", "kind": "check", "limit_lo": 1, "required": true }] });
    assert_eq!(f.call("PUT", "/api/fleet/test-plans/node", Some(bad)).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.call("GET", "/api/fleet/test-plans", None).await.1.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn another_server_claims_from_the_card() {
    let maker = farm().await;
    let p = provisioned(&maker, "OCN-2611-00003", 6).await;
    let card = p["card"].as_str().unwrap().to_owned();
    let record = p["factory_record"]["cose"].as_str().unwrap().to_owned();

    let farm2 = farm().await;
    // A server that never saw the unit takes the card at its word; the collar
    // itself checks the code when the bind reaches it.
    let wrong = card.rsplit_once(':').map(|(head, _)| format!("{head}:0000000000000000")).unwrap();
    let (s, _) = farm2.call("POST", "/api/fleet/claim", Some(json!({ "card": wrong, "herd_id": "herd_1" }))).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, e) = farm2.call("POST", "/api/fleet/claim", Some(json!({ "card": card, "herd_id": "herd_1" }))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "the same serial now has another code here: {e}");

    let farm3 = farm().await;
    let (s, c) = farm3.call("POST", "/api/fleet/claim", Some(json!({ "card": card, "factory_record": record, "herd_id": "herd_1" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    assert_eq!(c["unit"]["origin"], "card");
    assert_eq!(c["unit"]["device_class"], "node");
    assert_eq!(c["unit"]["maker_verified"], false, "an unknown maker is shown as unverified");

    // Enrol the maker's key and the next unit from them verifies.
    let maker_pk = op_fleet::keys::maker_key(maker.ctx.data_dir()).unwrap().verifying_key();
    let (s, k) = farm3.call("POST", "/api/fleet/signing-keys", Some(json!({ "name": "Test farm", "public_key": op_protocol::encode_public_key(&maker_pk), "purpose": "maker" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{k}");
    let p2 = provisioned(&maker, "OCN-2611-00004", 7).await;
    let (s, c) = farm3.call("POST", "/api/fleet/claim", Some(json!({ "card": p2["card"], "factory_record": p2["factory_record"]["cose"], "herd_id": "herd_1" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    assert_eq!(c["unit"]["maker_verified"], true);
}

fn manifest(image: &[u8], key: &SigningKey, version: &str) -> String {
    let m = Manifest {
        version: version.into(),
        digest: Sha256::digest(image).into(),
        size: image.len() as u64,
        classes: vec![DeviceClass::Node],
        hw_revs: vec!["N1.0".into()],
        security_counter: 1,
        min_battery: Some(50),
        revoke: vec![],
    };
    op_fleet::keys::hex(&m.sign(key))
}

async fn rollout_state(f: &Site, collar_id: &str) -> (String, Option<String>) {
    op_fleet::firmware::rollout::tick(&f.ctx).await.unwrap();
    let s = op_fleet::firmware::status_for_collar(&f.ctx, collar_id).await.unwrap().unwrap();
    let v = serde_json::to_value(&s).unwrap();
    (v["state"].as_str().unwrap().to_owned(), v["reason"].as_str().map(str::to_owned))
}

#[tokio::test]
async fn firmware_goes_out_only_when_the_collar_can_take_it() {
    let f = farm().await;
    let serial = "OCN-2611-00005";
    provisioned(&f, serial, 8).await;
    let p = f.call("GET", &format!("/api/fleet/units/{serial}"), None).await.1;
    let code = p["unit"]["claim_code"].as_str().unwrap().to_owned();
    let (_, c) = f.call("POST", "/api/fleet/claim", Some(json!({ "claim_code": code, "herd_id": f.herd }))).await;
    let collar = c["collar"]["id"].as_str().unwrap().to_owned();
    sqlx::query("UPDATE collars SET fw = '1.0.0', battery = 0.9, last_seen = ? WHERE id = ?").bind(time::to_db(&time::now())).bind(&collar).execute(f.ctx.db()).await.unwrap();

    // A release signed by a key this server doesn't trust is refused.
    let image = b"opencollar node firmware 1.1.0".to_vec();
    let key = SigningKey::from_bytes(&[42; 32]);
    let (s, e) = f.call("POST", "/api/fleet/firmware", Some(json!({ "manifest": manifest(&image, &key, "1.1.0") }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    let (s, k) = f.call("POST", "/api/fleet/signing-keys", Some(json!({ "name": "Bench builds", "public_key": op_protocol::encode_public_key(&key.verifying_key()) }))).await;
    assert_eq!(s, StatusCode::CREATED, "{k}");
    assert_eq!(k["purpose"], "image");
    let (s, fw) = f.call("POST", "/api/fleet/firmware", Some(json!({ "manifest": manifest(&image, &key, "1.1.0"), "notes": "faster fixes" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{fw}");
    assert_eq!(fw["classes"], json!(["node"]));
    assert_eq!(fw["has_image"], false);
    let fw_id = fw["id"].as_str().unwrap().to_owned();
    assert_eq!(f.raw("PUT", &format!("/api/fleet/firmware/{fw_id}/image"), b"tampered image bytes here!!!!!".to_vec()).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.raw("PUT", &format!("/api/fleet/firmware/{fw_id}/image"), image.clone()).await.0, StatusCode::OK);
    assert_eq!(op_fleet::firmware::image_bytes(&f.ctx, fw["sha256"].as_str().unwrap()).await.unwrap(), Some(image.clone()));

    // Automatic by default; this collar has no fota capability.
    let (state, reason) = rollout_state(&f, &collar).await;
    assert_eq!(state, "unsupported");
    assert_eq!(reason.as_deref(), Some(op_fleet::firmware::rollout::UNSUPPORTED));
    let rollouts = f.call("GET", "/api/fleet/firmware/rollouts", None).await.1;
    assert_eq!(rollouts.as_array().unwrap().len(), 1);
    assert_eq!(rollouts[0]["created_by"], "auto");
    sqlx::query(r#"UPDATE collars SET caps = '["holes","firmware"]', battery = 0.3 WHERE id = ?"#).bind(&collar).execute(f.ctx.db()).await.unwrap();
    assert_eq!(rollout_state(&f, &collar).await, ("held".into(), Some("Battery 30 %, needs 50 %".into())));
    sqlx::query("UPDATE collars SET battery = 0.9 WHERE id = ?").bind(&collar).execute(f.ctx.db()).await.unwrap();
    let at = time::to_db(&time::now());
    sqlx::query("INSERT INTO moves (id, herd_id, decision_id, target, status, warn_m, hysteresis_m, started_at, updated_at) VALUES ('mov_1', 'herd_1', 'dec_1', '{}', 'sweeping', 3, 1, ?, ?)")
        .bind(&at)
        .bind(&at)
        .execute(f.ctx.db())
        .await
        .unwrap();
    assert_eq!(rollout_state(&f, &collar).await, ("held".into(), Some("The herd is being moved".into())));
    assert_eq!(op_fleet::firmware::offer_for(&f.ctx, &collar).await.unwrap(), None);
    sqlx::query("UPDATE moves SET status = 'done'").execute(f.ctx.db()).await.unwrap();
    assert_eq!(rollout_state(&f, &collar).await.0, "ready");

    // Offered in the firmware command, then confirmed from the collar's version.
    let offer = op_fleet::firmware::offer_for(&f.ctx, &collar).await.unwrap().unwrap();
    assert_eq!(offer.version, "1.1.0");
    assert!(Manifest::read(&offer.manifest).is_ok(), "the signed manifest, as is");
    assert_eq!(rollout_state(&f, &collar).await.0, "offered");
    sqlx::query("UPDATE collars SET fw = '1.1.0' WHERE id = ?").bind(&collar).execute(f.ctx.db()).await.unwrap();
    assert_eq!(rollout_state(&f, &collar).await.0, "current", "the rollout is done and the collar runs the release");
    let r = f.call("GET", "/api/fleet/firmware/rollouts", None).await.1[0].clone();
    assert_eq!(r["state"], "done");
    assert_eq!(r["counts"]["confirmed"], 1);
    let units = f.call("GET", &format!("/api/fleet/firmware/units?rollout_id={}", r["id"].as_str().unwrap()), None).await.1;
    assert_eq!((units[0]["state"].as_str(), units[0]["current"].as_str()), (Some("confirmed"), Some("1.1.0")));

    // A collar that goes back down is a rollback, and the next rollout pauses on it.
    let newer = b"opencollar node firmware 1.2.0".to_vec();
    f.call("POST", "/api/fleet/firmware", Some(json!({ "manifest": manifest(&newer, &key, "1.2.0") }))).await;
    op_fleet::firmware::rollout::tick(&f.ctx).await.unwrap();
    let id = f.call("GET", "/api/fleet/firmware/rollouts", None).await.1[0]["id"].as_str().unwrap().to_owned();
    op_fleet::firmware::record_result(&f.ctx, &collar, "1.2.0", &op_fleet::firmware::FwResult::RolledBack).await.unwrap();
    assert_eq!(rollout_state(&f, &collar).await.0, "rolled_back");
    let rollouts = f.call("GET", "/api/fleet/firmware/rollouts", None).await.1;
    let r = rollouts.as_array().unwrap().iter().find(|r| r["id"] == id.as_str()).unwrap();
    assert_eq!(r["state"], "paused");
    assert!(r["paused_reason"].as_str().unwrap().contains("rolled back"), "{r}");
    let (s, r) = f.call("POST", &format!("/api/fleet/firmware/rollouts/{id}/resume"), None).await;
    assert_eq!((s, r["state"].as_str()), (StatusCode::OK, Some("paused")), "a rollback pauses it again at once");

    // The farmer pauses everything.
    let (s, p) = f.call_as(manager(), "PUT", "/api/fleet/firmware/policy", Some(json!({ "mode": "paused", "channel": "stable", "battery_floor": 0.5, "wave_hours": 24 }))).await;
    assert_eq!((s, p["mode"].as_str()), (StatusCode::OK, Some("paused")));
    let (s, _) = f.call("PUT", "/api/fleet/firmware/policy", Some(json!({ "mode": "auto", "channel": "stable", "battery_floor": 0.2, "wave_hours": 24 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "never below the collar's own floor");
}

#[tokio::test]
async fn the_herd_network_and_the_console() {
    let f = farm().await;
    let serial = "OCN-2611-00006";
    let p = provisioned(&f, serial, 9).await;
    let (_, c) = f.call("POST", "/api/fleet/claim", Some(json!({ "claim_code": p["claim_code"], "herd_id": f.herd }))).await;
    let collar = c["collar"]["id"].as_str().unwrap().to_owned();

    // Before its first report: waiting, nothing heard.
    let (s, con) = f.call("GET", &format!("/api/fleet/collars/{collar}/console"), None).await;
    assert_eq!(s, StatusCode::OK, "{con}");
    assert_eq!(con["contact"]["state"], "waiting");
    assert_eq!(con["unit"]["serial"], serial);
    assert_eq!(con["network"]["role"], "node");
    assert_eq!(con["network"]["quality"], "unknown");
    assert_eq!(con["firmware"]["state"], "unsupported");

    // A fixed anchor beacons; the node is heard through it.
    let (s, a) = f.call("POST", "/api/fleet/anchors", Some(json!({ "name": "Windmill", "point": [-93.62, 42.03], "herd_ids": ["herd_1"] }))).await;
    assert_eq!(s, StatusCode::CREATED, "{a}");
    let anchor = a["id"].as_str().unwrap().to_owned();
    let now = time::now();
    op_fleet::network::record_beacon(&f.ctx, &op_fleet::network::Beacon { anchor_id: anchor.clone(), herd_id: f.herd.clone(), lte_rsrp_dbm: Some(-98.0), at: now }).await.unwrap();
    op_fleet::network::record_uplink(&f.ctx, &op_fleet::network::Uplink { collar_id: collar.clone(), role: None, via: Some(anchor.clone()), rssi_dbm: Some(-95.0), snr_db: Some(6.5), at: now })
        .await
        .unwrap();
    let (s, n) = f.call("GET", &format!("/api/fleet/network/{}", f.herd), None).await;
    assert_eq!(s, StatusCode::OK, "{n}");
    assert_eq!(n["active_anchor"], anchor.as_str());
    assert_eq!(n["anchors"][0]["source"], "fixed");
    assert_eq!(n["anchors"][0]["lte_rsrp_dbm"], -98.0);
    let m = &n["members"][0];
    assert_eq!((m["role"].as_str(), m["quality"].as_str(), m["via"].as_str()), (Some("node"), Some("good"), Some(anchor.as_str())));
    assert_eq!(n["gaps"], json!([]));

    // Not heard for an hour: a gap, at its last fix.
    let old = time::to_db(&(now - Duration::hours(1)));
    sqlx::query("UPDATE fleet_links SET heard_at = ? WHERE collar_id = ?").bind(&old).bind(&collar).execute(f.ctx.db()).await.unwrap();
    sqlx::query(r#"UPDATE collars SET last_fix = '{"at":"2026-10-26T12:00:00Z","point":[-93.621,42.031],"accuracy_m":2.0,"sats":9}' WHERE id = ?"#).bind(&collar).execute(f.ctx.db()).await.unwrap();
    let n = f.call("GET", &format!("/api/fleet/network/{}", f.herd), None).await.1;
    assert_eq!(n["members"][0]["quality"], "lost");
    assert_eq!(n["gaps"][0]["point"], json!([-93.621, 42.031]));

    // Reports land: raw health, the latest fix with HDOP, a cue with no animal, a rejected boundary.
    let t = time::now();
    let at = time::to_db(&t);
    sqlx::query("UPDATE collars SET last_seen = ? WHERE id = ?").bind(&at).bind(&collar).execute(f.ctx.db()).await.unwrap();
    sqlx::query("INSERT INTO health (collar_id, herd_id, at, t, battery, battery_v, charging, temp_c, uptime_s, reset, rsrp_dbm, cell_mode) VALUES (?, 'herd_1', ?, ?, 0.81, 3.31, 1, 21.5, 86400, 'power_on', -104, 'ltem')")
        .bind(&collar)
        .bind(&at)
        .bind(time::unix_ms(&t))
        .execute(f.ctx.db())
        .await
        .unwrap();
    let fix_at = t - Duration::seconds(12);
    sqlx::query("INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m, sats, cn0, hdop) VALUES (?, 'herd_1', ?, ?, -93.62, 42.03, 1.8, 9, 41.0, 0.9)")
        .bind(&collar)
        .bind(time::to_db(&fix_at))
        .bind(time::unix_ms(&fix_at))
        .execute(f.ctx.db())
        .await
        .unwrap();
    sqlx::query("INSERT INTO cues (collar_id, herd_id, at, t, level, margin_m, kind, ring) VALUES (?, 'herd_1', ?, ?, 2, -1.4, 'outside', 0)")
        .bind(&collar)
        .bind(&at)
        .bind(time::unix_ms(&t))
        .execute(f.ctx.db())
        .await
        .unwrap();
    sqlx::query("INSERT INTO acks (collar_id, herd_id, command_id, version, status, code, at, received_at) VALUES (?, 'herd_1', 'bnd_1', 60, 'rejected', 'hole_too_close', ?, ?)")
        .bind(&collar)
        .bind(&at)
        .bind(&at)
        .execute(f.ctx.db())
        .await
        .unwrap();
    let con = f.call("GET", &format!("/api/fleet/collars/{collar}/console"), None).await.1;
    assert_eq!(con["contact"]["state"], "heard");
    assert_eq!(con["contact"]["latency_s"], 12.0);
    assert_eq!(con["health"]["battery_v"], 3.31);
    assert_eq!(con["health"]["charging"], true);
    assert_eq!(con["health"]["cell"]["rsrp_dbm"], -104.0);
    assert_eq!((con["fix"]["sats"].as_i64(), con["fix"]["hdop"].as_f64()), (Some(9), Some(0.9)));
    assert_eq!(con["cues"][0]["kind"], "outside");
    assert_eq!(con["acks"][0]["code"], "hole_too_close");
    assert_eq!(f.call("GET", "/api/fleet/collars/col_none/console", None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn station_tokens_do_five_things_until_revoked() {
    let f = farm().await;
    let (s, created) = f.call("POST", "/api/fleet/stations", Some(json!({ "name": "bench-1" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let token = created["token"].as_str().unwrap().to_owned();
    let id = created["station"]["id"].as_str().unwrap().to_owned();
    assert_eq!(f.call("POST", "/api/fleet/stations", Some(json!({ "name": "bench-1" }))).await.0, StatusCode::CONFLICT);

    let me = op_fleet::stations::admit(&f.ctx, &token, &Method::POST, "/api/fleet/units").await.unwrap();
    assert_eq!((me.role, me.name.as_deref()), (Role::Manager, Some("bench-1")));
    let e = op_fleet::stations::admit(&f.ctx, &token, &Method::GET, "/api/fleet/units").await.unwrap_err();
    assert_eq!(e.status, StatusCode::FORBIDDEN);

    // Its name goes on what it registers and tests.
    let mut body = new_unit("OCN-2611-00007", 10);
    body.as_object_mut().unwrap().remove("station");
    let (s, u) = f.call_as(me.clone(), "POST", "/api/fleet/units", Some(body)).await;
    assert_eq!(s, StatusCode::CREATED, "{u}");
    assert_eq!(u["station"], "bench-1");
    let (_, d) = f.call("GET", "/api/fleet/units/OCN-2611-00007", None).await;
    assert_eq!(d["events"][0]["by"], "bench-1");
    let listed = f.call("GET", "/api/fleet/stations", None).await.1;
    assert!(listed[0]["last_used_at"].is_string());

    f.call("POST", &format!("/api/fleet/stations/{id}/revoke"), None).await;
    let e = op_fleet::stations::admit(&f.ctx, &token, &Method::POST, "/api/fleet/units").await.unwrap_err();
    assert_eq!(e.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_release_goes_to_one_collar_then_its_herd_then_every_herd() {
    let f = farm().await;
    let now = time::now();
    f.ctx
        .store()
        .insert_herd(&Herd { id: "herd_2".into(), name: "Heifers".into(), species: Species::Cattle, count: 5, paddock_id: None, autonomy: Autonomy::Propose, timer_minutes: 60, created_at: now })
        .await
        .unwrap();
    let mut collars = Vec::new();
    for (i, herd) in [(11u8, "herd_1"), (12, "herd_1"), (13, "herd_2")] {
        let serial = format!("OCN-2611-000{i}");
        let p = provisioned(&f, &serial, i).await;
        let (_, c) = f.call("POST", "/api/fleet/claim", Some(json!({ "claim_code": p["claim_code"], "herd_id": herd }))).await;
        let id = c["collar"]["id"].as_str().unwrap().to_owned();
        sqlx::query(r#"UPDATE collars SET fw = '1.0.0', battery = 0.9, caps = '["firmware"]' WHERE id = ?"#).bind(&id).execute(f.ctx.db()).await.unwrap();
        collars.push(id);
    }
    let key = SigningKey::from_bytes(&[43; 32]);
    f.call("POST", "/api/fleet/signing-keys", Some(json!({ "name": "Bench", "public_key": op_protocol::encode_public_key(&key.verifying_key()) }))).await;
    let image = b"node 1.1.0".to_vec();
    let (_, fw) = f.call("POST", "/api/fleet/firmware", Some(json!({ "manifest": manifest(&image, &key, "1.1.0") }))).await;
    f.raw("PUT", &format!("/api/fleet/firmware/{}/image", fw["id"].as_str().unwrap()), image).await;

    op_fleet::firmware::rollout::tick(&f.ctx).await.unwrap();
    let r = f.call("GET", "/api/fleet/firmware/rollouts", None).await.1[0].clone();
    let first = r["waves"][0]["collar_id"].as_str().unwrap().to_owned();
    assert_eq!(r["waves"][1]["herd_id"], "herd_1", "the herd with the most collars goes first");
    let other = collars.iter().find(|c| c.as_str() != first && collars[..2].contains(c)).unwrap().clone();
    let states = || async {
        let mut out = Vec::new();
        for c in [&first, &other, &collars[2]] {
            out.push(rollout_state(&f, c).await);
        }
        out
    };
    let s = states().await;
    assert_eq!(s[0].0, "ready");
    assert_eq!(s[1], ("waiting".into(), Some("After the first collar confirms".into())));
    assert_eq!(s[2], ("waiting".into(), Some("After the first herd".into())));

    // Confirmed, but the herd waits its 24 h.
    sqlx::query("UPDATE collars SET fw = '1.1.0' WHERE id = ?").bind(&first).execute(f.ctx.db()).await.unwrap();
    assert_eq!(states().await[1].0, "waiting");
    let backdate = |hours: i64| {
        let id = r["id"].as_str().unwrap().to_owned();
        let ctx = f.ctx.clone();
        async move {
            let at = serde_json::to_value(time::now() - Duration::hours(hours)).unwrap();
            sqlx::query("UPDATE fleet_rollouts SET waves = json_set(waves, '$[0].started_at', ?) WHERE id = ?").bind(at.as_str().unwrap()).bind(&id).execute(ctx.db()).await.unwrap();
        }
    };
    backdate(25).await;
    let s = states().await;
    assert_eq!((s[1].0.as_str(), s[2].0.as_str()), ("ready", "waiting"));
    backdate(73).await;
    assert_eq!(states().await[2].0, "ready", "every herd after 72 h");
}
