//! Free-form questions (`Brain::ask`): "how many are outside?" by text gets
//! a short answer from the farm record and read tools.
//!
//! - Anthropic, OpenAI and compatible brains run the tool loop here over the
//!   caller's [`ToolRunner`]: at most [`MAX_TOOL_CALLS`] tool calls, all
//!   within [`BUDGET`].
//! - The Claude CLI answers through the brain-scoped MCP with a token that
//!   allows the runner's tools ([`crate::claude`]).
//! - The hosted brain asks another openpasture server (`POST /v1/ask`), whose
//!   own brain answers with no tools ([`crate::hosted::ask`]).
//! - Codex and the heuristic don't answer questions ([`AskError::Unsupported`],
//!   the trait's default).
//!
//! `run_sql` is never offered, whatever runner the caller built. Every answer
//! is tidied for a text and cut to `max_chars` at a sentence boundary.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use op_core::tools::{ToolInfo, ToolRunner};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::api::{is_auth, send_json};
use crate::parse::snippet;

/// Tool calls one question may make.
pub const MAX_TOOL_CALLS: usize = 6;
/// Time one question may take, tool calls included.
pub const BUDGET: Duration = Duration::from_secs(45);
/// Tools a question never gets.
pub const NEVER: [&str; 1] = ["run_sql"];
/// A tool result longer than this is cut before the model sees it.
const TOOL_RESULT_MAX: usize = 12_000;
/// Longest answer a model is asked to write, in tokens.
const MAX_TOKENS: u32 = 1024;

/// One question.
#[derive(Clone)]
pub struct AskRequest {
    pub question: String,
    /// What the caller knows up front (a farm summary); `null` for nothing.
    pub context: Value,
    /// Read tools the answer may use (`ctx.tool_runner(identity, &["run_sql"])`).
    pub tools: Arc<dyn ToolRunner>,
    /// Longest answer, in characters (320 for an SMS reply).
    pub max_chars: usize,
    /// Progress, one line at a time.
    pub log: Option<UnboundedSender<String>>,
}

impl AskRequest {
    pub(crate) fn say(&self, line: impl Into<String>) {
        if let Some(log) = &self.log {
            let _ = log.send(line.into());
        }
    }

    /// The tools this question may use: the runner's, never `run_sql`.
    pub fn offered(&self) -> Vec<ToolInfo> {
        self.tools.tools().into_iter().filter(|t| !NEVER.contains(&t.name.as_str())).collect()
    }
}

#[derive(Debug)]
pub enum AskError {
    /// This brain doesn't answer questions (Codex, the heuristic).
    Unsupported,
    Failed(anyhow::Error),
}

impl std::fmt::Display for AskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AskError::Unsupported => f.write_str("This brain doesn't answer questions."),
            AskError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for AskError {}

impl From<anyhow::Error> for AskError {
    fn from(e: anyhow::Error) -> Self {
        AskError::Failed(e)
    }
}

/// No tools: what a hosted question gets (outside input never reaches this
/// server's tools).
pub struct NoTools;

#[async_trait::async_trait]
impl ToolRunner for NoTools {
    fn tools(&self) -> Vec<ToolInfo> {
        vec![]
    }

    async fn call(&self, name: &str, _args: Value) -> Result<Value, String> {
        Err(format!("Unknown tool {name}."))
    }
}

/// System text for questions.
pub const SYSTEM: &str = "\
You answer questions about one farm for the people who work it, usually by text message. \
The farm uses openpasture: its animals wear GPS collars that hold a virtual fence.

Rules:
- Answer from the farm record and the tools. Never make up animals, paddocks, collars or numbers; \
if you can't tell, say so.
- Be brief: one to three short, plain sentences. No markdown, lists, emoji or links.
- Give measurements in the farm's units when the record says which.
- You only read. If asked to change something, say in one sentence that you can't.";

/// The user message: the record, the tools, the question and the limit.
pub fn prompt(question: &str, context: &Value, tools: &[String], max_chars: usize) -> String {
    let mut out = String::new();
    if !context.is_null() {
        out.push_str("## Farm record\n\n```json\n");
        out.push_str(&serde_json::to_string_pretty(context).unwrap_or_else(|_| context.to_string()));
        out.push_str("\n```\n\n");
    }
    if !tools.is_empty() {
        out.push_str("## Tools\n\nRead-only tools can look up more: ");
        out.push_str(&tools.join(", "));
        out.push_str(&format!(". Call them only when the record doesn't answer, at most {MAX_TOOL_CALLS} calls.\n\n"));
    }
    out.push_str("## Question\n\n");
    out.push_str(question.trim());
    out.push_str(&format!("\n\nAnswer in at most {max_chars} characters.\n"));
    out
}

