//! Seams between merged streams, on the whole server: the live feed batches
//! across herds (at most two batched messages a second for the farm, however
//! many herds are live), a collar's reject code reaches the bus and
//! `ack_batch`, an open socket closes once its sign-in is revoked over REST
//! (by the owner, by signing out, by removing the person), and someone
//! else's alert prefs are the owner's at the guard.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use http_body_util::BodyExt;
use op_core::{AckStatus, Autonomy, Collar, Ctx, Event, FenceState, Fix, Herd, Identity, Role, Species, Via};
use op_server::{ServeOptions, ServerInfo, build_app, serve};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const AWAY: &str = "203.0.113.9";

async fn call(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)], body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri).header("host", "127.0.0.1:7878");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
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

fn app_for(ctx: &Ctx, dir: &std::path::Path) -> Router {
    build_app(ctx.clone(), ServerInfo { data_dir: dir.into(), bind: "127.0.0.1".into(), port: 7878, lan_url: None }, false)
}

/// Herds of `sizes` collars each, linked through the API before any socket connects.
async fn herds(sizes: &[usize]) -> (tempfile::TempDir, Ctx, Router, Vec<(String, Vec<Collar>)>) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let app = app_for(&ctx, dir.path());
    let mut out = Vec::new();
    for (h, n) in sizes.iter().enumerate() {
        let herd = Herd {
            id: format!("herd_{}", h + 1),
            name: format!("Herd {}", h + 1),
            species: Species::Cattle,
            count: *n as u32,
            paddock_id: None,
            autonomy: Autonomy::Propose,
            timer_minutes: 60,
            created_at: op_core::time::now(),
        };
        ctx.store().insert_herd(&herd).await.unwrap();
        let mut collars = Vec::new();
        for i in 0..*n {
            let (s, v) = call(&app, "POST", "/api/collars", &[], Some(json!({ "herd_id": herd.id, "name": format!("{}{:03}", h + 1, i) }))).await;
            assert_eq!(s, StatusCode::CREATED, "{v}");
            collars.push(serde_json::from_value(v["collar"].clone()).unwrap());
        }
        out.push((herd.id, collars));
    }
    (dir, ctx, app, out)
}

/// `/api/live` as `identity` on a free port.
async fn connect(ctx: &Ctx, identity: Identity) -> Ws {
    let router = op_core::with_identity(Router::new().route("/api/live", axum::routing::get(op_server::live::handler)).with_state(ctx.clone()), identity);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    tokio_tungstenite::connect_async(format!("ws://{addr}/api/live")).await.unwrap().0
}

/// Every message (WebSocket frame) the socket receives within `wait`.
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

/// The events a message carries: itself, or the contents of a `batch`.
fn events(msgs: &[Value]) -> Vec<&Value> {
    msgs.iter().flat_map(|m| if m["type"] == "batch" { m["events"].as_array().unwrap().iter().collect() } else { vec![m] }).collect()
}

fn fix(i: usize) -> Fix {
    Fix { at: op_core::time::now(), point: [-93.62 + i as f64 * 1e-5, 42.03], accuracy_m: 3.0, sats: 9, cn0: None, ttf_s: None }
}

fn viewer() -> Identity {
    Identity { role: Role::Viewer, user_id: Some("usr_v".into()), name: None, via: Via::UserToken }
}

