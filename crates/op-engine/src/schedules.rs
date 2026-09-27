//! Strip schedules in farm time (field-ready §2.14): the `/api/schedules*`
//! routes, the MCP tools `get_schedule` and `schedule_strips`, the brief's
//! `schedule` line, the decision context's `schedule`, and HOLD.
//!
//! op-ingest stores schedules and stages their moves; everything that works
//! a time out from the cadence is here, because the farm's time zone is: each
//! occurrence is the cadence's time on its own farm-local date, so "daily
//! 07:00" opens at 07:00 local on both sides of a DST change.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Days, Duration, LocalResult, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use op_core::brief::BriefLine;
use op_core::schedule::{BackFence, Cadence, Schedule, ScheduleStatus, ScheduledMove};
use op_core::tools::{ToolCall, ToolSpec};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, DbEnum, Herd, Identity, Polygon, Role, time};
use op_ingest::schedule::{self as sched, NewSchedule};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{brief, layouts, strips};

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/schedules", get(list_route).post(create_route))
        .route("/api/schedules/preview", post(preview_route))
        .route("/api/schedules/{id}", get(get_route))
        .route("/api/schedules/{id}/moves", get(moves_route))
        .route("/api/schedules/{id}/pause", post(pause_route))
        .route("/api/schedules/{id}/resume", post(resume_route))
        .route("/api/schedules/{id}/hold", post(hold_route))
        .route("/api/schedules/{id}/skip", post(skip_route))
        .route("/api/schedules/{id}/move-now", post(move_now_route))
        .route("/api/schedules/{id}/time", post(time_route))
        .route("/api/schedules/{id}/end", post(end_route))
}

// ---- farm time ----------------------------------------------------------------------------------

pub async fn farm_tz(ctx: &Ctx) -> anyhow::Result<Tz> {
    Ok(ctx.store().get_farm().await?.and_then(|f| f.timezone.parse().ok()).unwrap_or(chrono_tz::UTC))
}

/// A farm-local time as UTC: the earlier of two in a fall-back hour, and in
/// a spring-forward gap the first time after it.
pub fn local_to_utc(tz: Tz, local: NaiveDateTime) -> DateTime<Utc> {
    for step in 0..=6 {
        match tz.from_local_datetime(&(local + Duration::minutes(30 * step))) {
            LocalResult::Single(t) => return t.with_timezone(&Utc),
            LocalResult::Ambiguous(a, _) => return a.with_timezone(&Utc),
            LocalResult::None => continue,
        }
    }
    Utc.from_utc_datetime(&local)
}

/// Occurrence `n` of `cadence` from `starts_at` (occurrence 0): the cadence's
/// time on the farm-local date `n × every_days` days after the start's.
pub fn occurrences(tz: Tz, starts_at: DateTime<Utc>, cadence: &Cadence) -> impl Fn(u32) -> DateTime<Utc> + Send + Sync + use<> {
    let day = starts_at.with_timezone(&tz).date_naive();
    let at = cadence.time().unwrap_or(NaiveTime::MIN);
    let every = u64::from(cadence.every_days.max(1));
    move |n: u32| {
        if n == 0 {
            return starts_at;
        }
        let date = day.checked_add_days(Days::new(every * u64::from(n))).unwrap_or(day);
        local_to_utc(tz, date.and_time(at))
    }
}

/// The next time after `now` (by at least a minute) that the farm's clock
/// reads the cadence's time.
pub fn next_at(tz: Tz, now: DateTime<Utc>, cadence: &Cadence) -> DateTime<Utc> {
    let at = cadence.time().unwrap_or(NaiveTime::MIN);
    let today = now.with_timezone(&tz).date_naive();
    (0..3)
        .filter_map(|d| today.checked_add_days(Days::new(d)))
        .map(|d| local_to_utc(tz, d.and_time(at)))
        .find(|t| *t > now + Duration::minutes(1))
        .unwrap_or(now + Duration::days(1))
}

/// "07:00" today in farm time, "Wed 07:00" another day.
pub fn when(at: DateTime<Utc>, tz: Tz, now: DateTime<Utc>) -> String {
    let local = at.with_timezone(&tz);
    if local.date_naive() == now.with_timezone(&tz).date_naive() { local.format("%H:%M").to_string() } else { local.format("%a %H:%M").to_string() }
}

