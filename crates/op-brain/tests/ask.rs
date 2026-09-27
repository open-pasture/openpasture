//! `Brain::ask` against real HTTP servers started in the test: an
//! Anthropic-shaped and an OpenAI-compatible model API, a second openpasture
//! server for the hosted brain, and a stand-in `claude` executable. The tools
//! go through the real registry and `Ctx::tool_runner`; the tools themselves
//! are test fixtures (op-brain sits below the crates that own the real ones).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use op_brain::api::{AnthropicBrain, OpenAiBrain};
use op_brain::{AskError, AskRequest, Brain, hosted};
use op_core::tools::{ToolCall, ToolSpec};
use op_core::{Ctx, Identity, Role, Via};
use serde_json::{Value, json};

type Handler = Arc<dyn Fn(&Value, usize) -> (u16, Value) + Send + Sync>;

/// A JSON API on a free port: every POST to `path` is recorded and answered
/// by `h(body, n)` (n counts requests from 0). Requests without the right key
/// get 401.
async fn api(path: &'static str, key_header: &'static str, key: &'static str, h: Handler) -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = axum::Router::new().route(
        path,
        axum::routing::post(move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
            let (log, h) = (log.clone(), h.clone());
            async move {
                let got = headers.get(key_header).and_then(|v| v.to_str().ok()).unwrap_or_default();
                if got != key {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": { "message": "bad key" } })));
                }
                let n = {
                    let mut l = log.lock().unwrap();
                    l.push(body.clone());
                    l.len() - 1
                };
                let (code, v) = h(&body, n);
                (StatusCode::from_u16(code).unwrap(), axum::Json(v))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

fn schema() -> Value {
    json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false })
}

/// A farm with two collars and four fixture tools: two reads, `run_sql`
/// (a read tool questions must never get) and a manager write.
async fn farm() -> (tempfile::TempDir, Ctx, Arc<AtomicUsize>) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let f = op_core::Farm {
        id: "farm_1".into(),
        name: "Test farm".into(),
        timezone: "America/Chicago".into(),
        center: [-93.62, 42.03],
        created_at: op_core::time::now(),
    };
    ctx.store().insert_farm(&f).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let read = |name: &'static str, description: &'static str, run| ToolSpec {
        name,
        description,
        input_schema: schema(),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run,
    };
    ctx.tools().register(read(
        "get_farm",
        "The farm record.",
        ToolSpec::run_fn(move |c: ToolCall| {
            let n = n.clone();
            async move {
                n.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "farm": c.ctx.store().get_farm().await? }))
            }
        }),
    ));
    ctx.tools().register(read("count_collars", "How many collars the farm has.", ToolSpec::run_fn(|_c: ToolCall| async move { Ok(json!({ "collars": 2 })) })));
    ctx.tools().register(read("run_sql", "Read-only SQL.", ToolSpec::run_fn(|_c: ToolCall| async move { Ok(json!({ "rows": [] })) })));
    ctx.tools().register(ToolSpec {
        name: "propose_boundary",
        description: "Propose a move.",
        input_schema: schema(),
        read: false,
        brain: false,
        min_role: Role::Manager,
        run: ToolSpec::run_fn(|_c: ToolCall| async move { Ok(json!({})) }),
    });
    (dir, ctx, calls)
}

fn question(ctx: &Ctx, q: &str, max_chars: usize, exclude: &[&str]) -> AskRequest {
    AskRequest {
        question: q.into(),
        context: json!({ "farm": { "name": "Test farm" }, "herds": [{ "name": "Cows", "count": 250 }] }),
        tools: ctx.tool_runner(Identity::owner(Via::Text), exclude),
        max_chars,
        log: None,
    }
}

fn tool_names(defs: &Value, key: &str) -> Vec<String> {
    defs.as_array().unwrap().iter().map(|t| t.pointer(key).and_then(Value::as_str).unwrap().to_owned()).collect()
}

const LONG: &str = "Test farm has two collars and both reported in the last hour. Both are inside P1. Nothing needs you today.";

