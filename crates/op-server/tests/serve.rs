use futures::StreamExt;
use op_server::{ServeOptions, ServerInfo, build_app, serve};
use serde_json::{Value, json};

#[tokio::test]
async fn serves_api_ui_and_live() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(ServeOptions { data_dir: Some(dir.path().into()), free_port: true, ..Default::default() }).await.unwrap();
    let base = handle.url().to_owned();
    assert!(base.starts_with("http://127.0.0.1:"));
    let http = reqwest::Client::new();

    let info: Value = http.get(format!("{base}/api/server")).send().await.unwrap().json().await.unwrap();
    assert_eq!(info["version"], op_server::VERSION);
    assert_eq!(info["port"], handle.addr().port());
    assert_eq!(info["bind"], "127.0.0.1");

    let state: Value = http.get(format!("{base}/api/state")).send().await.unwrap().json().await.unwrap();
    assert_eq!(state["farm"], Value::Null);

    // UI and SPA fallback.
    let res = http.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    let res = http.get(format!("{base}/herds/abc")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.text().await.unwrap().contains("<html"));
    let res = http.get(format!("{base}/api/nope")).send().await.unwrap();
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.unwrap();
    assert!(body["error"].is_string());
    // Non-UI prefixes never get index.html.
    for path in ["/v1/nope", "/v1", "/collar/v2/x", "/mcp/x"] {
        let res = http.get(format!("{base}{path}")).send().await.unwrap();
        assert_ne!(res.status(), 200, "{path}");
        assert!(!res.text().await.unwrap().contains("<html"), "{path}");
    }

    // Live events.
    let ws_url = format!("{}/api/live", base.replace("http://", "ws://"));
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
    // Give the server a moment to subscribe.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    handle.ctx().publish(op_core::Event::DecisionLog { decision_id: "dec_1".into(), line: "hello".into() });
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
    let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(v, json!({"type": "decision_log", "decision_id": "dec_1", "line": "hello"}));

    handle.shutdown().await.unwrap();
    assert!(http.get(format!("{base}/api/state")).send().await.is_err());
}

