//! Phone verification. `POST /api/notify/verify {user_id}` texts a 6-digit
//! code (through the farm's own SMS channel, else through the hosted relay,
//! which texts its own code); `POST /api/notify/verify/confirm {user_id, code}`
//! sets `users.phone_verified_at`. 5 tries, 10-minute codes, one new code per
//! 30 s. The code text is the first text a number gets and ends with the
//! opt-out line. Codes are stored hashed together with the phone they went to,
//! and never logged; the stored copy of the text is masked.
//!
//! A3 confirms a code texted back with [`confirm`].

use chrono::{DateTime, Utc};
use op_core::notify_config::configured_channels;
use op_core::time::{from_db, now, to_db};
use op_core::users::{self, User};
use op_core::{ApiError, ApiResult, Ctx, Role};
use serde::Serialize;
use sqlx::Row;

use super::relay::Relay;
use super::{ChannelError, Sent, channel, code_hash, code_text, draft, masked_code_text, new_code, record_sent, same};

/// Tries per code.
pub const MAX_ATTEMPTS: i64 = 5;
/// How long a code works.
pub const CODE_MINUTES: i64 = 10;
/// Wait between two codes for one person.
pub const RESEND_SECONDS: i64 = 30;

/// `code_hash` of a code the relay sent and checks itself.
const RELAY: &str = "relay";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodeSent {
    /// `sms` (the farm's Twilio) or `relay`.
    pub via: String,
    /// The relay had already proven this number for this server, so no code
    /// was needed and the phone is verified now.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verified {
    pub verified: bool,
    pub phone_verified_at: DateTime<Utc>,
}

async fn person_with_phone(ctx: &Ctx, user_id: &str) -> ApiResult<(User, String)> {
    let u = users::get_user(ctx, user_id).await?.ok_or_else(|| ApiError::not_found("No such person."))?;
    if u.disabled_at.is_some() {
        return Err(ApiError::bad_request("This person is switched off."));
    }
    let phone = u.phone.clone().ok_or_else(|| ApiError::bad_request("Add a phone number first."))?;
    Ok((u, phone))
}

/// Text `user_id` a code. 409 when the phone is already verified or nothing
/// can text; 429 within 30 s of the last code; 502 with the provider's words
/// when the text didn't go.
pub async fn send_code(ctx: &Ctx, user_id: &str) -> ApiResult<CodeSent> {
    let (user, phone) = person_with_phone(ctx, user_id).await?;
    if user.phone_verified_at.is_some() {
        return Err(ApiError::conflict("That phone is already verified."));
    }
    let last: Option<(String,)> = sqlx::query_as("SELECT sent_at FROM phone_codes WHERE user_id = ?").bind(user_id).fetch_optional(ctx.db()).await?;
    if let Some((t,)) = last
        && now() - from_db(&t)? < chrono::Duration::seconds(RESEND_SECONDS)
    {
        return Err(ApiError::new(axum::http::StatusCode::TOO_MANY_REQUESTS, "Wait a moment before sending another code."));
    }

    if let Some(sms) = channel(ctx, "sms").await? {
        let code = new_code();
        let msg = draft("sms", &phone, "verify", &code_text(&code), None);
        let res = sms.send(&msg).await;
        let (status, provider_id, error) = match &res {
            Ok(d) => (d.status.clone(), d.provider_id.clone(), None),
            Err(e) => ("failed".to_owned(), None, Some(e.message().to_owned())),
        };
        let poll = (status == "sent" && provider_id.is_some()).then(|| now() + chrono::Duration::seconds(super::sender::STATUS_POLLS[0]));
        record_sent(
            ctx,
            Sent {
                channel: "sms".into(),
                to: phone.clone(),
                text: masked_code_text(),
                kind: "verify".into(),
                user_id: Some(user.id.clone()),
                status,
                provider_id,
                error,
                next_attempt_at: poll,
                ..Default::default()
            },
        )
        .await?;
        if let Err(e) = res {
            return Err(ApiError::new(axum::http::StatusCode::BAD_GATEWAY, e.message()));
        }
        store_code(ctx, user_id, &code_hash(&[user_id, &phone, &code])).await?;
        return Ok(CodeSent { via: "sms".into(), verified: false });
    }

    if configured_channels(ctx).await?.contains(&"relay")
        && let Some(relay) = Relay::from_secrets(ctx)?
    {
        // Owners and managers hear from the relay when this server goes quiet (A3).
        let deadman = user.role >= Role::Manager;
        let answer = relay.add_recipient("sms", &phone, deadman).await;
        let error = match &answer {
            Ok(Ok(_)) => None,
            Ok(Err(r)) => Some(r.message.clone()),
            Err(e) => Some(e.message().to_owned()),
        };
        record_sent(
            ctx,
            Sent {
                channel: "relay".into(),
                to: phone.clone(),
                text: masked_code_text(),
                kind: "verify".into(),
                user_id: Some(user.id.clone()),
                status: if error.is_none() { "sent".into() } else { "failed".into() },
                error: error.clone(),
                ..Default::default()
            },
        )
        .await?;
        return match answer {
            Ok(Ok(v)) if v.get("status").and_then(|s| s.as_str()) == Some("verified") => {
                // The relay already proved this number for this server.
                verified(ctx, user_id).await?;
                Ok(CodeSent { via: "relay".into(), verified: true })
            }
            Ok(Ok(_)) => {
                store_code(ctx, user_id, RELAY).await?;
                Ok(CodeSent { via: "relay".into(), verified: false })
            }
            Ok(Err(r)) if r.status == 429 => Err(ApiError::new(axum::http::StatusCode::TOO_MANY_REQUESTS, r.message)),
            Ok(Err(r)) => Err(ApiError::new(axum::http::StatusCode::BAD_GATEWAY, r.message)),
            Err(e) => Err(ApiError::new(axum::http::StatusCode::BAD_GATEWAY, e.message())),
        };
    }
    Err(ApiError::conflict("Set up Twilio or the relay first."))
}

