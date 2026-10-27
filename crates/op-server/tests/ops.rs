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
