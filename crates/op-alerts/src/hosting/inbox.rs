//! The relay's inbound side, on the host: texts to the relay's shared number
//! go to the farm server whose key last texted that number, and a farm that
//! stops polling for them is reported to its dead-man recipients.
//!
//! ```text
//! GET /v1/notify/inbox?since=<cursor>&wait=<s>  → {messages: [{id, channel, from, text, at}], cursor}
//! ```
//!
//! - Only verified recipients' texts are routed, to the key whose text to
//!   that number was the last this host sent (its idempotency key starts
//!   `relay:<key id>:`). A 6-digit code from a recipient still being verified
//!   verifies it (the key that asked last) and is routed too, so the farm
//!   can confirm the person's phone.
//! - STOP and START (and Twilio's other opt-out and opt-in words) go to every
//!   key that has the number: Twilio opts a phone out per sender number, so
//!   STOP to the shared number stops texts from every farm on it.
//! - The long-poll waits up to `wait` seconds (at most 25) for a text. Rows up
//!   to the `since` a farm sends back are delivered and deleted; a farm that
//!   lost an answer asks again with its old cursor and gets them again (and
//!   takes each once, by id).
//! - **Dead-man**: a key that has polled before but not for more than
//!   `deadman_after_min` (farm power or internet down) gets its `deadman`
//!   recipients texted once; polling again ends the outage.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::alert::MessageLog;
use op_core::messages::{Outbound, enqueue};
use op_core::time::{from_db, now, opt_from_db, to_db};
use op_core::{ApiError, ApiResult, Ctx, id};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::sync::Notify;

use super::{caller, can_send, key_prefix};
use crate::inbound::commands::Command;
use crate::notify::verify::{CODE_MINUTES, MAX_ATTEMPTS};
use crate::notify::{code_hash, same};

/// `rin_…`
pub const INBOX: &str = "rin";
/// Longest wait of one long-poll, seconds.
pub const MAX_WAIT_S: u64 = 25;
/// Texts handed over per answer.
const PAGE: i64 = 100;

/// A text for a farm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxMessage {
    pub id: String,
    /// `sms` or `whatsapp`: how it reached the relay.
    pub channel: String,
    /// E.164.
    pub from: String,
    pub text: String,
    pub at: DateTime<Utc>,
}

/// `GET /v1/notify/inbox`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    pub messages: Vec<InboxMessage>,
    /// Send back as `since` next time.
    pub cursor: String,
}

pub fn router() -> Router<Ctx> {
    Router::new().route("/v1/notify/inbox", get(get_inbox))
}

/// Long-polls waiting per data dir.
static WAKERS: LazyLock<Mutex<HashMap<PathBuf, Arc<Notify>>>> = LazyLock::new(Default::default);

fn waker(ctx: &Ctx) -> Arc<Notify> {
    WAKERS.lock().unwrap_or_else(|e| e.into_inner()).entry(ctx.data_dir().to_path_buf()).or_default().clone()
}

#[derive(Debug, Deserialize)]
struct InboxQuery {
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    wait: Option<u64>,
}

async fn polled(ctx: &Ctx, key_id: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO relay_polls (key_id, polled_at) VALUES (?, ?) ON CONFLICT(key_id) DO UPDATE SET polled_at = excluded.polled_at")
        .bind(key_id)
        .bind(to_db(&now()))
        .execute(ctx.db())
        .await?;
    Ok(())
}

async fn waiting(ctx: &Ctx, key_id: &str, since: i64) -> anyhow::Result<Vec<(i64, InboxMessage)>> {
    let rows = sqlx::query("SELECT * FROM relay_inbox WHERE key_id = ? AND seq > ? ORDER BY seq LIMIT ?")
        .bind(key_id)
        .bind(since)
        .bind(PAGE)
        .fetch_all(ctx.db())
        .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("seq")?,
                InboxMessage {
                    id: r.try_get("id")?,
                    channel: r.try_get("channel")?,
                    from: r.try_get("address")?,
                    text: r.try_get("text")?,
                    at: from_db(&r.try_get::<String, _>("at")?)?,
                },
            ))
        })
        .collect()
}

