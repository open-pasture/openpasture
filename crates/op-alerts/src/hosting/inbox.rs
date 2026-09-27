//! The relay's inbound side, on the host: texts to the relay's shared number
//! go to the farm server whose key last texted that number, and a farm that
//! stops polling for them is reported to its dead-man recipients.
//!
//! ```text
//! GET /v1/notify/inbox?since=<cursor>&wait=<s>  → {messages: [{id, channel, from, text, at}], cursor}
//! ```
//!
//! - A text only ever reaches farms whose key proved the number it came from
//!   (a verified recipient), and of those the one it answers:
//!   - a Y, N, LATER or STOP MOVE with a decision's 4-digit code goes to the
//!     farm whose text carried that code ("… Code 4821");
//!   - a bare Y, N, LATER or STOP MOVE goes to the one farm that asked the
//!     person about a decision in the last [`PROMPT_HOURS`] (the farm flags
//!     those texts when it posts them); when more than one did, this host
//!     answers itself and asks for the code;
//!   - anything else goes to the farm whose text to that number was the last
//!     this host sent (its idempotency key starts `relay:<key id>:`).
//!
//!   This server's own farm is one of the candidates when it has a verified
//!   person with that phone (its texts carry no `relay:` key); a key that
//!   only asked to verify the number, or was deleted, never is.
//! - A 6-digit code from a recipient still being verified verifies the key
//!   whose pending code it is (any of them) and is routed there, so the farm
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
use op_core::users::User;
use op_core::{ApiError, ApiResult, Ctx, id};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::sync::Notify;

use super::{caller, can_send, key_prefix};
use crate::inbound::commands::{Command, Pick};
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

/// Hours a relayed decision text counts as what a bare Y or N answers.
pub const PROMPT_HOURS: i64 = 12;
/// The reply when a bare answer could be for more than one farm.
pub fn which_farm_text(verb: &str) -> String {
    format!("More than one farm asked you. Add the code from the text you mean, like {verb} 4821.")
}

/// Statuses of a text that reached the person.
const REACHED: &str = "('sending', 'sent', 'delivered')";

/// Who a text to this host's number goes to.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// This server's own farm handles it (or nobody: it is logged ignored).
    Host,
    /// Handed to this many farms' inboxes.
    Farms(usize),
    /// A bare answer more than one farm could be waiting for: this host
    /// asked for the code instead (the queued reply).
    Asked(MessageLog),
}

/// One farm a text may be for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Farm {
    Key(String),
    /// This server's own farm.
    Host,
}

/// Keys that proved `address` on `channel`.
async fn verified_keys(ctx: &Ctx, channel: &str, address: &str) -> anyhow::Result<Vec<String>> {
    let keys: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT key_id FROM notify_recipients WHERE channel = ? AND address = ? AND verified_at IS NOT NULL ORDER BY key_id")
            .bind(channel)
            .bind(address)
            .fetch_all(ctx.db())
            .await?;
    Ok(keys.into_iter().map(|(k,)| k).collect())
}

/// Whose text a `messages` row of this server is: the key in a `relay:<key>:`
/// idempotency key, else this server's own farm.
fn sender_of(idempotency_key: Option<&str>) -> Farm {
    match idempotency_key.and_then(|k| k.strip_prefix("relay:")).and_then(|r| r.split_once(':')) {
        Some((id, _)) => Farm::Key(id.to_owned()),
        None => Farm::Host,
    }
}

/// When `farm`'s last text that reached `address` was sent.
async fn last_text(ctx: &Ctx, address: &str, farm: &Farm) -> anyhow::Result<Option<DateTime<Utc>>> {
    let t: (Option<String>,) = match farm {
        Farm::Key(k) => {
            sqlx::query_as(&format!(
                "SELECT MAX(created_at) FROM messages WHERE address = ? AND direction = 'out' AND status IN {REACHED} AND idempotency_key >= ? AND idempotency_key < ?"
            ))
            .bind(address)
            .bind(key_prefix(k))
            .bind(format!("relay:{k};"))
            .fetch_one(ctx.db())
            .await?
        }
        Farm::Host => {
            sqlx::query_as(&format!(
                "SELECT MAX(created_at) FROM messages WHERE address = ? AND direction = 'out' AND status IN {REACHED}
                   AND (idempotency_key IS NULL OR idempotency_key NOT LIKE 'relay:%')"
            ))
            .bind(address)
            .fetch_one(ctx.db())
            .await?
        }
    };
    opt_from_db(t.0)
}

