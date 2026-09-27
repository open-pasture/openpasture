//! The guard gives every request an identity: `/api/me`, brain tokens with
//! their tool allowlist, JSON 404s under `/hooks`, and live events filtered
//! by the socket's role.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use http_body_util::BodyExt;
use op_core::{Ctx, Event, Identity, Role, Via};
use op_server::{ServerInfo, build_app};
use serde_json::{Value, json};
use tower::ServiceExt;

const LAN: &str = "192.168.1.20:50000";
const LO: &str = "127.0.0.1:50000";

async fn app() -> (tempfile::TempDir, Ctx, Router) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let info = ServerInfo { data_dir: dir.path().into(), bind: "127.0.0.1".into(), port: 7878, lan_url: None };
    let app = build_app(ctx.clone(), info, false);
    (dir, ctx, app)
}

async fn send(app: &Router, method: &str, uri: &str, peer: &str, headers: &[(&str, &str)], body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json").header("accept", "application/json, text/event-stream");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn api_me_says_who_is_asking() {
    let (_dir, ctx, app) = app().await;
    let (s, v) = send(&app, "GET", "/api/me", LO, &[("host", "127.0.0.1:7878")], None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({"role": "owner", "via": "local"}));

    let token = ctx.settings().await.unwrap().server.app_token;
    let bearer = format!("Bearer {token}");
    let (s, v) = send(&app, "GET", "/api/me", LAN, &[("host", "192.168.1.2:7878"), ("authorization", &bearer)], None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({"role": "owner", "via": "app_token"}));
    // Forwarding headers make even a loopback peer non-local.
    let (s, v) = send(&app, "GET", "/api/me", LO, &[("host", "127.0.0.1:7878"), ("x-forwarded-for", "203.0.113.9"), ("authorization", &bearer)], None).await;
    assert_eq!((s, v["via"].as_str()), (StatusCode::OK, Some("app_token")));
    let (s, _) = send(&app, "GET", "/api/me", LAN, &[("host", "192.168.1.2:7878")], None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn hooks_paths_are_json_404s() {
    let (_dir, _ctx, app) = app().await;
    for path in ["/hooks/x", "/hooks", "/hooks/twilio/nope"] {
        let (s, v) = send(&app, "GET", path, LAN, &[("host", "192.168.1.2:7878")], None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{path}");
        assert!(v["error"].is_string(), "{path}: {v}");
    }
    // A3's Twilio hook exists and takes only POST.
    let (s, _) = send(&app, "GET", "/hooks/twilio/sms", LAN, &[("host", "192.168.1.2:7878")], None).await;
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
}

async fn mcp(app: &Router, uri: &str, auth: &str, id: u64, method: &str, params: Value) -> (StatusCode, Value) {
    let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    send(app, "POST", uri, LAN, &[("host", "192.168.1.2:7878"), ("authorization", auth)], Some(body)).await
}

fn tool_names(v: &Value) -> Vec<String> {
    v["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn a_brain_token_opens_only_brain_mcp_with_its_tools() {
    let (_dir, ctx, app) = app().await;
    // Bound on the LAN, as a server reached with a brain token is.
    ctx.set_local_url("http://192.168.1.2:7878");
    op_engine::register_tools(&ctx);
    let token = ctx.mint_brain_token(std::time::Duration::from_secs(60), vec!["get_farm".into(), "list_paddocks".into(), "propose_boundary".into()]);
    let bearer = format!("Bearer {}", token.as_str());

    let (s, v) = mcp(&app, "/mcp?scope=brain", &bearer, 1, "tools/list", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(tool_names(&v), ["get_farm", "list_paddocks"], "its allowlist, read tools only");
    let (_, v) = mcp(&app, "/mcp?scope=brain", &bearer, 2, "tools/call", json!({"name": "get_farm", "arguments": {}})).await;
    assert_eq!(v["result"]["isError"], false, "{v}");
    let (_, v) = mcp(&app, "/mcp?scope=brain", &bearer, 3, "tools/call", json!({"name": "get_herd", "arguments": {}})).await;
    assert!(v["error"]["message"].as_str().unwrap().contains("Unknown tool"), "{v}");
    let (_, v) = mcp(&app, "/mcp?scope=brain", &bearer, 4, "tools/call", json!({"name": "propose_boundary", "arguments": {"reasoning": "x"}})).await;
    assert!(v["error"]["message"].as_str().unwrap().contains("Unknown tool"), "{v}");

    // Nothing else opens with it.
    assert_eq!(mcp(&app, "/mcp", &bearer, 5, "tools/list", json!({})).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&app, "GET", "/api/me", LAN, &[("host", "192.168.1.2:7878"), ("authorization", &bearer)], None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&app, "GET", "/api/state", LAN, &[("host", "192.168.1.2:7878"), ("authorization", &bearer)], None).await.0, StatusCode::UNAUTHORIZED);

    // The app token sees everything; with ?scope=brain it narrows to the brain tools.
    let app_bearer = format!("Bearer {}", ctx.settings().await.unwrap().server.app_token);
    let (_, v) = mcp(&app, "/mcp", &app_bearer, 6, "tools/list", json!({})).await;
    assert_eq!(tool_names(&v), ctx.tools().list().iter().map(|t| t.name).collect::<Vec<_>>());
    let (_, v) = mcp(&app, "/mcp?scope=brain", &app_bearer, 7, "tools/list", json!({})).await;
    assert_eq!(tool_names(&v), ctx.tools().brain_tools());

    drop(token);
    assert_eq!(mcp(&app, "/mcp?scope=brain", &bearer, 8, "tools/list", json!({})).await.0, StatusCode::UNAUTHORIZED);
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next(ws: &mut Ws) -> Value {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
    serde_json::from_str(msg.to_text().unwrap()).unwrap()
}

/// Serve `/api/live` as `identity` on a free port; returns its ws URL.
async fn live_as(ctx: &Ctx, identity: Identity) -> String {
    let router = op_core::with_identity(Router::new().route("/api/live", axum::routing::get(op_server::live::handler)).with_state(ctx.clone()), identity);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("ws://{addr}/api/live")
}

#[tokio::test]
async fn a_viewer_socket_never_receives_messages() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let viewer = Identity { role: Role::Viewer, user_id: Some("usr_v".into()), name: None, via: Via::UserToken };
    let manager = Identity { role: Role::Manager, ..viewer.clone() };
    let (mut v_ws, _) = tokio_tungstenite::connect_async(live_as(&ctx, viewer).await).await.unwrap();
    let (mut m_ws, _) = tokio_tungstenite::connect_async(live_as(&ctx, manager).await).await.unwrap();
    // Each socket subscribes to the live layer as it upgrades; wait until both have.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while op_server::live::subscribers(&ctx) < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("both sockets subscribed");

    let message = op_core::alert::MessageLog { id: "ntf_1".into(), address: "+15155550123".into(), ..Default::default() };
    ctx.publish(Event::Message { message });
    ctx.publish(Event::DecisionLog { decision_id: "dec_1".into(), line: "hello".into() });

    assert_eq!(next(&mut v_ws).await["type"], "decision_log", "the viewer skips the message");
    let first = next(&mut m_ws).await;
    assert_eq!((first["type"].as_str(), first["message"]["address"].as_str()), (Some("message"), Some("+15155550123")));
    assert_eq!(next(&mut m_ws).await["type"], "decision_log");
}
