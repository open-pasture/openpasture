//! What each command does, and the words of every reply. Replies are GSM-7
//! and at most 320 characters on SMS (1,000 on WhatsApp); names people typed
//! (herds, paddocks, tags) are made GSM-7 and cut to fit. Numbers go through
//! `op_core::units`, times are farm time.
//!
//! Security (field-ready §8.1 #33): a Y, N or STOP MOVE without the
//! decision's 4-digit code counts only within `approve_window_h` of our last
//! alert or brief to that number ([`last_prompt_to`]); after that the code
//! from the approval text is needed ("Y 4821"). Five wrong codes in an hour
//! from one number and codes from it stop counting for the hour. Roles:
//! answering, LATER and STOP MOVE need a manager; OK a hand; STATUS, WHERE
//! and questions anyone on the farm.

use chrono::{DateTime, Duration, Utc};
use op_brain::{AskError, AskRequest};
use op_core::alert::{AlertStatus, MessageLog};
use op_core::store::decision_from_row;
use op_core::time::{now, opt_from_db, to_db};
use op_core::units::Fmt;
use op_core::users::User;
use op_core::{Actor, Ctx, DbEnum, Decision, DecisionStatus, Herd, Identity, MoveStatus, Paddock, Role, Via};
use op_engine::cycle::{self, Response};
use serde_json::{Value, json};
use sqlx::Row;

use super::commands::{Command, Pick};
use super::{TextingConfig, can, reply_max, state};
use crate::engine::store;
use crate::text::{self, TextCtx, gsm7, septets};

pub const OPTED_OUT: &str = "You won't get openpasture messages here. Reply START to get them again.";
pub const OPTED_IN: &str = "You'll get openpasture messages here again.";
pub const VERIFIED: &str = "Phone verified. Reply STATUS any time.";
/// Wrong codes one number may text in an hour.
pub const MAX_WRONG_CODES: i64 = 5;
/// How the inbound text of a wrong code is marked (its `error`).
pub const WRONG_CODE: &str = "Wrong code.";
/// A "later" asks again after this long.
pub const LATER_MIN: i64 = 60;
/// Names in texts are cut to this many characters.
const NAME_MAX: usize = 30;

/// What a command came to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Out {
    /// The reply, if any.
    pub text: Option<String>,
    /// The decision it was about, kept on the reply.
    pub decision_id: Option<String>,
    /// Kept on the inbound text as its `error` (e.g. a wrong code).
    pub note: Option<String>,
}

impl Out {
    fn say(t: impl Into<String>) -> Self {
        Out { text: Some(t.into()), ..Default::default() }
    }
}

/// The person as a responder on records.
pub fn actor(user: &User) -> Actor {
    Actor { via: Via::Text, user_id: Some(user.id.clone()), name: Some(user.name.clone()) }
}

/// The person as a caller of read tools.
pub fn identity(user: &User) -> Identity {
    Identity { role: user.role, user_id: Some(user.id.clone()), name: Some(user.name.clone()), via: Via::Text }
}

/// Cut to `max` GSM-7 septets.
pub fn cut(s: &str, max: usize) -> String {
    let mut n = 0;
    let mut out = String::new();
    for c in s.chars() {
        n += septets(c.encode_utf8(&mut [0; 4]));
        if n > max {
            break;
        }
        out.push(c);
    }
    out
}

/// A name for a text: GSM-7 and at most 30 characters.
pub fn nm(s: &str) -> String {
    cut(&gsm7(s), NAME_MAX).trim().to_owned()
}

/// `s` in at most `max` septets, ending on a whole word with "..." when cut.
pub fn fit(s: &str, max: usize) -> String {
    if septets(s) <= max {
        return s.to_owned();
    }
    let head = cut(s, max.saturating_sub(3));
    let head = match head.rfind(' ') {
        Some(i) if i > max / 2 => &head[..i],
        _ => head.as_str(),
    };
    format!("{}...", head.trim_end_matches([' ', ',', '.', ';', ':']))
}

/// Lines joined by newlines while they fit; "+k more" for the rest.
fn fit_lines(lines: &[String], max: usize) -> String {
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        let more = lines.len() - i - 1;
        let tail = if more > 0 { format!("\n+{more} more") } else { String::new() };
        let next = if out.is_empty() { l.clone() } else { format!("{out}\n{l}") };
        if septets(&next) + septets(&tail) <= max || (out.is_empty() && more == 0) {
            out = next;
            continue;
        }
        if out.is_empty() {
            return fit(l, max);
        }
        return format!("{out}\n+{} more", lines.len() - i);
    }
    fit(&out, max)
}

