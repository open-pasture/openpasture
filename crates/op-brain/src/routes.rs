use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use op_core::{ApiError, ApiResult, BrainId, BrainInfo, Ctx, DbEnum};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{Action, DecisionRequest, detect, hosted};

const TEST_TIMEOUT: Duration = Duration::from_secs(360);

const TEST_INSTRUCTIONS: &str = "This is a connection test from the openpasture app. Decide from the farm record below \
only; do not call tools. Keep the reasoning to one sentence.";

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/brains", get(list))
        .route("/api/brains/{id}/test", post(test))
        .route("/api/brains/hosted/keys", get(hosted::get_keys).post(hosted::post_key))
        .route("/api/brains/hosted/keys/{id}", delete(hosted::remove_key))
        .route("/v1/decide", post(hosted::decide))
}

/// `GET /api/brains` (`?refresh=1` re-detects now).
async fn list(State(ctx): State<Ctx>, Query(q): Query<HashMap<String, String>>) -> Json<Vec<BrainInfo>> {
    let force = q.get("refresh").is_some_and(|v| v != "0" && v != "false");
    Json(detect::list(&ctx, force).await)
}

#[derive(Serialize)]
struct TestResult {
    ok: bool,
    detail: String,
    ms: u64,
}

/// `POST /api/brains/{id}/test`: one small real decision on this farm's record.
async fn test(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<TestResult>> {
    let id = BrainId::from_db(&id).map_err(|_| ApiError::not_found("No such brain."))?;
    let settings = ctx.settings().await?;
    let model = (settings.brain.id == id).then(|| settings.brain.model.clone()).flatten();
    let start = Instant::now();
    let ms = |s: Instant| s.elapsed().as_millis() as u64;

    let brain = match crate::brain_for(&ctx, id, model) {
        Ok(b) => b,
        Err(e) => return Ok(Json(TestResult { ok: false, detail: short(&format!("{e:#}")), ms: 0 })),
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            tracing::debug!(target: "op_brain::test", "{line}");
        }
    });
    let context = farm_context(&ctx).await?;
    let req = DecisionRequest {
        herd_id: context.pointer("/herd/id").and_then(Value::as_str).unwrap_or_default().to_owned(),
        context,
        instructions: TEST_INSTRUCTIONS.into(),
        mcp_url: String::new(),
        log: tx,
    };
    let result = tokio::time::timeout(TEST_TIMEOUT, brain.decide(req)).await;
    detect::invalidate();
    Ok(Json(match result {
        Ok(Ok(out)) => {
            let action = match out.action {
                Action::Stay => "STAY",
                Action::Move => "MOVE",
                Action::NeedsInfo => "NEEDS_INFO",
            };
            let detail = match out.model {
                Some(m) => format!("{action} ({m})"),
                None => action.to_owned(),
            };
            TestResult { ok: true, detail, ms: ms(start) }
        }
        Ok(Err(e)) => TestResult { ok: false, detail: short(&format!("{e:#}")), ms: ms(start) },
        Err(_) => TestResult { ok: false, detail: "Timed out".into(), ms: ms(start) },
    }))
}

/// A small context from the real farm record: farm, first herd, paddocks.
async fn farm_context(ctx: &Ctx) -> anyhow::Result<Value> {
    let store = ctx.store();
    let farm = store.get_farm().await?;
    let herds = store.list_herds().await?;
    let paddocks = store.list_paddocks().await?;
    let herd = herds.first();
    let current = herd.and_then(|h| h.paddock_id.clone());
    let candidates: Vec<&str> = paddocks.iter().map(|p| p.id.as_str()).filter(|id| Some(*id) != current.as_deref()).collect();
    Ok(json!({
        "as_of": op_core::time::to_db(&op_core::time::now()),
        "farm": farm,
        "herd": herd,
        "current_paddock_id": current,
        "position_source": if current.is_some() { "farm_record" } else { "unknown" },
        "paddocks": paddocks,
        "candidate_paddock_ids": candidates,
    }))
}

