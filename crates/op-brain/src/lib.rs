//! Brain trait and backends: codex, claude, anthropic, openai, compatible,
//! hosted, heuristic. Owns `GET /api/brains` and `POST /api/brains/{id}/test`.
//!
//! Every LLM backend gets the same prompt ([`prompt::build_prompt`]) and its
//! output goes through the same parser ([`parse::parse_output`]).
//!
//! Brains may also answer free-form questions ([`Brain::ask`], [`ask`]).

pub mod api;
pub mod ask;
pub mod claude;
mod cli;
pub mod codex;
pub mod detect;
#[cfg(test)]
pub(crate) mod fixture;
pub mod heuristic;
pub mod hosted;
pub mod parse;
pub mod prompt;
mod routes;
pub mod schema;

use op_core::{BrainId, Ctx};
use serde::{Deserialize, Serialize};

pub use ask::{AskError, AskRequest};
pub use op_core::DecisionAction as Action;
pub use schema::decision_schema;

/// What op-engine hands a brain for one decision.
#[derive(Debug, Clone)]
pub struct DecisionRequest {
    pub herd_id: String,
    /// Built by op-engine (see docs/API.md, Decision context).
    pub context: serde_json::Value,
    /// Skill text (`skills/daily-grazing-decision/SKILL.md`).
    pub instructions: String,
    /// The local MCP URL with read tools for the brain.
    pub mcp_url: String,
    /// The tools that URL lists (the registry's brain tools), named in the
    /// prompt. Empty with no `mcp_url`.
    pub tools: Vec<String>,
    /// Progress, one line at a time.
    pub log: tokio::sync::mpsc::UnboundedSender<String>,
}

impl DecisionRequest {
    pub(crate) fn say(&self, line: impl Into<String>) {
        let _ = self.log.send(line.into());
    }

    /// The tools the prompt names: none without an MCP URL.
    pub fn prompt_tools(&self) -> &[String] {
        if self.mcp_url.is_empty() { &[] } else { &self.tools }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionOutput {
    pub action: Action,
    /// For MOVE: the paddock to move to. With no geometry the engine uses the paddock's shape.
    pub to_paddock_id: Option<String>,
    /// A custom boundary, validated.
    pub geometry: Option<op_geo::Polygon>,
    pub reasoning: String,
    /// 0 to 1.
    pub confidence: f64,
    /// For NEEDS_INFO: what is missing.
    pub need: Option<String>,
    pub model: Option<String>,
}

#[async_trait::async_trait]
pub trait Brain: Send + Sync {
    fn id(&self) -> BrainId;
    async fn decide(&self, req: DecisionRequest) -> anyhow::Result<DecisionOutput>;

    /// Answer a free-form question (a text) in at most `req.max_chars`
    /// characters, using the read tools in `req.tools` (never `run_sql`).
    /// Brains that don't answer questions refuse: Codex (its tools can read
    /// this machine's files) and the heuristic.
    async fn ask(&self, _req: AskRequest) -> Result<String, AskError> {
        Err(AskError::Unsupported)
    }
}

/// The brain chosen in settings.
pub async fn resolve(ctx: &Ctx) -> anyhow::Result<Box<dyn Brain>> {
    let s = ctx.settings().await?;
    brain_for(ctx, s.brain.id, s.brain.model.clone())
}

/// A specific brain, configured from secrets. Fails with a short, readable
/// message when it can't run (not installed, no key).
pub fn brain_for(ctx: &Ctx, id: BrainId, model: Option<String>) -> anyhow::Result<Box<dyn Brain>> {
    let model = model.filter(|m| !m.trim().is_empty());
    let secret = |name: &str| -> anyhow::Result<Option<String>> { Ok(ctx.secrets().get(name)?.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())) };
    let loc = detect::Locator::from_env();
    Ok(match id {
        BrainId::Heuristic => Box::new(heuristic::HeuristicBrain),
        BrainId::Codex => {
            let bin = loc.find("codex").ok_or_else(|| anyhow::anyhow!("Codex CLI is not installed"))?;
            Box::new(codex::CodexBrain::new(bin, model))
        }
        BrainId::Claude => {
            let bin = loc.find("claude").ok_or_else(|| anyhow::anyhow!("Claude Code CLI is not installed"))?;
            Box::new(claude::ClaudeBrain::new(bin, model).with_ctx(ctx.clone()))
        }
        BrainId::Anthropic => {
            let key = secret("anthropic_api_key")?.ok_or_else(|| anyhow::anyhow!("Add an Anthropic API key"))?;
            Box::new(api::AnthropicBrain::new(key, model))
        }
        BrainId::Openai => {
            let key = secret("openai_api_key")?.ok_or_else(|| anyhow::anyhow!("Add an OpenAI API key"))?;
            Box::new(api::OpenAiBrain::openai(key, model))
        }
        BrainId::Compatible => {
            let base = secret("compatible_base_url")?.ok_or_else(|| anyhow::anyhow!("Add a base URL"))?;
            Box::new(api::OpenAiBrain::compatible(base, secret("compatible_api_key")?, model))
        }
        BrainId::Hosted => {
            let key = secret("hosted_api_key")?.ok_or_else(|| anyhow::anyhow!("Add an openpasture API key"))?;
            Box::new(api::HostedBrain::new(secret("hosted_url")?, key))
        }
    })
}

/// Take a `token=` query parameter out of an MCP URL. CLI brains pass the
/// token to the CLI through the environment or a 0600 file instead of argv,
/// where every local user could read it.
pub fn split_mcp_token(url: &str) -> (String, Option<String>) {
    let Some((base, query)) = url.split_once('?') else { return (url.to_owned(), None) };
    let mut token = None;
    let rest: Vec<&str> = query
        .split('&')
        .filter(|kv| match kv.strip_prefix("token=") {
            Some(t) => {
                token = Some(t.to_owned()).filter(|t| !t.is_empty());
                false
            }
            None => !kv.is_empty(),
        })
        .collect();
    let url = if rest.is_empty() { base.to_owned() } else { format!("{base}?{}", rest.join("&")) };
    (url, token)
}

/// Routes from docs/API.md, "Decisions and brains (op-engine, op-brain)".
pub fn router() -> axum::Router<Ctx> {
    routes::router()
}

/// Background tasks. Warms the brain detection cache so the first
/// `GET /api/brains` is fast.
pub async fn start(ctx: Ctx) -> anyhow::Result<()> {
    tokio::spawn(async move {
        detect::refresh(&ctx).await;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn mcp_token_leaves_the_url() {
        assert_eq!(
            super::split_mcp_token("http://10.0.0.2:7878/mcp?scope=brain&token=opb_x"),
            ("http://10.0.0.2:7878/mcp?scope=brain".into(), Some("opb_x".into()))
        );
        assert_eq!(super::split_mcp_token("http://h/mcp?token=t&scope=brain"), ("http://h/mcp?scope=brain".into(), Some("t".into())));
        assert_eq!(super::split_mcp_token("http://127.0.0.1:7878/mcp?scope=brain"), ("http://127.0.0.1:7878/mcp?scope=brain".into(), None));
        assert_eq!(super::split_mcp_token(""), (String::new(), None));
    }
}
