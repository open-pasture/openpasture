//! HTTP brains: Anthropic Messages API, OpenAI and OpenAI-compatible Chat
//! Completions, and the openpasture hosted brain (`POST /v1/decide`).

use std::time::Duration;

use anyhow::{Context, bail};
use op_core::BrainId;
use serde_json::{Value, json};

use crate::parse::{self, snippet};
use crate::prompt::{SYSTEM, build_prompt};
use crate::{Brain, DecisionOutput, DecisionRequest, decision_schema};

pub const ANTHROPIC_MODELS: [&str; 3] = ["claude-sonnet-5", "claude-opus-5-5", "claude-haiku-4-5-20251001"];
pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-sonnet-5";
pub const OPENAI_BASE: &str = "https://api.openai.com/v1";
pub const OPENAI_DEFAULT_MODEL: &str = "gpt-5.5";
pub const HOSTED_DEFAULT_URL: &str = "https://api.openpasture.dev";

const DECIDE_TIMEOUT: Duration = Duration::from_secs(180);
const HOSTED_TIMEOUT: Duration = Duration::from_secs(420);
const LIST_TIMEOUT: Duration = Duration::from_secs(4);

fn client() -> reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("openpasture/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client")
    })
    .clone()
}

/// The error message from an API error body, or the status.
fn api_error(status: reqwest::StatusCode, body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message").or_else(|| v.get("error")).or_else(|| v.get("message")).and_then(Value::as_str).map(str::to_owned));
    match msg {
        Some(m) => format!("HTTP {}: {}", status.as_u16(), snippet(&m)),
        None if body.trim().is_empty() => format!("HTTP {}", status.as_u16()),
        None => format!("HTTP {}: {}", status.as_u16(), snippet(body)),
    }
}

async fn send_json(rb: reqwest::RequestBuilder) -> anyhow::Result<Value> {
    let resp = rb.send().await.map_err(|e| anyhow::anyhow!("{}", describe(&e)))?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!(api_error(status, &body));
    }
    serde_json::from_str(&body).with_context(|| format!("not JSON: {}", snippet(&body)))
}

fn describe(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        "can't connect".into()
    } else {
        e.to_string()
    }
}

// ---- Anthropic ----

pub struct AnthropicBrain {
    key: String,
    model: String,
    base: String,
}

impl AnthropicBrain {
    pub fn new(key: String, model: Option<String>) -> Self {
        Self { key, model: model.unwrap_or_else(|| ANTHROPIC_DEFAULT_MODEL.into()), base: "https://api.anthropic.com".into() }
    }
}

#[async_trait::async_trait]
impl Brain for AnthropicBrain {
    fn id(&self) -> BrainId {
        BrainId::Anthropic
    }

    async fn decide(&self, req: DecisionRequest) -> anyhow::Result<DecisionOutput> {
        req.say(format!("Asking {}", self.model));
        let body = json!({
            "model": self.model,
            "max_tokens": 4096,
            "system": SYSTEM,
            "messages": [{ "role": "user", "content": build_prompt(&req.instructions, &req.context, &decision_schema(), &[]) }],
            "tools": [{
                "name": "submit_decision",
                "description": "Submit the grazing decision for this herd.",
                "input_schema": decision_schema(),
            }],
            "tool_choice": { "type": "tool", "name": "submit_decision" },
        });
        let v = send_json(
            client()
                .post(format!("{}/v1/messages", self.base))
                .header("x-api-key", &self.key)
                .header("anthropic-version", "2023-06-01")
                .timeout(DECIDE_TIMEOUT)
                .json(&body),
        )
        .await
        .context("Anthropic")?;
        let model = v.get("model").and_then(Value::as_str).unwrap_or(&self.model).to_owned();
        let content = v.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
        let input = content.iter().find(|c| c.get("type").and_then(Value::as_str) == Some("tool_use")).and_then(|c| c.get("input"));
        let out = match input {
            Some(input) => parse::finish(input, &req.context, Some(model))?,
            None => {
                let text: String = content.iter().filter_map(|c| c.get("text").and_then(Value::as_str)).collect();
                if v.get("stop_reason").and_then(Value::as_str) == Some("max_tokens") {
                    bail!("the reply was cut off");
                }
                let mut out = parse::parse_text(&text)?;
                parse::check_paddock(&mut out, &req.context)?;
                out.model = Some(model);
                out
            }
        };
        req.say("Decision received");
        Ok(out)
    }
}

