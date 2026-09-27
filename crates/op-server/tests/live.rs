//! `/api/live` at herd scale: fixes, acks, cues and telemetry-only collar
//! changes go out coalesced per herd every 500 ms, everything else passes
//! through in order, each message is serialized once for every socket, roles
//! filter what a socket sees, and a socket that falls behind is told to resync.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use futures::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use op_core::{AckStatus, Autonomy, Collar, Ctx, Event, FenceState, Fix, Herd, Identity, Role, Species, Via};
use op_server::{ServerInfo, build_app};
use serde_json::{Value, json};
use tower::ServiceExt;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Farm {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    app: Router,
    herd: String,
    collars: Vec<Collar>,
}

/// A herd with `n` collars linked through the API, before any socket connects.
async fn farm(n: usize) -> Farm {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let info = ServerInfo { data_dir: dir.path().into(), bind: "127.0.0.1".into(), port: 7878, lan_url: None };
    let app = build_app(ctx.clone(), info, false);
    let herd = Herd {
        id: "herd_1".into(),
        name: "Cows".into(),
        species: Species::Cattle,
        count: n as u32,
        paddock_id: None,
        autonomy: Autonomy::Propose,
        timer_minutes: 60,
        created_at: op_core::time::now(),
    };
    ctx.store().insert_herd(&herd).await.unwrap();
    let mut collars = Vec::new();
    for i in 0..n {
        let (s, v) = call(&app, "POST", "/api/collars", Some(json!({ "herd_id": herd.id, "name": format!("{:03}", i + 1) }))).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        collars.push(serde_json::from_value(v["collar"].clone()).unwrap());
    }
    Farm { _dir: dir, ctx, app, herd: herd.id, collars }
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri).header("host", "127.0.0.1:7878");
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(ConnectInfo("127.0.0.1:50000".parse::<SocketAddr>().unwrap()));
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn owner() -> Identity {
    Identity::owner(Via::Local)
}

/// Serve `/api/live` as `identity` on a free port and connect to it. The
/// socket is subscribed once the upgrade answers.
async fn connect(ctx: &Ctx, identity: Identity) -> Ws {
    let router = op_core::with_identity(Router::new().route("/api/live", axum::routing::get(op_server::live::handler)).with_state(ctx.clone()), identity);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    tokio_tungstenite::connect_async(format!("ws://{addr}/api/live")).await.unwrap().0
}

/// Every message the socket receives within `wait`.
async fn gather(ws: &mut Ws, wait: Duration) -> Vec<Value> {
    let end = tokio::time::Instant::now() + wait;
    let mut out = Vec::new();
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(end, ws.next()).await {
        if let Ok(text) = msg.to_text() {
            out.push(serde_json::from_str(text).unwrap());
        }
    }
    out
}

fn of<'a>(msgs: &'a [Value], ty: &str) -> Vec<&'a Value> {
    msgs.iter().filter(|m| m["type"] == ty).collect()
}

fn fix(i: usize) -> Fix {
    Fix { at: op_core::time::now(), point: [-93.62 + i as f64 * 1e-5, 42.03], accuracy_m: 3.0, sats: 9, cn0: None, ttf_s: None }
}