/// A model's answer as a text: markdown marks and bullets gone, lines joined
/// into sentences, whitespace collapsed.
pub fn tidy(s: &str) -> String {
    let s = s.replace("**", "").replace("__", "").replace('`', "");
    let lines: Vec<&str> = s
        .lines()
        .map(|l| {
            let l = l.trim().trim_start_matches('#').trim_start();
            l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")).or_else(|| l.strip_prefix("• ")).unwrap_or(l).trim()
        })
        .filter(|l| !l.is_empty())
        .collect();
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            if !out.ends_with(['.', '!', '?', ':', ';', ',']) {
                out.push('.');
            }
            out.push(' ');
        }
        out.push_str(l);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// [`tidy`], then cut to `max_chars` characters at the last sentence end
/// that fits. When not even the first sentence fits, whole words and "...".
pub fn fit(text: &str, max_chars: usize) -> String {
    let t = tidy(text);
    let chars: Vec<char> = t.chars().collect();
    if chars.len() <= max_chars {
        return t;
    }
    let max = max_chars.max(1);
    let mut cut = None;
    for i in 0..max {
        if matches!(chars[i], '.' | '!' | '?') && chars.get(i + 1).is_none_or(|c| c.is_whitespace()) {
            cut = Some(i + 1);
        }
    }
    if let Some(c) = cut {
        return chars[..c].iter().collect();
    }
    if max < 8 {
        return chars[..max].iter().collect();
    }
    let head: String = chars[..max - 3].iter().collect();
    let head = match head.rfind(char::is_whitespace) {
        Some(i) if i > 0 => &head[..i],
        _ => head.as_str(),
    };
    format!("{}...", head.trim_end_matches(|c: char| !c.is_alphanumeric()))
}

/// The final answer from a model's text, or "no answer".
pub(crate) fn answer(text: &str, max_chars: usize) -> Result<String, AskError> {
    let a = fit(text, max_chars);
    if a.is_empty() { Err(AskError::Failed(anyhow::anyhow!("The brain gave no answer."))) } else { Ok(a) }
}

struct Budget(Instant);

impl Budget {
    fn start() -> Self {
        Budget(Instant::now() + BUDGET)
    }

    fn left(&self) -> Result<Duration, AskError> {
        let left = self.0.saturating_duration_since(Instant::now());
        if left.is_zero() { Err(AskError::Failed(anyhow::anyhow!("No answer within {} s.", BUDGET.as_secs()))) } else { Ok(left) }
    }

    /// Run `f`, failing when the budget runs out first.
    async fn run<T>(&self, f: impl std::future::Future<Output = anyhow::Result<T>>) -> Result<T, AskError> {
        match tokio::time::timeout(self.left()?, f).await {
            Ok(r) => r.map_err(AskError::Failed),
            Err(_) => Err(AskError::Failed(anyhow::anyhow!("No answer within {} s.", BUDGET.as_secs()))),
        }
    }
}

/// The tool calls of one question: runs up to [`MAX_TOOL_CALLS`], then says no.
struct Calls<'a> {
    req: &'a AskRequest,
    names: Vec<String>,
    used: usize,
}

impl<'a> Calls<'a> {
    fn new(req: &'a AskRequest, tools: &[ToolInfo]) -> Self {
        Calls { req, names: tools.iter().map(|t| t.name.clone()).collect(), used: 0 }
    }

    fn spent(&self) -> bool {
        self.used >= MAX_TOOL_CALLS
    }

    /// One call: `(content, is_error)`.
    async fn run(&mut self, name: &str, args: Value, budget: &Budget) -> (String, bool) {
        if self.spent() {
            return (format!("Not run: the limit of {MAX_TOOL_CALLS} tool calls is reached. Answer with what you have."), true);
        }
        if !self.names.iter().any(|n| n == name) {
            return (format!("Unknown tool {name}."), true);
        }
        self.used += 1;
        self.req.say(format!("Tool {name}"));
        let left = match budget.left() {
            Ok(l) => l,
            Err(e) => return (e.to_string(), true),
        };
        match tokio::time::timeout(left, self.req.tools.call(name, args)).await {
            Ok(Ok(v)) => (cut_result(&serde_json::to_string(&v).unwrap_or_default()), false),
            Ok(Err(e)) => (e, true),
            Err(_) => ("Timed out.".into(), true),
        }
    }
}

fn cut_result(s: &str) -> String {
    if s.len() <= TOOL_RESULT_MAX {
        return s.to_owned();
    }
    let mut end = TOOL_RESULT_MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} (cut: {} more bytes)", &s[..end], s.len() - end)
}

/// Rounds of model calls one question may take: every tool call in its own
/// round, then the answer.
const MAX_ROUNDS: usize = MAX_TOOL_CALLS + 2;