#[tokio::test]
async fn several_herds_in_one_window_arrive_as_one_message() {
    let (_dir, ctx, _app, herds) = herds(&[3, 2, 2]).await;
    let mut owner = connect(&ctx, Identity::owner(Via::Local)).await;
    let mut view = connect(&ctx, viewer()).await;
    // Herd 1 fixes, herd 2 fixes and an ack, herd 3 a cue: one window.
    for (h, (herd, collars)) in herds.iter().enumerate() {
        for (i, c) in collars.iter().enumerate() {
            ctx.publish(Event::Fix { collar_id: c.id.clone(), animal_id: None, herd_id: herd.clone(), fix: fix(i), state: FenceState::Inside });
            if h == 1 {
                ctx.publish(Event::Ack { collar_id: c.id.clone(), herd_id: herd.clone(), version: 4, status: AckStatus::Applied, reason: None, code: None });
            }
        }
    }
    let c3 = &herds[2].1[0];
    ctx.publish(Event::Cue { collar_id: c3.id.clone(), at: op_core::time::now(), level: 1, margin_m: 1.5, kind: Some("warn".into()), ring: Some(0) });

    let msgs = gather(&mut owner, Duration::from_millis(1200)).await;
    assert_eq!(msgs.len(), 1, "one message for the whole farm: {msgs:?}");
    assert_eq!(msgs[0]["type"], "batch");
    let got: Vec<(&str, &str, usize)> =
        events(&msgs).iter().map(|e| (e["type"].as_str().unwrap(), e["herd_id"].as_str().unwrap(), e["items"].as_array().unwrap().len())).collect();
    assert_eq!(
        got,
        [("positions", "herd_1", 3), ("positions", "herd_2", 2), ("ack_batch", "herd_2", 2), ("positions", "herd_3", 2), ("cue_batch", "herd_3", 1)],
        "each herd's batches as before, in herd order"
    );
    // The same message, for a viewer too (every batch is theirs to see).
    let v = gather(&mut view, Duration::from_millis(100)).await;
    assert_eq!(v, msgs);

    // A window holding one message sends it as itself.
    ctx.publish(Event::Fix { collar_id: c3.id.clone(), animal_id: None, herd_id: herds[2].0.clone(), fix: fix(9), state: FenceState::Warning });
    let msgs = gather(&mut owner, Duration::from_millis(900)).await;
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    assert_eq!((msgs[0]["type"].as_str(), msgs[0]["herd_id"].as_str()), (Some("positions"), Some("herd_3")));
}

#[tokio::test]
async fn several_herds_live_stay_under_three_messages_a_second() {
    // 250 collars in three herds, every collar reporting each second (ten times
    // the fast sweep cadence), with cues and acks in two of the herds.
    let (_dir, ctx, _app, herds) = herds(&[84, 83, 83]).await;
    let mut ws = connect(&ctx, viewer()).await;
    let run = Duration::from_secs(5);
    let publisher = {
        let ctx = ctx.clone();
        let herds = herds.clone();
        tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            let mut tick = 0usize;
            while started.elapsed() < run {
                for (h, (herd, collars)) in herds.iter().enumerate() {
                    for (i, c) in collars.iter().enumerate().filter(|(i, _)| i % 10 == tick % 10) {
                        ctx.publish(Event::Fix { collar_id: c.id.clone(), animal_id: None, herd_id: herd.clone(), fix: fix(i), state: FenceState::Inside });
                        match h {
                            1 => ctx.publish(Event::Cue {
                                collar_id: c.id.clone(),
                                at: op_core::time::now(),
                                level: 1,
                                margin_m: 2.0,
                                kind: Some("warn".into()),
                                ring: None,
                            }),
                            2 => ctx.publish(Event::Ack {
                                collar_id: c.id.clone(),
                                herd_id: herd.clone(),
                                version: tick as u32,
                                status: AckStatus::Applied,
                                reason: None,
                                code: None,
                            }),
                            _ => {}
                        }
                    }
                }
                tick += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };
    let msgs = gather(&mut ws, run + Duration::from_millis(1200)).await;
    publisher.await.unwrap();
    let secs = run.as_secs_f64() + 1.2;
    assert!(msgs.len() as f64 / secs <= 3.0, "{} messages in {secs} s", msgs.len());
    assert!(msgs.len() as f64 <= (run.as_millis() as f64 / 500.0) + 2.0, "at most one per window: {}", msgs.len());
    // Nothing was lost: every collar's position, every herd's cues and acks.
    let evs = events(&msgs);
    let mut seen: Vec<&str> =
        evs.iter().filter(|e| e["type"] == "positions").flat_map(|e| e["items"].as_array().unwrap()).map(|i| i["collar_id"].as_str().unwrap()).collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 250);
    assert!(evs.iter().any(|e| e["type"] == "cue_batch" && e["herd_id"] == "herd_2"));
    assert!(evs.iter().any(|e| e["type"] == "ack_batch" && e["herd_id"] == "herd_3"));
    assert!(evs.iter().all(|e| ["positions", "ack_batch", "cue_batch"].contains(&e["type"].as_str().unwrap())), "no single fix, ack or cue");
}

