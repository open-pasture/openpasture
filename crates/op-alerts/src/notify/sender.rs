//! The sender: claims queued messages (`op_core::messages::claim`, at most 10
//! at a time and 4 in flight per channel), hands each to its channel and marks
//! it `sent`, `delivered` or `failed`. A send that may work later is queued
//! again with a backoff (Twilio, email and relay 5 s, 30 s, 2 min; webhook 1 s,
//! 5 s, 25 s). A provider this server can't reach at all (the farm's internet
//! is down) never saw the message, so that isn't counted as a try: it is
//! tried again every minute or sooner for up to [`OFFLINE_KEEP_H`] hours. An
//! alert's text that had to wait isn't sent once its alert has resolved.
//! Twilio texts are then read back at 30 s, 2 min and 10 min after
//! sending until Twilio says delivered or undelivered, so a failed delivery
//! shows without a public URL.
//!
//! One worker per channel, so a slow mail server never holds up a text. The
//! claim is one UPDATE … RETURNING, so two senders never send one message.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::join_all;
use op_core::alert::MessageLog;
use op_core::messages::{self, message_from_row};
use op_core::time::{now, to_db};
use op_core::{Ctx, Event};
use tokio::sync::{Notify, Semaphore};

use super::twilio::{Status, Twilio};
use super::{CHANNELS, Channel, ChannelError, channel, label};

/// Messages claimed at once, over every channel.
pub const MAX_CLAIMED: usize = 10;
/// Messages in flight on one channel.
pub const PER_CHANNEL: usize = 4;
/// Twilio delivery status is read this long after sending.
pub const STATUS_POLLS: [i64; 3] = [30, 120, 600];

/// When to try again after `attempts` tries on `channel`; `None`: give up.
pub fn backoff(channel: &str, attempts: u32) -> Option<chrono::Duration> {
    let steps: &[i64] = match channel {
        "webhook" => &[1, 5, 25],
        _ => &[5, 30, 120],
    };
    let i = usize::try_from(attempts).ok()?.checked_sub(1)?;
    steps.get(i).map(|s| chrono::Duration::seconds(*s))
}

/// Hours a message waits for a provider this server can't reach at all.
pub const OFFLINE_KEEP_H: i64 = 6;
/// Longest wait between two tries while it can't.
pub const OFFLINE_EVERY_S: i64 = 60;
/// A message this old has waited (a retry, an outage).
const WAITED_S: i64 = 60;

/// When to try again a message `age` old whose provider couldn't be reached:
/// soon at first, then every minute; `None` after [`OFFLINE_KEEP_H`] hours.
pub fn offline_backoff(age: chrono::Duration) -> Option<chrono::Duration> {
    (age < chrono::Duration::hours(OFFLINE_KEEP_H)).then(|| chrono::Duration::seconds(age.num_seconds().clamp(5, OFFLINE_EVERY_S)))
}

/// Why a text or email that waited isn't worth sending any more: its alert
/// resolved. (The webhook, a record for other systems, gets it anyway.)
async fn stale(ctx: &Ctx, m: &MessageLog, now: DateTime<Utc>) -> anyhow::Result<Option<&'static str>> {
    let waited = m.kind == "alert" && m.channel != "webhook" && now - m.created_at >= chrono::Duration::seconds(WAITED_S);
    let Some(alert_id) = m.alert_id.as_deref().filter(|_| waited) else {
        return Ok(None);
    };
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM alerts WHERE id = ?").bind(alert_id).fetch_optional(ctx.db()).await?;
    Ok((status.as_deref() == Some("resolved")).then_some("Resolved before it could be sent."))
}