/// Every text a person can send.
pub fn commands_text(cfg: &TextingConfig) -> String {
    format!(
        "Texts I answer: Y or N to a decision (add its code after {} h), LATER, OK to ack, STATUS, WHERE and a tag, STOP MOVE. STOP ends all texts.",
        cfg.approve_window_h
    )
}

pub async fn run(ctx: &Ctx, cfg: &TextingConfig, msg: &MessageLog, user: &User, cmd: Command) -> anyhow::Result<Out> {
    let now = now();
    match cmd {
        Command::Answer { approve, pick } => answer(ctx, cfg, msg, user, approve, &pick, now).await,
        Command::Later(pick) => later(ctx, cfg, msg, user, &pick, now).await,
        Command::Ack => ack(ctx, user, now).await,
        Command::Status => Ok(Out::say(status(ctx, now, reply_max(&msg.channel)).await?)),
        Command::Where(tag) => Ok(Out::say(where_is(ctx, &tag, now).await?)),
        Command::StopMove(pick) => stop_move(ctx, cfg, msg, user, &pick, now).await,
        _ => Ok(Out::default()),
    }
}

// ---- the reply window and codes -------------------------------------------------------

/// When we last texted `address` something that asks for an answer: an alert
/// (the decision prompt among them) or a brief. Replies don't count, or
/// texting us anything would open the window for whoever can fake that
/// number's caller id.
pub async fn last_prompt_to(ctx: &Ctx, address: &str) -> anyhow::Result<Option<DateTime<Utc>>> {
    let (t,): (Option<String>,) = sqlx::query_as(
        "SELECT MAX(created_at) FROM messages WHERE address = ? AND direction = 'out' AND kind IN ('alert', 'brief') AND status IN ('sending', 'sent', 'delivered')",
    )
    .bind(address)
    .fetch_one(ctx.db())
    .await?;
    opt_from_db(t)
}

async fn in_window(ctx: &Ctx, cfg: &TextingConfig, address: &str, now: DateTime<Utc>) -> anyhow::Result<bool> {
    Ok(last_prompt_to(ctx, address).await?.is_some_and(|t| now - t <= Duration::hours(cfg.approve_window_h as i64)))
}

async fn wrong_codes(ctx: &Ctx, address: &str, now: DateTime<Utc>) -> anyhow::Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages WHERE address = ? AND direction = 'in' AND error = ? AND created_at >= ?")
        .bind(address)
        .bind(WRONG_CODE)
        .bind(to_db(&(now - Duration::hours(1))))
        .fetch_one(ctx.db())
        .await?;
    Ok(n)
}