// ---- OpenAI and compatible ----

pub struct OpenAiBrain {
    id: BrainId,
    base: String,
    key: Option<String>,
    model: Option<String>,
}

impl OpenAiBrain {
    pub fn openai(key: String, model: Option<String>) -> Self {
        Self { id: BrainId::Openai, base: OPENAI_BASE.into(), key: Some(key), model: Some(model.unwrap_or_else(|| OPENAI_DEFAULT_MODEL.into())) }
    }

    pub fn compatible(base: String, key: Option<String>, model: Option<String>) -> Self {
        Self { id: BrainId::Compatible, base: base.trim().trim_end_matches('/').to_owned(), key, model }
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        let rb = client().post(format!("{}{path}", self.base)).timeout(DECIDE_TIMEOUT);
        match &self.key {
            Some(k) => rb.bearer_auth(k),
            None => rb,
        }
    }
}

#[async_trait::async_trait]
impl Brain for OpenAiBrain {
    fn id(&self) -> BrainId {
        self.id
    }

    async fn decide(&self, req: DecisionRequest) -> anyhow::Result<DecisionOutput> {
        let model = match &self.model {
            Some(m) => m.clone(),
            None => compatible_models(&self.base, self.key.as_deref()).await.ok().and_then(|m| m.into_iter().next()).context("choose a model")?,
        };
        req.say(format!("Asking {model}"));
        let prompt = build_prompt(&req.instructions, &req.context, &decision_schema(), &[]);
        let messages = json!([{ "role": "system", "content": SYSTEM }, { "role": "user", "content": prompt }]);
        let formats = [
            Some(json!({ "type": "json_schema", "json_schema": { "name": "grazing_decision", "strict": true, "schema": decision_schema() } })),
            Some(json!({ "type": "json_object" })),
            None,
        ];
        let label = if self.id == BrainId::Openai { "OpenAI" } else { "Compatible" };
        let mut last_err = None;
        for (i, format) in formats.into_iter().enumerate() {
            let mut body = json!({ "model": model, "messages": messages });
            if let Some(f) = format {
                body["response_format"] = f;
            }
            match send_json(self.post("/chat/completions").json(&body)).await {
                Ok(v) => {
                    let msg = v.pointer("/choices/0/message").cloned().unwrap_or(Value::Null);
                    if let Some(refusal) = msg.get("refusal").and_then(Value::as_str) {
                        bail!("{label} refused: {}", snippet(refusal));
                    }
                    let text = msg.get("content").and_then(Value::as_str).unwrap_or_default();
                    let used = v.get("model").and_then(Value::as_str).unwrap_or(&model).to_owned();
                    let mut out = parse::parse_text(text).with_context(|| label.to_string())?;
                    parse::check_paddock(&mut out, &req.context)?;
                    out.model = Some(used);
                    req.say("Decision received");
                    return Ok(out);
                }
                // Older servers reject structured formats: fall back to plain JSON.
                Err(e) if self.id == BrainId::Compatible && i < 2 && e.to_string().starts_with("HTTP 4") && !is_auth(&e) => {
                    req.say("Server has no JSON schema support; retrying");
                    last_err = Some(e);
                }
                Err(e) => return Err(e.context(label)),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no reply")).context(label))
    }
}

fn is_auth(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.starts_with("HTTP 401") || s.starts_with("HTTP 403") || s.starts_with("HTTP 404")
}

async fn list_models(base: &str, key: Option<&str>) -> anyhow::Result<Vec<String>> {
    let mut rb = client().get(format!("{}/models", base.trim().trim_end_matches('/'))).timeout(LIST_TIMEOUT);
    if let Some(k) = key {
        rb = rb.bearer_auth(k);
    }
    let v = send_json(rb).await?;
    let ids = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|m| m.get("id").or_else(|| m.get("name")).and_then(Value::as_str).map(str::to_owned)).collect())
        .unwrap_or_default();
    Ok(ids)
}

/// Chat models from OpenAI's `/v1/models`, newest names first, the default on top.
pub async fn openai_models(key: &str) -> anyhow::Result<Vec<String>> {
    Ok(filter_openai_models(list_models(OPENAI_BASE, Some(key)).await?))
}