fn short(s: &str) -> String {
    let first = s.lines().next().unwrap_or_default().trim();
    let mut out: String = first.chars().take(160).collect();
    if first.chars().count() > 160 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    async fn server() -> (Ctx, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let app = op_core::router().merge(router()).with_state(ctx.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (ctx, url, dir)
    }

    #[tokio::test]
    async fn hosted_brain_between_two_servers() {
        let http = reqwest::Client::new();
        // A serves decisions with its heuristic brain.
        let (_a, a_url, _ad) = server().await;
        let created: Value =
            http.post(format!("{a_url}/api/brains/hosted/keys")).json(&json!({ "label": "farm B" })).send().await.unwrap().json().await.unwrap();
        let key = created["key"].as_str().unwrap().to_owned();
        assert!(key.starts_with(hosted::KEY_PREFIX));
        let listed: Value = http.get(format!("{a_url}/api/brains/hosted/keys")).send().await.unwrap().json().await.unwrap();
        assert_eq!(listed[0]["label"], "farm B");
        assert!(listed[0].get("key").is_none() && listed[0]["last_used"].is_null());

        // B uses A as its hosted brain.
        let (b, b_url, _bd) = server().await;
        b.secrets().set("hosted_url", &a_url).unwrap();
        b.secrets().set("hosted_api_key", &key).unwrap();
        b.update_settings(&json!({ "brain": { "id": "hosted" } })).await.unwrap();
        let brain = crate::resolve(&b).await.unwrap();
        assert_eq!(brain.id(), BrainId::Hosted);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let req =
            DecisionRequest { herd_id: "herd_test".into(), context: fixture::context(), instructions: String::new(), mcp_url: String::new(), log: tx.clone() };
        let out = brain.decide(req).await.unwrap();
        assert_eq!(out.action, Action::Move);
        assert_eq!(out.to_paddock_id.as_deref(), Some("pad_creek"));
        let listed: Value = http.get(format!("{a_url}/api/brains/hosted/keys")).send().await.unwrap().json().await.unwrap();
        assert!(listed[0]["last_used"].is_string());

        // B can't serve: its own brain is hosted (no loops).
        let (b_key, b_secret) = hosted::create_key(&b, "").await.unwrap();
        let res = http.post(format!("{b_url}/v1/decide")).bearer_auth(&b_secret).json(&json!({ "context": {} })).send().await.unwrap();
        assert_eq!(res.status(), 409);
        let _ = b_key;

        // A's Codex brain is never offered to outside callers.
        _a.update_settings(&json!({ "brain": { "id": "codex" } })).await.unwrap();
        let res = http.post(format!("{a_url}/v1/decide")).bearer_auth(&key).json(&json!({ "context": {} })).send().await.unwrap();
        assert_eq!(res.status(), 409);
        let body: Value = res.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("Codex"), "{body}");
        _a.update_settings(&json!({ "brain": { "id": "heuristic" } })).await.unwrap();

        // Wrong and revoked keys.
        let res = http.post(format!("{a_url}/v1/decide")).bearer_auth("oph_nope").json(&json!({ "context": {} })).send().await.unwrap();
        assert_eq!(res.status(), 401);
        let id = created["id"].as_str().unwrap();
        assert_eq!(http.delete(format!("{a_url}/api/brains/hosted/keys/{id}")).send().await.unwrap().status(), 204);
        assert_eq!(http.delete(format!("{a_url}/api/brains/hosted/keys/{id}")).send().await.unwrap().status(), 404);
        let req = DecisionRequest { herd_id: "herd_test".into(), context: fixture::context(), instructions: String::new(), mcp_url: String::new(), log: tx };
        let err = brain.decide(req).await.unwrap_err();
        assert!(err.to_string().contains("key not accepted"), "{err:#}");

        // Nothing listening: the error says so.
        b.secrets().set("hosted_url", "http://127.0.0.1:9").unwrap();
        let brain = crate::resolve(&b).await.unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let req = DecisionRequest { herd_id: "h".into(), context: fixture::context(), instructions: String::new(), mcp_url: String::new(), log: tx };
        assert!(brain.decide(req).await.unwrap_err().to_string().contains("can't connect"));
    }

    #[tokio::test]
    async fn list_and_test_endpoints() {
        let http = reqwest::Client::new();
        let (ctx, url, _d) = server().await;
        let brains: Vec<BrainInfo> = http.get(format!("{url}/api/brains")).send().await.unwrap().json().await.unwrap();
        assert_eq!(brains.len(), 7);
        let h = brains.iter().find(|b| b.id == BrainId::Heuristic).unwrap();
        assert!(h.available && h.signed_in);
        let a = brains.iter().find(|b| b.id == BrainId::Anthropic).unwrap();
        assert!(!a.available);
        assert_eq!(a.needs, vec!["anthropic_api_key"]);
        assert_eq!(a.detail.as_deref(), Some("Add API key"));

        // Cached: fast.
        let t = Instant::now();
        let res = http.get(format!("{url}/api/brains")).send().await.unwrap();
        assert_eq!(res.status(), 200);
        assert!(t.elapsed() < Duration::from_millis(300), "{:?}", t.elapsed());

        // A key shows up at once.
        ctx.secrets().set("anthropic_api_key", "sk-ant-test").unwrap();
        let brains: Vec<BrainInfo> = http.get(format!("{url}/api/brains")).send().await.unwrap().json().await.unwrap();
        assert!(brains.iter().find(|b| b.id == BrainId::Anthropic).unwrap().available);

        let r: Value = http.post(format!("{url}/api/brains/heuristic/test")).send().await.unwrap().json().await.unwrap();
        assert_eq!(r["ok"], true);
        assert_eq!(r["detail"], "NEEDS_INFO");
        let r: Value = http.post(format!("{url}/api/brains/hosted/test")).send().await.unwrap().json().await.unwrap();
        assert_eq!(r["ok"], false);
        assert_eq!(r["detail"], "Add an openpasture API key");
        assert_eq!(http.post(format!("{url}/api/brains/nope/test")).send().await.unwrap().status(), 404);
    }
}

/// One real run of each signed-in CLI on the fixture. Run by hand:
/// `cargo test -p op-brain real_cli -- --ignored --nocapture`.
#[cfg(test)]
mod real {
    use super::*;
    use crate::{Brain, fixture};

    async fn run(brain: Box<dyn Brain>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let printer = tokio::spawn(async move {
            while let Some(l) = rx.recv().await {
                println!("  log: {l}");
            }
        });
        let req = DecisionRequest {
            herd_id: "herd_test".into(),
            context: fixture::context(),
            instructions: "Quick test farm. Decide from the context alone.".into(),
            // Nothing listens here: the run must still work without MCP.
            mcp_url: "http://127.0.0.1:9/mcp".into(),
            log: tx,
        };
        let t = Instant::now();
        let out = brain.decide(req).await;
        let _ = printer.await;
        println!("{:?} in {:?}: {out:#?}", brain.id(), t.elapsed());
        out.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn real_cli_codex() {
        let bin = detect::Locator::from_env().find("codex").expect("codex installed");
        run(Box::new(crate::codex::CodexBrain::new(bin, None))).await;
    }

    #[tokio::test]
    #[ignore]
    async fn real_cli_claude() {
        let bin = detect::Locator::from_env().find("claude").expect("claude installed");
        run(Box::new(crate::claude::ClaudeBrain::new(bin, Some("haiku".into())))).await;
    }
}
