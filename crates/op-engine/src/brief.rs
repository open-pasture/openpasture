//! The morning brief: each herd's call for today from the decision record,
//! in a few plain lines. Ported from the archived kit
//! (`skills/morning-brief/SKILL.md`, `briefing/engine.py::build_brief`). No
//! LLM: the same record always gives the same brief.
//!
//! Lines, in order:
//! 1. The call: "Cows: MOVE to P4 (30.6 ac)." / "Cows: STAY in P3." /
//!    "Cows: NEEDS_INFO." / "Cows: HOLD today's strip.". When today's
//!    decision is still running, failed or missing, one line says so and the
//!    brief goes on with the rest.
//! 2. Where it stands: "Reply Y or N." (waiting), "Sends 07:40 unless you
//!    reply N." (timer), "Sent, 248/250 collars confirmed." (sent),
//!    "Stopped 610 ft short, 250/250 collars confirmed." (its move stopped).
//! 3. The one thing to check, when the decision asks for it.
//! 4. Up to four reasons from the record, as written there.
//! 5. Stale or missing data: silent collars (and apart, collars that haven't
//!    reported yet), old imagery, a position from the farm record, no recent
//!    field note.
//! 6. The brief-line registry's lines (`ctx.brief_lines()`), in order.
//!
//! "Today" is the decision window: since the last `settings.decision_time`
//! in the farm's time zone. Numbers go through [`Fmt`]. `text` is the same
//! lines as one GSM-7 text of at most [`TEXT_MAX`] characters: lines that
//! don't fit are left out, the third and fourth reason first, the call and
//! where it stands never.

use axum::Json;
use axum::extract::{Query, State};
use axum::routing::get;
use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use op_core::store::decision_from_row;
use op_core::tools::{ToolCall, ToolSpec};
use op_core::units::Fmt;
use op_core::{AckStatus, ApiError, ApiResult, Collar, Ctx, DbEnum, Decision, DecisionStatus, Herd, MoveStatus, Paddock, Role, time};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

use crate::{context, land};

/// Longest brief text for one herd (GSM-7 characters).
pub const TEXT_MAX: usize = 480;
const MAX_REASONS: usize = 4;
/// A collar with no report for this long is silent.
const SILENT_HOURS: i64 = 24;
/// Imagery older than this is named.
const IMAGERY_OLD_DAYS: i64 = 14;
/// No field note for this long is named.
const FIELD_NOTE_DAYS: i64 = 7;
/// Names typed by people are cut to this many characters in the brief.
const MAX_NAME: usize = 40;

/// `GET /api/brief`: one herd's brief.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    pub herd_id: String,
    pub lines: Vec<String>,
    /// The lines as one text: GSM-7, at most [`TEXT_MAX`] characters.
    pub text: String,
}

/// A line of the brief and when it gives way in the text: 0 never, higher first.
struct Line {
    text: String,
    drop: u8,
}

impl Line {
    fn keep(text: String) -> Self {
        Line { text, drop: 0 }
    }
}

const DROP_NEED: u8 = 5;
const DROP_REGISTRY: u8 = 6;
const DROP_STALE: u8 = 8;
/// Reasons one to four.
const DROP_REASON: [u8; MAX_REASONS] = [4, 7, 9, 10];

/// The herd `herd_id` names, or the farm's only herd.
pub async fn resolve_herd(ctx: &Ctx, herd_id: Option<&str>) -> ApiResult<Herd> {
    if let Some(id) = herd_id.map(str::trim).filter(|s| !s.is_empty()) {
        return ctx.store().get_herd(id).await?.ok_or_else(|| ApiError::not_found(format!("No herd {id}.")));
    }
    let mut herds = ctx.store().list_herds().await?;
    match herds.len() {
        1 => Ok(herds.remove(0)),
        0 => Err(ApiError::not_found("The farm has no herds yet.")),
        n => Err(ApiError::bad_request(format!("The farm has {n} herds; pass herd_id."))),
    }
}

/// The brief for one herd at `now`, for someone who answers decisions.
pub async fn brief(ctx: &Ctx, herd: &Herd, now: DateTime<Utc>) -> anyhow::Result<Brief> {
    brief_for(ctx, herd, now, true).await
}

