//! The `messages` table: outbox for every channel and inbox for texts, so the
//! alert engine (which decides who gets what) and the sender (which delivers)
//! never call each other. Every new or changed row is published as
//! [`Event::Message`] (managers and up see it on `/api/live`).
//!
//! PII: never add `messages` to op-analytics' `SQL_TABLES`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::alert::MessageLog;
use crate::time::{from_db, now, opt_from_db, to_db};
use crate::{Ctx, Event, id};

/// `ntf_…`
pub const MESSAGE: &str = "ntf";

/// Statuses a message can have.
pub const STATUSES: [&str; 7] = ["queued", "sending", "sent", "delivered", "failed", "received", "ignored"];

/// Something to send. `idempotency_key` makes a repeat enqueue return the
/// first row instead of sending twice.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Outbound {
    pub idempotency_key: String,
    pub channel: String,
    pub to: String,
    pub text: String,
    #[serde(default)]
    pub subject: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub alert_id: Option<String>,
    #[serde(default)]
    pub decision_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

/// A text that came in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inbound {
    pub channel: String,
    pub from: String,
    pub text: String,
    #[serde(default)]
    pub provider_id: Option<String>,
    pub at: DateTime<Utc>,
}

pub fn message_from_row(r: &SqliteRow) -> anyhow::Result<MessageLog> {
    Ok(MessageLog {
        id: r.try_get("id")?,
        direction: r.try_get("direction")?,
        channel: r.try_get("channel")?,
        address: r.try_get("address")?,
        user_id: r.try_get("user_id")?,
        kind: r.try_get("kind")?,
        text: r.try_get("text")?,
        subject: r.try_get("subject")?,
        status: r.try_get("status")?,
        error: r.try_get("error")?,
        alert_id: r.try_get("alert_id")?,
        decision_id: r.try_get("decision_id")?,
        provider_id: r.try_get("provider_id")?,
        attempts: r.try_get::<i64, _>("attempts")?.max(0) as u32,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
        updated_at: from_db(&r.try_get::<String, _>("updated_at")?)?,
    })
}

/// When the row may be claimed again (`None`: now).
pub fn next_attempt_at(r: &SqliteRow) -> anyhow::Result<Option<DateTime<Utc>>> {
    opt_from_db(r.try_get("next_attempt_at")?)
}

