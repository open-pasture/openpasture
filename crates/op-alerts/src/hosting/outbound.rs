//! `POST /v1/notify`: a farm server's text or email, queued on this server's
//! own outbox and sent from its own channel.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use op_core::messages::{self, Outbound};
use op_core::{ApiError, ApiJson, ApiResult, Ctx};
use serde::{Deserialize, Serialize};

use super::{address, caller, can_send, key_prefix, take_quota};

/// Kinds a relayed message may say it is (default `alert`).
const KINDS: [&str; 5] = ["alert", "brief", "reply", "test", "verify"];

#[derive(Debug, Deserialize)]
pub struct NotifyBody {
    pub idempotency_key: String,
    pub channel: String,
    pub to: String,
    pub text: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Accepted {
    pub id: String,
    /// `queued`, or `duplicate` for an idempotency key seen before (sent once).
    pub status: &'static str,
}

/// 401 unknown key · 403 relaying off · 400 bad body · 202 duplicate ·
/// 409 loop guard · 503 can't send that channel · 403 recipient not verified ·
/// 429 minute or day cap · 202 queued.
pub async fn post_notify(State(ctx): State<Ctx>, headers: HeaderMap, ApiJson(b): ApiJson<NotifyBody>) -> ApiResult<(StatusCode, Json<Accepted>)> {
    let (key_id, cfg) = caller(&ctx, &headers).await?;
    let k = b.idempotency_key.trim();
    if k.is_empty() || k.len() > 200 {
        return Err(ApiError::bad_request("idempotency_key is 1 to 200 characters."));
    }
    let channel = b.channel.trim();
    let to = address(channel, &b.to)?;
    let text = b.text.trim();
    if text.is_empty() || text.chars().count() > 1600 {
        return Err(ApiError::bad_request("text is 1 to 1,600 characters."));
    }
    let kind = b.kind.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("alert");
    if !KINDS.contains(&kind) {
        return Err(ApiError::bad_request("kind is alert, brief, reply, test or verify."));
    }
    let idempotency_key = format!("{}{k}", key_prefix(&key_id));

    // A retry of something already taken: same answer, nothing sent again.
    let seen: Option<(String,)> = sqlx::query_as("SELECT id FROM messages WHERE idempotency_key = ?").bind(&idempotency_key).fetch_optional(ctx.db()).await?;
    if let Some((id,)) = seen {
        return Ok((StatusCode::ACCEPTED, Json(Accepted { id, status: "duplicate" })));
    }
    can_send(&ctx, channel).await?;
    let verified: Option<(String,)> =
        sqlx::query_as("SELECT id FROM notify_recipients WHERE key_id = ? AND channel = ? AND address = ? AND verified_at IS NOT NULL")
            .bind(&key_id)
            .bind(channel)
            .bind(&to)
            .fetch_optional(ctx.db())
            .await?;
    if verified.is_none() {
        return Err(ApiError::forbidden("That recipient isn't verified."));
    }

    let _held = super::LIMIT.lock().await;
    // Checked again under the lock: two copies of one request race here.
    let seen: Option<(String,)> = sqlx::query_as("SELECT id FROM messages WHERE idempotency_key = ?").bind(&idempotency_key).fetch_optional(ctx.db()).await?;
    if let Some((id,)) = seen {
        return Ok((StatusCode::ACCEPTED, Json(Accepted { id, status: "duplicate" })));
    }
    take_quota(&ctx, &key_id, &cfg).await?;
    let msg = messages::enqueue(
        &ctx,
        Outbound {
            idempotency_key,
            channel: channel.to_owned(),
            to,
            text: text.to_owned(),
            subject: b.subject.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()),
            kind: kind.to_owned(),
            ..Default::default()
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(Accepted { id: msg.id, status: "queued" })))
}
