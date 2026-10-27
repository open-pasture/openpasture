//! Limits on the public routes: refusals per peer, reports per collar key,
//! and the body size, all answered before the route pays for its own checks.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use op_server::{ServerInfo, build_app};
use serde_json::json;
use tower::ServiceExt;

async fn app() -> (tempfile::TempDir, op_core::Ctx, Router) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = op_core::Ctx::open(dir.path()).await.unwrap();
    let info = ServerInfo { data_dir: dir.path().into(), bind: "0.0.0.0".into(), port: 7878, lan_url: None };
    let app = build_app(ctx.clone(), info, false);
    (dir, ctx, app)
}

async fn send(app: &Router, method: &str, uri: &str, peer: &str, headers: &[(&str, &str)], body: Body) -> axum::response::Response {
    let mut b = Request::builder().method(method).uri(uri).header("content-type", "application/json");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    app.clone().oneshot(req).await.unwrap()
}

#[tokio::test]
async fn a_peer_refused_too_often_waits_a_minute() {
    let (_dir, _ctx, app) = app().await;
    let report = || Body::from(json!({ "fixes": [] }).to_string());
    for _ in 0..30 {
        let r = send(&app, "POST", "/collar/v1/report", "198.51.100.7:4000", &[("authorization", "Bearer nope")], report()).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
    let r = send(&app, "POST", "/collar/v1/report", "198.51.100.7:4000", &[("authorization", "Bearer nope")], report()).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers()["retry-after"], "60");
    // Another peer (another farm behind another address) is untouched.
    let r = send(&app, "POST", "/collar/v1/report", "198.51.100.8:4000", &[("authorization", "Bearer other")], report()).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn one_collar_reports_at_most_thirty_times_a_minute_and_bodies_are_capped() {
    let (_dir, ctx, app) = app().await;
    let token = ctx.settings().await.unwrap().server.app_token;
    let owner = format!("Bearer {token}");
    let ok = |s: StatusCode| assert!(s.is_success(), "{s}");
    ok(send(
        &app,
        "POST",
        "/api/farm",
        "10.0.0.2:1",
        &[("authorization", &owner)],
        Body::from(json!({"name": "F", "timezone": "UTC", "center": [-93.6, 42.0]}).to_string()),
    )
    .await
    .status());
    let r = send(
        &app,
        "POST",
        "/api/herds",
        "10.0.0.2:1",
        &[("authorization", &owner)],
        Body::from(json!({"name": "Cows", "species": "cattle", "count": 1}).to_string()),
    )
    .await;
    let herd: serde_json::Value = serde_json::from_slice(&http_body_util::BodyExt::collect(r.into_body()).await.unwrap().to_bytes()).unwrap();
    let r = send(&app, "POST", "/api/collars", "10.0.0.2:1", &[("authorization", &owner)], Body::from(json!({"herd_id": herd["id"]}).to_string())).await;
    let linked: serde_json::Value = serde_json::from_slice(&http_body_util::BodyExt::collect(r.into_body()).await.unwrap().to_bytes()).unwrap();
    let key = format!("Bearer {}", linked["key"].as_str().unwrap());
    for i in 0..30 {
        let r = send(&app, "POST", "/collar/v1/report", "100.64.0.1:4000", &[("authorization", &key)], Body::from(json!({ "fixes": [] }).to_string())).await;
        assert_eq!(r.status(), StatusCode::OK, "report {i}");
    }
    let r = send(&app, "POST", "/collar/v1/report", "100.64.0.1:4000", &[("authorization", &key)], Body::from(json!({ "fixes": [] }).to_string())).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(r.headers().contains_key("retry-after"));
    // Astra 02 F16: the same key with other spaces or the scheme's case is the same collar.
    let bare = linked["key"].as_str().unwrap();
    for spaced in [format!("Bearer  {bare}"), format!("bearer {bare}  "), format!("Bearer {bare}\t")] {
        let r = send(&app, "POST", "/collar/v1/report", "100.64.0.1:4000", &[("authorization", &spaced)], Body::from(json!({ "fixes": [] }).to_string())).await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS, "{spaced:?}");
    }
    // A body that says it is over 2 MB is refused before it is read.
    let r = send(&app, "POST", "/collar/v1/ack", "100.64.0.1:4000", &[("authorization", &key), ("content-length", "3000000")], Body::empty()).await;
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // Astra 02 F16: a chunked body (no length) is cut at the cap as it streams in, not read on.
    let read = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let track = read.clone();
    let chunk = 256 * 1024;
    let stream = futures::stream::iter((0..12).map(move |_| {
        track.fetch_add(chunk, std::sync::atomic::Ordering::SeqCst);
        Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![b' '; chunk]))
    }));
    let r = send(&app, "POST", "/collar/v1/ack", "100.64.0.1:4000", &[("authorization", &key)], Body::from_stream(stream)).await;
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let n = read.load(std::sync::atomic::Ordering::SeqCst);
    assert!(n <= 2 * 1024 * 1024 + chunk, "{n} bytes read past the 2 MB cap");
}
