//! Who hears about an alert, and when. Routing only queues messages
//! (`op_core::messages::enqueue`); the sender delivers them.
//!
//! - A person matches an alert when its severity is at least theirs, its herd
//!   is one of theirs and its kind isn't muted, and they can be reached over a
//!   configured channel: sms and whatsapp only to a verified phone that hasn't
//!   texted STOP; sms goes through the relay when the farm has no Twilio of
//!   its own; email only through the farm's own SMTP (the relay can't prove
//!   an address is the person's); whatsapp only with an approved template
//!   (`notify::alert_channels`).
//! - When anyone matching is on duty, the first send goes only to them.
//! - Warnings wait `group_window_s` and go out as one text for every alert of
//!   that kind in that herd opened in the window; critical alerts wait only
//!   `critical_window_s` (so a breakout is one rollup text). Info never pushes.
//! - Quiet hours (the person's, else the farm's, farm time) hold a send until
//!   they end; critical passes unless the person turned that off.
//! - Unacked critical alerts are sent again every `renotify_every_min` (up to
//!   `renotify_max` times) and escalate every `escalate_after_min` to matching
//!   people of the next role up not yet notified (hand → manager → owner).
//! - The farm webhook gets every notified alert.

pub mod prefs;

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, NaiveTime, Utc};
use chrono_tz::Tz;
use op_core::alert::Alert;
use op_core::messages::{Outbound, enqueue};
use op_core::time::to_db;
use op_core::users::User;
use op_core::{Ctx, Role, Severity};
use serde_json::Value;
use sqlx::Row as _;

use crate::engine::config::{Policy, policy};
use crate::engine::store::{self, Row};
use crate::text::{self, TextCtx};
pub use prefs::{PERSON_CHANNELS, Prefs};

/// A person who could get alerts.
#[derive(Debug, Clone)]
pub struct Person {
    pub user: User,
    pub prefs: Prefs,
    pub sms_opt_out: bool,
}

/// Enabled people with their prefs.
pub async fn people(ctx: &Ctx) -> anyhow::Result<Vec<Person>> {
    let mut out = Vec::new();
    for u in op_core::users::list_users(ctx).await?.into_iter().filter(|u| u.disabled_at.is_none()) {
        let (prefs, sms_opt_out) = prefs::get(ctx, &u.id).await?;
        out.push(Person { user: u, prefs, sms_opt_out });
    }
    Ok(out)
}

/// Channels a person may pick given what the farm has set up
/// (`notify::alert_channels`): sms also through the relay.
pub fn person_channels(configured: &[&str]) -> Vec<&'static str> {
    let relay = configured.contains(&"relay");
    PERSON_CHANNELS
        .into_iter()
        .filter(|c| match *c {
            "sms" => configured.contains(c) || relay,
            other => configured.contains(&other),
        })
        .collect()
}

