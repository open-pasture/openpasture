//! The collar address: refused unless it reaches this server, and shown on
//! `/api/server` with what the server can do and is doing.

use serde_json::{Value, json};

#[tokio::test]
async fn the_collar_address_is_checked_before_it_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    let handle = op_server::serve(op_server::ServeOptions { data_dir: Some(dir.path().into()), free_port: true, ..Default::default() }).await.unwrap();
    // Signed in as the owner: being on this machine grants nothing.
    let token = handle.ctx().settings().await.unwrap().server.app_token;
    let mut auth = reqwest::header::HeaderMap::new();
    auth.insert(reqwest::header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
    let http = reqwest::Client::builder().default_headers(auth).build().unwrap();
    let url = |p: &str| format!("{}{p}", handle.url());

    // This machine's own URL is never one collars get.
    let res = http.put(url("/api/server/collar-address")).json(&json!({"url": handle.url()})).send().await.unwrap();
    assert_eq!(res.status(), 422);
    let body: Value = res.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("https://"), "{body}");

    let check: Value = http.post(url("/api/server/collar-address/check")).json(&json!({"url": "https://127.0.0.1"})).send().await.unwrap().json().await.unwrap();
    assert_eq!((check["ok"].as_bool(), check["failed"].as_str()), (Some(false), Some("address")), "{check}");

    let res = http.put(url("/api/server/collar-address")).json(&json!({"url": "https://collar-address-check.invalid"})).send().await.unwrap();
    assert_eq!(res.status(), 422);
    assert_eq!(handle.ctx().settings().await.unwrap().server.collar_url, None, "nothing saved");

    // The nonce route answers only for a check in flight.
    assert_eq!(http.get(url("/collar/v1/reach/nope")).send().await.unwrap().status(), 404);

    // Cleared with null.
    handle.ctx().update_settings(&json!({"server": {"collar_url": "https://farm.example.com"}})).await.unwrap();
    let res = http.put(url("/api/server/collar-address")).json(&json!({"url": null})).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(handle.ctx().settings().await.unwrap().server.collar_url, None);

    let info: Value = http.get(url("/api/server")).send().await.unwrap().json().await.unwrap();
    assert!(info["capabilities"].as_array().unwrap().iter().any(|c| c == "sql"), "{info}");
    assert_eq!(info["work"], json!({"moves": 0, "escapes": 0, "live_collars": 0}));
    assert_eq!(info["bind"], "127.0.0.1");
    handle.shutdown().await.unwrap();
}