pub fn filter_openai_models(ids: Vec<String>) -> Vec<String> {
    const SKIP: [&str; 14] = [
        "audio",
        "realtime",
        "tts",
        "transcribe",
        "image",
        "search",
        "embedding",
        "instruct",
        "dall-e",
        "whisper",
        "moderation",
        "davinci",
        "babbage",
        "preview",
    ];
    let is_chat = |id: &str| id.starts_with("gpt-") || id.starts_with("chatgpt-") || (id.starts_with('o') && id[1..].starts_with(|c: char| c.is_ascii_digit()));
    // Dated snapshots (gpt-4o-2024-08-06) crowd the list; keep the aliases.
    let dated = |id: &str| {
        let parts: Vec<&str> = id.rsplitn(4, '-').collect();
        parts.len() == 4 && parts[..3].iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) && parts[2].len() == 4
    };
    let mut out: Vec<String> = ids.into_iter().filter(|id| is_chat(id) && !dated(id) && !SKIP.iter().any(|s| id.contains(s))).collect();
    out.sort_by(|a, b| b.cmp(a));
    out.dedup();
    if let Some(i) = out.iter().position(|m| m == OPENAI_DEFAULT_MODEL) {
        let d = out.remove(i);
        out.insert(0, d);
    }
    out
}

/// Every model an OpenAI-compatible server lists (`GET {base}/models`).
pub async fn compatible_models(base: &str, key: Option<&str>) -> anyhow::Result<Vec<String>> {
    list_models(base, key).await
}

// ---- Hosted ----

/// Another openpasture server (ours, or your own) deciding for this one.
pub struct HostedBrain {
    url: String,
    key: String,
}

impl HostedBrain {
    pub fn new(url: Option<String>, key: String) -> Self {
        let url = url.filter(|u| !u.trim().is_empty()).unwrap_or_else(|| HOSTED_DEFAULT_URL.into());
        Self { url: url.trim().trim_end_matches('/').to_owned(), key }
    }
}

#[async_trait::async_trait]
impl Brain for HostedBrain {
    fn id(&self) -> BrainId {
        BrainId::Hosted
    }

    async fn decide(&self, req: DecisionRequest) -> anyhow::Result<DecisionOutput> {
        req.say(format!("Asking {}", self.url));
        let body = json!({ "context": req.context, "instructions": req.instructions, "schema": decision_schema() });
        let resp = client()
            .post(format!("{}/v1/decide", self.url))
            .bearer_auth(&self.key)
            .timeout(HOSTED_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Hosted brain: {}", describe(&e)))?;
        let status = resp.status();
        let text = resp.text().await?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("Hosted brain: key not accepted");
        }
        if !status.is_success() {
            bail!("Hosted brain: {}", api_error(status, &text));
        }
        let v: Value = serde_json::from_str(&text).with_context(|| format!("Hosted brain: not JSON: {}", snippet(&text)))?;
        let out = parse::finish(&v, &req.context, None).context("Hosted brain")?;
        req.say("Decision received");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_model_filter() {
        let ids = [
            "gpt-4o",
            "gpt-4o-2024-08-06",
            "gpt-5.5",
            "gpt-6-astra",
            "o4-mini",
            "text-embedding-3-large",
            "gpt-4o-audio-preview",
            "dall-e-3",
            "gpt-image-1",
            "omni-moderation-latest",
            "whisper-1",
        ];
        let out = filter_openai_models(ids.iter().map(|s| s.to_string()).collect());
        assert_eq!(out, vec!["gpt-5.5", "o4-mini", "gpt-6-astra", "gpt-4o"]);
    }

    #[test]
    fn error_bodies() {
        let s = api_error(reqwest::StatusCode::UNAUTHORIZED, r#"{"error":{"message":"Incorrect API key"}}"#);
        assert_eq!(s, "HTTP 401: Incorrect API key");
        assert_eq!(api_error(reqwest::StatusCode::BAD_GATEWAY, ""), "HTTP 502");
        assert_eq!(api_error(reqwest::StatusCode::CONFLICT, r#"{"error":"loop"}"#), "HTTP 409: loop");
    }
}
