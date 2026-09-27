//! Recipients of the hosted relay: a farm server names a phone or email, this
//! server sends it a 6-digit code from its own channel, and the farm hands the
//! code back. Only verified recipients get relayed texts. 5 tries, 10-minute
//! codes, one new code per 30 s; codes are stored hashed and never logged.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use op_core::time::{from_db, now, opt_from_db, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

use super::{RECIPIENT, address, caller, can_send, key_prefix, take_quota};
use crate::notify::verify::{CODE_MINUTES, MAX_ATTEMPTS, RESEND_SECONDS};
use crate::notify::{Sent, code_hash, code_text, draft, masked_code_text, new_code, record_sent, same};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecipientView {
    pub channel: String,
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deadman: bool,
}

/// `GET /v1/notify/recipients`: every address this key named, verified or not.
pub async fn list(State(ctx): State<Ctx>, headers: HeaderMap) -> ApiResult<Json<Vec<RecipientView>>> {
    let (key_id, _) = caller(&ctx, &headers).await?;
    let rows = sqlx::query("SELECT channel, address, verified_at, deadman FROM notify_recipients WHERE key_id = ? ORDER BY channel, address")
        .bind(&key_id)
        .fetch_all(ctx.db())
        .await?;
    let out = rows
        .iter()
        .map(|r| {
            Ok(RecipientView {
                channel: r.try_get("channel")?,
                to: r.try_get("address")?,
                verified_at: opt_from_db(r.try_get("verified_at")?)?,
                deadman: r.try_get::<i64, _>("deadman")? != 0,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct AddBody {
    pub channel: String,
    pub to: String,
    #[serde(default)]
    pub deadman: Option<bool>,
}

/// `POST /v1/notify/recipients`: text (or email) `to` a code. An address that
/// is already verified only has its `deadman` flag updated.
pub async fn add(State(ctx): State<Ctx>, headers: HeaderMap, ApiJson(b): ApiJson<AddBody>) -> ApiResult<(StatusCode, Json<Value>)> {
    let (key_id, cfg) = caller(&ctx, &headers).await?;
    let channel = b.channel.trim().to_owned();
    let to = address(&channel, &b.to)?;
    can_send(&ctx, &channel).await?;
    let row = sqlx::query("SELECT id, sent_at, verified_at, deadman FROM notify_recipients WHERE key_id = ? AND channel = ? AND address = ?")
        .bind(&key_id)
        .bind(&channel)
        .bind(&to)
        .fetch_optional(ctx.db())
        .await?;
    let (rid, deadman) = match &row {
        Some(r) => {
            let rid: String = r.try_get("id")?;
            let deadman = b.deadman.unwrap_or(r.try_get::<i64, _>("deadman")? != 0);
            if r.try_get::<Option<String>, _>("verified_at")?.is_some() {
                sqlx::query("UPDATE notify_recipients SET deadman = ? WHERE id = ?").bind(deadman).bind(&rid).execute(ctx.db()).await?;
                return Ok((StatusCode::ACCEPTED, Json(json!({ "status": "verified" }))));
            }
            if let Some(t) = r.try_get::<Option<String>, _>("sent_at")?
                && now() - from_db(&t)? < chrono::Duration::seconds(RESEND_SECONDS)
            {
                return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Wait a moment before asking for another code."));
            }
            (rid, deadman)
        }
        None => (id::new_id(RECIPIENT), b.deadman.unwrap_or(false)),
    };

    {
        let _held = super::LIMIT.lock().await;
        take_quota(&ctx, &key_id, &cfg).await?;
    }
    let code = new_code();
    let t = now();
    sqlx::query(
        "INSERT INTO notify_recipients (id, key_id, channel, address, code_hash, attempts, sent_at, verified_at, deadman)
         VALUES (?, ?, ?, ?, ?, 0, ?, NULL, ?)
         ON CONFLICT(key_id, channel, address) DO UPDATE SET code_hash = excluded.code_hash, attempts = 0, sent_at = excluded.sent_at, deadman = excluded.deadman",
    )
    .bind(&rid)
    .bind(&key_id)
    .bind(&channel)
    .bind(&to)
    .bind(code_hash(&[&key_id, &channel, &to, &code]))
    .bind(to_db(&t))
    .bind(deadman)
    .execute(ctx.db())
    .await?;

    let (text, subject, stored) = if channel == "email" {
        (
            format!("Your openpasture code is {code}. It works for {CODE_MINUTES} minutes."),
            Some("openpasture code"),
            format!("Your openpasture code is ******. It works for {CODE_MINUTES} minutes."),
        )
    } else {
        (code_text(&code), None, masked_code_text())
    };
    let ch = crate::notify::channel(&ctx, &channel).await?.ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "This server can't send that now."))?;
    let res = ch.send(&draft(&channel, &to, "verify", &text, subject)).await;
    let (status, provider_id, error) = match &res {
        Ok(d) => (d.status.clone(), d.provider_id.clone(), None),
        Err(e) => ("failed".to_owned(), None, Some(e.message().to_owned())),
    };
    let poll = (matches!(channel.as_str(), "sms" | "whatsapp") && status == "sent" && provider_id.is_some())
        .then(|| now() + chrono::Duration::seconds(crate::notify::sender::STATUS_POLLS[0]));
    record_sent(
        &ctx,
        Sent {
            channel: channel.clone(),
            to: to.clone(),
            text: stored,
            subject: subject.map(Into::into),
            kind: "verify".into(),
            idempotency_key: Some(format!("{}code:{rid}:{}", key_prefix(&key_id), t.timestamp_millis())),
            status,
            provider_id,
            error,
            next_attempt_at: poll,
            ..Default::default()
        },
    )
    .await?;
    if let Err(e) = res {
        // Nothing arrived, so a new code may be asked for at once.
        sqlx::query("UPDATE notify_recipients SET code_hash = NULL, sent_at = NULL WHERE id = ?").bind(&rid).execute(ctx.db()).await?;
        return Err(ApiError::new(StatusCode::BAD_GATEWAY, e.message()));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "sent" }))))
}