/// [`brief`]; `answers`: whether its reader answers decisions (managers and
/// up). For anyone else a proposed decision reads as where it stands ("Sends
/// 07:40.", "Waiting for an answer.") without asking for a Y or N.
pub async fn brief_for(ctx: &Ctx, herd: &Herd, now: DateTime<Utc>, answers: bool) -> anyhow::Result<Brief> {
    let settings = ctx.settings().await?;
    let fmt = Fmt::new(settings.units);
    let tz: chrono_tz::Tz = ctx.store().get_farm().await?.and_then(|f| f.timezone.parse().ok()).unwrap_or(chrono_tz::UTC);
    let paddocks = ctx.store().list_paddocks().await?;
    let collars: Vec<Collar> = context::herd_collars(ctx, &herd.id).await?.into_iter().filter(|c| c.parked_at.is_none()).collect();
    let herd_name = name(&herd.name);
    let since = window_start(now, tz, &settings.decision_time);
    let decision = todays_decision(ctx, &herd.id, since).await?;

    let mut lines: Vec<Line> = Vec::new();
    let mut need = None;
    match &decision {
        None => lines.push(Line::keep(format!("{herd_name}: no decision yet today."))),
        Some(d) if d.status == DecisionStatus::Running => lines.push(Line::keep(format!("{herd_name}: today's decision is still running."))),
        Some(d) if d.status == DecisionStatus::Failed => {
            let why = d.error.as_deref().map(sentence).unwrap_or_else(|| "no reason recorded.".into());
            lines.push(Line::keep(format!("{herd_name}: today's decision failed: {why}")));
        }
        Some(d) => {
            lines.push(Line::keep(call(d, herd, &herd_name, &paddocks, &fmt)));
            if let Some(s) = standing(ctx, d, &collars, &fmt, tz, now, answers).await? {
                lines.push(Line::keep(s));
            }
            // The reasons and the check argued for the call as proposed; once the
            // farmer changed it (N on a STAY made it a HOLD, or they drew their
            // own boundary) they no longer describe it.
            if !changed_by_farmer(d) {
                need = d.need.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(sentence);
                if let Some(n) = &need {
                    lines.push(Line { text: n.clone(), drop: DROP_NEED });
                }
                for (i, r) in reasons(d.reasoning.as_deref().unwrap_or_default()).into_iter().take(MAX_REASONS).enumerate() {
                    lines.push(Line { text: r, drop: DROP_REASON[i] });
                }
            }
        }
    }
    for s in stale(ctx, herd, decision.as_ref(), &collars, &paddocks, need.is_some(), now).await? {
        lines.push(Line { text: s, drop: DROP_STALE });
    }
    for l in ctx.brief_lines().collect(ctx, &herd.id, now).await {
        lines.push(Line { text: l.trim().to_owned(), drop: DROP_REGISTRY });
    }
    Ok(Brief { herd_id: herd.id.clone(), text: fit_text(&lines, TEXT_MAX), lines: lines.into_iter().map(|l| l.text).collect() })
}

/// When today's decision window began: the last `decision_time` (`HH:MM`,
/// farm time) at or before `now`. Midnight when the setting doesn't parse.
pub fn window_start(now: DateTime<Utc>, tz: chrono_tz::Tz, decision_time: &str) -> DateTime<Utc> {
    let at = NaiveTime::parse_from_str(decision_time, "%H:%M").unwrap_or(NaiveTime::MIN);
    let local = now.with_timezone(&tz);
    for back in 0..3 {
        let day = local.date_naive() - Duration::days(back);
        let naive = day.and_time(at);
        // A time the clocks skip (DST) starts the window an hour later.
        let start = tz.from_local_datetime(&naive).earliest().or_else(|| tz.from_local_datetime(&(naive + Duration::hours(1))).earliest());
        if let Some(s) = start.map(|s| s.with_timezone(&Utc))
            && s <= now
        {
            return s;
        }
    }
    now - Duration::days(1)
}