/// 401 unknown key · 403 relaying off · 400 bad cursor · 200 the texts after
/// `since` (none after `wait` seconds without any).
async fn get_inbox(State(ctx): State<Ctx>, headers: HeaderMap, Query(q): Query<InboxQuery>) -> ApiResult<Json<Inbox>> {
    let (key_id, _) = caller(&ctx, &headers).await?;
    let since: i64 = match q.since.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => 0,
        Some(s) => s.parse().map_err(|_| ApiError::bad_request("since is the cursor from the last answer."))?,
    };
    let wait = Duration::from_secs(q.wait.unwrap_or(0).min(MAX_WAIT_S));
    polled(&ctx, &key_id).await?;
    // Everything up to the cursor it sent back has reached the farm.
    sqlx::query("DELETE FROM relay_inbox WHERE key_id = ? AND seq <= ?").bind(&key_id).bind(since).execute(ctx.db()).await?;
    let wake = waker(&ctx);
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let rows = waiting(&ctx, &key_id, since).await?;
        if !rows.is_empty() || tokio::time::Instant::now() >= deadline || ctx.is_shutting_down() {
            polled(&ctx, &key_id).await?;
            let cursor = rows.last().map_or(since, |(seq, _)| *seq).to_string();
            return Ok(Json(Inbox { messages: rows.into_iter().map(|(_, m)| m).collect(), cursor }));
        }
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep_until(deadline) => {}
            _ = ctx.on_shutdown() => {}
        }
    }
}

/// Hold `msg` for `key_id`'s farm.
async fn put(ctx: &Ctx, key_id: &str, msg: &MessageLog) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO relay_inbox (id, key_id, channel, address, text, at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(id::new_id(INBOX))
        .bind(key_id)
        .bind(&msg.channel)
        .bind(&msg.address)
        .bind(&msg.text)
        .bind(to_db(&msg.created_at))
        .execute(ctx.db())
        .await?;
    waker(ctx).notify_waiters();
    Ok(())
}

/// The key whose text to `address` was the last one this host sent.
async fn last_texting_key(ctx: &Ctx, address: &str) -> anyhow::Result<Option<String>> {
    let key: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT idempotency_key FROM messages WHERE address = ? AND direction = 'out' AND status IN ('sending', 'sent', 'delivered')
         ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .bind(address)
    .fetch_optional(ctx.db())
    .await?;
    Ok(key.and_then(|(k,)| k).and_then(|k| k.strip_prefix("relay:").and_then(|r| r.split_once(':')).map(|(id, _)| id.to_owned())))
}

/// A code texted back by a recipient still being verified: the key that
/// asked last, when it is that key's code.
async fn verify_by_text(ctx: &Ctx, channel: &str, address: &str, code: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query(
        "SELECT id, key_id, code_hash, attempts, sent_at FROM notify_recipients
         WHERE channel = ? AND address = ? AND verified_at IS NULL AND code_hash IS NOT NULL ORDER BY sent_at DESC LIMIT 1",
    )
    .bind(channel)
    .bind(address)
    .fetch_optional(ctx.db())
    .await?;
    let Some(r) = row else { return Ok(None) };
    let rid: String = r.try_get("id")?;
    let key_id: String = r.try_get("key_id")?;
    let hash: String = r.try_get("code_hash")?;
    let sent_at = opt_from_db(r.try_get("sent_at")?)?;
    if r.try_get::<i64, _>("attempts")? >= MAX_ATTEMPTS || sent_at.is_none_or(|t| now() - t > chrono::Duration::minutes(CODE_MINUTES)) {
        return Ok(None);
    }
    let n = sqlx::query("UPDATE notify_recipients SET attempts = attempts + 1 WHERE id = ? AND attempts < ?")
        .bind(&rid)
        .bind(MAX_ATTEMPTS)
        .execute(ctx.db())
        .await?
        .rows_affected();
    if n == 0 || !same(&hash, &code_hash(&[&key_id, channel, address, code])) {
        return Ok(None);
    }
    sqlx::query("UPDATE notify_recipients SET verified_at = ?, code_hash = NULL, attempts = 0 WHERE id = ?")
        .bind(to_db(&now()))
        .bind(&rid)
        .execute(ctx.db())
        .await?;
    Ok(Some(key_id))
}