async fn occ_for(ctx: &Ctx, s: &Schedule) -> anyhow::Result<impl Fn(u32) -> DateTime<Utc> + Send + Sync + use<>> {
    Ok(occurrences(farm_tz(ctx).await?, s.starts_at, &s.cadence))
}

// ---- making one ---------------------------------------------------------------------------------

/// `POST /api/schedules` (and `/preview`). Strips come from the body, else
/// the saved layout; the paddock from the body, the layout, else the herd's.
/// `starts_at` defaults to the next time the farm's clock reads `cadence.at`;
/// `next_index` to the strip after the one the herd's boundary covers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewBody {
    pub herd_id: String,
    #[serde(default)]
    pub layout_id: Option<String>,
    #[serde(default)]
    pub paddock_id: Option<String>,
    #[serde(default)]
    pub strips: Option<Vec<Polygon>>,
    #[serde(default)]
    pub next_index: Option<u32>,
    #[serde(default)]
    pub cadence: Option<Cadence>,
    #[serde(default)]
    pub starts_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub back_fence: Option<BackFence>,
}

/// The schedule as it would be made, and its moves.
#[derive(Debug, Clone, Serialize)]
pub struct Planned {
    pub schedule: Schedule,
    pub moves: Vec<ScheduledMove>,
}

async fn new_schedule(ctx: &Ctx, b: NewBody, identity: &Identity) -> ApiResult<(NewSchedule, Tz)> {
    let herd = ctx.store().get_herd(&b.herd_id).await?.ok_or_else(|| ApiError::not_found("No such herd."))?;
    let layout = match &b.layout_id {
        Some(id) => Some(layouts::get_layout(ctx, id).await?.ok_or_else(|| ApiError::not_found("No such layout."))?),
        None => None,
    };
    let strips = b.strips.or_else(|| layout.as_ref().map(|l| l.strips.clone())).ok_or_else(|| ApiError::bad_request("Give the strips or a saved layout."))?;
    let paddock_id = b
        .paddock_id
        .or_else(|| layout.as_ref().map(|l| l.paddock_id.clone()))
        .or(herd.paddock_id.clone())
        .ok_or_else(|| ApiError::bad_request("Say which paddock the strips are in."))?;
    if ctx.store().get_paddock(&paddock_id).await?.is_none() {
        return Err(ApiError::not_found("No such paddock."));
    }
    let cadence = b.cadence.unwrap_or_default();
    cadence.check().map_err(ApiError::bad_request)?;
    let tz = farm_tz(ctx).await?;
    let starts_at = b.starts_at.unwrap_or_else(|| next_at(tz, time::now(), &cadence));
    Ok((
        NewSchedule {
            herd_id: herd.id,
            paddock_id,
            layout_id: b.layout_id,
            strips,
            next_index: b.next_index,
            cadence,
            starts_at,
            back_fence: b.back_fence.unwrap_or_default(),
            created_by: identity.actor(),
        },
        tz,
    ))
}

pub async fn create(ctx: &Ctx, b: NewBody, identity: &Identity) -> ApiResult<Schedule> {
    let (n, tz) = new_schedule(ctx, b, identity).await?;
    let occ = occurrences(tz, whole_seconds(n.starts_at), &n.cadence);
    sched::create(ctx, n, &occ).await
}

pub async fn preview(ctx: &Ctx, b: NewBody, identity: &Identity) -> ApiResult<Planned> {
    let (n, tz) = new_schedule(ctx, b, identity).await?;
    let occ = occurrences(tz, whole_seconds(n.starts_at), &n.cadence);
    let (schedule, moves) = sched::preview(ctx, n, &occ).await?;
    Ok(Planned { schedule, moves })
}

/// Stored times are whole seconds, like the collars' clocks.
fn whole_seconds(t: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::Timelike;
    t.with_nanosecond(0).unwrap_or(t)
}

// ---- HOLD ---------------------------------------------------------------------------------------