/// The herd's newest decision since `since` (superseded ones gave way to a newer one).
async fn todays_decision(ctx: &Ctx, herd_id: &str, since: DateTime<Utc>) -> anyhow::Result<Option<Decision>> {
    let row = sqlx::query("SELECT * FROM decisions WHERE herd_id = ? AND created_at >= ? AND status != 'superseded' ORDER BY created_at DESC, id DESC LIMIT 1")
        .bind(herd_id)
        .bind(time::to_db(&since))
        .fetch_optional(ctx.db())
        .await?;
    row.map(|r| decision_from_row(&r)).transpose()
}

fn action_of(d: &Decision) -> String {
    d.action.map(|a| a.as_db()).unwrap_or_default()
}

/// The farmer answered with another call than the one proposed: S's HOLD
/// for a STAY (`inputs.proposed_action`), or their own boundary for the
/// proposed one (`inputs.proposed_geometry`).
fn changed_by_farmer(d: &Decision) -> bool {
    d.inputs.get("proposed_action").is_some_and(|a| a.as_str() != Some(action_of(d).as_str())) || d.inputs.get("proposed_geometry").is_some()
}

/// Is this a call that sends a boundary (MOVE, or S's HOLD)?
fn sends(d: &Decision) -> bool {
    matches!(action_of(d).as_str(), "MOVE" | "HOLD")
}

/// "Cows: MOVE to P4 (30.6 ac)."
fn call(d: &Decision, herd: &Herd, herd_name: &str, paddocks: &[Paddock], fmt: &Fmt) -> String {
    let paddock = |id: Option<&str>| id.and_then(|id| paddocks.iter().find(|p| p.id == id));
    match action_of(d).as_str() {
        // The area is the boundary's (what the herd gets, as the approval text says), else the paddock's.
        "MOVE" => {
            let drawn = d.geometry.as_ref().map(|g| g.area_ha()).filter(|a| *a > 0.0);
            match paddock(d.to_paddock_id.as_deref()) {
                Some(p) => match drawn.or((p.area_ha > 0.0).then_some(p.area_ha)) {
                    Some(a) => format!("{herd_name}: MOVE to {} ({}).", name(&p.name), fmt.area(a)),
                    None => format!("{herd_name}: MOVE to {}.", name(&p.name)),
                },
                None => match drawn {
                    Some(a) => format!("{herd_name}: MOVE to a drawn boundary ({}).", fmt.area(a)),
                    None => format!("{herd_name}: MOVE."),
                },
            }
        }
        "STAY" => {
            let here = d.inputs.get("from_paddock_id").and_then(Value::as_str).or(herd.paddock_id.as_deref());
            match paddock(here) {
                Some(p) => format!("{herd_name}: STAY in {}.", name(&p.name)),
                None => format!("{herd_name}: STAY."),
            }
        }
        "HOLD" => format!("{herd_name}: HOLD today's strip."),
        "" => format!("{herd_name}: no call."),
        other => format!("{herd_name}: {other}."),
    }
}

/// Where the decision stands, when there is something to say.
async fn standing(
    ctx: &Ctx,
    d: &Decision,
    collars: &[Collar],
    fmt: &Fmt,
    tz: chrono_tz::Tz,
    now: DateTime<Utc>,
    answers: bool,
) -> anyhow::Result<Option<String>> {
    Ok(match d.status {
        DecisionStatus::Proposed if sends(d) => Some(match (d.apply_at, answers) {
            (Some(at), true) => format!("Sends {} unless you reply N.", clock(at, tz, now)),
            (Some(at), false) => format!("Sends {}.", clock(at, tz, now)),
            (None, true) => "Reply Y or N.".into(),
            (None, false) => "Waiting for an answer.".into(),
        }),
        DecisionStatus::Approved if sends(d) => Some("Approved, sending.".into()),
        DecisionStatus::Approved => Some("Approved.".into()),
        DecisionStatus::Applied if sends(d) => Some(sent(ctx, d, collars, fmt, tz, now).await?),
        DecisionStatus::Rejected if sends(d) => Some("Rejected, nothing sent.".into()),
        DecisionStatus::Rejected => Some("Rejected.".into()),
        _ => None,
    })
}