/// Where a person's messages go now: `(channel, address)` pairs.
pub fn deliveries(p: &Person, configured: &[&str]) -> Vec<(&'static str, String)> {
    let phone = p.user.phone.clone().filter(|_| p.user.phone_verified_at.is_some() && !p.sms_opt_out);
    let relay = configured.contains(&"relay");
    let mut out: Vec<(&'static str, String)> = Vec::new();
    for c in &p.prefs.channels {
        let pick = match c.as_str() {
            "sms" => phone.clone().and_then(|ph| if configured.contains(&"sms") { Some(("sms", ph)) } else { relay.then_some(("relay", ph)) }),
            "whatsapp" => phone.clone().filter(|_| configured.contains(&"whatsapp")).map(|ph| ("whatsapp", ph)),
            "email" => p.user.email.clone().filter(|_| configured.contains(&"email")).map(|e| ("email", e)),
            _ => None,
        };
        if let Some(d) = pick
            && !out.contains(&d)
        {
            out.push(d);
        }
    }
    out
}

/// Whether a person wants this alert at all.
pub fn matches(p: &Person, severity: Severity, herd_id: Option<&str>, kind: &str) -> bool {
    severity >= p.prefs.min_severity
        && severity >= Severity::Warning
        && !p.prefs.muted_kinds.iter().any(|k| k == kind)
        && match (&p.prefs.herds, herd_id) {
            (None, _) => true,
            (Some(hs), Some(h)) => hs.iter().any(|x| x == h),
            (Some(_), None) => true,
        }
}

fn hhmm(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M").ok()
}

/// Within the person's quiet hours (theirs, else the farm's) at `now`.
pub fn in_quiet(p: &Person, policy: &Policy, tz: Tz, now: DateTime<Utc>) -> bool {
    let (start, end) = match (&p.prefs.quiet_start, &p.prefs.quiet_end) {
        (Some(a), Some(b)) => (a.as_str(), b.as_str()),
        _ => match (&policy.quiet_start, &policy.quiet_end) {
            (Some(a), Some(b)) => (a.as_str(), b.as_str()),
            _ => return false,
        },
    };
    let (Some(a), Some(b)) = (hhmm(start), hhmm(end)) else { return false };
    let t = now.with_timezone(&tz).time();
    if a == b {
        false
    } else if a < b {
        t >= a && t < b
    } else {
        t >= a || t < b
    }
}

/// A send to this person waits: quiet hours, unless critical and they let critical through.
pub fn holds(p: &Person, severity: Severity, policy: &Policy, tz: Tz, now: DateTime<Utc>) -> bool {
    in_quiet(p, policy, tz, now) && !(severity == Severity::Critical && p.prefs.critical_in_quiet)
}

/// What one routing pass sent.
#[derive(Debug, Default)]
pub struct Report {
    pub messages: Vec<op_core::alert::MessageLog>,
}

struct Farm {
    tz: Tz,
    policy: Policy,
    configured: Vec<&'static str>,
    webhook_url: Option<String>,
    tctx: TextCtx,
}

/// Queue everything due at `now`: first sends of batches whose window
/// closed, re-notifications and escalations.
pub async fn route(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<Report> {
    let mut report = Report::default();
    let due = sqlx::query("SELECT * FROM alerts WHERE status = 'open' AND notify = 1 AND routed_at IS NULL AND batch_at <= ? ORDER BY batch_at, opened_at, id")
        .bind(to_db(&now))
        .fetch_all(ctx.db())
        .await?
        .iter()
        .map(store::row_from)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let follow =
        sqlx::query("SELECT * FROM alerts WHERE status = 'open' AND notify = 1 AND severity = 'critical' AND escalated_at IS NOT NULL ORDER BY opened_at, id")
            .fetch_all(ctx.db())
            .await?
            .iter()
            .map(store::row_from)
            .collect::<anyhow::Result<Vec<_>>>()?;
    if due.is_empty() && follow.is_empty() {
        return Ok(report);
    }
    let farm = farm(ctx, now).await?;
    let people = people(ctx).await?;

    // First sends, one batch at a time.
    let mut batches: BTreeMap<(String, String, String), Vec<Row>> = BTreeMap::new();
    for r in due {
        let k = (r.alert.kind.clone(), r.alert.herd_id.clone().unwrap_or_default(), r.batch_at.map(|t| to_db(&t)).unwrap_or_default());
        batches.entry(k).or_default().push(r);
    }
    for rows in batches.into_values() {
        first_send(ctx, &farm, &people, &rows, now, &mut report).await?;
    }

    // Critical follow-ups, per batch (alerts that went out together).
    let mut groups: BTreeMap<(String, String, String), Vec<Row>> = BTreeMap::new();
    for r in follow {
        let k = (r.alert.kind.clone(), r.alert.herd_id.clone().unwrap_or_default(), r.escalated_at.map(|t| to_db(&t)).unwrap_or_default());
        groups.entry(k).or_default().push(r);
    }
    for rows in groups.into_values() {
        renotify(ctx, &farm, &people, &rows, now, &mut report).await?;
        escalate(ctx, &farm, &people, &rows, now, &mut report).await?;
    }
    Ok(report)
}

async fn farm(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<Farm> {
    let tctx = TextCtx::of(ctx, now).await?;
    let configured = crate::notify::alert_channels(ctx).await?;
    let webhook_url = if configured.contains(&"webhook") {
        ctx.store()
            .get_setting_json(op_core::notify_config::CHANNELS_KEY)
            .await?
            .and_then(|v| v.pointer("/webhook/url").and_then(Value::as_str).map(str::to_owned))
    } else {
        None
    };
    Ok(Farm { tz: tctx.tz, policy: policy(ctx).await?, configured, webhook_url, tctx })
}

fn severity_of(rows: &[Row]) -> Severity {
    rows.iter().map(|r| r.alert.severity).max().unwrap_or(Severity::Warning)
}

/// Where a single alert is, in words; nothing for a group.
async fn place_of(ctx: &Ctx, alerts: &[&Alert]) -> anyhow::Result<Option<String>> {
    match alerts {
        [one] => match one.at {
            Some(at) => op_core::place::describe(ctx, at).await,
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

fn decision_of(a: &Alert) -> Option<String> {
    a.targets.iter().find(|(k, _)| k == "decision").map(|(_, id)| id.clone())
}

/// Who was already told about which alert.
async fn notified(ctx: &Ctx, ids: &[String]) -> anyhow::Result<Vec<(String, Option<String>, String, u32)>> {
    let mut out = Vec::new();
    for id in ids {
        let rows = sqlx::query("SELECT alert_id, user_id, channel, tier FROM alert_notifications WHERE alert_id = ?").bind(id).fetch_all(ctx.db()).await?;
        for r in rows {
            out.push((r.try_get(0)?, r.try_get(1)?, r.try_get(2)?, r.try_get::<i64, _>(3)?.max(0) as u32));
        }
    }
    Ok(out)
}

/// Queue one text (or email) for these alerts to one person on every channel
/// they're reachable on, and record it against each alert.
#[allow(clippy::too_many_arguments)]
async fn send(ctx: &Ctx, farm: &Farm, p: &Person, alerts: &[&Alert], tier: u32, tag: &str, now: DateTime<Utc>, report: &mut Report) -> anyhow::Result<bool> {
    let ways = deliveries(p, &farm.configured);
    if ways.is_empty() || alerts.is_empty() {
        return Ok(false);
    }
    let owned: Vec<Alert> = alerts.iter().map(|a| (*a).clone()).collect();
    let place = place_of(ctx, alerts).await?;
    let text = text::group_text(&owned, place.as_deref(), &farm.tctx);
    for (channel, address) in ways {
        let email = channel == "email" || (channel == "relay" && address.contains('@'));
        let m = enqueue(
            ctx,
            Outbound {
                idempotency_key: format!("alert:{}:{}:{channel}:{}:{tag}", alerts[0].id, p.user.id, short_hash(&address)),
                channel: channel.to_owned(),
                to: address,
                text: text.clone(),
                subject: email.then(|| text::subject(&owned)),
                kind: "alert".into(),
                alert_id: Some(alerts[0].id.clone()),
                decision_id: decision_of(alerts[0]),
                user_id: Some(p.user.id.clone()),
            },
        )
        .await?;
        for a in alerts {
            record(ctx, &a.id, Some(&p.user.id), channel, tier, now, &m.id).await?;
        }
        report.messages.push(m);
    }
    Ok(true)
}

/// Eight hex digits of an address's SHA-256, so one person's two addresses on
/// one channel (the relay: phone and email) get their own idempotency keys.
fn short_hash(s: &str) -> String {
    use sha2::Digest;
    hex::encode(&sha2::Sha256::digest(s.as_bytes())[..4])
}

async fn record(ctx: &Ctx, alert_id: &str, user_id: Option<&str>, channel: &str, tier: u32, now: DateTime<Utc>, message_id: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO alert_notifications (alert_id, user_id, channel, tier, sent_at, message_id) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(alert_id)
        .bind(user_id)
        .bind(channel)
        .bind(tier as i64)
        .bind(to_db(&now))
        .bind(message_id)
        .execute(ctx.db())
        .await?;
    Ok(())
}

async fn first_send(ctx: &Ctx, farm: &Farm, people: &[Person], rows: &[Row], now: DateTime<Utc>, report: &mut Report) -> anyhow::Result<()> {
    let head = &rows[0].alert;
    let severity = severity_of(rows);
    let ids: Vec<String> = rows.iter().map(|r| r.alert.id.clone()).collect();
    let told: HashSet<(String, String)> = notified(ctx, &ids).await?.into_iter().filter_map(|(a, u, _, _)| Some((a, u?))).collect();
    let matching: Vec<&Person> =
        people.iter().filter(|p| matches(p, severity, head.herd_id.as_deref(), &head.kind) && !deliveries(p, &farm.configured).is_empty()).collect();
    let on_duty: Vec<&Person> = matching.iter().copied().filter(|p| p.prefs.on_duty).collect();
    let first = if on_duty.is_empty() { matching } else { on_duty };
    let mut held = false;
    let mut sent = false;
    for p in first {
        let todo: Vec<&Alert> = rows.iter().map(|r| &r.alert).filter(|a| !told.contains(&(a.id.clone(), p.user.id.clone()))).collect();
        if todo.is_empty() {
            continue;
        }
        if holds(p, severity, &farm.policy, farm.tz, now) {
            held = true;
            continue;
        }
        sent |= send(ctx, farm, p, &todo, 0, "0", now, report).await?;
    }
    // The farm webhook: every notified alert, once.
    if let Some(url) = &farm.webhook_url {
        let hooked: HashSet<String> = notified(ctx, &ids).await?.into_iter().filter(|(_, u, c, _)| u.is_none() && c == "webhook").map(|(a, ..)| a).collect();
        for r in rows.iter().filter(|r| !hooked.contains(&r.alert.id)) {
            let m = enqueue(
                ctx,
                Outbound {
                    idempotency_key: format!("alert:{}:webhook", r.alert.id),
                    channel: "webhook".into(),
                    to: url.clone(),
                    text: r.alert.title.clone(),
                    subject: None,
                    kind: "alert".into(),
                    alert_id: Some(r.alert.id.clone()),
                    decision_id: decision_of(&r.alert),
                    user_id: None,
                },
            )
            .await?;
            record(ctx, &r.alert.id, None, "webhook", 0, now, &m.id).await?;
            report.messages.push(m);
        }
    }
    for id in &ids {
        sqlx::query(
            "UPDATE alerts SET routed_at = CASE WHEN ?1 THEN routed_at ELSE ?2 END,
                 escalated_at = CASE WHEN ?3 THEN COALESCE(escalated_at, ?2) ELSE escalated_at END,
                 renotified_at = CASE WHEN ?3 THEN COALESCE(renotified_at, ?2) ELSE renotified_at END
             WHERE id = ?4",
        )
        .bind(held)
        .bind(to_db(&now))
        .bind(sent)
        .bind(id)
        .execute(ctx.db())
        .await?;
    }
    Ok(())
}

async fn renotify(ctx: &Ctx, farm: &Farm, people: &[Person], rows: &[Row], now: DateTime<Utc>, report: &mut Report) -> anyhow::Result<()> {
    let p = &farm.policy;
    let due: Vec<&Row> = rows
        .iter()
        .filter(|r| r.renotified < p.renotify_max && r.renotified_at.is_some_and(|t| now - t >= Duration::minutes(p.renotify_every_min as i64)))
        .collect();
    if due.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = due.iter().map(|r| r.alert.id.clone()).collect();
    // Per person: the alerts they were told about, and at which tier.
    let mut by_user: HashMap<String, (u32, Vec<String>)> = HashMap::new();
    for (a, u, _, tier) in notified(ctx, &ids).await? {
        if let Some(u) = u {
            let e = by_user.entry(u).or_insert((tier, vec![]));
            e.0 = e.0.min(tier);
            if !e.1.contains(&a) {
                e.1.push(a);
            }
        }
    }
    let round = due.iter().map(|r| r.renotified).max().unwrap_or(0) + 1;
    for person in people {
        let Some((tier, alert_ids)) = by_user.get(&person.user.id) else { continue };
        if holds(person, Severity::Critical, &farm.policy, farm.tz, now) {
            continue;
        }
        let alerts: Vec<&Alert> = due.iter().map(|r| &r.alert).filter(|a| alert_ids.contains(&a.id)).collect();
        send(ctx, farm, person, &alerts, *tier, &format!("r{round}"), now, report).await?;
    }
    for id in &ids {
        sqlx::query("UPDATE alerts SET renotified = renotified + 1, renotified_at = ? WHERE id = ?").bind(to_db(&now)).bind(id).execute(ctx.db()).await?;
    }
    Ok(())
}

async fn escalate(ctx: &Ctx, farm: &Farm, people: &[Person], rows: &[Row], now: DateTime<Utc>, report: &mut Report) -> anyhow::Result<()> {
    let due: Vec<&Row> = rows.iter().filter(|r| r.escalated_at.is_some_and(|t| now - t >= Duration::minutes(farm.policy.escalate_after_min as i64))).collect();
    if due.is_empty() {
        return Ok(());
    }
    let head = &due[0].alert;
    let severity = Severity::Critical;
    let ids: Vec<String> = due.iter().map(|r| r.alert.id.clone()).collect();
    let told: HashSet<String> = notified(ctx, &ids).await?.into_iter().filter_map(|(_, u, _, _)| u).collect();
    let top: Option<Role> = people.iter().filter(|p| told.contains(&p.user.id)).map(|p| p.user.role).max();
    let above: Vec<&Person> = people
        .iter()
        .filter(|p| !told.contains(&p.user.id) && top.is_none_or(|t| p.user.role > t))
        .filter(|p| matches(p, severity, head.herd_id.as_deref(), &head.kind) && !deliveries(p, &farm.configured).is_empty())
        .collect();
    let tier = due.iter().map(|r| r.tier).max().unwrap_or(0) + 1;
    if let Some(next) = above.iter().map(|p| p.user.role).min() {
        let alerts: Vec<&Alert> = due.iter().map(|r| &r.alert).collect();
        for p in above.iter().filter(|p| p.user.role == next) {
            if holds(p, severity, &farm.policy, farm.tz, now) {
                continue;
            }
            send(ctx, farm, p, &alerts, tier, &format!("e{tier}"), now, report).await?;
        }
        for id in &ids {
            sqlx::query("UPDATE alerts SET tier = ?, escalated_at = ? WHERE id = ?").bind(tier as i64).bind(to_db(&now)).bind(id).execute(ctx.db()).await?;
        }
    } else {
        // Nobody left above: look again only after another interval.
        for id in &ids {
            sqlx::query("UPDATE alerts SET escalated_at = ? WHERE id = ?").bind(to_db(&now)).bind(id).execute(ctx.db()).await?;
        }
    }
    Ok(())
}