#[tokio::test]
async fn anthropic_calls_a_read_tool_then_answers_within_max_chars() {
    let (_d, ctx, calls) = farm().await;
    let h: Handler = Arc::new(|_body, n| match n {
        0 => (
            200,
            json!({ "model": "claude-test", "stop_reason": "tool_use", "content": [
                { "type": "text", "text": "Let me look." },
                { "type": "tool_use", "id": "tu_1", "name": "get_farm", "input": {} }
            ] }),
        ),
        _ => (200, json!({ "model": "claude-test", "stop_reason": "end_turn", "content": [{ "type": "text", "text": LONG }] })),
    });
    let (url, seen) = api("/v1/messages", "x-api-key", "sk-ant-test", h).await;
    let brain = AnthropicBrain::new("sk-ant-test".into(), Some("claude-test".into())).with_base(&url);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut req = question(&ctx, "How many collars do I have?", 90, &["run_sql"]);
    req.log = Some(tx);

    let answer = brain.ask(req).await.unwrap();
    assert_eq!(answer, "Test farm has two collars and both reported in the last hour. Both are inside P1.");
    assert!(answer.chars().count() <= 90);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the tool ran once");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let first = &seen[0];
    assert_eq!(tool_names(&first["tools"], "/name"), ["get_farm", "count_collars"], "read tools only, never run_sql or writes");
    assert_eq!(first["tool_choice"]["type"], "auto");
    assert_eq!(first["model"], "claude-test");
    let prompt = first["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("How many collars do I have?") && prompt.contains("\"Cows\"") && prompt.contains("at most 90 characters"));
    assert!(first["system"].as_str().unwrap().contains("Never make up"));
    // The tool result went back with the tool_use id.
    let second = &seen[1];
    assert_eq!(second["messages"][1]["role"], "assistant");
    let result = &second["messages"][2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "tu_1");
    assert_eq!(result["is_error"], false);
    assert!(result["content"].as_str().unwrap().contains("Test farm"));
    drop(seen);

    let mut progress = vec![];
    while let Ok(l) = rx.try_recv() {
        progress.push(l);
    }
    assert_eq!(progress, ["Asking claude-test", "Tool get_farm", "Answer received"]);
}

#[tokio::test]
async fn run_sql_is_never_offered_even_when_the_runner_has_it() {
    let (_d, ctx, _) = farm().await;
    let h: Handler = Arc::new(|_body, _n| (200, json!({ "stop_reason": "end_turn", "content": [{ "type": "text", "text": "Two." }] })));
    let (url, seen) = api("/v1/messages", "x-api-key", "k", h).await;
    let brain = AnthropicBrain::new("k".into(), None).with_base(&url);
    // A runner that excludes nothing still never offers run_sql.
    let req = question(&ctx, "How many?", 320, &[]);
    assert!(req.tools.tools().iter().any(|t| t.name == "run_sql"), "the runner itself has it");
    assert_eq!(brain.ask(req).await.unwrap(), "Two.");
    let seen = seen.lock().unwrap();
    assert_eq!(tool_names(&seen[0]["tools"], "/name"), ["get_farm", "count_collars"]);
    assert!(!seen[0]["messages"][0]["content"].as_str().unwrap().contains("run_sql"));
    // Calling it anyway (a model making up a name) is refused as a tool error.
    drop(seen);
    let h: Handler = Arc::new(|_b, n| match n {
        0 => (200, json!({ "stop_reason": "tool_use", "content": [{ "type": "tool_use", "id": "t", "name": "run_sql", "input": { "query": "SELECT 1" } }] })),
        _ => (200, json!({ "stop_reason": "end_turn", "content": [{ "type": "text", "text": "I can't run SQL." }] })),
    });
    let (url, seen) = api("/v1/messages", "x-api-key", "k", h).await;
    let brain = AnthropicBrain::new("k".into(), None).with_base(&url);
    assert_eq!(brain.ask(question(&ctx, "Run SQL", 320, &[])).await.unwrap(), "I can't run SQL.");
    let r = &seen.lock().unwrap()[1]["messages"][2]["content"][0];
    assert_eq!(r["is_error"], true);
    assert_eq!(r["content"], "Unknown tool run_sql.");
}

