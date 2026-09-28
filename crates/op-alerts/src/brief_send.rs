//! The morning brief by text: at `texting.brief.time` farm time, while
//! `texting.brief.enabled`, everyone with the brief on (`alert_prefs.brief`)
//! gets each of their herds' brief (op-engine's `brief.rs` text: GSM-7, at
//! most 480 characters) on every way their alerts reach them: a verified
//! phone that hasn't texted STOP, or email. Once per farm day; a server that
//! was down at brief time sends it when it comes back within two hours, not
//! later. The time is taken per day in the farm's zone, so 06:30 stays 06:30
//! across a clock change (a time the clocks skip sends an hour later).

use std::time::Duration;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use op_core::alert::MessageLog;
use op_core::messages::{Outbound, enqueue};
use op_core::time::now;
use op_core::{Ctx, Herd};
use sha2::Digest;

use crate::inbound::{self, state};
use crate::routing;

/// State key: `cursor` = the farm date last sent.
pub const KEY: &str = "brief";
/// A brief that couldn't go at its time still goes this long after.
pub const LATE_MAX: chrono::Duration = chrono::Duration::hours(2);

/// When the brief of farm day `day` is due at `hhmm` in `tz`.
pub fn due_at(day: NaiveDate, hhmm: chrono::NaiveTime, tz: Tz) -> Option<DateTime<Utc>> {
    let local = day.and_time(hhmm);
    tz.from_local_datetime(&local)
        .earliest()
        .or_else(|| tz.from_local_datetime(&(local + chrono::Duration::hours(1))).earliest())
        .map(|t| t.with_timezone(&Utc))
}

/// Whether someone gets the brief by text.
pub async fn brief_on(ctx: &Ctx, user_id: &str) -> anyhow::Result<bool> {
    let v: Option<(bool,)> = sqlx::query_as("SELECT brief FROM alert_prefs WHERE user_id = ?").bind(user_id).fetch_optional(ctx.db()).await?;
    Ok(v.is_some_and(|(b,)| b))
}

/// Turn someone's brief on or off.
pub async fn set_brief(ctx: &Ctx, user_id: &str, on: bool) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO alert_prefs (user_id, brief, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(user_id) DO UPDATE SET brief = excluded.brief, updated_at = excluded.updated_at",
    )
    .bind(user_id)
    .bind(on)
    .bind(op_core::time::to_db(&now()))
    .execute(ctx.db())
    .await?;
    Ok(())
}

fn short_hash(s: &str) -> String {
    hex::encode(&sha2::Sha256::digest(s.as_bytes())[..4])
}

/// Send today's brief if it is due at `at` and hasn't gone. Returns what was queued.
pub async fn run_once(ctx: &Ctx, at: DateTime<Utc>) -> anyhow::Result<Vec<MessageLog>> {
    let cfg = inbound::load(ctx).await?;
    if !cfg.brief.enabled {
        return Ok(vec![]);
    }
    let Some(time) = inbound::hhmm(&cfg.brief.time) else { return Ok(vec![]) };
    let tz: Tz = ctx.store().get_farm().await?.and_then(|f| f.timezone.parse().ok()).unwrap_or(Tz::UTC);
    let day = at.with_timezone(&tz).date_naive();
    let Some(due) = due_at(day, time, tz) else { return Ok(vec![]) };
    if at < due || at - due > LATE_MAX {
        return Ok(vec![]);
    }
    let day_s = day.format("%Y-%m-%d").to_string();
    if state::get(ctx, KEY).await?.and_then(|s| s.cursor).as_deref() == Some(day_s.as_str()) {
        return Ok(vec![]);
    }
    let out = send(ctx, &day_s, at).await?;
    state::ok(ctx, KEY, Some(&day_s), None).await?;
    Ok(out)
}