/// "Sent, 248/250 collars confirmed." (the herd's collars that applied this
/// decision's boundary), with the distance left while its move still sweeps.
async fn sent(ctx: &Ctx, d: &Decision, collars: &[Collar], fmt: &Fmt, tz: chrono_tz::Tz, now: DateTime<Utc>) -> anyhow::Result<String> {
    if collars.is_empty() {
        return Ok("Applied.".into());
    }
    let status = op_ingest::boundary_status(ctx, &d.herd_id).await?;
    if let Some(b) = status.active.as_ref().filter(|b| b.decision_id == d.id) {
        let confirmed =
            status.acks.iter().filter(|a| a.version == b.version && a.status == AckStatus::Applied && collars.iter().any(|c| c.id == a.collar_id)).count();
        // Someone stopped this decision's move before the target: the collars hold where it stopped.
        if let Some(short) = stopped_short(ctx, &d.id).await? {
            return Ok(format!("Stopped {} short, {confirmed}/{} collars confirmed.", fmt.len(short), collars.len()));
        }
        let to_go = status
            .r#move
            .as_ref()
            .filter(|m| m.decision_id == d.id && m.status == MoveStatus::Sweeping)
            .map(|m| format!(", {} to go", fmt.len(m.remaining_m)))
            .unwrap_or_default();
        return Ok(format!("Sent, {confirmed}/{} collars confirmed{to_go}.", collars.len()));
    }
    if let Some(at) = status.pending.as_ref().filter(|b| b.decision_id == d.id).and_then(|b| b.effective_at) {
        return Ok(format!("Sent, opens {}.", clock(at, tz, now)));
    }
    Ok("Sent.".into())
}

/// How far short of its target the decision's move stopped (metres), when it was stopped.
/// Read from the move itself: the boundary status shows an ended move only briefly.
async fn stopped_short(ctx: &Ctx, decision_id: &str) -> anyhow::Result<Option<f64>> {
    let left: Option<f64> =
        sqlx::query_scalar("SELECT remaining_m FROM moves WHERE decision_id = ? AND status = 'stopped' ORDER BY started_at DESC, id DESC LIMIT 1")
            .bind(decision_id)
            .fetch_optional(ctx.db())
            .await?;
    // Stopped with nothing left to go is as good as there.
    Ok(left.filter(|m| *m >= 1.0))
}

/// Stale or missing data behind today's call.
async fn stale(
    ctx: &Ctx,
    herd: &Herd,
    d: Option<&Decision>,
    collars: &[Collar],
    paddocks: &[Paddock],
    asked: bool,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let quiet_since = now - Duration::hours(SILENT_HOURS);
    // A collar that has never reported (linked, not on yet) isn't silent: it says so apart.
    let silent = collars.iter().filter(|c| c.last_seen.is_some_and(|t| t < quiet_since)).count();
    if silent > 0 {
        out.push(format!("{silent} of {} collars silent for a day.", collars.len()));
    }
    let waiting = collars.iter().filter(|c| c.last_seen.is_none()).count();
    if waiting > 0 {
        out.push(format!("{waiting} of {} collars not reported yet.", collars.len()));
    }
    if !collars.is_empty() {
        match d.and_then(|d| d.inputs.get("position_source")).and_then(Value::as_str) {
            Some("farm_record") => out.push("Herd position from the farm record, not collars.".into()),
            Some("unknown") => out.push("Herd position unknown.".into()),
            _ => {}
        }
    }
    if let Some(p) = herd.paddock_id.as_deref().and_then(|id| paddocks.iter().find(|p| p.id == id))
        && let Some(days) = imagery_age_days(ctx, &p.id, now).await?
        && days > IMAGERY_OLD_DAYS
    {
        out.push(format!("Imagery for {} is {days} days old.", name(&p.name)));
    }
    if !asked {
        let since = time::to_db(&(now - Duration::days(FIELD_NOTE_DAYS)));
        let notes: i64 = sqlx::query("SELECT COUNT(*) FROM lessons WHERE kind = 'farmer' AND created_at >= ?").bind(since).fetch_one(ctx.db()).await?.get(0);
        if notes == 0 {
            out.push(format!("No field note in {FIELD_NOTE_DAYS} days."));
        }
    }
    Ok(out)
}

