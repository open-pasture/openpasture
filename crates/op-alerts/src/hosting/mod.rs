//! The hosted relay, host side: this server texts and emails for other
//! openpasture servers, from its own channels, to recipients each proved by a
//! one-time code. Callers use the same `oph_` keys as the hosted brain
//! (`op_brain::hosted::check_key`). Off unless `notify.hosting.enabled`.
//!
//! ```text
//! POST /v1/notify                    {idempotency_key, channel, to, text, subject?, kind?, prompt?} → 202 {id, status: queued|duplicate}
//! POST /v1/notify/recipients         {channel, to, deadman?} → 202 {status: sent|verified}
//! POST /v1/notify/recipients/verify  {channel, to, code} → 200 {verified: true}
//! GET  /v1/notify/recipients         → [{channel, to, verified_at?, deadman}]
//! ```
//!
//! Abuse limits: verified recipients only, `per_key_minute` (30) and
//! `per_key_day` (500) texts per key, idempotency keys, and a loop guard (a
//! server that itself sends through a relay doesn't relay).

pub mod outbound;
pub mod recipients;
// @A3
pub mod inbox;
// @M

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use op_core::users::normalize_phone;
use op_core::{ApiError, ApiJson, ApiResult, Ctx, patch};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `nrc_…`
pub const RECIPIENT: &str = "nrc";

/// The setting key.
pub const HOSTING_KEY: &str = "notify.hosting";

/// `notify.hosting`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostingConfig {
    pub enabled: bool,
    pub per_key_minute: u32,
    pub per_key_day: u32,
    /// Minutes without an inbox poll before a key's dead-man recipients hear (A3).
    pub deadman_after_min: u32,
}

impl Default for HostingConfig {
    fn default() -> Self {
        Self { enabled: false, per_key_minute: 30, per_key_day: 500, deadman_after_min: 15 }
    }
}

pub async fn load(ctx: &Ctx) -> anyhow::Result<HostingConfig> {
    let Some(v) = ctx.store().get_setting_json(HOSTING_KEY).await? else { return Ok(HostingConfig::default()) };
    Ok(serde_json::from_value(v).unwrap_or_else(|e| {
        tracing::warn!("notify.hosting is unreadable ({e}); relaying stays off");
        HostingConfig::default()
    }))
}

pub fn router() -> Router<Ctx> {
    let mut app = Router::new();
    for part in [
        Router::new()
            .route("/v1/notify", post(outbound::post_notify))
            .route("/v1/notify/recipients", get(recipients::list).post(recipients::add))
            .route("/v1/notify/recipients/verify", post(recipients::verify))
            .route("/api/notify/hosting", get(get_hosting).put(put_hosting)),
        // @A3
        inbox::router(),
        // @M
    ] {
        app = app.merge(part);
    }
    app
}

async fn get_hosting(State(ctx): State<Ctx>) -> ApiResult<Json<HostingConfig>> {
    Ok(Json(load(&ctx).await?))
}

/// A merge patch over [`HostingConfig`].
async fn put_hosting(State(ctx): State<Ctx>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<HostingConfig>> {
    let next: HostingConfig = patch::apply(&load(&ctx).await?, &body, &[])?;
    if !(1..=1000).contains(&next.per_key_minute) {
        return Err(ApiError::bad_request("per_key_minute is 1 to 1,000."));
    }
    if !(1..=100_000).contains(&next.per_key_day) {
        return Err(ApiError::bad_request("per_key_day is 1 to 100,000."));
    }
    if !(5..=1440).contains(&next.deadman_after_min) {
        return Err(ApiError::bad_request("deadman_after_min is 5 to 1,440."));
    }
    ctx.store().set_setting(HOSTING_KEY, &next).await?;
    Ok(Json(next))
}

/// The calling key's id: 401 without a key this server issued, 403 while
/// relaying is off.
pub async fn caller(ctx: &Ctx, headers: &HeaderMap) -> ApiResult<(String, HostingConfig)> {
    let bearer =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::trim).filter(|k| !k.is_empty());
    let Some(bearer) = bearer else { return Err(ApiError::unauthorized("Missing key.")) };
    let Some(key_id) = op_brain::hosted::check_key(ctx, bearer).await? else { return Err(ApiError::unauthorized("Key not accepted.")) };
    let cfg = load(ctx).await?;
    if !cfg.enabled {
        return Err(ApiError::forbidden("This server doesn't relay texts."));
    }
    Ok((key_id, cfg))
}

/// A relay address: `sms`/`whatsapp` → E.164, `email` → lowercased.
pub fn address(channel: &str, to: &str) -> ApiResult<String> {
    match channel {
        "sms" | "whatsapp" => {
            normalize_phone(to.trim().trim_start_matches("whatsapp:")).ok_or_else(|| ApiError::bad_request("Phone numbers look like +15155550123."))
        }
        "email" => {
            let e = to.trim().to_lowercase();
            if crate::notify::api::email_like(&e) { Ok(e) } else { Err(ApiError::bad_request("That email address doesn't look right.")) }
        }
        _ => Err(ApiError::bad_request("channel is sms, whatsapp or email.")),
    }
}

/// This server can send `channel` itself: 409 when it would have to go
/// through a relay (loop guard), 503 when it can't at all.
pub async fn can_send(ctx: &Ctx, channel: &str) -> ApiResult<()> {
    let own = op_core::notify_config::configured_channels(ctx).await?;
    if own.contains(&channel) {
        return Ok(());
    }
    if own.contains(&"relay") {
        return Err(ApiError::conflict("This server sends through a relay itself, so it can't relay."));
    }
    Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, format!("This server can't send {} now.", crate::notify::label(channel).to_lowercase())))
}

/// All the texts one key caused, in this server's `messages`, share the
/// idempotency-key prefix `relay:<key id>:`.
pub fn key_prefix(key_id: &str) -> String {
    format!("relay:{key_id}:")
}

/// Count one more text for `key_id`, or 429 when the minute or day cap is
/// reached. The caller holds [`LIMIT`] until its message is stored.
pub async fn take_quota(ctx: &Ctx, key_id: &str, cfg: &HostingConfig) -> ApiResult<()> {
    let prefix = key_prefix(key_id);
    // The next string after every `relay:<key>:…`.
    let upper = format!("relay:{key_id};");
    let since = op_core::time::to_db(&(op_core::time::now() - chrono::Duration::seconds(60)));
    let (minute,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages WHERE idempotency_key >= ? AND idempotency_key < ? AND created_at >= ?")
        .bind(&prefix)
        .bind(&upper)
        .bind(&since)
        .fetch_one(ctx.db())
        .await?;
    if minute >= i64::from(cfg.per_key_minute) {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Too many texts this minute. Try again shortly."));
    }
    let day = op_core::time::now().format("%Y-%m-%d").to_string();
    let counted: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO notify_usage (key_id, day, count) VALUES (?1, ?2, 1)
         ON CONFLICT(key_id, day) DO UPDATE SET count = count + 1 WHERE count < ?3
         RETURNING count",
    )
    .bind(key_id)
    .bind(&day)
    .bind(i64::from(cfg.per_key_day))
    .fetch_optional(ctx.db())
    .await?;
    if counted.is_none() {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "The daily text limit for this key is reached."));
    }
    Ok(())
}

/// Serializes quota checks with the message they let through, so parallel
/// requests can't all pass the last free slot.
pub static LIMIT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