async fn store_code(ctx: &Ctx, user_id: &str, hash: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO phone_codes (user_id, code_hash, attempts, sent_at) VALUES (?, ?, 0, ?)
         ON CONFLICT(user_id) DO UPDATE SET code_hash = excluded.code_hash, attempts = 0, sent_at = excluded.sent_at",
    )
    .bind(user_id)
    .bind(hash)
    .bind(to_db(&now()))
    .execute(ctx.db())
    .await?;
    Ok(())
}

/// Check `code` for `user_id`: sets `phone_verified_at` and forgets the code
/// when right. 400 wrong or no code, 410 expired, 429 after 5 wrong tries.
pub async fn confirm(ctx: &Ctx, user_id: &str, code: &str) -> ApiResult<Verified> {
    let (_, phone) = person_with_phone(ctx, user_id).await?;
    let code: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    let row = sqlx::query("SELECT code_hash, attempts, sent_at FROM phone_codes WHERE user_id = ?").bind(user_id).fetch_optional(ctx.db()).await?;
    let Some(row) = row else { return Err(ApiError::bad_request("Send a code first.")) };
    let hash: String = row.try_get("code_hash")?;
    let attempts: i64 = row.try_get("attempts")?;
    let sent_at = from_db(&row.try_get::<String, _>("sent_at")?)?;

    if hash == RELAY {
        return confirm_by_relay(ctx, user_id, &phone, &code).await;
    }
    if attempts >= MAX_ATTEMPTS {
        return Err(ApiError::new(axum::http::StatusCode::TOO_MANY_REQUESTS, "Too many tries. Send a new code."));
    }
    if now() - sent_at > chrono::Duration::minutes(CODE_MINUTES) {
        return Err(ApiError::new(axum::http::StatusCode::GONE, "That code has expired. Send a new one."));
    }
    // Count the try before comparing, so parallel guesses can't skip the limit.
    let n = sqlx::query("UPDATE phone_codes SET attempts = attempts + 1 WHERE user_id = ? AND attempts < ?")
        .bind(user_id)
        .bind(MAX_ATTEMPTS)
        .execute(ctx.db())
        .await?
        .rows_affected();
    if n == 0 {
        return Err(ApiError::new(axum::http::StatusCode::TOO_MANY_REQUESTS, "Too many tries. Send a new code."));
    }
    if code.len() != 6 || !same(&hash, &code_hash(&[user_id, &phone, &code])) {
        return Err(ApiError::bad_request("That code isn't right."));
    }
    verified(ctx, user_id).await
}

async fn confirm_by_relay(ctx: &Ctx, user_id: &str, phone: &str, code: &str) -> ApiResult<Verified> {
    let relay = Relay::from_secrets(ctx)?.ok_or_else(|| ApiError::conflict("The relay isn't set up any more. Send a new code."))?;
    match relay.verify_recipient("sms", phone, code).await {
        Ok(Ok(_)) => verified(ctx, user_id).await,
        Ok(Err(r)) => Err(ApiError::new(
            match r.status {
                404 => axum::http::StatusCode::BAD_REQUEST,
                s => axum::http::StatusCode::from_u16(s).unwrap_or(axum::http::StatusCode::BAD_GATEWAY),
            },
            if r.status == 404 { "Send a code first.".to_owned() } else { r.message },
        )),
        Err(ChannelError::Retry(m) | ChannelError::Fail(m)) => Err(ApiError::new(axum::http::StatusCode::BAD_GATEWAY, m)),
    }
}

async fn verified(ctx: &Ctx, user_id: &str) -> ApiResult<Verified> {
    let at = now();
    users::set_phone_verified(ctx, user_id, at).await?;
    sqlx::query("DELETE FROM phone_codes WHERE user_id = ?").bind(user_id).execute(ctx.db()).await?;
    Ok(Verified { verified: true, phone_verified_at: at })
}