/// Days since the paddock's latest imagery was taken, when its latest land
/// report has imagery.
async fn imagery_age_days(ctx: &Ctx, paddock_id: &str, now: DateTime<Utc>) -> anyhow::Result<Option<i64>> {
    let Some(report) = land::latest(ctx, paddock_id).await? else { return Ok(None) };
    let Some(imagery) = land::ok_section(&report, "imagery") else { return Ok(None) };
    let Some(captured) = imagery.get("latest").and_then(|l| l["captured_at"].as_str()) else { return Ok(None) };
    let day = captured.get(..10).and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    Ok(day.map(|d| (now.date_naive() - d).num_days()))
}

/// "07:40" today in farm time, "Wed 07:40" another day.
fn clock(at: DateTime<Utc>, tz: chrono_tz::Tz, now: DateTime<Utc>) -> String {
    let local = at.with_timezone(&tz);
    if local.date_naive() == now.with_timezone(&tz).date_naive() { local.format("%H:%M").to_string() } else { local.format("%a %H:%M").to_string() }
}

/// The record's reasoning as sentences.
pub fn reasons(reasoning: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = reasoning.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '\n' {
            push_sentence(&mut out, &mut cur);
            continue;
        }
        cur.push(c);
        if matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_none_or(|n| n.is_whitespace()) {
            push_sentence(&mut out, &mut cur);
        }
    }
    push_sentence(&mut out, &mut cur);
    out
}

fn push_sentence(out: &mut Vec<String>, cur: &mut String) {
    let s = cur.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.trim_start_matches(['-', '*', '•']).trim();
    if !s.is_empty() {
        out.push(sentence(s));
    }
    cur.clear();
}

/// Trimmed, ending in a full stop unless it already ends a sentence.
fn sentence(s: &str) -> String {
    let s = s.trim();
    if s.ends_with(['.', '!', '?']) { s.to_owned() } else { format!("{s}.") }
}

/// A person's name for a thing, cut to [`MAX_NAME`] characters.
fn name(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= MAX_NAME { s.to_owned() } else { s.chars().take(MAX_NAME).collect::<String>().trim_end().to_owned() }
}

/// The lines as one text: each made GSM-7, then lines given up (highest
/// `drop` first, later lines first among equals) until the text fits `max`;
/// then lines given up are taken back, most needed first, while they fit.
fn fit_text(lines: &[Line], max: usize) -> String {
    let texts: Vec<String> = lines.iter().map(|l| gsm7(&l.text)).collect();
    let mut keep: Vec<bool> = texts.iter().map(|t| !t.is_empty()).collect();
    let len = |keep: &[bool]| {
        let kept: Vec<usize> = (0..texts.len()).filter(|&i| keep[i]).collect();
        kept.iter().map(|&i| gsm7_len(&texts[i])).sum::<usize>() + kept.len().saturating_sub(1)
    };
    let mut order: Vec<usize> = (0..lines.len()).filter(|&i| lines[i].drop > 0 && keep[i]).collect();
    order.sort_by(|&a, &b| lines[b].drop.cmp(&lines[a].drop).then(b.cmp(&a)));
    let mut dropped = Vec::new();
    for &i in &order {
        if len(&keep) <= max {
            break;
        }
        keep[i] = false;
        dropped.push(i);
    }
    for &i in dropped.iter().rev() {
        keep[i] = true;
        if len(&keep) > max {
            keep[i] = false;
        }
    }
    let mut out = String::new();
    let mut used = 0;
    for t in texts.iter().enumerate().filter(|(i, _)| keep[*i]).map(|(_, t)| t) {
        let sep = usize::from(!out.is_empty());
        let room = max.saturating_sub(used + sep);
        if room == 0 {
            break;
        }
        if sep == 1 {
            out.push('\n');
        }
        let (part, n) = take_septets(t, room);
        out.push_str(&part);
        used += sep + n;
    }
    out
}

/// The longest prefix of `s` within `max` septets, and its length.
fn take_septets(s: &str, max: usize) -> (String, usize) {
    let mut out = String::new();
    let mut n = 0;
    for c in s.chars() {
        let w = if GSM7_EXTENDED.contains(c) { 2 } else { 1 };
        if n + w > max {
            break;
        }
        out.push(c);
        n += w;
    }
    (out, n)
}

