//! op-fleet through the guard: a factory station's token reaches its five
//! routes and nothing else, stops working when revoked, and the fleet API is
//! for managers and up. Fleet care (`GET /api/fleet`, op-analytics) keeps
//! its routes beside it.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::Ctx;
use op_server::{ServerInfo, build_app};
use serde_json::{Value, json};
use tower::ServiceExt;

const PEER: &str = "127.0.0.1:50000";
const HOST: &str = "127.0.0.1:7878";

/// From off this machine (a forwarding header), with `token` when given.
async fn away(app: &Router, token: Option<&str>, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri).header("host", HOST).header("x-forwarded-for", "203.0.113.9");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(ConnectInfo(PEER.parse::<SocketAddr>().unwrap()));
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_station_token_reaches_its_routes_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let app = build_app(ctx.clone(), ServerInfo { data_dir: dir.path().into(), bind: "127.0.0.1".into(), port: 7878, lan_url: None }, false);
    let owner = ctx.settings().await.unwrap().server.app_token;

    let (s, created) = away(&app, Some(&owner), "POST", "/api/fleet/stations", Some(json!({ "name": "bench-1" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let station = created["token"].as_str().unwrap().to_owned();
    let id = created["station"]["id"].as_str().unwrap().to_owned();

    // Any Ed25519 public key stands in for the unit's: another server's.
    let other = tempfile::tempdir().unwrap();
    let device_key = Ctx::open(other.path()).await.unwrap().public_key_b64();
    let unit = json!({ "serial": "OCN-2611-00042", "device_class": "node", "hw_rev": "N1.0", "fw_version": "1.0.0", "device_pubkey": device_key });
    let (s, u) = away(&app, Some(&station), "POST", "/api/fleet/units", Some(unit.clone())).await;
    assert_eq!(s, StatusCode::CREATED, "{u}");
    assert_eq!(u["station"], "bench-1");
    let (s, plan) = away(&app, Some(&station), "GET", "/api/fleet/test-plans/node", None).await;
    assert_eq!((s, plan["id"].as_str()), (StatusCode::OK, Some("node-fct")));
    let (s, e) = away(&app, Some(&station), "POST", "/api/fleet/units/OCN-2611-00042/provision", None).await;
    assert_eq!(s, StatusCode::CONFLICT, "not tested yet: {e}");

    for (method, path) in [
        ("GET", "/api/fleet/units"),
        ("GET", "/api/fleet/units/OCN-2611-00042"),
        ("POST", "/api/fleet/units/OCN-2611-00042/claim"),
        ("GET", "/api/state"),
        ("GET", "/api/fleet"),
        ("GET", "/api/fleet/stations"),
    ] {
        assert_eq!(away(&app, Some(&station), method, path, Some(json!({}))).await.0, StatusCode::FORBIDDEN, "{method} {path}");
    }

    // The owner sees it; fleet care still answers beside the fleet API.
    assert_eq!(away(&app, Some(&owner), "GET", "/api/fleet/units/OCN-2611-00042", None).await.0, StatusCode::OK);
    assert_eq!(away(&app, Some(&owner), "GET", "/api/fleet", None).await.0, StatusCode::OK);

    assert_eq!(away(&app, Some(&owner), "POST", &format!("/api/fleet/stations/{id}/revoke"), None).await.0, StatusCode::OK);
    assert_eq!(away(&app, Some(&station), "POST", "/api/fleet/units", Some(unit)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(away(&app, Some("ops_not-a-token"), "GET", "/api/fleet/test-plans/node", None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_fleet_api_is_for_managers_and_owners() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let app = build_app(ctx.clone(), ServerInfo { data_dir: dir.path().into(), bind: "127.0.0.1".into(), port: 7878, lan_url: None }, false);
    let owner = ctx.settings().await.unwrap().server.app_token;
    let mut tokens = Vec::new();
    for (name, role) in [("Vic", "viewer"), ("Hal", "hand"), ("Max", "manager")] {
        let (s, inv) = away(&app, Some(&owner), "POST", "/api/invites", Some(json!({ "name": name, "role": role }))).await;
        assert_eq!(s, StatusCode::CREATED, "{inv}");
        let (s, acc) = away(&app, None, "POST", "/api/invites/accept", Some(json!({ "code": inv["code"] }))).await;
        assert_eq!(s, StatusCode::OK, "{acc}");
        tokens.push(acc["token"].as_str().unwrap().to_owned());
    }
    let (viewer, hand, manager) = (&tokens[0], &tokens[1], &tokens[2]);
    for path in ["/api/fleet/units", "/api/fleet/test-plans/node", "/api/fleet/firmware", "/api/fleet/summary", "/api/fleet/yield?plan_id=node-fct"] {
        assert_eq!(away(&app, Some(viewer), "GET", path, None).await.0, StatusCode::FORBIDDEN, "viewer {path}");
        assert_eq!(away(&app, Some(hand), "GET", path, None).await.0, StatusCode::FORBIDDEN, "hand {path}");
        assert_eq!(away(&app, Some(manager), "GET", path, None).await.0, StatusCode::OK, "manager {path}");
    }
    // Owner only: stations, plan edits, releases and keys.
    assert_eq!(away(&app, Some(manager), "GET", "/api/fleet/stations", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(away(&app, Some(manager), "PUT", "/api/fleet/test-plans/node", Some(json!({ "steps": [] }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(away(&app, Some(manager), "POST", "/api/fleet/signing-keys", Some(json!({}))).await.0, StatusCode::FORBIDDEN);
    // The farmer can pause updates.
    let pause = json!({ "mode": "paused", "channel": "stable", "battery_floor": 0.4, "wave_hours": 24 });
    assert_eq!(away(&app, Some(manager), "PUT", "/api/fleet/firmware/policy", Some(pause)).await.0, StatusCode::OK);
}