/// Where a text to this host's own number goes. Returns how many farms it
/// went to (0: it's the host's own to handle).
pub async fn route(ctx: &Ctx, msg: &MessageLog, cmd: &Command) -> anyhow::Result<usize> {
    let (channel, address) = (msg.channel.as_str(), msg.address.as_str());
    match cmd {
        Command::OptOut | Command::OptIn => {
            let keys: Vec<(String,)> =
                sqlx::query_as("SELECT DISTINCT key_id FROM notify_recipients WHERE channel = ? AND address = ? AND verified_at IS NOT NULL ORDER BY key_id")
                    .bind(channel)
                    .bind(address)
                    .fetch_all(ctx.db())
                    .await?;
            for (k,) in &keys {
                put(ctx, k, msg).await?;
            }
            return Ok(keys.len());
        }
        Command::Code(code) => {
            if let Some(k) = verify_by_text(ctx, channel, address, code).await? {
                put(ctx, &k, msg).await?;
                return Ok(1);
            }
        }
        _ => {}
    }
    let Some(key_id) = last_texting_key(ctx, address).await? else { return Ok(0) };
    let verified: Option<(String,)> =
        sqlx::query_as("SELECT id FROM notify_recipients WHERE key_id = ? AND channel = ? AND address = ? AND verified_at IS NOT NULL")
            .bind(&key_id)
            .bind(channel)
            .bind(address)
            .fetch_optional(ctx.db())
            .await?;
    if verified.is_none() {
        return Ok(0);
    }
    put(ctx, &key_id, msg).await?;
    Ok(1)
}

// ---- dead-man -------------------------------------------------------------------------

/// The dead-man text: "openpasture: Test farm hasn't checked in for 16 min. Its power or internet may be down."
pub fn deadman_text(label: &str, minutes: i64) -> String {
    let name = crate::inbound::act::nm(label);
    let name = if name.is_empty() { "A farm server".to_owned() } else { name };
    format!("openpasture: {name} hasn't checked in for {minutes} min. Its power or internet may be down.")
}

/// Text the dead-man recipients of every key quiet for longer than
/// `deadman_after_min`, once per outage. Returns what was queued.
pub async fn deadman_pass(ctx: &Ctx, at: DateTime<Utc>) -> anyhow::Result<Vec<MessageLog>> {
    let cfg = super::load(ctx).await?;
    if !cfg.enabled {
        return Ok(vec![]);
    }
    let cutoff = at - chrono::Duration::minutes(cfg.deadman_after_min as i64);
    let rows = sqlx::query(
        "SELECT p.key_id, p.polled_at, k.label FROM relay_polls p JOIN brain_hosted_keys k ON k.id = p.key_id
         WHERE p.polled_at < ? AND (p.deadman_for IS NULL OR p.deadman_for < p.polled_at)",
    )
    .bind(to_db(&cutoff))
    .fetch_all(ctx.db())
    .await?;
    let mut out = Vec::new();
    for r in &rows {
        let key_id: String = r.try_get("key_id")?;
        let polled_at = from_db(&r.try_get::<String, _>("polled_at")?)?;
        let label: String = r.try_get("label")?;
        let text = deadman_text(&label, (at - polled_at).num_minutes());
        let recipients =
            sqlx::query("SELECT id, channel, address FROM notify_recipients WHERE key_id = ? AND deadman = 1 AND verified_at IS NOT NULL ORDER BY id")
                .bind(&key_id)
                .fetch_all(ctx.db())
                .await?;
        for rc in &recipients {
            let channel: String = rc.try_get("channel")?;
            if can_send(ctx, &channel).await.is_err() {
                continue;
            }
            let rid: String = rc.try_get("id")?;
            out.push(
                enqueue(
                    ctx,
                    Outbound {
                        idempotency_key: format!("{}deadman:{}:{rid}", key_prefix(&key_id), polled_at.timestamp_millis()),
                        channel: channel.clone(),
                        to: rc.try_get("address")?,
                        text: text.clone(),
                        subject: (channel == "email").then(|| "openpasture: a farm server is quiet".to_owned()),
                        kind: "alert".into(),
                        ..Default::default()
                    },
                )
                .await?,
            );
        }
        sqlx::query("UPDATE relay_polls SET deadman_for = polled_at WHERE key_id = ?").bind(&key_id).execute(ctx.db()).await?;
    }
    Ok(out)
}

/// Check for quiet keys every 30 s.
pub fn start(ctx: Ctx) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tick.tick() => {
                    if let Err(e) = deadman_pass(&ctx, now()).await {
                        tracing::warn!("relay dead-man: {e:#}");
                    }
                }
            }
        }
    });
}
