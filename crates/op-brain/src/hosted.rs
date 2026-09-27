//! The serving side of the hosted brain: this server decides for other
//! openpasture servers. `POST /v1/decide` with a key this server issued runs
//! this server's own configured brain. Our paid hosting runs exactly this.
//! `POST /v1/ask` answers their questions the same way, with no tools.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use op_core::{ApiError, ApiJson, ApiResult, BrainId, Ctx, id, keys, time};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;

use crate::ask::NoTools;
use crate::{AskError, AskRequest, DecisionOutput, DecisionRequest};

pub const KEY_PREFIX: &str = "oph_";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedKey {
    pub id: String,
    pub label: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used: Option<chrono::DateTime<chrono::Utc>>,
}

/// A new key. The key itself is returned once and only its hash is stored.
pub async fn create_key(ctx: &Ctx, label: &str) -> anyhow::Result<(HostedKey, String)> {
    let key = format!("{KEY_PREFIX}{}", keys::new_collar_key());
    let k = HostedKey { id: id::new_id("hkey"), label: label.trim().to_owned(), created_at: time::now(), last_used: None };
    sqlx::query("INSERT INTO brain_hosted_keys (id, label, key_hash, created_at) VALUES (?, ?, ?, ?)")
        .bind(&k.id)
        .bind(&k.label)
        .bind(keys::hash_key(&key))
        .bind(time::to_db(&k.created_at))
        .execute(ctx.db())
        .await?;
    Ok((k, key))
}

pub async fn list_keys(ctx: &Ctx) -> anyhow::Result<Vec<HostedKey>> {
    let rows = sqlx::query("SELECT id, label, created_at, last_used FROM brain_hosted_keys ORDER BY created_at DESC").fetch_all(ctx.db()).await?;
    rows.iter()
        .map(|r| {
            Ok(HostedKey {
                id: r.try_get("id")?,
                label: r.try_get("label")?,
                created_at: time::from_db(&r.try_get::<String, _>("created_at")?)?,
                last_used: time::opt_from_db(r.try_get("last_used")?)?,
            })
        })
        .collect()
}

pub async fn delete_key(ctx: &Ctx, key_id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query("DELETE FROM brain_hosted_keys WHERE id = ?").bind(key_id).execute(ctx.db()).await?.rows_affected() > 0)
}

/// The key's id if it is valid; marks it used.
pub async fn check_key(ctx: &Ctx, key: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT id FROM brain_hosted_keys WHERE key_hash = ?").bind(keys::hash_key(key.trim())).fetch_optional(ctx.db()).await?;
    let Some(row) = row else { return Ok(None) };
    let key_id: String = row.try_get("id")?;
    sqlx::query("UPDATE brain_hosted_keys SET last_used = ? WHERE id = ?").bind(time::to_db(&time::now())).bind(&key_id).execute(ctx.db()).await?;
    Ok(Some(key_id))
}

#[derive(Deserialize)]
pub struct NewKey {
    #[serde(default, alias = "name")]
    pub label: String,
}

#[derive(Serialize)]
pub struct CreatedKey {
    #[serde(flatten)]
    pub info: HostedKey,
    pub key: String,
}

pub async fn post_key(State(ctx): State<Ctx>, body: axum::body::Bytes) -> ApiResult<(StatusCode, Json<CreatedKey>)> {
    // The body is optional.
    let label = if body.iter().all(u8::is_ascii_whitespace) {
        String::new()
    } else {
        serde_json::from_slice::<NewKey>(&body).map_err(|e| ApiError::bad_request(e.to_string()))?.label
    };
    let (info, key) = create_key(&ctx, &label).await?;
    Ok((StatusCode::CREATED, Json(CreatedKey { info, key })))
}