/// Hold the herd's running schedule (see [`sched::hold`]). 409 without one.
pub async fn hold_herd(ctx: &Ctx, herd_id: &str) -> ApiResult<Schedule> {
    let s = sched::running(ctx, herd_id).await?.ok_or_else(|| ApiError::conflict("This herd has no strip schedule to hold."))?;
    let occ = occ_for(ctx, &s).await?;
    sched::hold(ctx, &s.id, &occ).await
}

/// The herd's schedule while it is active (a HOLD needs one).
pub async fn active(ctx: &Ctx, herd_id: &str) -> anyhow::Result<Option<Schedule>> {
    Ok(sched::running(ctx, herd_id).await?.filter(|s| s.status == ScheduleStatus::Active))
}

// ---- what it says -------------------------------------------------------------------------------

/// The next open and how far it has reached the collars.
#[derive(Debug, Clone, Serialize)]
pub struct Next {
    /// 1-based, as people count strips.
    pub strip: u32,
    pub of: u32,
    pub opens_at: DateTime<Utc>,
    /// "07:00" today, "Wed 07:00" another day (farm time).
    pub opens: String,
    /// Collars holding it (stored or already applied), once it is staged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stored: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collars: Option<u32>,
}

pub async fn next(ctx: &Ctx, s: &Schedule, tz: Tz, now: DateTime<Utc>) -> anyhow::Result<Option<Next>> {
    let Some((m, count)) = sched::next_open(ctx, s).await? else { return Ok(None) };
    Ok(Some(Next {
        strip: m.index + 1,
        of: s.strips.len() as u32,
        opens_at: m.at,
        opens: when(m.at, tz, now),
        stored: count.as_ref().map(|c| c.applied + c.stored),
        collars: count.as_ref().map(|c| c.collars),
    }))
}

/// `schedule` in the decision context (null without one): strip k of n, the
/// next open, what today's strip holds, and how many collars store the next.
pub async fn context(ctx: &Ctx, herd: &Herd, now: DateTime<Utc>) -> anyhow::Result<Value> {
    let Some(s) = sched::running(ctx, &herd.id).await? else { return Ok(Value::Null) };
    let tz = farm_tz(ctx).await?;
    let current = sched::current_strip(ctx, &s).await?;
    let mut v = json!({
        "id": s.id,
        "status": s.status,
        "paddock_id": s.paddock_id,
        "strips": s.strips.len(),
        "strip": current.map(|c| c + 1),
        "cadence": s.cadence,
        "back_fence": s.back_fence.enabled,
        "next": next(ctx, &s, tz, now).await?,
        "rule": "STAY keeps the schedule (the next strip opens on time); HOLD repeats today's strip one more cadence; MOVE to another paddock ends the schedule.",
    });
    if let (Some(k), Some(paddock)) = (current, ctx.store().get_paddock(&s.paddock_id).await?)
        && let Some(strip) = s.strips.get(k as usize)
    {
        let area = strip.area_ha();
        let mut today = json!({ "area_ha": (area * 1000.0).round() / 1000.0 });
        if let Some(f) = strips::paddock_forage(ctx, &paddock).await? {
            let (_, au) = strips::feeding(Some(herd), None);
            today["forage_kg_dm"] = json!((f.kg_dm_per_ha * area).round());
            if let Some(d) = strips::strip_days(f.kg_dm_per_ha, area, au) {
                today["days"] = json!(d);
            }
        }
        v["today"] = today;
    }
    Ok(v)
}

/// The `schedule` brief line (order 20): "Strip 4 of 12 opens 07:00,
/// 248/250 stored"; "Schedule paused before strip 4 of 12."
pub fn register_brief(ctx: &Ctx) {
    if ctx.brief_lines().has("schedule") {
        return;
    }
    ctx.brief_lines().register(BriefLine {
        name: "schedule",
        order: 20,
        run: BriefLine::run_fn(|ctx, herd_id, now| async move { Ok(brief_line(&ctx, &herd_id, now).await?.into_iter().collect()) }),
    });
}

