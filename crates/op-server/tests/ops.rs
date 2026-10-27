//! Server operations: one server per data dir, `server.json`, `/healthz`,
//! the job table and disk use in `/api/server`, retention, and opaque 500s.

use std::time::Duration;

use op_core::instance::{self, AlreadyRunning};
use op_server::{ServeOptions, serve};
use serde_json::{Value, json};

fn opts(dir: &std::path::Path) -> ServeOptions {
    ServeOptions { data_dir: Some(dir.into()), free_port: true, ..Default::default() }
}

#[tokio::test]
async fn one_server_per_data_dir_and_server_json_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(opts(dir.path())).await.unwrap();
    let running = instance::running(dir.path()).expect("server.json");
    assert_eq!(running.pid, std::process::id());
    assert_eq!(running.url.as_deref(), Some(handle.url()));
    assert_eq!(running.port, Some(handle.addr().port()));

    // A second server on the same data dir is refused (not started on another port).
    let err = serve(opts(dir.path())).await.err().expect("refused");
    let busy = err.downcast_ref::<AlreadyRunning>().expect("typed error");
    assert_eq!(busy.server.as_ref().and_then(|s| s.url.as_deref()), Some(handle.url()));

    let base = handle.url().to_owned();
    handle.shutdown().await.unwrap();
    assert!(!dir.path().join(instance::SERVER_FILE).exists(), "server.json goes with the server");
    assert!(instance::running(dir.path()).is_none());
    assert!(
        !dir.path().join("openpasture.db-wal").exists() || std::fs::metadata(dir.path().join("openpasture.db-wal")).unwrap().len() == 0,
        "WAL checkpointed on close"
    );
    assert!(reqwest::get(format!("{base}/healthz")).await.is_err());

    // Free again.
    serve(opts(dir.path())).await.unwrap().shutdown().await.unwrap();
}

#[tokio::test]
async fn healthz_is_open_and_says_which_job_is_in_trouble() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(ServeOptions { bind: Some("127.0.0.1".into()), ..opts(dir.path()) }).await.unwrap();
    let base = handle.url().to_owned();
    let http = reqwest::Client::new();

    // No token, and a forwarded request (as through a proxy) is not "local".
    let res = http.get(format!("{base}/healthz")).header("x-forwarded-for", "203.0.113.9").send().await.unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["status"], "ok", "{body}");
    assert_eq!(body["db"], "ok");
    let names: Vec<&str> = body["jobs"].as_array().unwrap().iter().map(|j| j["name"].as_str().unwrap()).collect();
    for job in [
        "alerts.engine",
        "ingest.moves",
        "ingest.escapes",
        "ingest.schedules",
        "ingest.activations",
        "engine.scheduler",
        "analytics.rollup",
        "store.checkpoint",
    ] {
        assert!(names.contains(&job), "{job} supervised: {names:?}");
    }
    let token = handle.ctx().settings().await.unwrap().server.app_token;
    assert!(!body.to_string().contains(&token));
    assert!(!body.to_string().contains(dir.path().to_str().unwrap()), "no paths");

    // A job that panics shows as restarting, with its error in /api/server only.
    handle.ctx().supervise("test.broken", None, |_ctx| async { panic!("boom at /secret/path") });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let body: Value = http.get(format!("{base}/healthz")).send().await.unwrap().json().await.unwrap();
    assert_eq!(body["status"], "degraded");
    assert!(!body.to_string().contains("boom"));
    let info: Value = http.get(format!("{base}/api/server")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    let broken = info["tasks"].as_array().unwrap().iter().find(|t| t["name"] == "test.broken").unwrap().clone();
    assert_eq!(broken["status"], "restarting");
    assert!(broken["last_error"].as_str().unwrap().contains("boom"));
    assert!(info["disk"]["db_bytes"].as_u64().unwrap() > 0);
    assert!(info["disk"]["total_bytes"].as_u64().unwrap() >= info["disk"]["db_bytes"].as_u64().unwrap());

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn retention_defaults_to_keeping_everything_and_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(opts(dir.path())).await.unwrap();
    let base = handle.url().to_owned();
    let http = {
        let mut auth = reqwest::header::HeaderMap::new();
        auth.insert("authorization", format!("Bearer {}", handle.ctx().settings().await.unwrap().server.app_token).parse().unwrap());
        reqwest::Client::builder().default_headers(auth).build().unwrap()
    };
    let r: Value = http.get(format!("{base}/api/server/retention")).send().await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"fixes_days": null, "cues_days": null, "health_days": null}));
    let res = http.put(format!("{base}/api/server/retention")).json(&json!({"cues_days": 2})).send().await.unwrap();
    assert_eq!(res.status(), 400);
    let res = http.put(format!("{base}/api/server/retention")).json(&json!({"fixes_days": 400, "health_days": 90})).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let r: Value = http.get(format!("{base}/api/server/retention")).send().await.unwrap().json().await.unwrap();
    assert_eq!(r, json!({"fixes_days": 400, "cues_days": null, "health_days": 90}));
    handle.shutdown().await.unwrap();
}