const GSM7_BASIC: &str = "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞÆæßÉ !\"#¤%&'()*+,-./0123456789:;<=>?¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ§¿abcdefghijklmnopqrstuvwxyzäöñüà";
const GSM7_EXTENDED: &str = "^{}\\[~]|€\u{c}";

/// Every character is in the GSM 03.38 default alphabet or its extension.
pub fn is_gsm7(s: &str) -> bool {
    s.chars().all(|c| GSM7_BASIC.contains(c) || GSM7_EXTENDED.contains(c))
}

/// Length in GSM-7 septets (extension characters count two).
pub fn gsm7_len(s: &str) -> usize {
    s.chars().map(|c| if GSM7_EXTENDED.contains(c) { 2 } else { 1 }).sum()
}

/// `s` in GSM-7: curly quotes, dashes, ellipses and accented letters become
/// their plain forms, other characters (emoji) are left out, and the spaces
/// that leaves are closed up.
pub fn gsm7(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c != '\r' && (GSM7_BASIC.contains(c) || GSM7_EXTENDED.contains(c)) {
            if !(c == ' ' && out.ends_with(' ')) {
                out.push(c);
            }
            continue;
        }
        let plain = match c {
            '‘' | '’' | '‚' | '‛' | '′' | '`' | '´' => "'",
            '“' | '”' | '„' | '‟' | '″' => "\"",
            '–' | '—' | '‒' | '―' | '−' | '‐' | '‑' | '•' | '·' => "-",
            '…' => "...",
            '²' => "2",
            '³' => "3",
            '½' => "1/2",
            '¼' => "1/4",
            '¾' => "3/4",
            '×' => "x",
            '\t' | '\r' | '\u{a0}' | '\u{2002}' | '\u{2003}' | '\u{2009}' | '\u{202f}' => " ",
            'á' | 'â' | 'ã' | 'ā' | 'ă' | 'ą' => "a",
            'Á' | 'Â' | 'Ã' | 'À' | 'Ā' => "A",
            'ç' | 'ć' | 'č' => "c",
            'Ć' | 'Č' => "C",
            'ê' | 'ë' | 'ē' | 'ę' | 'ě' => "e",
            'È' | 'Ê' | 'Ë' | 'Ē' => "E",
            'í' | 'î' | 'ï' | 'ī' => "i",
            'Í' | 'Î' | 'Ï' | 'Ì' => "I",
            'ó' | 'ô' | 'õ' | 'ō' => "o",
            'Ó' | 'Ô' | 'Õ' | 'Ò' => "O",
            'ú' | 'û' | 'ū' => "u",
            'Ú' | 'Û' | 'Ù' => "U",
            'ý' | 'ÿ' => "y",
            'š' => "s",
            'Š' => "S",
            'ž' => "z",
            'Ž' => "Z",
            'ł' => "l",
            'Ł' => "L",
            'ń' | 'ň' => "n",
            'ř' => "r",
            _ => "",
        };
        if !(plain == " " && out.ends_with(' ')) {
            out.push_str(plain);
        }
    }
    out
}

#[derive(Deserialize)]
struct BriefQuery {
    herd_id: Option<String>,
}

/// `GET /api/brief?herd_id=`; the herd is optional when the farm has one.
async fn get_brief(State(ctx): State<Ctx>, Query(q): Query<BriefQuery>) -> ApiResult<Json<Brief>> {
    let herd = resolve_herd(&ctx, q.herd_id.as_deref()).await?;
    Ok(Json(brief(&ctx, &herd, time::now()).await?))
}

pub fn router() -> axum::Router<Ctx> {
    axum::Router::new().route("/api/brief", get(get_brief))
}