pub async fn get_message(ctx: &Ctx, id: &str) -> anyhow::Result<Option<MessageLog>> {
    let row = sqlx::query("SELECT * FROM messages WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    row.map(|r| message_from_row(&r)).transpose()
}

/// Queue a message (status `queued`) and publish it. A second enqueue with
/// the same `idempotency_key` returns the first row and publishes nothing.
pub async fn enqueue(ctx: &Ctx, m: Outbound) -> anyhow::Result<MessageLog> {
    anyhow::ensure!(!m.idempotency_key.is_empty(), "idempotency_key is required");
    let t = to_db(&now());
    let row = sqlx::query(
        "INSERT INTO messages (id, direction, channel, address, user_id, kind, text, subject, status, alert_id, decision_id, idempotency_key, attempts, created_at, updated_at)
         VALUES (?, 'out', ?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?, 0, ?, ?)
         ON CONFLICT DO NOTHING RETURNING *",
    )
    .bind(id::new_id(MESSAGE))
    .bind(&m.channel)
    .bind(&m.to)
    .bind(&m.user_id)
    .bind(&m.kind)
    .bind(&m.text)
    .bind(&m.subject)
    .bind(&m.alert_id)
    .bind(&m.decision_id)
    .bind(&m.idempotency_key)
    .bind(&t)
    .bind(&t)
    .fetch_optional(ctx.db())
    .await?;
    if let Some(r) = row {
        let msg = message_from_row(&r)?;
        ctx.publish(Event::Message { message: msg.clone() });
        return Ok(msg);
    }
    let r = sqlx::query("SELECT * FROM messages WHERE idempotency_key = ?").bind(&m.idempotency_key).fetch_one(ctx.db()).await?;
    message_from_row(&r)
}

/// Record a text that came in, with its sender (when known) and `status`
/// (`received` or `ignored`). `None` when `provider_id` was seen before on
/// this channel (a webhook retry, or a poll that overlapped).
pub async fn record_inbound(ctx: &Ctx, m: Inbound, user_id: Option<&str>, status: &str) -> anyhow::Result<Option<MessageLog>> {
    anyhow::ensure!(STATUSES.contains(&status), "unknown message status {status:?}");
    let row = sqlx::query(
        "INSERT INTO messages (id, direction, channel, address, user_id, kind, text, status, provider_id, attempts, created_at, updated_at)
         VALUES (?, 'in', ?, ?, ?, 'inbound', ?, ?, ?, 0, ?, ?)
         ON CONFLICT DO NOTHING RETURNING *",
    )
    .bind(id::new_id(MESSAGE))
    .bind(&m.channel)
    .bind(&m.from)
    .bind(user_id)
    .bind(&m.text)
    .bind(status)
    .bind(&m.provider_id)
    .bind(to_db(&m.at))
    .bind(to_db(&now()))
    .fetch_optional(ctx.db())
    .await?;
    let Some(r) = row else { return Ok(None) };
    let msg = message_from_row(&r)?;
    ctx.publish(Event::Message { message: msg.clone() });
    Ok(Some(msg))
}

/// Take up to `limit` queued outgoing messages on `channels` that are due at
/// `now`, oldest first, moving them to `sending` and counting an attempt, in
/// one statement: two senders never claim the same row.
pub async fn claim(ctx: &Ctx, channels: &[&str], now: DateTime<Utc>, limit: usize) -> anyhow::Result<Vec<MessageLog>> {
    if channels.is_empty() || limit == 0 {
        return Ok(vec![]);
    }
    let marks = vec!["?"; channels.len()].join(", ");
    let sql = format!(
        "UPDATE messages SET status = 'sending', attempts = attempts + 1, updated_at = ?
         WHERE id IN (
             SELECT id FROM messages
             WHERE status = 'queued' AND direction = 'out' AND channel IN ({marks})
               AND (next_attempt_at IS NULL OR next_attempt_at <= ?)
             ORDER BY created_at, id LIMIT ?
         ) RETURNING *"
    );
    let t = to_db(&now);
    let mut q = sqlx::query(&sql).bind(&t);
    for c in channels {
        q = q.bind(*c);
    }
    let rows = q.bind(&t).bind(limit as i64).fetch_all(ctx.db()).await?;
    let mut out = rows.iter().map(message_from_row).collect::<anyhow::Result<Vec<_>>>()?;
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// Set a message's status (one of [`STATUSES`]) and publish it. `provider_id`
/// is kept when `None`; `error` replaces the stored one. `retry_at` sets when
/// it may be claimed again, so a retry is `mark(id, "queued", None,
/// Some(why), Some(at))`.
pub async fn mark(ctx: &Ctx, id: &str, status: &str, provider_id: Option<&str>, error: Option<&str>, retry_at: Option<DateTime<Utc>>) -> anyhow::Result<()> {
    anyhow::ensure!(STATUSES.contains(&status), "unknown message status {status:?}");
    let row = sqlx::query(
        "UPDATE messages SET status = ?, provider_id = COALESCE(?, provider_id), error = ?, next_attempt_at = ?, updated_at = ?
         WHERE id = ? RETURNING *",
    )
    .bind(status)
    .bind(provider_id)
    .bind(error)
    .bind(retry_at.as_ref().map(to_db))
    .bind(to_db(&now()))
    .bind(id)
    .fetch_optional(ctx.db())
    .await?;
    let r = row.ok_or_else(|| anyhow::anyhow!("no message {id}"))?;
    ctx.publish(Event::Message { message: message_from_row(&r)? });
    Ok(())
}

/// When we last sent `address` something (queued and failed messages don't
/// count). For the reply window of code-less text commands.
pub async fn last_out_to(ctx: &Ctx, address: &str) -> anyhow::Result<Option<DateTime<Utc>>> {
    let (t,): (Option<String>,) =
        sqlx::query_as("SELECT MAX(created_at) FROM messages WHERE address = ? AND direction = 'out' AND status IN ('sending', 'sent', 'delivered')")
            .bind(address)
            .fetch_one(ctx.db())
            .await?;
    opt_from_db(t)
}
