//! Roles end to end through the guard (field-ready §2.2): people added with
//! and without sign-in, sign-in links, what each role may do over REST, MCP
//! and the live socket, redaction, revocation, and who answered a decision.
//! Requests act as someone else the way a browser off this machine does:
//! with a forwarding header (so they're never local) and a bearer token.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::Ctx;
use op_server::{ServeOptions, ServerInfo, build_app, serve};
use serde_json::{Value, json};
use tower::ServiceExt;

const LO: &str = "127.0.0.1:50000";
const HOST: &str = "127.0.0.1:7878";
const AWAY: &str = "203.0.113.9";

struct Farm {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    app: Router,
    herd: String,
    paddock: String,
    p2: String,
}

fn square(lon: f64) -> Value {
    json!({"type": "Polygon", "coordinates": [[[lon, 42.03], [lon + 0.005, 42.03], [lon + 0.005, 42.0336], [lon, 42.0336], [lon, 42.03]]]})
}

async fn farm() -> Farm {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let info = ServerInfo { data_dir: dir.path().into(), bind: "127.0.0.1".into(), port: 7878, lan_url: None };
    let app = build_app(ctx.clone(), info, false);
    let mut f = Farm { _dir: dir, ctx, app, herd: String::new(), paddock: String::new(), p2: String::new() };
    f.local("POST", "/api/farm", Some(json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]}))).await;
    f.paddock = f.local("POST", "/api/paddocks", Some(json!({"name": "P1", "geometry": square(-93.625)}))).await.1["id"].as_str().unwrap().to_owned();
    f.p2 = f.local("POST", "/api/paddocks", Some(json!({"name": "P2", "geometry": square(-93.62)}))).await.1["id"].as_str().unwrap().to_owned();
    let herd = json!({"name": "Cows", "species": "cattle", "count": 250, "paddock_id": f.paddock});
    f.herd = f.local("POST", "/api/herds", Some(herd)).await.1["id"].as_str().unwrap().to_owned();
    f
}

async fn send(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)], body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri).header("host", HOST);
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
    req.extensions_mut().insert(ConnectInfo(LO.parse::<SocketAddr>().unwrap()));
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

impl Farm {
    /// This machine: the owner.
    async fn local(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        send(&self.app, method, uri, &[], body).await
    }

    /// From elsewhere, with `token` (or none).
    async fn away(&self, token: Option<&str>, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let bearer = token.map(|t| format!("Bearer {t}"));
        let mut h = vec![("x-forwarded-for", AWAY)];
        if let Some(b) = &bearer {
            h.push(("authorization", b.as_str()));
        }
        send(&self.app, method, uri, &h, body).await
    }

    /// A new person with `role`, signed in from elsewhere through their link: their token.
    async fn signed_in(&self, name: &str, role: &str) -> String {
        let (s, inv) = self.local("POST", "/api/invites", Some(json!({"name": name, "role": role}))).await;
        assert_eq!(s, StatusCode::CREATED, "{inv}");
        let (s, acc) = self.away(None, "POST", "/api/invites/accept", Some(json!({"code": inv["code"]}))).await;
        assert_eq!(s, StatusCode::OK, "{acc}");
        acc["token"].as_str().unwrap().to_owned()
    }