#[tokio::test]
async fn a_rejected_acks_code_reaches_the_bus_and_the_ack_batch() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let app = app_for(&ctx, dir.path());
    let (s, _) = call(&app, "POST", "/api/farm", &[], Some(json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let p1 = json!({"type": "Polygon", "coordinates": [[[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]]]});
    let (_, pad) = call(&app, "POST", "/api/paddocks", &[], Some(json!({"name": "P1", "geometry": p1}))).await;
    let (_, herd) = call(&app, "POST", "/api/herds", &[], Some(json!({"name": "Cows", "species": "cattle", "count": 1, "paddock_id": pad["id"]}))).await;
    let herd = herd["id"].as_str().unwrap().to_owned();
    let (_, c) = call(&app, "POST", "/api/collars", &[], Some(json!({"herd_id": herd}))).await;
    let (collar, key) = (c["collar"]["id"].as_str().unwrap().to_owned(), c["key"].as_str().unwrap().to_owned());
    let bearer = format!("Bearer {key}");
    let collar_auth = [("authorization", bearer.as_str())];
    let device = json!({"fw": "0.2.0", "caps": ["holes", "slots", "collar_id", "cue_mode", "episodes", "config"], "limits": {"outer": 128, "holes": 16, "hole_vertices": 32, "total": 384, "slots": 16, "slot_bytes": 24576}});
    let (s, v) = call(&app, "POST", "/collar/v1/report", &collar_auth, Some(json!({"device": device}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = call(&app, "POST", &format!("/api/herds/{herd}/boundary"), &[], Some(json!({"geometry": p1}))).await;
    assert!(s.is_success(), "{v}");
    let (s, cmd) = call(&app, "GET", "/collar/v1/boundary?have=0&free=16", &collar_auth, None).await;
    assert_eq!(s, StatusCode::OK, "{cmd}");

    let mut bus = ctx.subscribe();
    let mut ws = connect(&ctx, Identity::owner(Via::Local)).await;
    let at = op_core::time::to_db(&op_core::time::now());
    let ack = json!({"command_id": cmd["command_id"], "version": cmd["version"], "status": "rejected", "code": "hole_too_close", "at": at});
    let (s, v) = call(&app, "POST", "/collar/v1/ack", &collar_auth, Some(ack)).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");

    let on_bus = loop {
        match tokio::time::timeout(Duration::from_secs(2), bus.recv()).await.unwrap().unwrap() {
            Event::Ack { code, status, .. } => break (status, code),
            _ => continue,
        }
    };
    assert_eq!(on_bus, (AckStatus::Rejected, Some("hole_too_close".to_owned())));
    let msgs = gather(&mut ws, Duration::from_millis(1000)).await;
    let item = events(&msgs).into_iter().find(|e| e["type"] == "ack_batch").map(|e| e["items"][0].clone()).expect("an ack_batch");
    assert_eq!((item["collar_id"].as_str(), item["status"].as_str(), item["code"].as_str()), (Some(collar.as_str()), Some("rejected"), Some("hole_too_close")));
}

// ---- sign-ins revoked over REST close open sockets -------------------------------------

struct Served {
    _dir: tempfile::TempDir,
    handle: op_server::ServerHandle,
    http: reqwest::Client,
}

impl Served {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let handle = serve(ServeOptions { data_dir: Some(dir.path().into()), free_port: true, ..Default::default() }).await.unwrap();
        Self { _dir: dir, handle, http: reqwest::Client::new() }
    }

    /// A person with a sign-in, and their token (`opu_…`).
    async fn person(&self, name: &str, role: Role) -> (String, String) {
        let ctx = self.handle.ctx();
        let invite = op_core::people::NewInvite { name: Some(name.into()), role: Some(role), ..Default::default() };
        let (_, code) = op_core::people::create_invite(ctx, invite, &Identity::owner(Via::Local).actor()).await.unwrap();
        let accepted = op_core::people::accept_invite(ctx, &code, None).await.unwrap();
        (accepted.user.id, accepted.token)
    }

    /// Another browser for the same person: a second link, a second token.
    async fn another_browser(&self, user_id: &str) -> String {
        let ctx = self.handle.ctx();
        let invite = op_core::people::NewInvite { user_id: Some(user_id.into()), ..Default::default() };
        let (_, code) = op_core::people::create_invite(ctx, invite, &Identity::owner(Via::Local).actor()).await.unwrap();
        op_core::people::accept_invite(ctx, &code, None).await.unwrap().token
    }

    async fn socket(&self, token: &str) -> Ws {
        let mut req = format!("{}/api/live?token={token}", self.handle.url().replace("http://", "ws://")).into_client_request().unwrap();
        req.headers_mut().insert("x-forwarded-for", AWAY.parse().unwrap());
        tokio_tungstenite::connect_async(req).await.unwrap().0
    }

    /// A request from this machine: the owner.
    async fn local(&self, method: reqwest::Method, path: &str) -> reqwest::StatusCode {
        self.http.request(method, format!("{}{path}", self.handle.url())).send().await.unwrap().status()
    }

    /// A request from elsewhere with a person's token.
    async fn away(&self, token: &str, method: reqwest::Method, path: &str, body: Option<&str>) -> reqwest::StatusCode {
        let mut r = self.http.request(method, format!("{}{path}", self.handle.url())).header("x-forwarded-for", AWAY).bearer_auth(token);
        if let Some(b) = body {
            r = r.header("content-type", "application/json").body(b.to_owned());
        }
        r.send().await.unwrap().status()
    }
}

/// Whether the server closes the socket within `wait` (messages before it are skipped).
async fn closed_within(ws: &mut Ws, wait: Duration) -> bool {
    let end = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(end, ws.next()).await {
            Ok(Some(Ok(m))) if m.is_close() => return true,
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => return true,
            Err(_) => return false,
        }
    }
}

const CLOSE_WITHIN: Duration = Duration::from_secs(7);

#[tokio::test]
async fn revoking_a_token_over_rest_closes_its_open_socket() {
    let s = Served::new().await;
    let (user, token) = s.person("Vic", Role::Viewer).await;
    let mut ws = s.socket(&token).await;
    let tokens = op_core::people::list_tokens(s.handle.ctx(), Some(&user)).await.unwrap();
    assert_eq!(s.local(reqwest::Method::DELETE, &format!("/api/tokens/{}", tokens[0].id)).await, reqwest::StatusCode::NO_CONTENT);
    assert!(closed_within(&mut ws, CLOSE_WITHIN).await, "the owner revoked it");
    assert_eq!(s.away(&token, reqwest::Method::GET, "/api/state", None).await, reqwest::StatusCode::UNAUTHORIZED);
    s.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn signing_out_closes_that_browsers_socket_and_no_other() {
    let s = Served::new().await;
    let (user, phone) = s.person("Hal", Role::Hand).await;
    let laptop = s.another_browser(&user).await;
    let mut phone_ws = s.socket(&phone).await;
    let mut laptop_ws = s.socket(&laptop).await;
    assert_eq!(s.away(&phone, reqwest::Method::POST, "/api/me/signout", None).await, reqwest::StatusCode::NO_CONTENT);
    assert!(closed_within(&mut phone_ws, CLOSE_WITHIN).await, "the phone signed out");
    assert!(!closed_within(&mut laptop_ws, Duration::from_secs(1)).await, "the laptop is still signed in");
    s.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn removing_a_person_closes_their_socket() {
    let s = Served::new().await;
    let (user, token) = s.person("Ana", Role::Manager).await;
    let mut ws = s.socket(&token).await;
    assert_eq!(s.local(reqwest::Method::DELETE, &format!("/api/users/{user}")).await, reqwest::StatusCode::NO_CONTENT);
    assert!(closed_within(&mut ws, CLOSE_WITHIN).await, "the person is gone");
    s.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn someone_elses_alert_prefs_are_the_owners_at_the_guard() {
    let s = Served::new().await;
    let (hand, hand_token) = s.person("Hal", Role::Hand).await;
    let (_, manager_token) = s.person("Ana", Role::Manager).await;
    // A hand still sets their own.
    assert_eq!(s.away(&hand_token, reqwest::Method::PUT, "/api/alerts/prefs/me", Some(r#"{"on_duty": true}"#)).await, reqwest::StatusCode::OK);
    // A manager is refused by the guard before the body is read: a bad body
    // would be a 4xx from the handler otherwise.
    let path = format!("/api/alerts/prefs/{hand}");
    assert_eq!(s.away(&manager_token, reqwest::Method::PUT, &path, Some("not json")).await, reqwest::StatusCode::FORBIDDEN);
    assert_eq!(s.away(&manager_token, reqwest::Method::PUT, &path, Some(r#"{"on_duty": false}"#)).await, reqwest::StatusCode::FORBIDDEN);
    assert_eq!(s.away(&hand_token, reqwest::Method::PUT, &path, Some(r#"{"on_duty": false}"#)).await, reqwest::StatusCode::FORBIDDEN);
    s.handle.shutdown().await.unwrap();
}
