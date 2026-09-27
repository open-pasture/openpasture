//! The engine's tools moved into the registry unchanged: `tools/list` and
//! the brain prompt are byte-identical to what they were before (fixtures
//! captured from the previous code).

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Via};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn list(app: &Router, path: &str) -> Value {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "127.0.0.1")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

#[tokio::test]
async fn mcp_tool_list_is_unchanged_and_brain_scope_drops_writes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let app = op_core::with_identity(Router::new().merge(op_engine::router()).with_state(ctx.clone()), Identity::owner(Via::Local));
    let before: Value = serde_json::from_str(&fixture("mcp_tools_list.json")).unwrap();
    let full = list(&app, "/mcp").await;
    // The existing tools come first, unchanged; tools added under the anchors in
    // `tools.rs` (get_morning_brief, …) follow them.
    let tools = full["result"]["tools"].as_array().unwrap();
    assert_eq!(Value::from(tools[..12].to_vec()), before["full"], "names, descriptions, schemas, annotations and order");
    let brain = list(&app, "/mcp?scope=brain").await;
    assert_eq!(brain["result"]["tools"], before["brain"]);
    assert!(!brain["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "propose_boundary"));

    // The registry holds the same tools first: 11 brain read tools, one manager
    // write. Tools added after them are never offered to the decision brain.
    let specs = ctx.tools().list();
    assert_eq!(specs.len(), tools.len());
    let names: Vec<&str> = specs.iter().map(|s| s.name).collect();
    assert_eq!(names[..12], [&op_engine::mcp::READ_TOOLS[..], &op_engine::mcp::WRITE_TOOLS[..]].concat()[..]);
    assert_eq!(ctx.tools().brain_tools(), op_engine::mcp::READ_TOOLS);
    let propose = ctx.tools().get("propose_boundary").unwrap();
    assert!(!propose.read && !propose.brain && propose.min_role == op_core::Role::Manager);
    // Registering again changes nothing.
    op_engine::register_tools(&ctx);
    assert_eq!(ctx.tools().list().len(), specs.len());
}

#[test]
fn brain_prompt_is_unchanged_for_the_existing_tools() {
    let context = json!({ "herd": { "id": "herd_1", "name": "Cows" }, "paddocks": [{ "id": "pad_1", "name": "P1" }] });
    let tools: Vec<String> = op_engine::mcp::READ_TOOLS.iter().map(|t| t.to_string()).collect();
    let with = op_brain::prompt::build_prompt("Follow the skill.", &context, &op_brain::decision_schema(), &tools);
    assert_eq!(with, fixture("prompt_with_tools.txt"));
    let without = op_brain::prompt::build_prompt("Follow the skill.", &context, &op_brain::decision_schema(), &[]);
    assert_eq!(without, fixture("prompt_without_tools.txt"));
}

#[tokio::test]
async fn the_decision_brain_gets_the_registry_brain_tools() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    // Loopback: no token.
    let (url, token) = op_engine::cycle::brain_mcp_url(&ctx);
    assert!(url.ends_with("/mcp?scope=brain") && token.is_none());
    // Off loopback: a token carrying the brain tools.
    ctx.set_local_url("http://192.168.1.2:7878");
    let (url, token) = op_engine::cycle::brain_mcp_url(&ctx);
    let token = token.unwrap();
    assert!(url.contains(token.as_str()));
    assert_eq!(ctx.check_brain_token(token.as_str()).unwrap(), op_engine::mcp::READ_TOOLS);
}