pub async fn get_keys(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<HostedKey>>> {
    Ok(Json(list_keys(&ctx).await?))
}

pub async fn remove_key(State(ctx): State<Ctx>, axum::extract::Path(key_id): axum::extract::Path<String>) -> ApiResult<StatusCode> {
    if delete_key(&ctx, &key_id).await? { Ok(StatusCode::NO_CONTENT) } else { Err(ApiError::not_found("No such key.")) }
}

#[derive(Deserialize)]
pub struct DecideBody {
    pub context: Value,
    #[serde(default)]
    pub instructions: String,
    /// The caller's copy of the decision schema. This server answers with its own,
    /// which is the same for the same version.
    #[serde(default)]
    pub schema: Option<Value>,
}

/// `POST /v1/decide`.
pub async fn decide(State(ctx): State<Ctx>, headers: HeaderMap, ApiJson(body): ApiJson<DecideBody>) -> ApiResult<Json<DecisionOutput>> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|k| !k.is_empty());
    let Some(bearer) = bearer else { return Err(ApiError::unauthorized("Missing key.")) };
    let Some(key_id) = check_key(&ctx, bearer).await? else { return Err(ApiError::unauthorized("Key not accepted.")) };

    let settings = ctx.settings().await?;
    if settings.brain.id == BrainId::Hosted {
        return Err(ApiError::conflict("This server's own brain is hosted, so it can't serve decisions."));
    }
    // Codex's tools can read files on this machine even in its read-only
    // sandbox, and there is no way to switch every one of them off for good,
    // so an outside caller's prompt must never reach it.
    if settings.brain.id == BrainId::Codex {
        return Err(ApiError::conflict("Codex can't serve hosted decisions: its tools can read files on this server. Choose another brain to serve."));
    }
    let brain = crate::brain_for(&ctx, settings.brain.id, settings.brain.model.clone())
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
    let herd_id = body.context.pointer("/herd/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            tracing::debug!(target: "op_brain::hosted", "{line}");
        }
    });
    tracing::info!(key = %key_id, brain = ?settings.brain.id, "serving a hosted decision");
    let req = DecisionRequest { herd_id, context: body.context, instructions: body.instructions, mcp_url: String::new(), tools: vec![], log: tx };
    let _ = body.schema;
    let out = brain.decide(req).await.map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct AskBody {
    pub question: String,
    #[serde(default)]
    pub context: Value,
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
}

fn default_max_chars() -> usize {
    320
}

/// Longest answer a caller may ask for.
pub const ASK_MAX_CHARS: usize = 2000;

#[derive(Debug, Serialize, Deserialize)]
pub struct AskAnswer {
    pub answer: String,
}

/// `POST /v1/ask`: this server's brain answers another server's question from
/// the context it sent, with no tools (outside input never reaches this
/// server's farm).
pub async fn ask(State(ctx): State<Ctx>, headers: HeaderMap, ApiJson(body): ApiJson<AskBody>) -> ApiResult<Json<AskAnswer>> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|k| !k.is_empty());
    let Some(bearer) = bearer else { return Err(ApiError::unauthorized("Missing key.")) };
    let Some(key_id) = check_key(&ctx, bearer).await? else { return Err(ApiError::unauthorized("Key not accepted.")) };
    let question = body.question.trim().to_owned();
    if question.is_empty() {
        return Err(ApiError::bad_request("Ask a question."));
    }

    let settings = ctx.settings().await?;
    if settings.brain.id == BrainId::Hosted {
        return Err(ApiError::conflict("This server's own brain is hosted, so it can't answer questions."));
    }
    // Codex never sees outside input (see `decide`), and doesn't answer questions anyway.
    if settings.brain.id == BrainId::Codex {
        return Err(ApiError::new(StatusCode::NOT_IMPLEMENTED, "This server's brain doesn't answer questions."));
    }
    let brain = crate::brain_for(&ctx, settings.brain.id, settings.brain.model.clone())
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
    tracing::info!(key = %key_id, brain = ?settings.brain.id, "answering a hosted question");
    let req = AskRequest { question, context: body.context, tools: std::sync::Arc::new(NoTools), max_chars: body.max_chars.clamp(1, ASK_MAX_CHARS), log: None };
    match brain.ask(req).await {
        Ok(answer) => Ok(Json(AskAnswer { answer })),
        Err(AskError::Unsupported) => Err(ApiError::new(StatusCode::NOT_IMPLEMENTED, "This server's brain doesn't answer questions.")),
        Err(AskError::Failed(e)) => Err(ApiError::new(StatusCode::BAD_GATEWAY, format!("{e:#}"))),
    }
}