#[tokio::test]
async fn tool_calls_stop_at_six_then_the_model_must_answer() {
    let (_d, ctx, calls) = farm().await;
    // A model that asks for four tools a round until tools are switched off.
    let h: Handler = Arc::new(|body, n| {
        if body["tool_choice"]["type"] == "none" {
            return (200, json!({ "stop_reason": "end_turn", "content": [{ "type": "text", "text": "Test farm, two collars." }] }));
        }
        let uses: Vec<Value> = (0..4).map(|i| json!({ "type": "tool_use", "id": format!("t{n}_{i}"), "name": "get_farm", "input": {} })).collect();
        (200, json!({ "stop_reason": "tool_use", "content": uses }))
    });
    let (url, seen) = api("/v1/messages", "x-api-key", "k", h).await;
    let brain = AnthropicBrain::new("k".into(), None).with_base(&url);
    assert_eq!(brain.ask(question(&ctx, "Everything?", 320, &["run_sql"])).await.unwrap(), "Test farm, two collars.");
    assert_eq!(calls.load(Ordering::SeqCst), 6, "six tool calls ran");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[1]["tool_choice"]["type"], "auto");
    assert_eq!(seen[2]["tool_choice"]["type"], "none", "no tools after six calls");
    // Round two ran two calls and refused the other two, each still answered.
    let results = seen[2]["messages"][4]["content"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    assert_eq!(results.iter().filter(|r| r["is_error"] == true).count(), 2);
    assert!(results[3]["content"].as_str().unwrap().starts_with("Not run: the limit of 6 tool calls"));
}