/// Of `farms`, the one whose text to `address` was the last this host sent
/// (the first when none has one).
async fn newest(ctx: &Ctx, address: &str, farms: &[Farm]) -> anyhow::Result<Farm> {
    let mut best: Option<(DateTime<Utc>, &Farm)> = None;
    for f in farms {
        if let Some(t) = last_text(ctx, address, f).await?
            && best.is_none_or(|(b, _)| t > b)
        {
            best = Some((t, f));
        }
    }
    Ok(best.map_or_else(|| farms[0].clone(), |(_, f)| f.clone()))
}

/// "Code 4821" (any case) as a whole code in `text`.
pub fn carries_code(text: &str, code: &str) -> bool {
    let low = text.to_ascii_lowercase();
    let needle = format!("code {code}");
    low.match_indices(&needle).any(|(i, m)| !low[i + m.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

/// Of `farms`, the one whose newest text to `address` carried decision code `code`.
async fn by_code(ctx: &Ctx, address: &str, code: &str, farms: &[Farm]) -> anyhow::Result<Option<Farm>> {
    let rows: Vec<(Option<String>, String)> = sqlx::query_as(&format!(
        "SELECT idempotency_key, text FROM messages WHERE address = ? AND direction = 'out' AND status IN {REACHED} AND instr(text, ?) > 0
         ORDER BY created_at DESC, rowid DESC LIMIT 50"
    ))
    .bind(address)
    .bind(code)
    .fetch_all(ctx.db())
    .await?;
    Ok(rows.iter().filter(|(_, text)| carries_code(text, code)).map(|(k, _)| sender_of(k.as_deref())).find(|f| farms.contains(f)))
}

/// Of `farms`, those that asked `address` about a decision in the last [`PROMPT_HOURS`].
async fn asked(ctx: &Ctx, address: &str, farms: &[Farm], now: DateTime<Utc>) -> anyhow::Result<Vec<Farm>> {
    let since = to_db(&(now - chrono::Duration::hours(PROMPT_HOURS)));
    let mut out = Vec::new();
    for f in farms {
        let n: i64 = match f {
            Farm::Key(k) => {
                sqlx::query_scalar(&format!(
                    "SELECT COUNT(*) FROM relay_prompts p JOIN messages m ON m.id = p.message_id
                     WHERE p.key_id = ? AND p.address = ? AND p.at >= ? AND m.status IN {REACHED}"
                ))
                .bind(k)
                .bind(address)
                .bind(&since)
                .fetch_one(ctx.db())
                .await?
            }
            Farm::Host => {
                sqlx::query_scalar(&format!(
                    "SELECT COUNT(*) FROM messages m WHERE m.address = ? AND m.direction = 'out' AND m.status IN {REACHED} AND m.created_at >= ? AND {}",
                    crate::inbound::act::ASKS
                ))
                .bind(address)
                .bind(&since)
                .fetch_one(ctx.db())
                .await?
            }
        };
        if n > 0 {
            out.push(f.clone());
        }
    }
    Ok(out)
}

/// Note that the relayed text `message_id` asked `address` about a decision.
pub async fn record_prompt(ctx: &Ctx, message_id: &str, key_id: &str, address: &str) -> anyhow::Result<()> {
    let t = now();
    sqlx::query("DELETE FROM relay_prompts WHERE at < ?").bind(to_db(&(t - chrono::Duration::days(2)))).execute(ctx.db()).await?;
    sqlx::query("INSERT INTO relay_prompts (message_id, key_id, address, at) VALUES (?, ?, ?, ?) ON CONFLICT(message_id) DO NOTHING")
        .bind(message_id)
        .bind(key_id)
        .bind(address)
        .bind(to_db(&t))
        .execute(ctx.db())
        .await?;
    Ok(())
}

/// A code texted back by a recipient still being verified: it is tried
/// against every key's pending code for that number, and verifies the key
/// whose code it is. A wrong code uses up one try of each pending code; a
/// right one costs the other keys' codes nothing.
async fn verify_by_text(ctx: &Ctx, channel: &str, address: &str, code: &str) -> anyhow::Result<Option<String>> {
    let rows = sqlx::query(
        "SELECT id, key_id, code_hash, attempts, sent_at FROM notify_recipients
         WHERE channel = ? AND address = ? AND verified_at IS NULL AND code_hash IS NOT NULL ORDER BY sent_at DESC, id",
    )
    .bind(channel)
    .bind(address)
    .fetch_all(ctx.db())
    .await?;
    let mut tried = Vec::new();
    for r in &rows {
        let sent_at = opt_from_db(r.try_get("sent_at")?)?;
        if r.try_get::<i64, _>("attempts")? >= MAX_ATTEMPTS || sent_at.is_none_or(|t| now() - t > chrono::Duration::minutes(CODE_MINUTES)) {
            continue;
        }
        let rid: String = r.try_get("id")?;
        // Count the try before comparing, so parallel guesses can't skip the limit.
        let n = sqlx::query("UPDATE notify_recipients SET attempts = attempts + 1 WHERE id = ? AND attempts < ?")
            .bind(&rid)
            .bind(MAX_ATTEMPTS)
            .execute(ctx.db())
            .await?
            .rows_affected();
        if n == 1 {
            tried.push((rid, r.try_get::<String, _>("key_id")?, r.try_get::<String, _>("code_hash")?));
        }
    }
    let Some(i) = tried.iter().position(|(_, key_id, hash)| same(hash, &code_hash(&[key_id, channel, address, code]))) else { return Ok(None) };
    let (rid, key_id, _) = tried.remove(i);
    sqlx::query("UPDATE notify_recipients SET verified_at = ?, code_hash = NULL, attempts = 0 WHERE id = ?")
        .bind(to_db(&now()))
        .bind(&rid)
        .execute(ctx.db())
        .await?;
    for (other, ..) in &tried {
        sqlx::query("UPDATE notify_recipients SET attempts = attempts - 1 WHERE id = ? AND attempts > 0").bind(other).execute(ctx.db()).await?;
    }
    Ok(Some(key_id))
}

/// The decision pick of a command that answers one (Y, N, LATER, STOP MOVE).
fn answer_pick(cmd: &Command) -> Option<(&Pick, &'static str)> {
    match cmd {
        Command::Answer { approve: true, pick } => Some((pick, "Y")),
        Command::Answer { approve: false, pick } => Some((pick, "N")),
        Command::Later(pick) => Some((pick, "LATER")),
        Command::StopMove(pick) => Some((pick, "STOP MOVE")),
        _ => None,
    }
}

/// Where a text to this host's own number goes. `own` is this server's own
/// person with that phone, if any.
pub async fn route(ctx: &Ctx, msg: &MessageLog, cmd: &Command, own: Option<&User>) -> anyhow::Result<Route> {
    let (channel, address) = (msg.channel.as_str(), msg.address.as_str());
    let keys = verified_keys(ctx, channel, address).await?;
    match cmd {
        Command::OptOut | Command::OptIn => {
            for k in &keys {
                put(ctx, k, msg).await?;
            }
            return Ok(if keys.is_empty() { Route::Host } else { Route::Farms(keys.len()) });
        }
        Command::Code(code) => {
            if let Some(k) = verify_by_text(ctx, channel, address, code).await? {
                put(ctx, &k, msg).await?;
                return Ok(Route::Farms(1));
            }
            // This server's own person still proving the phone.
            if own.is_some_and(|u| u.phone_verified_at.is_none()) {
                return Ok(Route::Host);
            }
        }
        _ => {}
    }
    if keys.is_empty() {
        return Ok(Route::Host);
    }
    let mut farms: Vec<Farm> = keys.into_iter().map(Farm::Key).collect();
    if own.is_some_and(|u| u.phone_verified_at.is_some()) {
        farms.push(Farm::Host);
    }
    let to = match answer_pick(cmd) {
        Some((Pick::Code(code), _)) => match by_code(ctx, address, code, &farms).await? {
            Some(f) => f,
            None => newest(ctx, address, &farms).await?,
        },
        Some((_, verb)) => {
            let asking = asked(ctx, address, &farms, now()).await?;
            match asking.as_slice() {
                [one] => one.clone(),
                [] => newest(ctx, address, &farms).await?,
                _ => {
                    let reply = enqueue(
                        ctx,
                        Outbound {
                            idempotency_key: format!("reply:{}", msg.id),
                            channel: crate::inbound::reply_channel(channel).into(),
                            to: address.to_owned(),
                            text: which_farm_text(verb),
                            kind: "reply".into(),
                            user_id: own.map(|u| u.id.clone()),
                            ..Default::default()
                        },
                    )
                    .await?;
                    return Ok(Route::Asked(reply));
                }
            }
        }
        None => newest(ctx, address, &farms).await?,
    };
    match to {
        Farm::Host => Ok(Route::Host),
        Farm::Key(k) => {
            put(ctx, &k, msg).await?;
            Ok(Route::Farms(1))
        }
    }
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