/// One pass over every channel: claim what is due at `now` (≤ 4 per channel,
/// ≤ 10 in all), send, mark. Returns how many messages it handled.
pub async fn run_once(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<usize> {
    let mut claimed = Vec::new();
    let mut room = MAX_CLAIMED;
    for kind in CHANNELS {
        if room == 0 {
            break;
        }
        let batch = messages::claim(ctx, &[kind], now, room.min(PER_CHANNEL)).await?;
        room -= batch.len();
        if !batch.is_empty() {
            claimed.push((kind, batch));
        }
    }
    let n = claimed.iter().map(|(_, b)| b.len()).sum();
    join_all(claimed.into_iter().map(|(kind, batch)| deliver_batch(ctx, kind, batch, now))).await;
    Ok(n)
}

/// Send one channel's claimed batch at once and mark each result.
async fn deliver_batch(ctx: &Ctx, kind: &str, batch: Vec<MessageLog>, now: DateTime<Utc>) {
    let ch = match channel(ctx, kind).await {
        Ok(ch) => ch,
        Err(e) => {
            // Settings or secrets unreadable: put them back for the next pass.
            tracing::error!("reading the {kind} channel: {e:#}");
            for m in &batch {
                let _ =
                    messages::mark(ctx, &m.id, "queued", None, Some("Couldn't read the channel settings."), Some(now + chrono::Duration::seconds(30))).await;
            }
            return;
        }
    };
    let Some(ch) = ch else {
        let why = format!("{} isn't set up.", label(kind));
        for m in &batch {
            if let Err(e) = messages::mark(ctx, &m.id, "failed", None, Some(&why), None).await {
                tracing::error!("marking {}: {e:#}", m.id);
            }
        }
        return;
    };
    join_all(batch.iter().map(|m| deliver(ctx, ch.as_ref(), m, now))).await;
}

async fn deliver(ctx: &Ctx, ch: &dyn Channel, m: &MessageLog, now: DateTime<Utc>) {
    match stale(ctx, m, now).await {
        Ok(Some(why)) => {
            if let Err(e) = messages::mark(ctx, &m.id, "failed", None, Some(why), None).await {
                tracing::error!("marking {}: {e:#}", m.id);
            }
            return;
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(message = %m.id, "checking its alert: {e:#}"),
    }
    let res = ch.send(m).await;
    let marked = match res {
        Ok(d) => {
            // A Twilio text is read back later; everything else is done.
            let poll = (matches!(m.channel.as_str(), "sms" | "whatsapp") && d.status == "sent" && d.provider_id.is_some())
                .then(|| op_core::time::now() + chrono::Duration::seconds(STATUS_POLLS[0]));
            messages::mark(ctx, &m.id, &d.status, d.provider_id.as_deref(), None, poll).await
        }
        Err(ChannelError::Retry(why)) => match backoff(&m.channel, m.attempts) {
            Some(wait) => messages::mark(ctx, &m.id, "queued", None, Some(&why), Some(now + wait)).await,
            None => messages::mark(ctx, &m.id, "failed", None, Some(&format!("{why} Gave up after {} tries.", m.attempts)), None).await,
        },
        Err(ChannelError::Offline(why)) => match offline_backoff(now - m.created_at) {
            Some(wait) => {
                // The provider never saw it: not one of its tries.
                let _ = sqlx::query("UPDATE messages SET attempts = MAX(attempts - 1, 0) WHERE id = ?").bind(&m.id).execute(ctx.db()).await;
                messages::mark(ctx, &m.id, "queued", None, Some(&why), Some(now + wait)).await
            }
            None => messages::mark(ctx, &m.id, "failed", None, Some(&format!("{why} Gave up after {OFFLINE_KEEP_H} h.")), None).await,
        },
        Err(ChannelError::Fail(why)) => messages::mark(ctx, &m.id, "failed", None, Some(&why), None).await,
    };
    if let Err(e) = marked {
        tracing::error!("marking {}: {e:#}", m.id);
    }
}

/// Read back Twilio texts whose next status check is due at `now`. Returns how
/// many it checked.
pub async fn poll_statuses(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<usize> {
    let rows = sqlx::query(
        "SELECT * FROM messages WHERE status = 'sent' AND next_attempt_at IS NOT NULL AND next_attempt_at <= ?
           AND channel IN ('sms', 'whatsapp') AND provider_id IS NOT NULL
         ORDER BY next_attempt_at LIMIT 20",
    )
    .bind(to_db(&now))
    .fetch_all(ctx.db())
    .await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let cfg = super::load(ctx).await?;
    let mut twilio: HashMap<String, Option<Twilio>> = HashMap::new();
    for r in &rows {
        let m = message_from_row(r)?;
        if !twilio.contains_key(&m.channel) {
            twilio.insert(m.channel.clone(), Twilio::from_config(ctx, &cfg, &m.channel)?);
        }
        let Some(tw) = twilio.get(&m.channel).and_then(Option::as_ref) else {
            // Twilio was removed since: nothing to ask.
            next_poll(ctx, &m, None).await?;
            continue;
        };
        let sid = m.provider_id.as_deref().unwrap_or_default();
        match tw.status(sid).await {
            Ok(Status::Delivered) => messages::mark(ctx, &m.id, "delivered", None, None, None).await?,
            Ok(Status::Failed(why)) => messages::mark(ctx, &m.id, "failed", None, Some(&why), None).await?,
            Ok(Status::Pending(_)) | Err(ChannelError::Retry(_) | ChannelError::Offline(_)) => next_poll(ctx, &m, Some(now)).await?,
            Err(ChannelError::Fail(why)) => {
                tracing::warn!(message = %m.id, "Twilio status check refused: {why}");
                next_poll(ctx, &m, None).await?;
            }
        }
    }
    Ok(rows.len())
}

/// The next status check after `now` (from the send time, `updated_at`), or
/// none. Not published: nothing a person sees changed.
async fn next_poll(ctx: &Ctx, m: &MessageLog, now: Option<DateTime<Utc>>) -> anyhow::Result<()> {
    let next = now.and_then(|now| STATUS_POLLS.iter().map(|s| m.updated_at + chrono::Duration::seconds(*s)).find(|t| *t > now));
    sqlx::query("UPDATE messages SET next_attempt_at = ? WHERE id = ? AND status = 'sent'")
        .bind(next.as_ref().map(to_db))
        .bind(&m.id)
        .execute(ctx.db())
        .await?;
    Ok(())
}

/// Messages left `sending` by a server that stopped mid-send go back in the
/// queue. Run once at start, before any worker.
pub async fn requeue_stranded(ctx: &Ctx) -> anyhow::Result<u64> {
    Ok(sqlx::query("UPDATE messages SET status = 'queued', updated_at = ? WHERE status = 'sending' AND direction = 'out'")
        .bind(to_db(&now()))
        .execute(ctx.db())
        .await?
        .rows_affected())
}

/// Start the workers (one per channel), the status poller and the wake-up
/// listener. Returns at once.
pub async fn spawn(ctx: Ctx) -> anyhow::Result<()> {
    let stranded = requeue_stranded(&ctx).await?;
    if stranded > 0 {
        tracing::info!(stranded, "requeued messages a stopped server left sending");
    }
    let global = Arc::new(Semaphore::new(MAX_CLAIMED));
    let mut wakes: HashMap<&'static str, Arc<Notify>> = HashMap::new();
    for kind in CHANNELS {
        let wake = Arc::new(Notify::new());
        wakes.insert(kind, wake.clone());
        tokio::spawn(worker(ctx.clone(), kind, wake, global.clone()));
    }
    // A newly queued message wakes its channel's worker at once.
    let mut rx = ctx.subscribe();
    let c = ctx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = c.on_shutdown() => break,
                ev = rx.recv() => match ev {
                    Ok(Event::Message { message }) if message.direction == "out" && message.status == "queued" => {
                        if let Some(w) = wakes.get(message.channel.as_str()) {
                            w.notify_one();
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => wakes.values().for_each(|w| w.notify_one()),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
    let c = ctx.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = c.on_shutdown() => break,
                _ = tick.tick() => {
                    if let Err(e) = poll_statuses(&c, now()).await {
                        tracing::warn!("reading Twilio delivery status: {e:#}");
                    }
                }
            }
        }
    });
    Ok(())
}

/// One channel: claim up to 4 (taking room from the shared 10), send, repeat;
/// when nothing is due, wait for a wake-up or the next second.
async fn worker(ctx: Ctx, kind: &'static str, wake: Arc<Notify>, global: Arc<Semaphore>) {
    loop {
        if ctx.is_shutting_down() {
            break;
        }
        let Ok(first) = global.clone().acquire_owned().await else { break };
        let mut permits = vec![first];
        while permits.len() < PER_CHANNEL {
            match global.clone().try_acquire_owned() {
                Ok(p) => permits.push(p),
                Err(_) => break,
            }
        }
        let t = now();
        let handled = match messages::claim(&ctx, &[kind], t, permits.len()).await {
            Ok(batch) if batch.is_empty() => 0,
            Ok(batch) => {
                let n = batch.len();
                permits.truncate(n);
                deliver_batch(&ctx, kind, batch, t).await;
                n
            }
            Err(e) => {
                tracing::error!("claiming {kind} messages: {e:#}");
                0
            }
        };
        drop(permits);
        if handled == 0 {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_steps_per_channel() {
        let secs = |c: &str| (1..=4).map(|a| backoff(c, a).map(|d| d.num_seconds())).collect::<Vec<_>>();
        assert_eq!(secs("sms"), vec![Some(5), Some(30), Some(120), None]);
        assert_eq!(secs("whatsapp"), vec![Some(5), Some(30), Some(120), None]);
        assert_eq!(secs("webhook"), vec![Some(1), Some(5), Some(25), None]);
        assert_eq!(backoff("sms", 0), None);
    }

    #[test]
    fn offline_waits_up_to_a_minute_for_six_hours() {
        let at = |s: i64| offline_backoff(chrono::Duration::seconds(s)).map(|d| d.num_seconds());
        assert_eq!([at(0), at(20), at(90), at(5 * 3600)], [Some(5), Some(20), Some(60), Some(60)]);
        assert_eq!(at(6 * 3600), None);
    }
}