/// Anthropic Messages API with tools.
pub(crate) async fn anthropic(http: reqwest::Client, base: &str, key: &str, model: &str, req: AskRequest) -> Result<String, AskError> {
    let budget = Budget::start();
    let tools = req.offered();
    let mut calls = Calls::new(&req, &tools);
    let defs: Vec<Value> = tools.iter().map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema })).collect();
    let mut messages = vec![json!({ "role": "user", "content": prompt(&req.question, &req.context, &calls.names, req.max_chars) })];
    req.say(format!("Asking {model}"));
    for _ in 0..MAX_ROUNDS {
        let mut body = json!({ "model": model, "max_tokens": MAX_TOKENS, "system": SYSTEM, "messages": messages });
        if !defs.is_empty() {
            body["tools"] = json!(defs);
            body["tool_choice"] = if calls.spent() { json!({ "type": "none" }) } else { json!({ "type": "auto" }) };
        }
        let rb =
            http.post(format!("{base}/v1/messages")).header("x-api-key", key).header("anthropic-version", "2023-06-01").timeout(budget.left()?).json(&body);
        let v = budget.run(async { send_json(rb).await.context("Anthropic") }).await?;
        let content = v.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
        let uses: Vec<&Value> = content.iter().filter(|c| c.get("type").and_then(Value::as_str) == Some("tool_use")).collect();
        if uses.is_empty() {
            let text: String = content.iter().filter_map(|c| c.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(" ");
            req.say("Answer received");
            return answer(&text, req.max_chars);
        }
        let mut results = Vec::new();
        for u in &uses {
            let name = u.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = u.get("input").cloned().unwrap_or_else(|| json!({}));
            let (out, is_error) = calls.run(name, args, &budget).await;
            results.push(json!({ "type": "tool_result", "tool_use_id": u.get("id").cloned().unwrap_or(Value::Null), "content": out, "is_error": is_error }));
        }
        messages.push(json!({ "role": "assistant", "content": content }));
        messages.push(json!({ "role": "user", "content": results }));
    }
    Err(AskError::Failed(anyhow::anyhow!("Anthropic kept calling tools without answering.")))
}

/// OpenAI Chat Completions with tools (OpenAI and compatible servers). A
/// compatible server that refuses tools answers from the record alone.
pub(crate) async fn openai(
    post: impl Fn(&str) -> reqwest::RequestBuilder,
    label: &str,
    model: &str,
    compatible: bool,
    req: AskRequest,
) -> Result<String, AskError> {
    let budget = Budget::start();
    let tools = req.offered();
    let mut calls = Calls::new(&req, &tools);
    let defs: Vec<Value> = tools
        .iter()
        .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.input_schema } }))
        .collect();
    let mut with_tools = !defs.is_empty();
    let user = |names: &[String]| prompt(&req.question, &req.context, names, req.max_chars);
    let mut messages = vec![json!({ "role": "system", "content": SYSTEM }), json!({ "role": "user", "content": user(&calls.names) })];
    req.say(format!("Asking {model}"));
    let mut round = 0;
    while round < MAX_ROUNDS {
        round += 1;
        let mut body = json!({ "model": model, "messages": messages });
        if with_tools {
            body["tools"] = json!(defs);
            body["tool_choice"] = json!(if calls.spent() { "none" } else { "auto" });
        }
        let v = match budget.run(send_json(post("/chat/completions").timeout(budget.left()?).json(&body))).await {
            Ok(v) => v,
            // Servers without tool support refuse the request: ask again without tools.
            Err(AskError::Failed(e)) if compatible && with_tools && round == 1 && e.to_string().starts_with("HTTP 4") && !is_auth(&e) => {
                req.say("Server has no tool support; answering from the record");
                with_tools = false;
                messages[1] = json!({ "role": "user", "content": user(&[]) });
                round = 0;
                continue;
            }
            Err(AskError::Failed(e)) => return Err(AskError::Failed(e.context(label.to_owned()))),
            Err(e) => return Err(e),
        };
        let msg = v.pointer("/choices/0/message").cloned().unwrap_or(Value::Null);
        if let Some(refusal) = msg.get("refusal").and_then(Value::as_str) {
            return Err(AskError::Failed(anyhow::anyhow!("{label} refused: {}", snippet(refusal))));
        }
        let tool_calls = msg.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        if tool_calls.is_empty() {
            req.say("Answer received");
            return answer(msg.get("content").and_then(Value::as_str).unwrap_or_default(), req.max_chars);
        }
        messages.push(json!({ "role": "assistant", "content": msg.get("content").cloned().unwrap_or(Value::Null), "tool_calls": tool_calls }));
        for c in &tool_calls {
            let name = c.pointer("/function/name").and_then(Value::as_str).unwrap_or_default();
            let raw = c.pointer("/function/arguments").and_then(Value::as_str).unwrap_or_default();
            let (out, is_error) = match serde_json::from_str::<Value>(if raw.trim().is_empty() { "{}" } else { raw }) {
                Ok(args) => calls.run(name, args, &budget).await,
                Err(_) => ("The arguments were not JSON.".into(), true),
            };
            let content = if is_error { format!("Error: {out}") } else { out };
            messages.push(json!({ "role": "tool", "tool_call_id": c.get("id").cloned().unwrap_or(Value::Null), "content": content }));
        }
    }
    Err(AskError::Failed(anyhow::anyhow!("{label} kept calling tools without answering.")))
}