/// MCP `get_morning_brief` (read).
pub fn tool() -> ToolSpec {
    ToolSpec {
        name: "get_morning_brief",
        description: "The morning brief for a herd: today's call (STAY, MOVE, NEEDS_INFO), where it stands (waiting for a reply, on a timer, sent and how many collars confirmed), its reasons from the decision record, stale or missing data, and the farm's other brief lines. Plain sentences in the farm's units; `text` is the same as one text message of at most 480 characters.",
        input_schema: json!({
            "type": "object",
            "properties": { "herd_id": { "type": "string", "description": "Herd id. Optional when the farm has one herd." } },
            "required": [],
            "additionalProperties": false,
        }),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run: ToolSpec::run_fn(|c: ToolCall| async move {
            let herd = resolve_herd(&c.ctx, c.args.get("herd_id").and_then(Value::as_str)).await?;
            Ok(serde_json::to_value(brief(&c.ctx, &herd, time::now()).await?).map_err(anyhow::Error::from)?)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_split_into_sentences() {
        assert_eq!(
            reasons("Grass is short near the water. P4 has rested 34 days!\n- Rain due Friday\n\nIs it 30.6 ac? Yes."),
            ["Grass is short near the water.", "P4 has rested 34 days!", "Rain due Friday.", "Is it 30.6 ac?", "Yes."]
        );
        assert!(reasons("  ").is_empty());
    }

    #[test]
    fn texts_become_gsm7() {
        let s = gsm7("It’s “short” — 12°C… café, señora, açaí 🐄\tok");
        assert_eq!(s, "It's \"short\" - 12C... café, señora, acai ok", "é and ñ are GSM-7; ç and í are not");
        assert!(is_gsm7(&s));
        assert_eq!(gsm7_len("a[b]"), 6);
    }

    #[test]
    fn the_window_starts_at_the_last_decision_time() {
        let tz: chrono_tz::Tz = "America/Chicago".parse().unwrap();
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        // 06:00 CDT is 11:00 UTC.
        assert_eq!(window_start(t("2026-09-27T12:00:00Z"), tz, "06:00"), t("2026-09-27T11:00:00Z"));
        assert_eq!(window_start(t("2026-09-27T10:59:00Z"), tz, "06:00"), t("2026-09-26T11:00:00Z"));
        assert_eq!(window_start(t("2026-09-27T11:00:00Z"), tz, "06:00"), t("2026-09-27T11:00:00Z"));
        // After the fall-back change 06:00 is CST (12:00 UTC).
        assert_eq!(window_start(t("2026-11-01T13:00:00Z"), tz, "06:00"), t("2026-11-01T12:00:00Z"));
        // 02:30 doesn't exist on 2027-03-14 in Chicago: the window starts at 03:30.
        assert_eq!(window_start(t("2027-03-14T12:00:00Z"), tz, "02:30"), t("2027-03-14T08:30:00Z"));
        assert_eq!(window_start(t("2026-09-27T12:00:00Z"), tz, "junk"), t("2026-09-27T05:00:00Z"));
    }

    #[test]
    fn the_text_drops_the_least_needed_lines_first() {
        let l = |t: &str, drop| Line { text: t.into(), drop };
        let lines = [
            l("Cows: MOVE to P4 (30.6 ac).", 0),
            l("Reply Y or N.", 0),
            l(&"a".repeat(100), DROP_REASON[0]),
            l(&"b".repeat(100), DROP_REASON[1]),
            l(&"c".repeat(100), DROP_REASON[2]),
            l(&"d".repeat(100), DROP_REASON[3]),
            l("2 of 250 collars silent for a day.", DROP_STALE),
            l("Battery low: 031 14%.", DROP_REGISTRY),
        ];
        let t = fit_text(&lines, TEXT_MAX);
        assert!(gsm7_len(&t) <= TEXT_MAX);
        assert_eq!(t.lines().count(), 7, "{t}");
        assert!(t.contains("ccc") && !t.contains("ddd"), "the 4th reason went first");
        assert!(t.contains("silent") && t.contains("Battery"));
        let t = fit_text(&lines, 380);
        assert_eq!(t.lines().count(), 6, "{t}");
        assert!(!t.contains("ccc") && !t.contains("ddd") && t.contains("bbb"), "then the 3rd");
        // Short lines given up for room come back when a long one had to go.
        let t = fit_text(&lines, 120);
        assert_eq!(t, "Cows: MOVE to P4 (30.6 ac).\nReply Y or N.\n2 of 250 collars silent for a day.\nBattery low: 031 14%.");
        // Nothing droppable left: the kept lines are cut to the limit.
        assert_eq!(fit_text(&lines, 20), "Cows: MOVE to P4 (30");
    }
}