#[tokio::test]
async fn openai_compatible_calls_a_read_tool_then_answers() {
    let (_d, ctx, _) = farm().await;
    let h: Handler = Arc::new(|_body, n| match n {
        0 => (
            200,
            json!({ "model": "local-7b", "choices": [{ "finish_reason": "tool_calls", "message": { "role": "assistant", "content": null, "tool_calls": [
                { "id": "call_1", "type": "function", "function": { "name": "count_collars", "arguments": "{}" } }
            ] } }] }),
        ),
        _ => (
            200,
            json!({ "model": "local-7b", "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": "You have **2** collars." } }] }),
        ),
    });
    let (url, seen) = api("/v1/chat/completions", "authorization", "Bearer sk-local", h).await;
    let brain = OpenAiBrain::compatible(format!("{url}/v1"), Some("sk-local".into()), Some("local-7b".into()));
    assert_eq!(brain.ask(question(&ctx, "How many collars?", 320, &["run_sql"])).await.unwrap(), "You have 2 collars.");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(tool_names(&seen[0]["tools"], "/function/name"), ["get_farm", "count_collars"]);
    assert_eq!(seen[0]["tool_choice"], "auto");
    assert_eq!(seen[0]["messages"][0]["role"], "system");
    let m = seen[1]["messages"].as_array().unwrap();
    assert_eq!(m[2]["role"], "assistant");
    assert_eq!(m[2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(m[3]["role"], "tool");
    assert_eq!(m[3]["tool_call_id"], "call_1");
    assert_eq!(m[3]["content"], r#"{"collars":2}"#);
}

#[tokio::test]
async fn a_compatible_server_without_tools_answers_from_the_record() {
    let (_d, ctx, _) = farm().await;
    let h: Handler = Arc::new(|body, _n| {
        if body.get("tools").is_some() {
            return (400, json!({ "error": { "message": "tools are not supported" } }));
        }
        (200, json!({ "choices": [{ "message": { "content": "Cows: 250 head." } }] }))
    });
    let (url, seen) = api("/chat/completions", "authorization", "", h).await;
    let brain = OpenAiBrain::compatible(url, None, Some("m".into()));
    assert_eq!(brain.ask(question(&ctx, "How many cows?", 320, &["run_sql"])).await.unwrap(), "Cows: 250 head.");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen[1].get("tools").is_none());
    assert!(!seen[1]["messages"][1]["content"].as_str().unwrap().contains("## Tools"));

    // Nothing listening: the error says so.
    drop(seen);
    let brain = OpenAiBrain::compatible("http://127.0.0.1:9".into(), None, Some("m".into()));
    let err = brain.ask(question(&ctx, "?", 320, &["run_sql"])).await.unwrap_err();
    assert!(matches!(&err, AskError::Failed(_)) && err.to_string().contains("can't connect"), "{err}");
}

#[tokio::test]
async fn codex_and_the_heuristic_do_not_answer_questions() {
    let (_d, ctx, _) = farm().await;
    let heuristic = op_brain::heuristic::HeuristicBrain;
    assert!(matches!(heuristic.ask(question(&ctx, "Where are the cows?", 320, &["run_sql"])).await, Err(AskError::Unsupported)));
    // Codex is never run for a question (the binary doesn't even exist).
    let codex = op_brain::codex::CodexBrain::new("/nonexistent/codex".into(), None);
    assert!(matches!(codex.ask(question(&ctx, "Where are the cows?", 320, &["run_sql"])).await, Err(AskError::Unsupported)));
}

/// An openpasture server on a free port (op-core and op-brain routes).
async fn server() -> (tempfile::TempDir, Ctx, String) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let app = op_core::router().merge(op_brain::router()).with_state(ctx.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (dir, ctx, url)
}

#[tokio::test]
async fn hosted_ask_round_trip_through_another_server() {
    // The model API the host's brain uses.
    let h: Handler = Arc::new(|_b, _n| (200, json!({ "choices": [{ "message": { "content": LONG } }] })));
    let (model_url, seen) = api("/chat/completions", "authorization", "Bearer sk-host", h).await;
    // Host A answers with its compatible brain.
    let (_ad, a, a_url) = server().await;
    a.secrets().set("compatible_base_url", &model_url).unwrap();
    a.secrets().set("compatible_api_key", "sk-host").unwrap();
    a.update_settings(&json!({ "brain": { "id": "compatible", "model": "host-model" } })).await.unwrap();
    let (_k, key) = hosted::create_key(&a, "farm B").await.unwrap();

    // Farm B uses A as its hosted brain.
    let (_bd, b, _) = farm().await;
    b.secrets().set("hosted_url", &a_url).unwrap();
    b.secrets().set("hosted_api_key", &key).unwrap();
    b.update_settings(&json!({ "brain": { "id": "hosted" } })).await.unwrap();
    let brain = op_brain::resolve(&b).await.unwrap();
    let answer = brain.ask(question(&b, "How are the collars?", 70, &["run_sql"])).await.unwrap();
    assert_eq!(answer, "Test farm has two collars and both reported in the last hour.");

    // The host's brain got the question and B's context, and no tools.
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].get("tools").is_none());
        assert_eq!(seen[0]["model"], "host-model");
        let prompt = seen[0]["messages"][1]["content"].as_str().unwrap();
        assert!(prompt.contains("How are the collars?") && prompt.contains("\"Cows\"") && prompt.contains("at most 70 characters"));
        assert!(!prompt.contains("## Tools"));
    }

    // The host's brain doesn't answer questions: B hears Unsupported.
    a.update_settings(&json!({ "brain": { "id": "heuristic" } })).await.unwrap();
    assert!(matches!(brain.ask(question(&b, "?", 320, &["run_sql"])).await, Err(AskError::Unsupported)));
    a.update_settings(&json!({ "brain": { "id": "codex" } })).await.unwrap();
    assert!(matches!(brain.ask(question(&b, "?", 320, &["run_sql"])).await, Err(AskError::Unsupported)));
    // A host whose own brain is hosted refuses (no loops).
    a.update_settings(&json!({ "brain": { "id": "hosted" } })).await.unwrap();
    let err = brain.ask(question(&b, "?", 320, &["run_sql"])).await.unwrap_err();
    assert!(err.to_string().contains("own brain is hosted"), "{err}");

    // Keys.
    let http = reqwest::Client::new();
    let res = http.post(format!("{a_url}/v1/ask")).json(&json!({ "question": "?" })).send().await.unwrap();
    assert_eq!(res.status(), 401);
    let res = http.post(format!("{a_url}/v1/ask")).bearer_auth(&key).json(&json!({ "question": "  " })).send().await.unwrap();
    assert_eq!(res.status(), 400);
    b.secrets().set("hosted_api_key", "oph_wrong").unwrap();
    let brain = op_brain::resolve(&b).await.unwrap();
    let err = brain.ask(question(&b, "?", 320, &["run_sql"])).await.unwrap_err();
    assert!(err.to_string().contains("key not accepted"), "{err}");
}