#[tokio::test]
async fn two_hundred_fifty_fixes_arrive_as_at_most_two_positions_messages() {
    let f = farm(250).await;
    let mut ws = connect(&f.ctx, owner()).await;
    let started = Instant::now();
    for (i, c) in f.collars.iter().enumerate() {
        f.ctx.publish(Event::Fix { collar_id: c.id.clone(), animal_id: None, herd_id: f.herd.clone(), fix: fix(i), state: FenceState::Inside });
        if i % 25 == 24 {
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
    }
    // One window opens with the first fix; anything under a second fits two.
    assert!(started.elapsed() < Duration::from_millis(900), "published in {:?}", started.elapsed());
    let msgs = gather(&mut ws, Duration::from_millis(1500)).await;
    let positions = of(&msgs, "positions");
    assert!(!positions.is_empty() && positions.len() <= 2, "{} positions messages", positions.len());
    assert!(of(&msgs, "fix").is_empty());
    let mut ids: Vec<&str> = positions.iter().flat_map(|m| m["items"].as_array().unwrap()).map(|i| i["collar_id"].as_str().unwrap()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 250, "every collar's fix, once");
    assert!(positions.iter().all(|m| m["herd_id"] == "herd_1"));
    let item = &positions[0]["items"][0];
    assert!(item["fix"]["point"].is_array() && item["state"] == "inside", "{item}");
}

#[tokio::test]
async fn a_sweep_steps_acks_arrive_as_at_most_two_ack_batches() {
    let f = farm(250).await;
    let mut ws = connect(&f.ctx, owner()).await;
    // A sweep step as device.rs publishes it: each collar acks received, then
    // applied, each ack followed by the collar (whose boundary_version moves on applied).
    for c in &f.collars {
        f.ctx.publish(Event::Ack { collar_id: c.id.clone(), herd_id: f.herd.clone(), version: 7, status: AckStatus::Received, reason: None });
        f.ctx.publish(Event::Collar { collar: c.clone() });
    }
    for c in &f.collars {
        f.ctx.publish(Event::Ack { collar_id: c.id.clone(), herd_id: f.herd.clone(), version: 7, status: AckStatus::Applied, reason: None });
        f.ctx.publish(Event::Collar { collar: Collar { boundary_version: Some(7), ..c.clone() } });
    }
    let msgs = gather(&mut ws, Duration::from_millis(1500)).await;
    let batches = of(&msgs, "ack_batch");
    assert!(!batches.is_empty() && batches.len() <= 2, "{} ack batches of {} messages", batches.len(), msgs.len());
    assert!(of(&msgs, "ack").is_empty() && of(&msgs, "collar").is_empty(), "{msgs:?}");
    let last = batches.last().unwrap();
    let items = last["items"].as_array().unwrap();
    assert_eq!(items.len(), 250);
    assert!(items.iter().all(|i| i["version"] == 7 && i["status"] == "applied"), "{last}");
}

#[tokio::test]
async fn telemetry_only_collar_changes_ride_in_positions_and_a_renamed_collar_goes_out() {
    let f = farm(2).await;
    let mut ws = connect(&f.ctx, owner()).await;
    let c = &f.collars[0];
    // A report: battery, last contact, a new fix and state. Nothing that matters changed.
    let fx = fix(0);
    let reported = Collar { battery: Some(0.64), last_seen: Some(fx.at), last_fix: Some(fx.clone()), state: FenceState::Warning, ..c.clone() };
    f.ctx.publish(Event::Collar { collar: reported.clone() });
    let msgs = gather(&mut ws, Duration::from_millis(900)).await;
    assert!(of(&msgs, "collar").is_empty(), "{msgs:?}");
    let positions = of(&msgs, "positions");
    assert_eq!(positions.len(), 1, "{msgs:?}");
    let item = &positions[0]["items"][0];
    assert_eq!((item["collar_id"].as_str(), item["battery"].as_f64(), item["state"].as_str()), (Some(c.id.as_str()), Some(0.64), Some("warning")));

    // The same collar reporting only a boundary version: an applied ack.
    f.ctx.publish(Event::Collar { collar: Collar { boundary_version: Some(3), ..reported.clone() } });
    let msgs = gather(&mut ws, Duration::from_millis(900)).await;
    assert!(of(&msgs, "collar").is_empty() && of(&msgs, "positions").is_empty(), "{msgs:?}");
    assert_eq!(of(&msgs, "ack_batch")[0]["items"], json!([{ "collar_id": c.id, "version": 3, "status": "applied" }]));

    // Renamed through the API: sent on its own, right away.
    let (s, _) = call(&f.app, "PATCH", &format!("/api/collars/{}", c.id), Some(json!({ "name": "Bessie" }))).await;
    assert_eq!(s, StatusCode::OK);
    let msgs = gather(&mut ws, Duration::from_millis(300)).await;
    let collars = of(&msgs, "collar");
    assert_eq!(collars.len(), 1, "{msgs:?}");
    assert_eq!(collars[0]["collar"]["name"], "Bessie");
    // Fields other streams add count too: parked is not telemetry.
    f.ctx.publish(Event::Collar { collar: Collar { parked_at: Some(op_core::time::now()), name: "Bessie".into(), ..reported } });
    let msgs = gather(&mut ws, Duration::from_millis(300)).await;
    assert_eq!(of(&msgs, "collar").len(), 1, "{msgs:?}");
}

#[tokio::test]
async fn cues_go_out_together_in_the_order_they_came() {
    let f = farm(3).await;
    let mut ws = connect(&f.ctx, owner()).await;
    let t0 = op_core::time::now();
    for (i, c) in f.collars.iter().enumerate() {
        for k in 0..2u8 {
            let kind = if k == 0 { "warn" } else { "outside" };
            f.ctx.publish(Event::Cue {
                collar_id: c.id.clone(),
                at: op_core::time::from_unix_ms(t0.timestamp_millis() + (i as i64 * 2 + k as i64) * 1000),
                level: k + 1,
                margin_m: 2.0 - k as f64 * 3.0,
                kind: Some(kind.into()),
                ring: Some(0),
            });
        }
    }
    // A collar linked now goes out whole.
    call(&f.app, "POST", "/api/collars", Some(json!({ "herd_id": f.herd, "name": "late" }))).await;
    let msgs = gather(&mut ws, Duration::from_millis(1200)).await;
    assert!(of(&msgs, "cue").is_empty());
    let batches = of(&msgs, "cue_batch");
    assert_eq!(batches.len(), 1, "{msgs:?}");
    let items = batches[0]["items"].as_array().unwrap();
    assert_eq!(items.len(), 6);
    let kinds: Vec<&str> = items.iter().map(|i| i["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["warn", "outside", "warn", "outside", "warn", "outside"]);
    assert_eq!(items[0]["collar_id"], f.collars[0].id.as_str());
    assert_eq!(of(&msgs, "collar").len(), 1, "the new collar goes out whole");

    // A collar linked where this bus didn't hear it (another server on the
    // same data dir here; a bulk link in practice): its herd comes from the database.
    let other = Ctx::open(f._dir.path()).await.unwrap();
    let info = ServerInfo { data_dir: f._dir.path().into(), bind: "127.0.0.1".into(), port: 7878, lan_url: None };
    let (_, v) = call(&build_app(other, info, false), "POST", "/api/collars", Some(json!({ "herd_id": f.herd, "name": "unheard" }))).await;
    let late = v["collar"]["id"].as_str().unwrap().to_owned();
    f.ctx.publish(Event::Cue { collar_id: late.clone(), at: t0, level: 1, margin_m: 1.0, kind: Some("warn".into()), ring: None });
    let msgs = gather(&mut ws, Duration::from_millis(1200)).await;
    assert_eq!(of(&msgs, "cue_batch")[0]["items"][0]["collar_id"], late.as_str());
}

#[tokio::test]
async fn other_events_pass_through_in_order_and_at_once() {
    let f = farm(1).await;
    let mut ws = connect(&f.ctx, owner()).await;
    let started = Instant::now();
    for i in 0..5 {
        f.ctx.publish(Event::DecisionLog { decision_id: "dec_1".into(), line: format!("line {i}") });
    }
    f.ctx.publish(Event::AnimalsChanged { herd_id: Some(f.herd.clone()) });
    let mut got = Vec::new();
    while got.len() < 6 {
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
        got.push(serde_json::from_str::<Value>(msg.to_text().unwrap()).unwrap());
    }
    assert!(started.elapsed() < Duration::from_millis(400), "not held for a batch: {:?}", started.elapsed());
    let lines: Vec<&str> = got[..5].iter().map(|m| m["line"].as_str().unwrap()).collect();
    assert_eq!(lines, ["line 0", "line 1", "line 2", "line 3", "line 4"]);
    assert_eq!(got[5]["type"], "animals_changed");
}

#[tokio::test]
async fn a_viewer_socket_never_receives_messages() {
    let f = farm(1).await;
    let viewer = Identity { role: Role::Viewer, user_id: Some("usr_v".into()), name: None, via: Via::UserToken };
    let manager = Identity { role: Role::Manager, ..viewer.clone() };
    let mut v_ws = connect(&f.ctx, viewer).await;
    let mut m_ws = connect(&f.ctx, manager).await;
    let message = op_core::alert::MessageLog { id: "ntf_1".into(), address: "+15155550123".into(), ..Default::default() };
    f.ctx.publish(Event::Message { message });
    f.ctx.publish(Event::Fix { collar_id: f.collars[0].id.clone(), animal_id: None, herd_id: f.herd.clone(), fix: fix(0), state: FenceState::Inside });
    let v = gather(&mut v_ws, Duration::from_millis(900)).await;
    let m = gather(&mut m_ws, Duration::from_millis(100)).await;
    assert!(of(&v, "message").is_empty(), "{v:?}");
    assert_eq!(of(&v, "positions").len(), 1, "viewers see positions");
    assert_eq!(of(&m, "message")[0]["message"]["address"], "+15155550123");
    assert_eq!(of(&m, "positions").len(), 1);
}

#[tokio::test]
async fn every_socket_shares_one_serialization_per_message() {
    let f = farm(250).await;
    let hub = op_server::coalesce::hub(&f.ctx);
    let mut sockets = Vec::new();
    for _ in 0..4 {
        sockets.push(connect(&f.ctx, owner()).await);
    }
    assert_eq!(op_server::live::subscribers(&f.ctx), 4);
    let before = hub.serialized();
    for (i, c) in f.collars.iter().enumerate() {
        f.ctx.publish(Event::Fix { collar_id: c.id.clone(), animal_id: None, herd_id: f.herd.clone(), fix: fix(i), state: FenceState::Inside });
    }
    f.ctx.publish(Event::DecisionLog { decision_id: "dec_1".into(), line: "done".into() });
    let mut received = Vec::new();
    for ws in &mut sockets {
        received.push(gather(ws, Duration::from_millis(900)).await.len());
    }
    assert!(received.iter().all(|n| *n == received[0] && *n >= 2), "{received:?}");
    assert_eq!(hub.serialized() - before, received[0] as u64, "one serialization per message, not per socket");
}

#[tokio::test]
async fn a_socket_that_falls_behind_is_told_to_resync() {
    let f = farm(1).await;
    let mut ws = connect(&f.ctx, owner()).await;
    // Don't read while far more than the socket's buffer and the TCP window is sent.
    let line = "x".repeat(4096);
    for i in 0..6000 {
        f.ctx.publish(Event::DecisionLog { decision_id: "dec_1".into(), line: line.clone() });
        if i % 500 == 499 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut resync = false;
    let end = tokio::time::Instant::now() + Duration::from_secs(10);
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(end, ws.next()).await {
        let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        if v["type"] == "resync" {
            resync = true;
            break;
        }
    }
    assert!(resync, "a lagging socket gets resync");
    let _ = ws.close(None).await;
    let _ = ws.flush().await;
}

#[tokio::test]
async fn a_client_closing_gets_a_clean_close_back() {
    let f = farm(1).await;
    let mut ws = connect(&f.ctx, owner()).await;
    ws.send(tokio_tungstenite::tungstenite::Message::Close(None)).await.unwrap();
    let mut saw_close = false;
    let end = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match tokio::time::timeout_at(end, ws.next()).await {
            Ok(Some(Ok(m))) if m.is_close() => saw_close = true,
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => panic!("{e}"),
            Ok(None) | Err(_) => break,
        }
    }
    assert!(saw_close, "the server answered the close");
}