/// `POST {url}/v1/ask` on another openpasture server. Its 501 (its brain
/// doesn't answer questions) is [`AskError::Unsupported`] here too.
pub(crate) async fn hosted(http: reqwest::Client, url: &str, key: &str, req: AskRequest) -> Result<String, AskError> {
    req.say(format!("Asking {url}"));
    let body = json!({ "question": req.question, "context": req.context, "max_chars": req.max_chars });
    // The host has its own budget; allow for the trip there and back.
    let resp = http
        .post(format!("{url}/v1/ask"))
        .bearer_auth(key)
        .timeout(BUDGET + Duration::from_secs(15))
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Hosted brain: {}", crate::api::describe(&e)))?;
    let status = resp.status();
    let text = resp.text().await.map_err(anyhow::Error::from)?;
    match status {
        reqwest::StatusCode::UNAUTHORIZED => return Err(AskError::Failed(anyhow::anyhow!("Hosted brain: key not accepted"))),
        reqwest::StatusCode::NOT_IMPLEMENTED => return Err(AskError::Unsupported),
        s if !s.is_success() => return Err(AskError::Failed(anyhow::anyhow!("Hosted brain: {}", crate::api::api_error(s, &text)))),
        _ => {}
    }
    let v: Value = serde_json::from_str(&text).with_context(|| format!("Hosted brain: not JSON: {}", snippet(&text)))?;
    req.say("Answer received");
    answer(v.get("answer").and_then(Value::as_str).unwrap_or_default(), req.max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_answers_pass_through_tidied() {
        assert_eq!(fit("  **214** is outside   P3.\n", 320), "214 is outside P3.");
        assert_eq!(fit("Outside:\n- 214\n- 031", 320), "Outside: 214. 031");
        assert_eq!(fit("## Status\nAll 250 inside", 320), "Status. All 250 inside");
        assert_eq!(fit("", 320), "");
    }

    #[test]
    fn long_answers_cut_at_the_last_sentence_that_fits() {
        let a = "All 250 collars reported in the last hour. Two are outside P3 near the east gate. Battery is fine on every collar.";
        let out = fit(a, 90);
        assert_eq!(out, "All 250 collars reported in the last hour. Two are outside P3 near the east gate.");
        assert!(out.chars().count() <= 90);
        // A decimal point is not a sentence end.
        assert_eq!(fit("P4 is 30.6 ac and rested 34 days. It has water.", 40), "P4 is 30.6 ac and rested 34 days.");
        // Exactly at the limit.
        let s = "One. Two.";
        assert_eq!(fit(s, 9), s);
        assert_eq!(fit(s, 8), "One.");
    }

    #[test]
    fn a_first_sentence_too_long_ends_on_a_whole_word() {
        let out = fit("The herd is grazing the north half of P3 and moving slowly toward the water", 30);
        assert!(out.chars().count() <= 30, "{out}");
        assert_eq!(out, "The herd is grazing the...");
        assert!(fit("abcdefghijklmnop", 5).chars().count() <= 5);
    }

    #[test]
    fn prompt_names_the_tools_and_the_limit() {
        let p = prompt("How many outside?", &json!({ "farm": "Test farm" }), &["get_farm".into(), "get_herd".into()], 320);
        assert!(p.contains("\"Test farm\""));
        assert!(p.contains("get_farm, get_herd"));
        assert!(p.contains("at most 6 calls"));
        assert!(p.contains("How many outside?"));
        assert!(p.ends_with("Answer in at most 320 characters.\n"));
        let p = prompt("Hi", &Value::Null, &[], 160);
        assert!(!p.contains("Farm record") && !p.contains("Tools"));
    }

    #[test]
    fn errors_read_as_sentences() {
        assert_eq!(AskError::Unsupported.to_string(), "This brain doesn't answer questions.");
        assert_eq!(AskError::from(anyhow::anyhow!("boom")).to_string(), "boom");
    }
}