/// Queue the brief for every person who has it on (once per day, person,
/// herd and address).
async fn send(ctx: &Ctx, day: &str, at: DateTime<Utc>) -> anyhow::Result<Vec<MessageLog>> {
    let configured = crate::notify::alert_channels(ctx).await?;
    let herds: Vec<Herd> = ctx.store().list_herds().await?;
    // Per herd: the text for those who answer decisions (with the decision it asks about), and for
    // everyone else (asking nothing: only managers and up answer).
    let mut briefs: Vec<(String, String, Option<String>, String)> = Vec::new();
    for h in &herds {
        let asking = op_engine::brief::brief_for(ctx, h, at, true).await?.text;
        let telling = op_engine::brief::brief_for(ctx, h, at, false).await?.text;
        briefs.push((h.id.clone(), asking, asks_about(ctx, h, at).await?, telling));
    }
    let mut out = Vec::new();
    for p in routing::people(ctx).await? {
        if !brief_on(ctx, &p.user.id).await? {
            continue;
        }
        let ways = routing::deliveries(&p, &configured);
        let answers = p.user.role >= op_core::Role::Manager;
        for (herd_id, asking, decision_id, telling) in &briefs {
            let (text, decision_id) = if answers { (asking, decision_id.clone()) } else { (telling, None) };
            if p.prefs.herds.as_ref().is_some_and(|hs| !hs.contains(herd_id)) {
                continue;
            }
            let herd = herds.iter().find(|h| &h.id == herd_id).map(|h| h.name.clone()).unwrap_or_default();
            for (channel, address) in &ways {
                let email = *channel == "email" || (*channel == "relay" && address.contains('@'));
                // A WhatsApp template variable can't hold line breaks.
                let body = if *channel == "whatsapp" { text.replace('\n', " ") } else { text.clone() };
                out.push(
                    enqueue(
                        ctx,
                        Outbound {
                            idempotency_key: format!("brief:{day}:{}:{herd_id}:{channel}:{}", p.user.id, short_hash(address)),
                            channel: (*channel).to_owned(),
                            to: address.clone(),
                            text: body,
                            subject: email.then(|| format!("{herd}: morning brief")),
                            kind: "brief".into(),
                            alert_id: None,
                            decision_id: decision_id.clone(),
                            user_id: Some(p.user.id.clone()),
                        },
                    )
                    .await?,
                );
            }
        }
    }
    Ok(out)
}

/// The decision a herd's brief asks about ("Reply Y or N.", "Sends 07:40
/// unless you reply N."), as op-engine's brief picks it: the herd's newest
/// decision since the decision window began, not superseded, while it is a
/// proposed MOVE or HOLD. A bare Y or N to the brief answers that one.
async fn asks_about(ctx: &Ctx, herd: &Herd, at: DateTime<Utc>) -> anyhow::Result<Option<String>> {
    let settings = ctx.settings().await?;
    let tz: Tz = ctx.store().get_farm().await?.and_then(|f| f.timezone.parse().ok()).unwrap_or(Tz::UTC);
    let since = op_engine::brief::window_start(at, tz, &settings.decision_time);
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, status, action FROM decisions WHERE herd_id = ? AND created_at >= ? AND status != 'superseded' ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(&herd.id)
    .bind(op_core::time::to_db(&since))
    .fetch_optional(ctx.db())
    .await?;
    Ok(row.filter(|(_, status, action)| status == "proposed" && matches!(action.as_deref(), Some("MOVE" | "HOLD"))).map(|(id, ..)| id))
}

/// Check every 20 s.
pub fn start(ctx: Ctx) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(20));
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tick.tick() => {
                    if let Err(e) = run_once(&ctx, now()).await {
                        tracing::warn!("morning brief: {e:#}");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_time_is_farm_time_on_both_sides_of_a_clock_change() {
        let tz: Tz = "America/Chicago".parse().unwrap();
        let t = chrono::NaiveTime::from_hms_opt(6, 30, 0).unwrap();
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(due_at(d("2026-10-31"), t, tz).unwrap().to_rfc3339(), "2026-10-31T11:30:00+00:00");
        assert_eq!(due_at(d("2026-11-01"), t, tz).unwrap().to_rfc3339(), "2026-11-01T12:30:00+00:00");
        // 02:30 doesn't exist on 2027-03-14: an hour later.
        let skipped = chrono::NaiveTime::from_hms_opt(2, 30, 0).unwrap();
        assert_eq!(due_at(d("2027-03-14"), skipped, tz).unwrap().to_rfc3339(), "2027-03-14T08:30:00+00:00");
    }
}