#[test]
fn an_internal_error_reaches_the_client_as_a_reference_only() {
    let e = op_core::ApiError::from(anyhow::anyhow!("no such table: secret_table at /var/lib/openpasture"));
    assert_eq!(e.status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!e.message.contains("secret_table") && !e.message.contains("/var/lib"), "{}", e.message);
    let r = e.message.split("(ref ").nth(1).and_then(|s| s.split(')').next()).expect("a reference");
    assert_eq!(r.len(), 10);
}

fn job(name: &'static str, critical: bool, down_for: Option<chrono::Duration>, at: chrono::DateTime<chrono::Utc>) -> op_core::tasks::TaskInfo {
    use op_core::tasks::{TaskInfo, TaskStatus};
    TaskInfo {
        name,
        status: if down_for.is_some() { TaskStatus::Restarting } else { TaskStatus::Running },
        started_at: at,
        every_s: None,
        last_beat_at: None,
        restarts: if down_for.is_some() { 5 } else { 0 },
        beats: 0,
        last_error: None,
        last_error_at: None,
        critical,
        down_since: down_for.map(|d| at - d),
    }
}

#[test]
fn healthz_is_down_when_a_critical_job_has_failed_for_five_minutes() {
    use axum::http::StatusCode;
    use op_server::ops::health_of;
    let at = op_core::time::now();
    let min = chrono::Duration::minutes;

    // A critical job failing for a minute: degraded, still 200.
    let (code, h) = health_of(true, false, &[job("ingest.moves", true, Some(min(1)), at)], at);
    assert_eq!((code, h.status), (StatusCode::OK, "degraded"));
    // For six: down, 503, and named.
    let (code, h) = health_of(true, false, &[job("ingest.moves", true, Some(min(6)), at), job("test.fine", false, None, at)], at);
    assert_eq!((code, h.status, h.failing), (StatusCode::SERVICE_UNAVAILABLE, "down", vec!["ingest.moves"]));
    // A non-critical job failing for an hour only degrades.
    let (code, h) = health_of(true, false, &[job("analytics.rollup", false, Some(min(60)), at)], at);
    assert_eq!((code, h.status), (StatusCode::OK, "degraded"));
    let (code, h) = health_of(true, false, &[job("ingest.moves", true, None, at)], at);
    assert_eq!((code, h.status), (StatusCode::OK, "ok"));
    let (code, _) = health_of(false, false, &[], at);
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn while_the_database_opens_healthz_says_migrating_and_the_rest_waits() {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let progress = op_core::store::Progress::default();
    {
        let mut p = progress.lock().unwrap();
        p.phase = "migrating";
        p.pending = vec![890, 907];
    }
    let gate = Arc::new(op_server::ops::Gate::new(progress.clone()));
    let app = axum::Router::new().fallback(op_server::ops::gated).with_state(gate.clone());
    let get = |path: &str| Request::get(path).body(Body::empty()).unwrap();
    let json = |res: axum::response::Response| async move { serde_json::from_slice::<Value>(&res.into_body().collect().await.unwrap().to_bytes()).unwrap() };

    let res = app.clone().oneshot(get("/healthz")).await.unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json(res).await;
    assert_eq!(body["status"], "migrating");
    assert_eq!(body["progress"]["pending"], json!([890, 907]));

    let res = app.clone().oneshot(get("/api/state")).await.unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(res.headers()[header::RETRY_AFTER], "5");
    assert_eq!(json(res).await["status"], "migrating");

    progress.lock().unwrap().phase = "opening";
    assert_eq!(json(app.clone().oneshot(get("/healthz")).await.unwrap()).await["status"], "starting");

    // Started: everything goes to the app.
    gate.open(axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" })));
    let res = app.oneshot(get("/healthz")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn server_json_names_the_url_and_healthz_answers_once_serving() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(opts(dir.path())).await.unwrap();
    let body: Value = reqwest::get(format!("{}/healthz", handle.url())).await.unwrap().json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["jobs"].as_array().unwrap().iter().any(|j| j["name"] == "ingest.moves" && j["critical"] == true), "{body}");
    handle.shutdown().await.unwrap();
}