#[tokio::test]
async fn the_host_answers_over_http_with_its_own_brain() {
    let h: Handler = Arc::new(|_b, _n| (200, json!({ "choices": [{ "message": { "content": "Plenty of grass in P4." } }] })));
    let (model_url, _seen) = api("/chat/completions", "authorization", "", h).await;
    let (_ad, a, a_url) = server().await;
    a.secrets().set("compatible_base_url", &model_url).unwrap();
    a.update_settings(&json!({ "brain": { "id": "compatible", "model": "m" } })).await.unwrap();
    let (_k, key) = hosted::create_key(&a, "").await.unwrap();
    let http = reqwest::Client::new();
    let res = http
        .post(format!("{a_url}/v1/ask"))
        .bearer_auth(&key)
        .json(&json!({ "question": "Grass in P4?", "context": { "paddocks": ["P4"] }, "max_chars": 320 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let v: Value = res.json().await.unwrap();
    assert_eq!(v, json!({ "answer": "Plenty of grass in P4." }));
    // Heuristic host: 501 with the sentence.
    a.update_settings(&json!({ "brain": { "id": "heuristic" } })).await.unwrap();
    let res = http.post(format!("{a_url}/v1/ask")).bearer_auth(&key).json(&json!({ "question": "Grass?" })).send().await.unwrap();
    assert_eq!(res.status(), 501);
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["error"], "This server's brain doesn't answer questions.");
}

#[cfg(unix)]
mod claude_cli {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use op_brain::claude::ClaudeBrain;

    use super::*;

    /// A `claude` that records its arguments, prompt and MCP config next to
    /// itself, holds the run for a moment when it has an MCP config (so the
    /// test can look at the live token), then prints a stream-json result.
    fn fake_claude(dir: &std::path::Path, result: &str) -> std::path::PathBuf {
        let p = dir.join("claude");
        let body = format!(
            r#"#!/bin/sh
here=$(dirname "$0")
printf '%s\n' "$@" > "$here/args.txt"
cat > "$here/prompt.txt"
cfg=""; prev=""
for a in "$@"; do
  if [ "$prev" = "--mcp-config" ]; then cfg="$a"; fi
  prev="$a"
done
if [ -n "$cfg" ]; then cp "$cfg" "$here/mcp.tmp"; mv "$here/mcp.tmp" "$here/mcp.json"; sleep 1; fi
cat <<'EOF'
{{"type":"system","subtype":"init","model":"claude-haiku-test","mcp_servers":[{{"name":"openpasture","status":"connected"}}]}}
{result}
EOF
"#
        );
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    const OK: &str =
        r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":900,"result":"Two collars, both inside P1. Nothing needs you today."}"#;

    #[tokio::test]
    async fn claude_answers_through_brain_mcp_with_a_token_that_never_allows_run_sql() {
        let (_d, ctx, _) = farm().await;
        let bin = tempfile::tempdir().unwrap();
        let claude = fake_claude(bin.path(), OK);
        let brain = ClaudeBrain::new(claude, Some("haiku".into())).with_ctx(ctx.clone());
        // Even a runner that has run_sql.
        let req = question(&ctx, "How many collars?", 320, &[]);
        let run = tokio::spawn(async move { brain.ask(req).await });

        // While the run is live, its token allows the question's tools only.
        let cfg_path = bin.path().join("mcp.json");
        let mut waited = 0;
        while !cfg_path.exists() {
            assert!(waited < 250, "the CLI never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += 1;
        }
        let cfg: Value = serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        let server = &cfg["mcpServers"]["openpasture"];
        assert_eq!(server["url"], format!("{}/mcp?scope=brain", ctx.local_url()), "no token in the URL");
        let token = server["headers"]["Authorization"].as_str().unwrap().strip_prefix("Bearer ").unwrap().to_owned();
        assert!(token.starts_with("opb_"));
        assert_eq!(ctx.check_brain_token(&token).unwrap(), ["get_farm", "count_collars"]);

        let answer = run.await.unwrap().unwrap();
        assert_eq!(answer, "Two collars, both inside P1. Nothing needs you today.");
        assert!(ctx.check_brain_token(&token).is_none(), "the token ends with the run");

        let args = std::fs::read_to_string(bin.path().join("args.txt")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        let i = args.iter().position(|a| *a == "--allowedTools").unwrap();
        assert_eq!(args[i + 1], "mcp__openpasture__get_farm,mcp__openpasture__count_collars");
        assert!(!args.iter().any(|a| a.contains("run_sql")));
        assert!(args.contains(&"--restricted") && args.contains(&"--strict-mcp-config"));
        assert!(!args.contains(&"--json-schema"), "a question has no output schema");
        assert!(!args.contains(&"--safe-mode"));
        assert!(args.windows(2).any(|w| w[0] == "--model" && w[1] == "haiku"));
        assert!(!args.iter().any(|a| a.contains(&token)), "the token never reaches argv");
        let prompt = std::fs::read_to_string(bin.path().join("prompt.txt")).unwrap();
        assert!(prompt.contains("How many collars?") && prompt.contains("get_farm, count_collars") && !prompt.contains("run_sql"));
    }

    #[tokio::test]
    async fn claude_without_this_server_answers_in_safe_mode_from_the_record() {
        let (_d, ctx, _) = farm().await;
        let bin = tempfile::tempdir().unwrap();
        let claude = fake_claude(bin.path(), OK);
        let answer = ClaudeBrain::new(claude, None).ask(question(&ctx, "How many?", 30, &["run_sql"])).await.unwrap();
        assert_eq!(answer, "Two collars, both inside P1.");
        let args = std::fs::read_to_string(bin.path().join("args.txt")).unwrap();
        assert!(args.lines().any(|a| a == "--safe-mode"));
        assert!(!args.lines().any(|a| a == "--mcp-config" || a == "--allowedTools"));
        assert!(!bin.path().join("mcp.json").exists());

        // An error result is the error.
        let claude = fake_claude(bin.path(), r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}"#);
        let err = ClaudeBrain::new(claude, None).ask(question(&ctx, "How many?", 320, &["run_sql"])).await.unwrap_err();
        assert!(err.to_string().contains("Not logged in"), "{err}");
    }
}

/// One real question to the signed-in Claude Code CLI (no MCP). Run by hand:
/// `cargo test -p op-brain --test ask real_claude -- --ignored --nocapture`.
#[cfg(unix)]
#[tokio::test]
#[ignore]
async fn real_claude_answers_a_question() {
    let bin = op_brain::detect::Locator::from_env().find("claude").expect("claude installed");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(l) = rx.recv().await {
            println!("  log: {l}");
        }
    });
    let req = AskRequest {
        question: "How many cows are there and which paddock are they in?".into(),
        context: json!({ "farm": { "name": "Test farm" }, "herds": [{ "name": "Cows", "count": 250, "paddock": "P1" }], "units": "imperial" }),
        tools: Arc::new(op_brain::ask::NoTools),
        max_chars: 160,
        log: Some(tx),
    };
    let t = std::time::Instant::now();
    let answer = op_brain::claude::ClaudeBrain::new(bin, Some("haiku".into())).ask(req).await;
    let _ = printer.await;
    println!("{:?}: {answer:?}", t.elapsed());
    let answer = answer.unwrap();
    assert!(!answer.is_empty() && answer.chars().count() <= 160);
}