pub async fn brief_line(ctx: &Ctx, herd_id: &str, now: DateTime<Utc>) -> anyhow::Result<Option<String>> {
    let Some(s) = sched::running(ctx, herd_id).await? else { return Ok(None) };
    let tz = farm_tz(ctx).await?;
    let Some(n) = next(ctx, &s, tz, now).await? else { return Ok(None) };
    if s.status == ScheduleStatus::Paused {
        return Ok(Some(format!("Schedule paused before strip {} of {}.", n.strip, n.of)));
    }
    let stored = match (n.stored, n.collars) {
        (Some(st), Some(c)) if c > 0 => format!(", {st}/{c} stored"),
        _ => String::new(),
    };
    Ok(Some(format!("Strip {} of {} opens {}{stored}", n.strip, n.of, n.opens)))
}

// ---- routes -------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct ListQuery {
    herd_id: Option<String>,
    /// `running` = active or paused.
    status: Option<String>,
}

async fn list_route(State(ctx): State<Ctx>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Schedule>>> {
    let mut all = sched::list(&ctx, q.herd_id.as_deref()).await?;
    match q.status.as_deref() {
        None | Some("") => {}
        Some("running") => all.retain(|s| s.status != ScheduleStatus::Done),
        Some(st) => {
            let want = ScheduleStatus::from_db(st).map_err(|_| ApiError::bad_request("status is active, paused, done or running."))?;
            all.retain(|s| s.status == want);
        }
    }
    Ok(Json(all))
}

async fn create_route(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<NewBody>) -> ApiResult<(StatusCode, Json<Schedule>)> {
    identity.require(Role::Manager)?;
    Ok((StatusCode::CREATED, Json(create(&ctx, b, &identity).await?)))
}

async fn preview_route(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<NewBody>) -> ApiResult<Json<Planned>> {
    identity.require(Role::Manager)?;
    Ok(Json(preview(&ctx, b, &identity).await?))
}

async fn found(ctx: &Ctx, id: &str) -> ApiResult<Schedule> {
    sched::get(ctx, id).await?.ok_or_else(|| ApiError::not_found("No such schedule."))
}

async fn get_route(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<Schedule>> {
    Ok(Json(found(&ctx, &id).await?))
}

async fn moves_route(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<Vec<ScheduledMove>>> {
    found(&ctx, &id).await?;
    Ok(Json(sched::moves(&ctx, &id).await?))
}

async fn pause_route(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>) -> ApiResult<Json<Schedule>> {
    identity.require(Role::Manager)?;
    Ok(Json(sched::pause(&ctx, &id).await?))
}

async fn resume_route(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>) -> ApiResult<Json<Schedule>> {
    identity.require(Role::Manager)?;
    let s = found(&ctx, &id).await?;
    let occ = occ_for(&ctx, &s).await?;
    Ok(Json(sched::resume(&ctx, &id, &occ).await?))
}

async fn hold_route(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>) -> ApiResult<Json<Schedule>> {
    identity.require(Role::Manager)?;
    let s = found(&ctx, &id).await?;
    let occ = occ_for(&ctx, &s).await?;
    Ok(Json(sched::hold(&ctx, &id, &occ).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StripBody {
    /// The strip (0-based, as in `moves`).
    index: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimeBody {
    index: u32,
    at: DateTime<Utc>,
}

async fn skip_route(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>, ApiJson(b): ApiJson<StripBody>) -> ApiResult<Json<Schedule>> {
    identity.require(Role::Manager)?;
    Ok(Json(sched::skip(&ctx, &id, b.index).await?))
}

async fn move_now_route(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>) -> ApiResult<Json<Schedule>> {
    identity.require(Role::Manager)?;
    Ok(Json(sched::move_now(&ctx, &id).await?))
}

async fn time_route(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>, ApiJson(b): ApiJson<TimeBody>) -> ApiResult<Json<Schedule>> {
    identity.require(Role::Manager)?;
    Ok(Json(sched::set_time(&ctx, &id, b.index, b.at).await?))
}

async fn end_route(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>) -> ApiResult<Json<Schedule>> {
    identity.require(Role::Manager)?;
    Ok(Json(sched::end(&ctx, &id).await?))
}

// ---- MCP ----------------------------------------------------------------------------------------

fn obj(props: Value) -> Value {
    json!({ "type": "object", "properties": props, "required": [], "additionalProperties": false })
}

fn herd_prop() -> Value {
    json!({ "type": "string", "description": "Herd id. Optional when the farm has one herd." })
}

/// MCP `get_schedule` (read).
pub fn get_schedule_spec() -> ToolSpec {
    let herd = herd_prop();
    ToolSpec {
        name: "get_schedule",
        description: "A herd's strip schedule: the strips, the cadence (every N days at a farm-local time), the back fence, and every move (each strip's open and back-fence close steps) with its time, state (planned, staged on the collars, done, skipped) and boundary version; `next` is the next open and how many collars hold it. Null schedule when the herd has none running.",
        input_schema: obj(json!({ "herd_id": herd })),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run: ToolSpec::run_fn(|c: ToolCall| async move { get_schedule_tool(&c.ctx, &c.args).await }),
    }
}

/// MCP `schedule_strips` (Manager).
pub fn schedule_strips_spec() -> ToolSpec {
    let herd = herd_prop();
    ToolSpec {
        name: "schedule_strips",
        description: "Put a herd on a strip schedule: strips from a saved layout (layout_id) or cut across the herd's paddock (orientation_deg with count, width_m or days per strip), opened every every_days days at `at` (farm time, HH:MM) from starts_at (default: the next time the clock reads `at`), with a back fence closing the old strip behind the herd. Each open is staged on the collars ahead of time, so strips open even when the server is away. The herd needs a boundary already; the first strip is the one after the strip it is on.",
        input_schema: obj(json!({
            "herd_id": herd,
            "layout_id": { "type": "string" },
            "orientation_deg": { "type": "number", "description": "Bearing the strips advance toward; 0 = north." },
            "count": { "type": "integer", "minimum": 1, "maximum": 200 },
            "width_m": { "type": "number", "exclusiveMinimum": 0 },
            "days": { "type": "number", "exclusiveMinimum": 0, "description": "Days of grazing per strip." },
            "every_days": { "type": "integer", "minimum": 1, "maximum": 60 },
            "at": { "type": "string", "description": "Open time, HH:MM farm time." },
            "starts_at": { "type": "string", "description": "RFC 3339 time of the first open." },
            "back_fence": { "type": "boolean" },
            "next_index": { "type": "integer", "minimum": 0, "description": "First strip to open, 0-based." }
        })),
        read: false,
        brain: false,
        min_role: Role::Manager,
        run: ToolSpec::run_fn(|c: ToolCall| async move { schedule_strips_tool(&c.ctx, &c.args, &c.identity).await }),
    }
}

async fn get_schedule_tool(ctx: &Ctx, a: &Value) -> ApiResult<Value> {
    let herd = brief::resolve_herd(ctx, a.get("herd_id").and_then(Value::as_str)).await?;
    let Some(s) = sched::running(ctx, &herd.id).await? else { return Ok(json!({ "herd_id": herd.id, "schedule": null, "moves": [] })) };
    let tz = farm_tz(ctx).await?;
    let now = time::now();
    let moves = sched::moves(ctx, &s.id).await?;
    Ok(json!({ "herd_id": herd.id, "next": next(ctx, &s, tz, now).await?, "moves": moves, "schedule": s }))
}

async fn schedule_strips_tool(ctx: &Ctx, a: &Value, identity: &Identity) -> ApiResult<Value> {
    let herd = brief::resolve_herd(ctx, a.get("herd_id").and_then(Value::as_str)).await?;
    let num = |k: &str| a.get(k).and_then(Value::as_f64);
    let layout_id = a.get("layout_id").and_then(Value::as_str).map(str::to_owned);
    let mut paddock_id = None;
    let strips = if layout_id.is_some() {
        None
    } else {
        let pid = herd.paddock_id.clone().ok_or_else(|| ApiError::bad_request("The herd isn't in a paddock; give a layout_id."))?;
        let paddock = ctx.store().get_paddock(&pid).await?.ok_or_else(|| ApiError::not_found("No such paddock."))?;
        let params = strips::StripParams {
            orientation_deg: num("orientation_deg").unwrap_or(0.0),
            width_m: num("width_m"),
            count: a.get("count").and_then(Value::as_u64).map(|c| c as u32),
            days: num("days"),
            ..Default::default()
        };
        let p = strips::preview(ctx, &paddock, Some(&herd), &params).await?;
        paddock_id = Some(pid);
        Some(p.strips.into_iter().map(|s| s.geometry).collect())
    };
    let mut cadence = Cadence::default();
    if let Some(d) = a.get("every_days").and_then(Value::as_u64) {
        cadence.every_days = d as u32;
    }
    if let Some(t) = a.get("at").and_then(Value::as_str) {
        cadence.at = t.to_owned();
    }
    let starts_at = match a.get("starts_at").and_then(Value::as_str) {
        Some(t) => Some(DateTime::parse_from_rfc3339(t).map_err(|_| ApiError::bad_request("starts_at must be an RFC 3339 time."))?.with_timezone(&Utc)),
        None => None,
    };
    let back_fence = a.get("back_fence").and_then(Value::as_bool).map(|on| BackFence { enabled: on, ..BackFence::default() });
    let body = NewBody {
        herd_id: herd.id.clone(),
        layout_id,
        paddock_id,
        strips,
        next_index: a.get("next_index").and_then(Value::as_u64).map(|n| n as u32),
        cadence: Some(cadence),
        starts_at,
        back_fence,
    };
    let s = create(ctx, body, identity).await?;
    let tz = farm_tz(ctx).await?;
    let next = next(ctx, &s, tz, time::now()).await?;
    let message = match &next {
        Some(n) => format!("Scheduled: strip {} of {} opens {}.", n.strip, n.of, n.opens),
        None => "Scheduled.".into(),
    };
    Ok(json!({ "schedule": s, "next": next, "message": message }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn daily_seven_oclock_holds_across_the_fall_back_change() {
        let tz: Tz = "America/Chicago".parse().unwrap();
        let c = Cadence { every_days: 1, at: "07:00".into() };
        // Starts Saturday 2026-10-31 07:00 CDT (12:00 UTC).
        let occ = occurrences(tz, t("2026-10-31T12:00:00Z"), &c);
        assert_eq!(occ(0), t("2026-10-31T12:00:00Z"));
        // Sunday 2026-11-01 is CST: 07:00 local is 13:00 UTC.
        assert_eq!(occ(1), t("2026-11-01T13:00:00Z"));
        assert_eq!(occ(2), t("2026-11-02T13:00:00Z"));
        for n in 0..5 {
            assert_eq!(occ(n).with_timezone(&tz).format("%H:%M").to_string(), "07:00");
        }
    }

    #[test]
    fn a_time_the_clocks_skip_opens_just_after() {
        let tz: Tz = "America/Chicago".parse().unwrap();
        let c = Cadence { every_days: 1, at: "02:30".into() };
        let occ = occurrences(tz, t("2027-03-13T08:30:00Z"), &c);
        // 02:30 doesn't exist on 2027-03-14: 03:00 CDT.
        assert_eq!(occ(1).with_timezone(&tz).format("%m-%d %H:%M").to_string(), "03-14 03:00");
        // Every other day.
        let c2 = Cadence { every_days: 2, at: "07:00".into() };
        let o2 = occurrences(tz, t("2026-09-28T12:00:00Z"), &c2);
        assert_eq!(o2(1), t("2026-09-30T12:00:00Z"));
    }

    #[test]
    fn the_next_start_is_the_next_time_the_clock_reads_it() {
        let tz: Tz = "America/Chicago".parse().unwrap();
        let c = Cadence { every_days: 1, at: "07:00".into() };
        assert_eq!(next_at(tz, t("2026-09-27T11:00:00Z"), &c), t("2026-09-27T12:00:00Z"));
        assert_eq!(next_at(tz, t("2026-09-27T12:30:00Z"), &c), t("2026-09-28T12:00:00Z"));
        assert_eq!(when(t("2026-09-28T12:00:00Z"), tz, t("2026-09-27T20:00:00Z")), "Mon 07:00");
        assert_eq!(when(t("2026-09-27T21:00:00Z"), tz, t("2026-09-27T20:00:00Z")), "16:00");
    }
}
