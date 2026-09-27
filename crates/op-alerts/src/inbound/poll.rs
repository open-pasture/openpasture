//! Without a public URL (a farm behind NAT) Twilio can't reach this server,
//! so it reads Twilio's message list instead, every `texting.poll_s` (10 s):
//! `GET {twilio_api_base}/2010-04-01/Accounts/{sid}/Messages.json?To=<our
//! number>&DateSent>=<yesterday>`, one list per Twilio channel (SMS, and
//! WhatsApp with `whatsapp:` numbers). Each inbound text is taken once, by
//! its `MessageSid` (`messages.provider_id`). Texts sent before checking
//! began for a number are never acted on (the first check isn't a replay
//! of yesterday). A Messaging Service sender (`MG…`) has no number to
//! filter by, so its list is read whole and only inbound texts count.

use std::time::Duration;

use chrono::{DateTime, Utc};
use op_core::Ctx;
use op_core::messages::Inbound;
use op_core::time::now;
use serde::Deserialize;

use super::{Mode, state};
use crate::notify::{self, ChannelError, http, secret, set};

/// Pages of Twilio's list read per check (newest first).
const MAX_PAGES: usize = 5;

#[derive(Debug, Deserialize)]
struct Page {
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default)]
    next_page_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Message {
    sid: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    date_sent: Option<String>,
    #[serde(default)]
    date_created: Option<String>,
}

/// Twilio's dates are RFC 2822 ("Sat, 27 Sep 2026 12:00:00 +0000").
fn twilio_time(s: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(s?.trim()).ok().map(|t| t.with_timezone(&Utc))
}

/// One check of every Twilio number when polling is the mode. Returns how
/// many new texts were taken.
pub async fn run_once(ctx: &Ctx, at: DateTime<Utc>) -> anyhow::Result<usize> {
    if super::mode(ctx).await? != Mode::Polling {
        return Ok(0);
    }
    let cfg = notify::load(ctx).await?;
    let (Some(sid), Some(token)) = (secret(ctx, "twilio_account_sid")?, secret(ctx, "twilio_auth_token")?) else { return Ok(0) };
    let base = cfg.twilio_api_base.trim().trim_end_matches('/').to_owned();
    let mut taken = 0;
    for channel in super::twilio_channels(ctx).await? {
        let from = match channel {
            "whatsapp" => set(&cfg.whatsapp.from),
            _ => set(&cfg.sms.from),
        };
        let Some(from) = from else { continue };
        let key = format!("poll:{channel}");
        match check(ctx, channel, &base, &sid, &token, from, &key, at).await {
            Ok(n) => taken += n,
            Err(e) => {
                tracing::warn!("checking Twilio for {channel} texts: {e}");
                state::failed(ctx, &key, &e.to_string()).await?;
            }
        }
    }
    Ok(taken)
}

#[allow(clippy::too_many_arguments)]
async fn check(ctx: &Ctx, channel: &str, base: &str, sid: &str, token: &str, from: &str, key: &str, at: DateTime<Utc>) -> anyhow::Result<usize> {
    // Checking begins now for a number seen for the first time (or a new one).
    let st = state::get(ctx, key).await?;
    let since = match &st {
        Some(s) if s.cursor.as_deref() == Some(from) && s.since.is_some() => s.since.unwrap_or(at),
        _ => at,
    };
    let to = match (channel, from.starts_with("MG")) {
        (_, true) => None,
        ("whatsapp", _) => Some(format!("whatsapp:{}", from.trim_start_matches("whatsapp:"))),
        _ => Some(from.to_owned()),
    };
    let yesterday = (at - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
    let mut url = format!("{base}/2010-04-01/Accounts/{sid}/Messages.json");
    let mut query: Vec<(&str, String)> = vec![("DateSent>", yesterday), ("PageSize", "100".into())];
    if let Some(to) = &to {
        query.insert(0, ("To", to.clone()));
    }
    let mut found: Vec<Message> = Vec::new();
    for page in 0..MAX_PAGES {
        let mut req = http().get(&url).basic_auth(sid, Some(token));
        if page == 0 {
            req = req.query(&query);
        }
        let res = req.send().await.map_err(|e| anyhow::anyhow!(notify::net_error("Twilio", &e).to_string()))?;
        let status = res.status();
        let body = res.bytes().await.map_err(|e| anyhow::anyhow!(notify::net_error("Twilio", &e).to_string()))?;
        if !status.is_success() {
            let e: ChannelError = notify::twilio::error_for(status.as_u16(), &body);
            anyhow::bail!("{e}");
        }
        let p: Page = serde_json::from_slice(&body).map_err(|_| anyhow::anyhow!("Twilio sent a message list that can't be read."))?;
        found.extend(p.messages);
        match p.next_page_uri.filter(|u| !u.trim().is_empty()) {
            Some(next) => url = format!("{base}{next}"),
            None => break,
        }
    }
    // Oldest first, so commands run in the order they were sent.
    let mut todo: Vec<(DateTime<Utc>, Message)> = found
        .into_iter()
        .filter(|m| m.direction.as_deref().is_none_or(|d| d == "inbound"))
        .filter_map(|m| {
            let t = twilio_time(m.date_sent.as_deref()).or_else(|| twilio_time(m.date_created.as_deref()))?;
            (t >= since).then_some((t, m))
        })
        .collect();
    todo.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.sid.cmp(&b.1.sid)));
    let mut taken = 0;
    for (t, m) in todo {
        let seen: Option<(String,)> =
            sqlx::query_as("SELECT id FROM messages WHERE channel = ? AND provider_id = ?").bind(channel).bind(&m.sid).fetch_optional(ctx.db()).await?;
        if seen.is_some() {
            continue;
        }
        let Some(from) = m.from.filter(|f| !f.trim().is_empty()) else { continue };
        let inbound = Inbound { channel: channel.to_owned(), from, text: m.body.unwrap_or_default(), provider_id: Some(m.sid), at: t };
        if super::receive(ctx, inbound).await?.is_some() {
            taken += 1;
        }
    }
    state::ok(ctx, key, Some(from), Some(since)).await?;
    Ok(taken)
}

/// Check every `poll_s` seconds while polling is the mode.
pub fn spawn(ctx: Ctx) {
    tokio::spawn(async move {
        loop {
            let wait = match super::load(&ctx).await {
                Ok(c) => c.poll_s.clamp(5, 300),
                Err(_) => 10,
            };
            if let Err(e) = run_once(&ctx, now()).await {
                tracing::warn!("checking Twilio for texts: {e:#}");
            }
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tokio::time::sleep(Duration::from_secs(wait as u64)) => {}
            }
        }
    });
}