/// `/api` and `/mcp` need the app token unless the request is genuinely
/// local: loopback peer, loopback Host, no forwarding headers.
#[tokio::test]
async fn token_required_unless_local() {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    let dir = tempfile::tempdir().unwrap();
    let ctx = op_core::Ctx::open(dir.path()).await.unwrap();
    let token = ctx.settings().await.unwrap().server.app_token;
    let info = ServerInfo { data_dir: dir.path().into(), bind: "127.0.0.1".into(), port: 7878, lan_url: None };
    let app = build_app(ctx.clone(), info, false);

    let lo: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    let lan: SocketAddr = "192.168.1.20:50000".parse().unwrap();
    let req = |method: &str, uri: &str, peer: Option<SocketAddr>, headers: &[(&str, &str)]| {
        let mut b = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let mut r = b.body(Body::empty()).unwrap();
        if let Some(p) = peer {
            r.extensions_mut().insert(ConnectInfo(p));
        }
        r
    };
    let status = |r: Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(r).await.unwrap().status() }
    };
    let local_host = [("host", "127.0.0.1:7878")];
    let bearer = format!("Bearer {token}");

    // Local: no token needed.
    assert_eq!(status(req("GET", "/api/state", Some(lo), &local_host)).await, StatusCode::OK);
    assert_eq!(status(req("GET", "/api/settings", Some(lo), &[("host", "localhost:7878")])).await, StatusCode::OK);
    // A stale browser token doesn't lock the local UI out.
    assert_eq!(status(req("GET", "/api/state", Some(lo), &[("host", "127.0.0.1:7878"), ("authorization", "Bearer old")])).await, StatusCode::OK);
    // Not local: no peer info, LAN peer, rebinding Host, tunnel headers.
    assert_eq!(status(req("GET", "/api/state", None, &local_host)).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(req("GET", "/api/state", Some(lan), &local_host)).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(req("GET", "/api/settings", Some(lo), &[("host", "evil.example:7878")])).await, StatusCode::UNAUTHORIZED);
    for h in ["x-forwarded-for", "forwarded", "cf-connecting-ip", "tailscale-user-login"] {
        let r = req("GET", "/api/settings", Some(lo), &[("host", "127.0.0.1:7878"), (h, "203.0.113.9")]);
        assert_eq!(status(r).await, StatusCode::UNAUTHORIZED, "{h}");
    }
    let tunnel = [("host", "farm.example.com"), ("cf-connecting-ip", "203.0.113.9"), ("x-forwarded-for", "203.0.113.9")];
    assert_eq!(status(req("GET", "/api/settings", Some(lo), &tunnel)).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(req("POST", "/mcp", Some(lo), &tunnel)).await, StatusCode::UNAUTHORIZED);
    // With the token, from anywhere.
    assert_eq!(status(req("GET", "/api/state", Some(lo), &[tunnel[0], tunnel[1], ("authorization", &bearer)])).await, StatusCode::OK);
    assert_eq!(status(req("GET", &format!("/api/state?token={token}"), Some(lan), &local_host)).await, StatusCode::OK);
    assert_eq!(status(req("GET", "/api/state", Some(lan), &[("host", "x"), ("authorization", "Bearer wrong")])).await, StatusCode::UNAUTHORIZED);
    // The UI and collar endpoints aren't behind the app token.
    assert_eq!(status(req("GET", "/", None, &tunnel)).await, StatusCode::OK);
    assert_eq!(status(req("POST", "/collar/v1/report", Some(lo), &tunnel)).await, StatusCode::UNAUTHORIZED); // collar key, not app token
    let body: serde_json::Value = {
        let res = app.clone().oneshot(req("GET", "/collar/v1/boundary", Some(lo), &tunnel)).await.unwrap();
        let bytes = http_body_util::BodyExt::collect(res.into_body()).await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    };
    assert!(body["error"].as_str().unwrap().contains("collar key"), "{body}");

    // A brain token opens /mcp?scope=brain only, and dies with its guard.
    let bt = ctx.mint_brain_token(std::time::Duration::from_secs(60), vec![]);
    let brain_bearer = format!("Bearer {}", bt.as_str());
    let mcp = |uri: &str, auth: &str| {
        req(
            "POST",
            uri,
            Some(lan),
            &[("host", "192.168.1.2:7878"), ("authorization", auth), ("content-type", "application/json"), ("accept", "application/json, text/event-stream")],
        )
    };
    assert_ne!(status(mcp("/mcp?scope=brain", &brain_bearer)).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(mcp("/mcp", &brain_bearer)).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status(req("GET", "/api/settings", Some(lan), &[("host", "x"), ("authorization", &brain_bearer)])).await, StatusCode::UNAUTHORIZED);
    drop(bt);
    assert_eq!(status(mcp("/mcp?scope=brain", &brain_bearer)).await, StatusCode::UNAUTHORIZED);

    // Cross-site writes and WebSocket upgrades on local ambient authority are refused.
    let evil = [("host", "127.0.0.1:7878"), ("origin", "http://evil.example"), ("content-type", "text/plain")];
    assert_eq!(status(req("POST", "/api/herds/h/decide", Some(lo), &evil)).await, StatusCode::FORBIDDEN);
    assert_eq!(status(req("PUT", "/api/settings", Some(lo), &evil)).await, StatusCode::FORBIDDEN);
    let ws_evil = [("host", "127.0.0.1:7878"), ("origin", "http://evil.example"), ("upgrade", "websocket"), ("connection", "upgrade")];
    assert_eq!(status(req("GET", "/api/live", Some(lo), &ws_evil)).await, StatusCode::FORBIDDEN);
    // Same origin is fine.
    let same = [("host", "127.0.0.1:7878"), ("origin", "http://127.0.0.1:7878")];
    assert_eq!(status(req("POST", "/api/herds/nope/decide", Some(lo), &same)).await, StatusCode::NOT_FOUND);
    // The Vite dev origin only in dev mode.
    let vite = [("host", "127.0.0.1:7878"), ("origin", "http://localhost:5173")];
    assert_eq!(status(req("POST", "/api/herds/nope/decide", Some(lo), &vite)).await, StatusCode::FORBIDDEN);
    let dev_app = build_app(ctx.clone(), ServerInfo { data_dir: dir.path().into(), bind: "127.0.0.1".into(), port: 7878, lan_url: None }, true);
    assert_eq!(dev_app.clone().oneshot(req("POST", "/api/herds/nope/decide", Some(lo), &vite)).await.unwrap().status(), StatusCode::NOT_FOUND);
    // Dev CORS answers the Vite origin only.
    let pre =
        |origin: &str| req("OPTIONS", "/api/state", Some(lo), &[("host", "127.0.0.1:7878"), ("origin", origin), ("access-control-request-method", "POST")]);
    let res = dev_app.clone().oneshot(pre("http://localhost:5173")).await.unwrap();
    assert_eq!(res.headers().get("access-control-allow-origin").unwrap(), "http://localhost:5173");
    let res = dev_app.clone().oneshot(pre("http://evil.example")).await.unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
}