#[derive(Debug, Deserialize)]
pub struct VerifyBody {
    pub channel: String,
    pub to: String,
    pub code: String,
}

/// `POST /v1/notify/recipients/verify`: 200 `{verified: true}`; 404 no code
/// asked for, 400 wrong, 410 expired, 429 after 5 wrong tries.
pub async fn verify(State(ctx): State<Ctx>, headers: HeaderMap, ApiJson(b): ApiJson<VerifyBody>) -> ApiResult<Json<Value>> {
    let (key_id, _) = caller(&ctx, &headers).await?;
    let channel = b.channel.trim().to_owned();
    let to = address(&channel, &b.to)?;
    let code: String = b.code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    let row = sqlx::query("SELECT id, code_hash, attempts, sent_at, verified_at FROM notify_recipients WHERE key_id = ? AND channel = ? AND address = ?")
        .bind(&key_id)
        .bind(&channel)
        .bind(&to)
        .fetch_optional(ctx.db())
        .await?;
    let Some(row) = row else { return Err(ApiError::not_found("Ask for a code first.")) };
    let rid: String = row.try_get("id")?;
    let hash: Option<String> = row.try_get("code_hash")?;
    let Some(hash) = hash else {
        return if row.try_get::<Option<String>, _>("verified_at")?.is_some() {
            Ok(Json(json!({ "verified": true })))
        } else {
            Err(ApiError::not_found("Ask for a code first."))
        };
    };
    if row.try_get::<i64, _>("attempts")? >= MAX_ATTEMPTS {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Too many tries. Ask for a new code."));
    }
    let sent_at = opt_from_db(row.try_get("sent_at")?)?;
    if sent_at.is_none_or(|t| now() - t > chrono::Duration::minutes(CODE_MINUTES)) {
        return Err(ApiError::new(StatusCode::GONE, "That code has expired. Ask for a new one."));
    }
    let n = sqlx::query("UPDATE notify_recipients SET attempts = attempts + 1 WHERE id = ? AND attempts < ?")
        .bind(&rid)
        .bind(MAX_ATTEMPTS)
        .execute(ctx.db())
        .await?
        .rows_affected();
    if n == 0 {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Too many tries. Ask for a new code."));
    }
    if code.len() != 6 || !same(&hash, &code_hash(&[&key_id, &channel, &to, &code])) {
        return Err(ApiError::bad_request("That code isn't right."));
    }
    sqlx::query("UPDATE notify_recipients SET verified_at = ?, code_hash = NULL, attempts = 0 WHERE id = ?")
        .bind(to_db(&now()))
        .bind(&rid)
        .execute(ctx.db())
        .await?;
    Ok(Json(json!({ "verified": true })))
}