    async fn mcp(&self, token: &str, id: u64, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let (s, v) = self.away(Some(token), "POST", "/mcp", Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    fn reads(&self) -> Vec<String> {
        let (h, p) = (&self.herd, &self.paddock);
        [
            "/api/me".to_owned(),
            "/api/state".into(),
            "/api/farm".into(),
            "/api/paddocks".into(),
            format!("/api/paddocks/{p}"),
            "/api/herds".into(),
            format!("/api/herds/{h}"),
            format!("/api/herds/{h}/boundary"),
            "/api/animals".into(),
            "/api/collars".into(),
            "/api/positions".into(),
            "/api/settings".into(),
            "/api/decisions".into(),
            "/api/brains".into(),
            format!("/api/signals?herd_id={h}"),
            format!("/api/analytics/pasture?herd_id={h}"),
            "/api/server".into(),
        ]
        .into()
    }

    /// Writes a manager may make, as (method, path, body).
    fn farm_writes(&self) -> Vec<(&'static str, String, Option<Value>)> {
        let (h, p) = (&self.herd, &self.paddock);
        vec![
            ("POST", "/api/paddocks".into(), Some(json!({"name": "P9", "geometry": square(-93.60)}))),
            ("PATCH", format!("/api/paddocks/{p}"), Some(json!({"notes": "wet corner"}))),
            ("PATCH", format!("/api/herds/{h}"), Some(json!({"autonomy": "propose"}))),
            ("POST", "/api/herds".into(), Some(json!({"name": "Heifers", "species": "cattle", "count": 12}))),
            ("POST", format!("/api/herds/{h}/boundary"), Some(json!({"geometry": square(-93.625)}))),
            ("POST", "/api/collars".into(), Some(json!({"herd_id": h}))),
            ("POST", "/api/sql".into(), Some(json!({"sql": "select 1"}))),
            ("POST", format!("/api/herds/{h}/decide"), None),
            ("POST", "/api/decisions/dec_x/respond".into(), Some(json!({"action": "reject"}))),
        ]
    }

    fn owner_only(&self) -> Vec<(&'static str, String, Option<Value>)> {
        vec![
            ("PUT", "/api/settings".into(), Some(json!({"decision_time": "05:00"}))),
            ("GET", "/api/secrets".into(), None),
            ("PUT", "/api/secrets/firecrawl_api_key".into(), Some(json!({"value": "x"}))),
            ("GET", "/api/users".into(), None),
            ("POST", "/api/users".into(), Some(json!({"name": "X", "role": "owner"}))),
            ("GET", "/api/invites".into(), None),
            ("POST", "/api/invites".into(), Some(json!({"name": "X", "role": "owner"}))),
            ("GET", "/api/tokens".into(), None),
            ("GET", "/api/brains/hosted/keys".into(), None),
            ("POST", "/api/brains/hosted/keys".into(), Some(json!({}))),
            ("GET", "/api/notify/channels".into(), None),
            ("GET", "/api/texting".into(), None),
            ("PUT", "/api/push/settings".into(), Some(json!({}))),
            ("POST", "/api/collars/col_x/rekey".into(), None),
        ]
    }

    fn hand_routes(&self) -> Vec<(&'static str, String)> {
        let h = &self.herd;
        vec![
            ("POST", "/api/alerts/alr_x/ack".into()),
            ("POST", "/api/alerts/alr_x/resolve".into()),
            ("PUT", "/api/alerts/prefs/me".into()),
            ("POST", format!("/api/herds/{h}/move/stop")),
            ("POST", "/api/collars/col_x/escape/stop".into()),
            ("POST", "/api/collars/col_x/park".into()),
            ("POST", "/api/collars/col_x/unpark".into()),
            ("POST", "/api/fleet/col_x/fit-checks".into()),
            ("POST", "/api/fleet/fit-checks".into()),
            ("POST", format!("/api/paddocks/{}/heights", self.paddock)),
            ("POST", "/api/feed-log".into()),
            ("POST", format!("/api/herds/{h}/check")),
        ]
    }
}

fn names(v: &Value) -> Vec<String> {
    v["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect()
}

fn refused(s: StatusCode) -> bool {
    s == StatusCode::FORBIDDEN || s == StatusCode::UNAUTHORIZED
}

#[tokio::test]
async fn a_person_added_without_sign_in_has_no_token() {
    let f = farm().await;
    let (s, p) = f.local("POST", "/api/users", Some(json!({"name": "Luis", "role": "hand", "phone": "+1 515 555 0123"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    assert_eq!((p["phone"].as_str(), p["tokens"].as_u64()), (Some("+15155550123"), Some(0)));
    let (_, tokens) = f.local("GET", "/api/tokens", None).await;
    assert_eq!(tokens, json!([]));
    let (_, invites) = f.local("GET", "/api/invites", None).await;
    assert_eq!(invites, json!([]));
    let (_, people) = f.local("GET", "/api/users", None).await;
    assert_eq!(people.as_array().unwrap().len(), 1);
    // From elsewhere nobody gets in without a token.
    assert_eq!(f.away(None, "GET", "/api/state", None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_accepted_link_signs_that_person_in() {
    let f = farm().await;
    let (_, inv) = f.local("POST", "/api/invites", Some(json!({"name": "Ana", "role": "manager", "phone": "5155550111"}))).await;
    let code = inv["code"].as_str().unwrap();
    assert!(inv["url"].as_str().unwrap().ends_with(&format!("/#/join/{code}")));
    // Accepting needs no token, even from elsewhere.
    let (s, acc) = f.away(None, "POST", "/api/invites/accept", Some(json!({"code": code}))).await;
    assert_eq!(s, StatusCode::OK, "{acc}");
    let token = acc["token"].as_str().unwrap();
    assert!(token.starts_with("opu_") && token.len() == 68, "{token}");

    let (s, me) = f.away(Some(token), "GET", "/api/me", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((me["role"].as_str(), me["via"].as_str()), (Some("manager"), Some("user_token")));
    assert_eq!(
        (me["user"]["name"].as_str(), me["user"]["phone"].as_str(), me["user"]["phone_verified"].as_bool()),
        (Some("Ana"), Some("+15155550111"), Some(false))
    );

    // Used once.
    let (s, e) = f.away(None, "POST", "/api/invites/accept", Some(json!({"code": code}))).await;
    assert_eq!(s, StatusCode::GONE, "{e}");
    // A new link for the same person signs in a second browser.
    let (_, inv2) = f.local("POST", "/api/invites", Some(json!({"user_id": me["user"]["id"]}))).await;
    let (s, _) = f.away(None, "POST", "/api/invites/accept", Some(json!({"code": inv2["code"]}))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, p) = f.local("GET", &format!("/api/users/{}", me["user"]["id"].as_str().unwrap()), None).await;
    assert_eq!(p["tokens"], 2);
}

#[tokio::test]
async fn a_viewer_reads_and_never_writes() {
    let f = farm().await;
    let viewer = f.signed_in("Vic", "viewer").await;
    for path in f.reads() {
        let (s, v) = f.away(Some(&viewer), "GET", &path, None).await;
        assert_eq!(s, StatusCode::OK, "GET {path}: {v}");
    }
    let mut unsafe_writes = f.farm_writes();
    unsafe_writes.extend(f.owner_only().into_iter().filter(|(m, ..)| *m != "GET"));
    unsafe_writes.extend(f.hand_routes().into_iter().map(|(m, p)| (m, p, Some(json!({})))));
    unsafe_writes.push(("DELETE", format!("/api/paddocks/{}", f.paddock), None));
    unsafe_writes.push(("DELETE", format!("/api/herds/{}", f.herd), None));
    for (m, p, b) in unsafe_writes {
        let (s, v) = f.away(Some(&viewer), m, &p, b).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{m} {p}: {v}");
        assert_eq!(v["error"], "Your role can't do this.");
    }
    for (m, p) in [("GET", "/api/messages"), ("GET", "/api/alerts/prefs"), ("GET", "/api/users")] {
        assert_eq!(f.away(Some(&viewer), m, p, None).await.0, StatusCode::FORBIDDEN, "{m} {p}");
    }
    // Their own profile and push subscription are theirs.
    let (s, u) = f.away(Some(&viewer), "PATCH", "/api/me/profile", Some(json!({"email": "vic@example.com"}))).await;
    assert_eq!(s, StatusCode::OK, "{u}");
    for (m, p) in [("POST", "/api/push/subscriptions"), ("DELETE", "/api/push/subscriptions/psh_x")] {
        assert!(!refused(f.away(Some(&viewer), m, p, Some(json!({}))).await.0), "{m} {p} passes the guard");
    }

    // MCP: read tools only; a write call fails.
    op_engine::register_tools(&f.ctx);
    let listed = names(&f.mcp(&viewer, 1, "tools/list", json!({})).await);
    assert_eq!(listed, op_engine::mcp::READ_TOOLS, "no write tools");
    let v = f.mcp(&viewer, 2, "tools/call", json!({"name": "propose_boundary", "arguments": {"reasoning": "x", "to_paddock_id": f.p2}})).await;
    assert!(v["error"]["message"].as_str().unwrap().contains("Unknown tool"), "{v}");
    let v = f.mcp(&viewer, 3, "tools/call", json!({"name": "get_farm", "arguments": {}})).await;
    assert_eq!(v["result"]["isError"], false, "{v}");
}

#[tokio::test]
async fn a_viewer_socket_opens() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(ServeOptions { data_dir: Some(dir.path().into()), free_port: true, ..Default::default() }).await.unwrap();
    let ctx = handle.ctx().clone();
    let (_, code) = op_core::people::create_invite(
        &ctx,
        op_core::people::NewInvite { name: Some("Vic".into()), role: Some(op_core::Role::Viewer), ..Default::default() },
        &op_core::Identity::owner(op_core::Via::Local).actor(),
    )
    .await
    .unwrap();
    let token = op_core::people::accept_invite(&ctx, &code, None).await.unwrap().token;
    let ws = handle.url().replace("http://", "ws://") + "/api/live";

    let mut req = format!("{ws}?token={token}").into_client_request().unwrap();
    req.headers_mut().insert("x-forwarded-for", AWAY.parse().unwrap());
    let (_socket, res) = tokio_tungstenite::connect_async(req).await.expect("the viewer's socket opens");
    assert_eq!(res.status(), StatusCode::SWITCHING_PROTOCOLS);

    // A token that doesn't sign anyone in doesn't open it.
    let mut bad = format!("{ws}?token=opu_{}", "0".repeat(64)).into_client_request().unwrap();
    bad.headers_mut().insert("x-forwarded-for", AWAY.parse().unwrap());
    assert!(tokio_tungstenite::connect_async(bad).await.is_err());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_hand_passes_its_allowlist_and_nothing_else() {
    let f = farm().await;
    let hand = f.signed_in("Hal", "hand").await;
    for (m, p) in f.hand_routes() {
        let (s, v) = f.away(Some(&hand), m, &p, Some(json!({}))).await;
        assert!(!refused(s), "{m} {p} passes the guard: {s} {v}");
    }
    for (m, p, b) in f.farm_writes() {
        let (s, v) = f.away(Some(&hand), m, &p, b).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{m} {p}: {v}");
    }
    assert_eq!(f.away(Some(&hand), "GET", "/api/messages", None).await.0, StatusCode::FORBIDDEN);
    for path in f.reads() {
        assert_eq!(f.away(Some(&hand), "GET", &path, None).await.0, StatusCode::OK, "GET {path}");
    }
    op_engine::register_tools(&f.ctx);
    let listed = names(&f.mcp(&hand, 1, "tools/list", json!({})).await);
    assert!(!listed.iter().any(|n| n == "propose_boundary"), "{listed:?}");
}

#[tokio::test]
async fn a_manager_runs_the_farm_but_not_the_owners_things() {
    let f = farm().await;
    let manager = f.signed_in("Ana", "manager").await;
    for (m, p, b) in f.farm_writes() {
        let (s, v) = f.away(Some(&manager), m, &p, b).await;
        assert!(!refused(s), "{m} {p}: {s} {v}");
    }
    let (s, p) = f.away(Some(&manager), "POST", "/api/paddocks", Some(json!({"name": "P5", "geometry": square(-93.59)}))).await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    for (m, p, b) in f.owner_only() {
        let (s, v) = f.away(Some(&manager), m, &p, b).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{m} {p}: {v}");
    }
    for p in ["/api/messages", "/api/alerts/prefs"] {
        assert!(!refused(f.away(Some(&manager), "GET", p, None).await.0), "GET {p}");
    }
    op_engine::register_tools(&f.ctx);
    let listed = names(&f.mcp(&manager, 1, "tools/list", json!({})).await);
    assert!(listed.iter().any(|n| n == "propose_boundary"), "all tools: {listed:?}");
}

#[tokio::test]
async fn the_owner_does_everything() {
    let f = farm().await;
    let app_token = f.ctx.settings().await.unwrap().server.app_token;
    let person_token = f.signed_in("Cody", "owner").await;
    for token in [app_token.as_str(), person_token.as_str()] {
        for (m, p, b) in f.owner_only().into_iter().chain(f.farm_writes()) {
            let (s, v) = f.away(Some(token), m, &p, b).await;
            assert!(!refused(s), "{m} {p}: {s} {v}");
        }
    }
    for (m, p, b) in f.owner_only() {
        let (s, v) = f.local(m, &p, b).await;
        assert!(!refused(s), "local {m} {p}: {s} {v}");
    }
    // The app token and local requests act as the owner person now that there is one.
    let (_, me) = f.local("GET", "/api/me", None).await;
    assert_eq!((me["via"].as_str(), me["user"]["name"].as_str()), (Some("local"), Some("Cody")));
    let (_, me) = f.away(Some(&app_token), "GET", "/api/me", None).await;
    assert_eq!((me["via"].as_str(), me["user"]["name"].as_str()), (Some("app_token"), Some("Cody")));
}

#[tokio::test]
async fn only_owners_see_the_app_token() {
    let f = farm().await;
    let app_token = f.ctx.settings().await.unwrap().server.app_token;
    let cases = [
        (f.signed_in("Vic", "viewer").await, ""),
        (f.signed_in("Hal", "hand").await, ""),
        (f.signed_in("Ana", "manager").await, ""),
        (f.signed_in("Cody", "owner").await, app_token.as_str()),
        (app_token.clone(), app_token.as_str()),
    ];
    for (token, want) in &cases {
        let (_, s) = f.away(Some(token), "GET", "/api/settings", None).await;
        let (_, st) = f.away(Some(token), "GET", "/api/state", None).await;
        assert_eq!(s["server"]["app_token"], *want);
        assert_eq!(st["settings"]["server"]["app_token"], *want);
    }
    assert_eq!(f.local("GET", "/api/settings", None).await.1["server"]["app_token"], app_token.as_str());
}

#[tokio::test]
async fn a_revoked_token_is_refused_at_once() {
    let f = farm().await;
    let token = f.signed_in("Ana", "manager").await;
    assert_eq!(f.away(Some(&token), "GET", "/api/state", None).await.0, StatusCode::OK);
    let (_, tokens) = f.local("GET", "/api/tokens", None).await;
    let id = tokens[0]["id"].as_str().unwrap();
    assert!(tokens[0]["last_used"].is_string(), "the use was recorded");
    assert_eq!(f.local("DELETE", &format!("/api/tokens/{id}"), None).await.0, StatusCode::NO_CONTENT);
    let (s, e) = f.away(Some(&token), "GET", "/api/state", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{e}");
    // Even from this machine a revoked person token is refused, not the owner.
    let bearer = format!("Bearer {token}");
    assert_eq!(send(&f.app, "GET", "/api/state", &[("authorization", &bearer)], None).await.0, StatusCode::UNAUTHORIZED);

    // Revoking a person signs out every browser and drops their open link.
    let t2 = f.signed_in("Hal", "hand").await;
    let (_, me) = f.away(Some(&t2), "GET", "/api/me", None).await;
    let uid = me["user"]["id"].as_str().unwrap();
    f.local("POST", "/api/invites", Some(json!({"user_id": uid}))).await;
    let (s, p) = f.local("POST", &format!("/api/users/{uid}/revoke"), None).await;
    assert_eq!((s, p["tokens"].as_u64()), (StatusCode::OK, Some(0)));
    assert!(p.get("invite_until").is_none());
    assert_eq!(f.away(Some(&t2), "GET", "/api/me", None).await.0, StatusCode::UNAUTHORIZED);

    // Signing out revokes only this browser's token.
    let t3 = f.signed_in("Kim", "viewer").await;
    assert_eq!(f.away(Some(&t3), "POST", "/api/me/signout", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(f.away(Some(&t3), "GET", "/api/me", None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_used_or_dropped_link_does_not_sign_in() {
    let f = farm().await;
    let (_, inv) = f.local("POST", "/api/invites", Some(json!({"name": "Ana", "role": "hand"}))).await;
    let code = inv["code"].as_str().unwrap();
    assert_eq!(f.away(None, "POST", "/api/invites/accept", Some(json!({"code": code}))).await.0, StatusCode::OK);
    assert_eq!(f.away(None, "POST", "/api/invites/accept", Some(json!({"code": code}))).await.0, StatusCode::GONE);

    let (_, inv) = f.local("POST", "/api/invites", Some(json!({"user_id": inv["user_id"]}))).await;
    assert_eq!(f.local("DELETE", &format!("/api/invites/{}", inv["id"].as_str().unwrap()), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(f.away(None, "POST", "/api/invites/accept", Some(json!({"code": inv["code"]}))).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn accepting_links_is_limited_per_peer() {
    let f = farm().await;
    let from = |ip: &'static str| vec![("x-forwarded-for", ip)];
    for i in 0..5 {
        let (s, _) = send(&f.app, "POST", "/api/invites/accept", &from("198.51.100.7"), Some(json!({"code": format!("{i:032}")}))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "try {i}");
    }
    let (s, e) = send(&f.app, "POST", "/api/invites/accept", &from("198.51.100.7"), Some(json!({"code": "x"}))).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{e}");
    // Even a good code waits: the limit is per peer, not per code.
    let (_, inv) = f.local("POST", "/api/invites", Some(json!({"name": "Ana", "role": "hand"}))).await;
    assert_eq!(send(&f.app, "POST", "/api/invites/accept", &from("198.51.100.7"), Some(json!({"code": inv["code"]}))).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(send(&f.app, "POST", "/api/invites/accept", &from("198.51.100.8"), Some(json!({"code": inv["code"]}))).await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_person_token_wins_over_being_local() {
    let f = farm().await;
    let viewer = f.signed_in("Vic", "viewer").await;
    let bearer = format!("Bearer {viewer}");
    let (s, me) = send(&f.app, "GET", "/api/me", &[("authorization", &bearer)], None).await;
    assert_eq!((s, me["role"].as_str()), (StatusCode::OK, Some("viewer")), "a second browser profile on this machine is that person");
    assert_eq!(
        send(&f.app, "POST", "/api/paddocks", &[("authorization", &bearer)], Some(json!({"name": "P9", "geometry": square(-93.6)}))).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn answers_record_who_answered() {
    let f = farm().await;
    let manager = f.signed_in("Ana", "manager").await;
    let (_, me) = f.away(Some(&manager), "GET", "/api/me", None).await;
    let uid = me["user"]["id"].as_str().unwrap().to_owned();
    op_engine::register_tools(&f.ctx);

    // MCP: a proposal by the manager keeps who proposed it.
    let propose = |id| {
        let args = json!({"herd_id": f.herd, "to_paddock_id": f.p2, "reasoning": "P1 is short."});
        f.mcp(&manager, id, "tools/call", json!({"name": "propose_boundary", "arguments": args}))
    };
    let v = propose(1).await;
    let d = &v["result"]["structuredContent"]["decision"];
    assert_eq!(d["status"], "proposed", "{v}");
    assert_eq!(d["inputs"]["by"], json!({"via": "user_token", "user_id": uid, "name": "Ana"}));
    let first = d["id"].as_str().unwrap().to_owned();

    // REST (the app's path): rejecting records the manager.
    let (s, d) = f.away(Some(&manager), "POST", &format!("/api/decisions/{first}/respond"), Some(json!({"action": "reject", "note": "Too wet."}))).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    let (_, d) = f.away(Some(&manager), "GET", &format!("/api/decisions/{first}"), None).await;
    assert_eq!(d["inputs"]["farmer_response"]["by"], json!({"via": "user_token", "user_id": uid, "name": "Ana"}));
    assert_eq!(d["inputs"]["farmer_response"]["action"], "reject");
    let events = f.ctx.store().list_events(Some(("decision", &first)), 10).await.unwrap();
    let rejected = events.iter().find(|e| e.kind == "decision.rejected").expect("rejected event");
    assert_eq!(rejected.payload["by"], "Ana");

    // Approving a MOVE: the answer and the applied event name her too.
    let v = propose(2).await;
    let second = v["result"]["structuredContent"]["decision"]["id"].as_str().unwrap().to_owned();
    let (s, d) = f.away(Some(&manager), "POST", &format!("/api/decisions/{second}/respond"), Some(json!({"action": "approve"}))).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    let (_, d) = f.local("GET", &format!("/api/decisions/{second}"), None).await;
    assert_eq!(d["inputs"]["farmer_response"]["by"]["name"], "Ana");
    let events = f.ctx.store().list_events(Some(("decision", &second)), 10).await.unwrap();
    let applied = events.iter().find(|e| e.kind == "decision.applied").expect("applied event");
    assert_eq!(applied.payload["by"], "Ana");

    // The owner on this machine without a person reads as "owner".
    let v = propose(3).await;
    let third = v["result"]["structuredContent"]["decision"]["id"].as_str().unwrap().to_owned();
    f.local("POST", &format!("/api/decisions/{third}/respond"), Some(json!({"action": "reject"}))).await;
    let (_, d) = f.local("GET", &format!("/api/decisions/{third}"), None).await;
    assert_eq!(d["inputs"]["farmer_response"]["by"], json!({"via": "local"}));
    let events = f.ctx.store().list_events(Some(("decision", &third)), 10).await.unwrap();
    assert_eq!(events.iter().find(|e| e.kind == "decision.rejected").unwrap().payload["by"], "owner");
}