/// Decisions whose `decision_waiting` alert carries `code`; `open` only
/// while unresolved.
async fn decisions_by_code(ctx: &Ctx, code: &str, open: bool) -> anyhow::Result<Vec<String>> {
    let sql = format!(
        "SELECT targets FROM alerts WHERE kind = 'decision_waiting' AND json_extract(data, '$.code') = ? {} ORDER BY opened_at DESC",
        if open { "AND status != 'resolved'" } else { "" }
    );
    let rows: Vec<(String,)> = sqlx::query_as(&sql).bind(code).fetch_all(ctx.db()).await?;
    let mut out = Vec::new();
    for (targets,) in rows {
        let t: Vec<(String, String)> = serde_json::from_str(&targets).unwrap_or_default();
        for (k, id) in t {
            if k == "decision" && !out.contains(&id) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

// ---- decisions --------------------------------------------------------------------------

/// Proposed decisions a text can answer (MOVE, STAY, HOLD), oldest first,
/// in the person's herds when they chose some.
pub async fn pending(ctx: &Ctx, user: &User) -> anyhow::Result<Vec<Decision>> {
    let rows =
        sqlx::query("SELECT * FROM decisions WHERE status = ? ORDER BY created_at, id").bind(DecisionStatus::Proposed.as_db()).fetch_all(ctx.db()).await?;
    let (prefs, _) = crate::routing::prefs::get(ctx, &user.id).await?;
    let mut out = Vec::new();
    for r in &rows {
        let d = decision_from_row(r)?;
        if !answerable(&d) {
            continue;
        }
        if prefs.herds.as_ref().is_some_and(|hs| !hs.contains(&d.herd_id)) {
            continue;
        }
        out.push(d);
    }
    Ok(out)
}

/// The decision's action as stored ("MOVE", "STAY", "NEEDS_INFO", "HOLD"),
/// so actions added later read without a new match arm.
fn action(d: &Decision) -> String {
    d.action.map(|a| a.as_db()).unwrap_or_default()
}

/// A decision a text can answer: a MOVE, STAY or HOLD proposal.
fn answerable(d: &Decision) -> bool {
    matches!(action(d).as_str(), "MOVE" | "STAY" | "HOLD")
}

async fn decision(ctx: &Ctx, id: &str) -> anyhow::Result<Option<Decision>> {
    let row = sqlx::query("SELECT * FROM decisions WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    row.map(|r| decision_from_row(&r)).transpose()
}

fn herd_name(herds: &[Herd], id: &str) -> String {
    herds.iter().find(|h| h.id == id).map_or_else(|| "Herd".to_owned(), |h| nm(&h.name))
}

fn paddock_name(paddocks: &[Paddock], id: Option<&str>) -> Option<String> {
    paddocks.iter().find(|p| Some(p.id.as_str()) == id).map(|p| nm(&p.name))
}

/// Where a MOVE goes: its paddock, else the paddock holding the target's middle.
fn move_target(d: &Decision, paddocks: &[Paddock]) -> String {
    paddock_name(paddocks, d.to_paddock_id.as_deref())
        .or_else(|| {
            let c = d.geometry.as_ref()?.centroid()?;
            paddocks.iter().filter(|p| p.geometry.contains(c)).min_by(|a, b| a.area_ha.total_cmp(&b.area_ha)).map(|p| nm(&p.name))
        })
        .unwrap_or_else(|| "the new boundary".into())
}

/// "Cows: move to P4", "Cows: stay in P3", "Cows: hold today's strip".
pub fn phrase(d: &Decision, herds: &[Herd], paddocks: &[Paddock]) -> String {
    let herd = herd_name(herds, &d.herd_id);
    let here = herds.iter().find(|h| h.id == d.herd_id).and_then(|h| paddock_name(paddocks, h.paddock_id.as_deref()));
    match action(d).as_str() {
        "MOVE" => format!("{herd}: move to {}", move_target(d, paddocks)),
        "STAY" => match here {
            Some(p) => format!("{herd}: stay in {p}"),
            None => format!("{herd}: stay"),
        },
        "HOLD" => format!("{herd}: hold today's strip"),
        _ => format!("{herd}: a decision"),
    }
}

/// The numbered list last texted to a person, while the reply window lasts.
async fn saved_list(ctx: &Ctx, cfg: &TextingConfig, user: &User, kind: &str, now: DateTime<Utc>) -> anyhow::Result<Option<Vec<String>>> {
    let Some(s) = state::get(ctx, &format!("list:{}", user.id)).await? else { return Ok(None) };
    if s.ran_at.is_none_or(|t| now - t > Duration::hours(cfg.approve_window_h as i64)) {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(s.cursor.as_deref().unwrap_or("null")).unwrap_or(Value::Null);
    if v["kind"] != kind {
        return Ok(None);
    }
    Ok(v["ids"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()))
}

async fn save_list(ctx: &Ctx, user: &User, kind: &str, ids: &[String]) -> anyhow::Result<()> {
    state::ok(ctx, &format!("list:{}", user.id), Some(&json!({ "kind": kind, "ids": ids }).to_string()), None).await
}

/// "2 decisions waiting. 1 Cows: move to P4. 2 Heifers: stay in P3. Reply Y or N and the number, like Y 1."
fn list_text(what: &str, items: &[String], how: &str, max: usize) -> String {
    let tail = format!(" {how}");
    let mut out = format!("{} {what} waiting.", items.len());
    for (i, it) in items.iter().enumerate() {
        let next = format!("{out} {}. {it}.", i + 1);
        if septets(&next) + septets(&tail) > max {
            break;
        }
        out = next;
    }
    fit(&format!("{out}{tail}"), max)
}

/// The decision a Y, N or LATER means. `Err` is the reply instead.
#[allow(clippy::too_many_arguments)]
async fn pick_decision(
    ctx: &Ctx,
    cfg: &TextingConfig,
    msg: &MessageLog,
    user: &User,
    pick: &Pick,
    verb: &str,
    need_window: bool,
    now: DateTime<Utc>,
) -> anyhow::Result<Result<Decision, Out>> {
    let max = reply_max(&msg.channel);
    match pick {
        Pick::Code(code) => {
            if wrong_codes(ctx, &msg.address, now).await? >= MAX_WRONG_CODES {
                return Ok(Err(Out::say("Too many wrong codes from this number. Try again in an hour.")));
            }
            let mut found = Vec::new();
            for id in decisions_by_code(ctx, code, true).await? {
                if let Some(d) = decision(ctx, &id).await?.filter(|d| d.status == DecisionStatus::Proposed) {
                    found.push(d);
                }
            }
            match found.len() {
                0 => Ok(Err(Out { text: Some(format!("No decision waits on code {code}.")), note: Some(WRONG_CODE.into()), ..Default::default() })),
                1 => Ok(Ok(found.remove(0))),
                _ => Ok(Err(Out::say(format!("Code {code} fits more than one decision. Answer in the app.")))),
            }
        }
        Pick::Only | Pick::Number(_) if need_window && !in_window(ctx, cfg, &msg.address, now).await? => {
            Ok(Err(Out::say(format!("Add the code from the decision's text, like {verb} 4821."))))
        }
        Pick::Only => {
            let mut all = pending(ctx, user).await?;
            match all.len() {
                0 => Ok(Err(Out::say("No decision is waiting for an answer."))),
                1 => Ok(Ok(all.remove(0))),
                _ => {
                    let herds = ctx.store().list_herds().await?;
                    let paddocks = ctx.store().list_paddocks().await?;
                    let ids: Vec<String> = all.iter().map(|d| d.id.clone()).collect();
                    save_list(ctx, user, "decision", &ids).await?;
                    let items: Vec<String> = all.iter().map(|d| phrase(d, &herds, &paddocks)).collect();
                    let how = if verb == "LATER" {
                        "Reply LATER and the number, like LATER 1.".to_owned()
                    } else {
                        "Reply Y or N and the number, like Y 1.".to_owned()
                    };
                    Ok(Err(Out::say(list_text("decisions", &items, &how, max))))
                }
            }
        }
        Pick::Number(i) => {
            let ids = match saved_list(ctx, cfg, user, "decision", now).await? {
                Some(ids) => ids,
                None => pending(ctx, user).await?.into_iter().map(|d| d.id).collect(),
            };
            let Some(id) = ids.get(i - 1) else { return Ok(Err(Out::say(format!("There's no decision {i}. Reply {verb} for the list.")))) };
            match decision(ctx, id).await? {
                Some(d) if d.status == DecisionStatus::Proposed => Ok(Ok(d)),
                _ => Ok(Err(Out::say(format!("Decision {i} is already answered.")))),
            }
        }
    }
}

/// Y or N.
async fn answer(ctx: &Ctx, cfg: &TextingConfig, msg: &MessageLog, user: &User, approve: bool, pick: &Pick, now: DateTime<Utc>) -> anyhow::Result<Out> {
    if !can(user, Role::Manager) {
        return Ok(Out::say("Your role can't answer decisions."));
    }
    let d = match pick_decision(ctx, cfg, msg, user, pick, if approve { "Y" } else { "N" }, true, now).await? {
        Ok(d) => d,
        Err(out) => return Ok(out),
    };
    let how = if approve { Response::Approve } else { Response::Reject };
    match cycle::respond(ctx, &d.id, how, None, None, actor(user)).await {
        Ok(done) => {
            let herds = ctx.store().list_herds().await?;
            let paddocks = ctx.store().list_paddocks().await?;
            Ok(Out { text: Some(answered(&done, &herds, &paddocks)), decision_id: Some(done.id.clone()), note: None })
        }
        Err(e) if e.status.is_client_error() => Ok(Out { text: Some(fit(&gsm7(&e.message), 160)), decision_id: Some(d.id), note: None }),
        Err(e) => anyhow::bail!("answering {}: {}", d.id, e.message),
    }
}

/// The reply once a decision is answered.
fn answered(d: &Decision, herds: &[Herd], paddocks: &[Paddock]) -> String {
    let herd = herd_name(herds, &d.herd_id);
    match (d.status, action(d).as_str()) {
        (DecisionStatus::Rejected, _) => format!("Rejected. Nothing sent for {herd}."),
        (_, "HOLD") => format!("{herd} holds today's strip."),
        (DecisionStatus::Applied, "MOVE") => format!("Approved. {herd} moving to {}.", move_target(d, paddocks)),
        (_, "MOVE") => format!("Approved. Sending {herd} to {}.", move_target(d, paddocks)),
        (_, "STAY") => format!("Approved. {}.", phrase(d, herds, paddocks).replacen(": stay", " stays", 1)),
        (s, _) => format!("{herd}: decision {}.", s.as_db()),
    }
}

/// The approval prompt again: the alert's own text (with its code) while the
/// decision's alert is open, else "Cows: move to P4? Reply Y or N".
pub async fn prompt(ctx: &Ctx, d: &Decision, now: DateTime<Utc>) -> anyhow::Result<String> {
    let row =
        sqlx::query("SELECT * FROM alerts WHERE key = ? AND status != 'resolved'").bind(format!("decision_waiting:{}", d.id)).fetch_optional(ctx.db()).await?;
    if let Some(r) = row {
        let a = store::alert_from_row(&r)?;
        return Ok(text::alert_text(&a, None, &TextCtx::of(ctx, now).await?));
    }
    let herds = ctx.store().list_herds().await?;
    let paddocks = ctx.store().list_paddocks().await?;
    Ok(fit(&format!("{}? Reply Y or N", phrase(d, &herds, &paddocks)), text::MAX_ALERT))
}

/// LATER: ask again in an hour on this channel, if still waiting then.
async fn later(ctx: &Ctx, cfg: &TextingConfig, msg: &MessageLog, user: &User, pick: &Pick, now: DateTime<Utc>) -> anyhow::Result<Out> {
    if !can(user, Role::Manager) {
        return Ok(Out::say("Your role can't answer decisions."));
    }
    let d = match pick_decision(ctx, cfg, msg, user, pick, "LATER", false, now).await? {
        Ok(d) => d,
        Err(out) => return Ok(out),
    };
    let due = now + Duration::minutes(LATER_MIN);
    sqlx::query("INSERT INTO text_reminders (user_id, decision_id, channel, address, due_at, created_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&user.id)
        .bind(&d.id)
        .bind(super::reply_channel(&msg.channel))
        .bind(&msg.address)
        .bind(to_db(&due))
        .bind(to_db(&now))
        .execute(ctx.db())
        .await?;
    let tz = TextCtx::of(ctx, now).await?.tz;
    Ok(Out { text: Some(format!("OK. I'll ask again at {}.", text::clock(due, tz))), decision_id: Some(d.id), note: None })
}

// ---- alerts -------------------------------------------------------------------------------

/// OK: ack every alert the last alert text to this person covered.
async fn ack(ctx: &Ctx, user: &User, now: DateTime<Utc>) -> anyhow::Result<Out> {
    if !can(user, Role::Hand) {
        return Ok(Out::say("Your role can't ack alerts."));
    }
    let last: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT id, alert_id FROM messages WHERE user_id = ? AND direction = 'out' AND kind = 'alert' AND alert_id IS NOT NULL
           AND status IN ('sending', 'sent', 'delivered')
         ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .bind(&user.id)
    .fetch_optional(ctx.db())
    .await?;
    let Some((message_id, first)) = last else { return Ok(Out::say("No alert has been texted to you.")) };
    let mut ids: Vec<String> = first.into_iter().collect();
    for (id,) in sqlx::query_as::<_, (String,)>("SELECT alert_id FROM alert_notifications WHERE message_id = ? ORDER BY rowid")
        .bind(&message_id)
        .fetch_all(ctx.db())
        .await?
    {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let mut acked = Vec::new();
    let mut was = Vec::new();
    for id in &ids {
        let Some(a) = store::get(ctx, id).await? else { continue };
        if a.status == AlertStatus::Open {
            acked.push(store::ack(ctx, id, &actor(user), now).await.map_err(|e| anyhow::anyhow!(e.message))?);
        } else {
            was.push(a);
        }
    }
    let titles = |v: &[op_core::alert::Alert]| -> String {
        let mut s: Vec<String> = v.iter().take(3).map(|a| nm(&a.title)).collect();
        if v.len() > 3 {
            s.push(format!("+{}", v.len() - 3));
        }
        s.join(", ")
    };
    Ok(Out::say(match (acked.len(), was.as_slice()) {
        (0, [one]) if one.status == AlertStatus::Resolved => format!("{} is already resolved.", nm(&one.title)),
        (0, [one]) => format!("{} is already acked.", nm(&one.title)),
        (0, []) => "Nothing to ack.".to_owned(),
        (0, _) => "Those alerts are already acked or resolved.".to_owned(),
        (1, _) => format!("Acked: {}.", titles(&acked)),
        (n, _) => format!("Acked {n}: {}.", titles(&acked)),
    }))
}

// ---- status -------------------------------------------------------------------------------

/// What one herd looks like now.
#[derive(Debug, Clone)]
pub struct HerdFacts {
    pub herd: Herd,
    pub paddock: Option<String>,
    /// A running move: where to, and how far the back line has to go (m).
    pub moving: Option<(String, f64)>,
    /// Unresolved alerts (not the decision prompts), most urgent first.
    pub alerts: Vec<String>,
    /// Decisions waiting for an answer, as phrases.
    pub waiting: Vec<String>,
}

pub async fn facts(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<Vec<HerdFacts>> {
    let herds = ctx.store().list_herds().await?;
    let paddocks = ctx.store().list_paddocks().await?;
    let rows =
        sqlx::query("SELECT * FROM decisions WHERE status = ? ORDER BY created_at, id").bind(DecisionStatus::Proposed.as_db()).fetch_all(ctx.db()).await?;
    let proposed = rows.iter().map(decision_from_row).collect::<anyhow::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for h in &herds {
        let moving = match op_ingest::moves::current_move(ctx.db(), &h.id, now).await? {
            Some(m) if m.status == MoveStatus::Sweeping => {
                let to = decision(ctx, &m.decision_id).await?.map(|d| move_target(&d, &paddocks)).unwrap_or_else(|| {
                    m.target
                        .centroid()
                        .and_then(|c| paddocks.iter().filter(|p| p.geometry.contains(c)).min_by(|a, b| a.area_ha.total_cmp(&b.area_ha)).map(|p| nm(&p.name)))
                        .unwrap_or_else(|| "the new boundary".into())
                });
                Some((to, m.remaining_m))
            }
            _ => None,
        };
        let q = store::ListQuery { filter: store::Filter::Active, herd_id: Some(&h.id), from: None, to: None, limit: 50 };
        let alerts = store::list(ctx, &q).await?.into_iter().filter(|a| a.kind != "decision_waiting").map(|a| a.title).collect();
        let waiting = proposed
            .iter()
            .filter(|d| d.herd_id == h.id && answerable(d))
            .map(|d| phrase(d, &herds, &paddocks).split_once(": ").map(|(_, p)| p.to_owned()).unwrap_or_default())
            .collect();
        out.push(HerdFacts { herd: h.clone(), paddock: paddock_name(&paddocks, h.paddock_id.as_deref()), moving, alerts, waiting });
    }
    Ok(out)
}

/// "Cows: 250 hd in P3. 2 open: 214 outside P3, 031 battery 14%. Waiting: move to P4." or,
/// during a move, "Cows: 250 hd moving to P4, 660 ft to go. …"
pub fn status_line(f: &HerdFacts, fmt: &Fmt) -> String {
    let mut s = format!("{}: {} hd", nm(&f.herd.name), f.herd.count);
    // While a move runs, the herd's paddock already names where it is going.
    match (&f.moving, &f.paddock) {
        (Some((to, left)), _) => s.push_str(&format!(" moving to {to}, {} to go.", fmt.len(*left))),
        (None, Some(p)) => s.push_str(&format!(" in {p}.")),
        (None, None) => s.push('.'),
    }
    if !f.alerts.is_empty() {
        let mut t: Vec<String> = f.alerts.iter().take(2).map(|a| nm(a)).collect();
        if f.alerts.len() > 2 {
            t.push(format!("+{}", f.alerts.len() - 2));
        }
        s.push_str(&format!(" {} open: {}.", f.alerts.len(), t.join(", ")));
    }
    if let Some(w) = f.waiting.first() {
        s.push_str(&format!(" Waiting: {w}."));
        if f.waiting.len() > 1 {
            s.push_str(&format!(" +{}", f.waiting.len() - 1));
        }
    }
    s
}

async fn status(ctx: &Ctx, now: DateTime<Utc>, max: usize) -> anyhow::Result<String> {
    let fmt = Fmt::of(ctx).await?;
    let lines: Vec<String> = facts(ctx, now).await?.iter().map(|f| status_line(f, &fmt)).collect();
    if lines.is_empty() {
        return Ok("No herds yet.".into());
    }
    Ok(fit_lines(&lines, max))
}

// ---- where --------------------------------------------------------------------------------

/// WHERE: an animal's last fix in words, how old it is, and a map link.
pub async fn where_is(ctx: &Ctx, tag: &str, now: DateTime<Utc>) -> anyhow::Result<String> {
    let label = nm(tag);
    let animal: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT tag, collar_id FROM animals WHERE lower(tag) = lower(?) AND removed_at IS NULL ORDER BY created_at LIMIT 1")
            .bind(tag.trim())
            .fetch_optional(ctx.db())
            .await?;
    let collar_id = match animal {
        Some((_, Some(c))) => Some(c),
        Some((_, None)) => return Ok(format!("{label} has no collar.")),
        None => {
            sqlx::query_scalar::<_, String>("SELECT id FROM collars WHERE lower(name) = lower(?) LIMIT 1").bind(tag.trim()).fetch_optional(ctx.db()).await?
        }
    };
    let Some(collar_id) = collar_id else { return Ok(format!("No animal {label}.")) };
    let row = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(&collar_id).fetch_optional(ctx.db()).await?;
    let Some(collar) = row.map(|r| op_core::store::collar_from_row(&r)).transpose()? else { return Ok(format!("No animal {label}.")) };
    let Some(fix) = collar.last_fix.clone() else { return Ok(format!("{label} has no position yet.")) };
    let place = op_core::place::describe(ctx, fix.point).await?.map(|p| gsm7(&p));
    let [lon, lat] = fix.point;
    let link = format!("https://maps.google.com/?q={lat:.6},{lon:.6}");
    let age = text::age(fix.at, now);
    let head = if collar.state == op_core::FenceState::Outside { format!("{label} is outside") } else { label };
    let where_words = match place {
        Some(p) => format!("{head}, {p}, {age} ago."),
        None => format!("{head}, {age} ago."),
    };
    Ok(format!("{} {link}", fit(&where_words, 320 - link.len() - 1)))
}

// ---- stop move --------------------------------------------------------------------------

/// STOP MOVE: stop a running move where it is.
async fn stop_move(ctx: &Ctx, cfg: &TextingConfig, msg: &MessageLog, user: &User, pick: &Pick, now: DateTime<Utc>) -> anyhow::Result<Out> {
    if !can(user, Role::Manager) {
        return Ok(Out::say("Your role can't stop moves."));
    }
    let herds = ctx.store().list_herds().await?;
    let paddocks = ctx.store().list_paddocks().await?;
    let (prefs, _) = crate::routing::prefs::get(ctx, &user.id).await?;
    let rows = sqlx::query("SELECT herd_id, decision_id FROM moves WHERE status = 'sweeping' ORDER BY started_at, id").fetch_all(ctx.db()).await?;
    let mut running: Vec<(String, String)> = Vec::new();
    for r in &rows {
        running.push((r.try_get("herd_id")?, r.try_get("decision_id")?));
    }
    let herd_id = match pick {
        Pick::Code(code) => {
            if wrong_codes(ctx, &msg.address, now).await? >= MAX_WRONG_CODES {
                return Ok(Out::say("Too many wrong codes from this number. Try again in an hour."));
            }
            let ids = decisions_by_code(ctx, code, false).await?;
            match running.iter().find(|(_, d)| ids.contains(d)) {
                Some((h, _)) => h.clone(),
                None => return Ok(Out { text: Some(format!("No running move has code {code}.")), note: Some(WRONG_CODE.into()), ..Default::default() }),
            }
        }
        _ if !in_window(ctx, cfg, &msg.address, now).await? => return Ok(Out::say("Add the code from the decision's text, like STOP MOVE 4821.")),
        Pick::Only => {
            let mine: Vec<&(String, String)> = running.iter().filter(|(h, _)| prefs.herds.as_ref().is_none_or(|hs| hs.contains(h))).collect();
            match mine.as_slice() {
                [] => return Ok(Out::say("No move is running.")),
                [one] => one.0.clone(),
                many => {
                    let ids: Vec<String> = many.iter().map(|(h, _)| h.clone()).collect();
                    save_list(ctx, user, "move", &ids).await?;
                    let mut items = Vec::new();
                    for (h, d) in many.iter() {
                        let to = decision(ctx, d).await?.map(|d| move_target(&d, &paddocks)).unwrap_or_else(|| "the new boundary".into());
                        items.push(format!("{} to {to}", herd_name(&herds, h)));
                    }
                    return Ok(Out::say(list_text("moves", &items, "Reply STOP MOVE and the number, like STOP MOVE 1.", reply_max(&msg.channel))));
                }
            }
        }
        Pick::Number(i) => {
            let ids = saved_list(ctx, cfg, user, "move", now).await?.unwrap_or_else(|| running.iter().map(|(h, _)| h.clone()).collect());
            match ids.get(i - 1) {
                Some(h) if running.iter().any(|(r, _)| r == h) => h.clone(),
                Some(_) => return Ok(Out::say(format!("Move {i} isn't running any more."))),
                None => return Ok(Out::say(format!("There's no move {i}. Reply STOP MOVE for the list."))),
            }
        }
    };
    let decision_id = running.iter().find(|(h, _)| *h == herd_id).map(|(_, d)| d.clone());
    match op_ingest::moves::stop_move(ctx, &herd_id).await {
        Ok(m) => {
            let fmt = Fmt::of(ctx).await?;
            let to = decision(ctx, &m.decision_id).await?.map(|d| move_target(&d, &paddocks)).unwrap_or_else(|| "the new boundary".into());
            let herd = herd_name(&herds, &herd_id);
            let text = if m.remaining_m >= 1.0 {
                format!("Stopped. {herd} keep the boundary they have, {} short of {to}.", fmt.len(m.remaining_m))
            } else {
                format!("Stopped. {herd} keep the boundary they have.")
            };
            Ok(Out { text: Some(text), decision_id, note: None })
        }
        Err(e) if e.status.is_client_error() => Ok(Out::say(fit(&gsm7(&e.message), 160))),
        Err(e) => anyhow::bail!("stopping the move: {}", e.message),
    }
}

// ---- verification by reply ------------------------------------------------------------

/// A 6-digit code texted back by a person with a pending verification.
/// `Some(reply)` when it verified the phone.
pub async fn confirm_code(ctx: &Ctx, user: &User, code: &str) -> anyhow::Result<Option<String>> {
    let pending: Option<(String,)> = sqlx::query_as("SELECT user_id FROM phone_codes WHERE user_id = ?").bind(&user.id).fetch_optional(ctx.db()).await?;
    if pending.is_none() {
        return Ok(None);
    }
    Ok(match crate::notify::verify::confirm(ctx, &user.id, code).await {
        Ok(_) => Some(VERIFIED.into()),
        Err(e) => {
            tracing::info!(user = %user.id, "a texted code didn't verify: {}", e.message);
            None
        }
    })
}

// ---- questions ------------------------------------------------------------------------

/// What a question gets to start from: the farm, its herds, alerts and
/// decisions, and who asks.
pub async fn summary(ctx: &Ctx, user: &User, now: DateTime<Utc>) -> anyhow::Result<Value> {
    let fmt = Fmt::of(ctx).await?;
    let farm = ctx.store().get_farm().await?;
    let tctx = TextCtx::of(ctx, now).await?;
    let herds: Vec<Value> = facts(ctx, now)
        .await?
        .iter()
        .map(|f| {
            let mut h = json!({ "name": f.herd.name, "head": f.herd.count, "species": f.herd.species });
            if let Some(p) = &f.paddock {
                h["paddock"] = json!(p);
            }
            if let Some((to, left)) = &f.moving {
                h["moving_to"] = json!(to);
                h["to_go"] = json!(fmt.len(*left));
            }
            if !f.alerts.is_empty() {
                h["open_alerts"] = json!(f.alerts);
            }
            if !f.waiting.is_empty() {
                h["waiting_for_an_answer"] = json!(f.waiting);
            }
            h
        })
        .collect();
    let paddocks: Vec<Value> = ctx.store().list_paddocks().await?.iter().map(|p| json!({ "name": p.name, "area": fmt.area(p.area_ha) })).collect();
    Ok(json!({
        "farm": farm.map(|f| f.name),
        "local_time": now.with_timezone(&tctx.tz).format("%a %Y-%m-%d %H:%M").to_string(),
        "units": fmt.units,
        "asked_by": { "name": user.name, "role": user.role },
        "herds": herds,
        "paddocks": paddocks,
    }))
}

/// Anything else: the farm's brain answers with read tools (never
/// `run_sql`). A brain that doesn't answer questions (Codex, the heuristic)
/// or isn't set up gets the command list back.
pub async fn ask(ctx: &Ctx, cfg: &TextingConfig, msg: &MessageLog, user: &User, question: &str) -> String {
    let max = reply_max(&msg.channel);
    let brain = match op_brain::resolve(ctx).await {
        Ok(b) => b,
        Err(e) => {
            tracing::info!("no brain for a texted question: {e:#}");
            return commands_text(cfg);
        }
    };
    let context = match summary(ctx, user, now()).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("farm summary for a texted question: {e:#}");
            Value::Null
        }
    };
    let req = AskRequest { question: question.to_owned(), context, tools: ctx.tool_runner(identity(user), &["run_sql"]), max_chars: max, log: None };
    match brain.ask(req).await {
        Ok(answer) if msg.channel == "whatsapp" => answer.chars().take(max).collect(),
        Ok(answer) => fit(&gsm7(&answer), max),
        Err(AskError::Unsupported) => commands_text(cfg),
        Err(AskError::Failed(e)) => {
            tracing::warn!("a texted question failed: {e:#}");
            fit(&format!("No answer right now. {}", commands_text(cfg)), max)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitting_keeps_whole_words() {
        assert_eq!(fit("short", 10), "short");
        let s = "one two three four five six seven";
        let f = fit(s, 20);
        assert!(septets(&f) <= 20, "{f}");
        assert!(f.ends_with("..."), "{f}");
        assert_eq!(nm("Cow “Daisy” — 🐄 the very long name that goes on and on"), "Cow \"Daisy\" - the very long na");
    }

    #[test]
    fn lists_fit_and_say_how_to_pick() {
        let items: Vec<String> = (0..20).map(|i| format!("Herd number {i}: move to paddock {i}")).collect();
        let t = list_text("decisions", &items, "Reply Y or N and the number, like Y 1.", 320);
        assert!(septets(&t) <= 320, "{t}");
        assert!(t.starts_with("20 decisions waiting. 1. Herd number 0"), "{t}");
        assert!(t.ends_with("like Y 1."), "{t}");
    }

    #[test]
    fn the_command_list_is_one_text() {
        let t = commands_text(&TextingConfig::default());
        assert!(crate::text::is_gsm7(&t) && septets(&t) <= 160, "{} {t}", septets(&t));
    }
}
