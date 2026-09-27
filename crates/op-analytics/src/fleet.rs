//! Fleet care (field-ready G): each collar's battery trend from
//! `battery_days` (see [`crate::days`]), when its fit was last checked and
//! when the next check is due. `GET /api/fleet` reads the day tables and the
//! collar rows, never `health`.
//!
//! - `GET /api/fleet?herd_id=&collar_id=` → rows, one per collar
//! - `GET /api/fleet/{collar_id}/fit-checks` → that collar's checks, newest first
//! - `POST /api/fleet/{collar_id}/fit-checks {checked_at?, notes?}` (hand)
//! - `POST /api/fleet/fit-checks {collar_ids, checked_at?, notes?}` (hand; chute day)
//! - `GET/PUT /api/fleet/settings` `{fit_check_days: 30}` (PUT: manager)

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use op_core::tools::{ToolCall, ToolSpec};
use op_core::{ActivityEvent, Actor, ApiError, ApiJson, ApiResult, Ctx, Identity, Role, id, time};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

use crate::days::DayBattery;

/// Id prefix of a fit check (field-ready §1.3).
pub const FIT_CHECK: &str = "fit";
/// Setting key holding [`FleetSettings`].
pub const SETTINGS_KEY: &str = "fleet";
/// Days of `battery_days` the trend reads (today and the six before).
pub const TREND_DAYS: u64 = 7;
/// Days of daily battery a row carries for its sparkline.
pub const DAILY_DAYS: u64 = 14;
/// A day's mean this much above the day before's means the collar was
/// charged in between; the trend starts after it.
const CHARGE_JUMP: f64 = 0.05;
const DAY_MS: f64 = 86_400_000.0;
const MAX_NOTES: usize = 500;
const MAX_BULK: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetSettings {
    /// Days between fit checks.
    pub fit_check_days: u32,
}

impl Default for FleetSettings {
    fn default() -> Self {
        Self { fit_check_days: 30 }
    }
}

pub async fn settings(ctx: &Ctx) -> anyhow::Result<FleetSettings> {
    Ok(ctx.store().get_setting::<FleetSettings>(SETTINGS_KEY).await?.unwrap_or_default())
}

/// A person checked how a collar sits on its animal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FitCheck {
    pub id: String,
    pub collar_id: String,
    pub checked_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<Actor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// One collar's care.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FleetRow {
    pub collar_id: String,
    pub name: String,
    pub herd_id: String,
    /// Tag of the animal wearing it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Latest battery, 0-1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<f64>,
    /// Percentage points a day over the last 7 days (since the last charge),
    /// with two days of data or more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trend_pct_day: Option<f64>,
    /// Days until empty at that trend: only when it falls, with three days
    /// of data or more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_left: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fit_checked_at: Option<DateTime<Utc>>,
    /// The last check (or, never checked, when the collar was added) plus
    /// the fit-check interval.
    pub fit_due_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    pub parked: bool,
    /// Mean battery of each of the last 14 UTC days, oldest first, today
    /// last; null for a day without readings.
    pub daily: Vec<Option<f64>>,
}

/// A battery trend.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Trend {
    pub pct_day: Option<f64>,
    pub days_left: Option<f64>,
}

/// Least-squares slope of y on x.
fn slope_xy(pts: &[(f64, f64)]) -> Option<f64> {
    if pts.len() < 2 {
        return None;
    }
    let n = pts.len() as f64;
    let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n);
    let (mut num, mut den) = (0.0, 0.0);
    for (x, y) in pts {
        num += (x - mx) * (y - my);
        den += (x - mx) * (x - mx);
    }
    (den > 1e-9).then(|| num / den)
}

fn round(v: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (v * m).round() / m
}

/// The trend of a collar's daily means over the last [`TREND_DAYS`] days
/// (ascending by date), back to the last charge. Each day's mean sits at the
/// middle of its readings, so a partial today counts where it is. `now` is
/// the latest battery, else the newest day's last reading.
pub fn trend(days: &[(NaiveDate, DayBattery)], today: NaiveDate, now: Option<f64>) -> Trend {
    let from = today - chrono::Days::new(TREND_DAYS - 1);
    let recent: Vec<&(NaiveDate, DayBattery)> = days.iter().filter(|(d, _)| *d >= from && *d <= today).collect();
    let Some(newest) = recent.last() else { return Trend::default() };
    let mut start = recent.len() - 1;
    while start > 0 && recent[start].1.mean <= recent[start - 1].1.mean + CHARGE_JUMP {
        start -= 1;
    }
    let seg = &recent[start..];
    let pts: Vec<(f64, f64)> = seg.iter().map(|(_, b)| ((b.first_t + b.last_t) as f64 / 2.0 / DAY_MS, b.mean)).collect();
    let Some(slope) = slope_xy(&pts) else { return Trend::default() };
    let days_left = (slope < 0.0 && seg.len() >= 3).then(|| round((now.unwrap_or(newest.1.last).max(0.0) / -slope).max(0.0), 1));
    Trend { pct_day: Some(round(slope * 100.0, 1)), days_left }
}

/// When the next fit check is due.
pub fn fit_due(last_check: Option<DateTime<Utc>>, added: DateTime<Utc>, days: u32) -> DateTime<Utc> {
    last_check.unwrap_or(added) + Duration::days(days as i64)
}

/// The fleet rows for a herd, one collar, or every collar.
pub async fn rows(ctx: &Ctx, herd_id: Option<&str>, collar_id: Option<&str>) -> anyhow::Result<Vec<FleetRow>> {
    let cfg = settings(ctx).await?;
    let now = time::now();
    let today = now.date_naive();

    let mut sql = String::from(
        "SELECT c.id, c.name, c.herd_id, c.battery, c.last_seen, c.parked_at, c.created_at, a.tag FROM collars c LEFT JOIN animals a ON a.collar_id = c.id WHERE 1 = 1",
    );
    if herd_id.is_some() {
        sql.push_str(" AND c.herd_id = ?");
    }
    if collar_id.is_some() {
        sql.push_str(" AND c.id = ?");
    }
    sql.push_str(" ORDER BY c.name, c.id");
    let mut q = sqlx::query(&sql);
    if let Some(h) = herd_id {
        q = q.bind(h);
    }
    if let Some(c) = collar_id {
        q = q.bind(c);
    }
    let collars = q.fetch_all(ctx.db()).await?;
    let ids: HashSet<String> = collars.iter().filter_map(|r| r.try_get::<String, _>(0).ok()).collect();

    let first = (today - chrono::Days::new(DAILY_DAYS - 1)).format("%Y-%m-%d").to_string();
    let mut days: HashMap<String, Vec<(NaiveDate, DayBattery)>> = HashMap::new();
    let bat = sqlx::query_as::<_, (String, String, f64, f64, f64, f64, i64, i64, i64)>(
        "SELECT collar_id, date, min, max, mean, last, n, first_t, last_t FROM battery_days WHERE date >= ? ORDER BY collar_id, date",
    )
    .bind(&first)
    .fetch_all(ctx.db())
    .await?;
    for (collar, date, min, max, mean, last, n, first_t, last_t) in bat {
        if !ids.contains(&collar) {
            continue;
        }
        let Ok(date) = NaiveDate::parse_from_str(&date, "%Y-%m-%d") else { continue };
        days.entry(collar).or_default().push((date, DayBattery { min, max, mean, last, n: n.max(0) as u32, first_t, last_t }));
    }

    let checks: HashMap<String, String> = sqlx::query_as::<_, (String, String)>("SELECT collar_id, MAX(checked_at) FROM collar_fit_checks GROUP BY collar_id")
        .fetch_all(ctx.db())
        .await?
        .into_iter()
        .collect();

    let mut out = Vec::with_capacity(collars.len());
    for r in collars {
        let id: String = r.try_get(0)?;
        let battery: Option<f64> = r.try_get_unchecked::<Option<f64>, _>(3).ok().flatten();
        let added = time::from_db(&r.try_get::<String, _>(6)?)?;
        let checked = checks.get(&id).map(|s| time::from_db(s)).transpose()?;
        let mine = days.remove(&id).unwrap_or_default();
        let t = trend(&mine, today, battery);
        let daily = (0..DAILY_DAYS)
            .map(|i| {
                let d = today - chrono::Days::new(DAILY_DAYS - 1 - i);
                mine.iter().find(|(x, _)| *x == d).map(|(_, b)| round(b.mean, 3))
            })
            .collect();
        out.push(FleetRow {
            name: r.try_get(1)?,
            herd_id: r.try_get(2)?,
            tag: r.try_get(7)?,
            battery,
            trend_pct_day: t.pct_day,
            days_left: t.days_left,
            fit_checked_at: checked,
            fit_due_at: fit_due(checked, added, cfg.fit_check_days),
            last_seen: time::opt_from_db(r.try_get(4)?)?,
            parked: r.try_get::<Option<String>, _>(5)?.is_some(),
            daily,
            collar_id: id,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- fit checks

async fn fit_checks_of(ctx: &Ctx, collar_id: &str) -> anyhow::Result<Vec<FitCheck>> {
    let rows = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>)>(
        "SELECT id, collar_id, checked_at, \"by\", notes FROM collar_fit_checks WHERE collar_id = ? ORDER BY checked_at DESC, id DESC",
    )
    .bind(collar_id)
    .fetch_all(ctx.db())
    .await?;
    rows.into_iter()
        .map(|(id, collar_id, at, by, notes)| {
            Ok(FitCheck { id, collar_id, checked_at: time::from_db(&at)?, by: by.and_then(|b| serde_json::from_str(&b).ok()), notes })
        })
        .collect()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCheck {
    #[serde(default)]
    pub checked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewChecks {
    pub collar_ids: Vec<String>,
    #[serde(default)]
    pub checked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub notes: Option<String>,
}

fn tidy(at: Option<DateTime<Utc>>, notes: Option<String>) -> ApiResult<(DateTime<Utc>, Option<String>)> {
    let now = time::now();
    let at = at.map(|t| time::from_unix_ms(t.timestamp_millis())).unwrap_or(now);
    if at > now + Duration::minutes(5) {
        return Err(ApiError::bad_request("A fit check can't be in the future."));
    }
    let notes = notes.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
    if notes.as_ref().is_some_and(|n| n.chars().count() > MAX_NOTES) {
        return Err(ApiError::bad_request(format!("Notes fit in {MAX_NOTES} characters.")));
    }
    Ok((at, notes))
}

/// Record the same check for these collars (all known, or none recorded).
pub async fn record_checks(ctx: &Ctx, collar_ids: &[String], at: DateTime<Utc>, notes: Option<String>, by: &Actor) -> ApiResult<Vec<FitCheck>> {
    let mut seen = HashSet::new();
    let ids: Vec<&String> = collar_ids.iter().filter(|c| seen.insert(c.as_str())).collect();
    if ids.is_empty() {
        return Err(ApiError::bad_request("Name at least one collar."));
    }
    if ids.len() > MAX_BULK {
        return Err(ApiError::bad_request(format!("At most {MAX_BULK} collars at once.")));
    }
    let mut names: Vec<String> = Vec::new();
    for id in &ids {
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT c.name, a.tag FROM collars c LEFT JOIN animals a ON a.collar_id = c.id WHERE c.id = ?")
                .bind(id)
                .fetch_optional(ctx.db())
                .await?;
        match row {
            Some((name, tag)) => names.push(tag.unwrap_or(name)),
            None => return Err(ApiError::not_found(format!("No collar {id}."))),
        }
    }
    let by_json = serde_json::to_string(by).map_err(anyhow::Error::from)?;
    let mut out = Vec::with_capacity(ids.len());
    let mut tx = ctx.db().begin().await?;
    for id in &ids {
        let c = FitCheck { id: id::new_id(FIT_CHECK), collar_id: (*id).clone(), checked_at: at, by: Some(by.clone()), notes: notes.clone() };
        sqlx::query("INSERT INTO collar_fit_checks (id, collar_id, checked_at, \"by\", notes) VALUES (?, ?, ?, ?, ?)")
            .bind(&c.id)
            .bind(&c.collar_id)
            .bind(time::to_db(&c.checked_at))
            .bind(&by_json)
            .bind(&c.notes)
            .execute(&mut *tx)
            .await?;
        out.push(c);
    }
    tx.commit().await?;

    let title = match names.as_slice() {
        [one] => format!("Fit checked: {one}"),
        many => format!("Fit checked: {} collars", many.len()),
    };
    let e = ActivityEvent {
        id: id::new_id(id::EVENT),
        kind: "fleet.fit_checked".into(),
        source: "farmer".into(),
        occurred_at: at,
        recorded_at: time::now(),
        title,
        body: notes,
        payload: json!({ "collar_ids": ids, "by": by.name }),
        targets: ids.iter().map(|c| ("collar".to_owned(), (*c).clone())).collect(),
    };
    if let Err(err) = ctx.store().record_event(&e).await {
        tracing::warn!("activity log: {err:#}");
    }
    Ok(out)
}

// ---------------------------------------------------------------- HTTP

pub fn router() -> axum::Router<Ctx> {
    axum::Router::new()
        .route("/api/fleet", get(get_fleet))
        .route("/api/fleet/settings", get(get_settings).put(put_settings))
        .route("/api/fleet/fit-checks", post(post_bulk))
        .route("/api/fleet/{collar_id}/fit-checks", get(get_checks).post(post_check))
}

#[derive(Debug, Default, Deserialize)]
pub struct Params {
    pub herd_id: Option<String>,
    pub collar_id: Option<String>,
}

async fn get_fleet(State(ctx): State<Ctx>, Query(p): Query<Params>) -> ApiResult<Json<Vec<FleetRow>>> {
    let herd = p.herd_id.as_deref().filter(|s| !s.is_empty());
    let collar = p.collar_id.as_deref().filter(|s| !s.is_empty());
    Ok(Json(rows(&ctx, herd, collar).await?))
}

async fn get_settings(State(ctx): State<Ctx>) -> ApiResult<Json<FleetSettings>> {
    Ok(Json(settings(&ctx).await?))
}

async fn put_settings(State(ctx): State<Ctx>, ApiJson(s): ApiJson<FleetSettings>) -> ApiResult<Json<FleetSettings>> {
    if !(1..=365).contains(&s.fit_check_days) {
        return Err(ApiError::bad_request("Fit checks are 1 to 365 days apart."));
    }
    ctx.store().set_setting(SETTINGS_KEY, &s).await?;
    Ok(Json(s))
}

async fn collar_exists(ctx: &Ctx, id: &str) -> ApiResult<()> {
    match sqlx::query_scalar::<_, i64>("SELECT 1 FROM collars WHERE id = ?").bind(id).fetch_optional(ctx.db()).await? {
        Some(_) => Ok(()),
        None => Err(ApiError::not_found(format!("No collar {id}."))),
    }
}

async fn get_checks(State(ctx): State<Ctx>, Path(collar_id): Path<String>) -> ApiResult<Json<Vec<FitCheck>>> {
    collar_exists(&ctx, &collar_id).await?;
    Ok(Json(fit_checks_of(&ctx, &collar_id).await?))
}

/// The body is optional: no body is a check now with no notes.
async fn post_check(State(ctx): State<Ctx>, identity: Identity, Path(collar_id): Path<String>, body: Bytes) -> ApiResult<(StatusCode, Json<FitCheck>)> {
    let NewCheck { checked_at, notes } = if body.iter().all(u8::is_ascii_whitespace) {
        NewCheck::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("Can't read that check: {e}.")))?
    };
    let (at, notes) = tidy(checked_at, notes)?;
    let mut made = record_checks(&ctx, std::slice::from_ref(&collar_id), at, notes, &identity.actor()).await?;
    Ok((StatusCode::CREATED, Json(made.remove(0))))
}

async fn post_bulk(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<NewChecks>) -> ApiResult<(StatusCode, Json<Vec<FitCheck>>)> {
    let (at, notes) = tidy(b.checked_at, b.notes)?;
    Ok((StatusCode::CREATED, Json(record_checks(&ctx, &b.collar_ids, at, notes, &identity.actor()).await?)))
}

// ---------------------------------------------------------------- MCP

pub fn tool() -> ToolSpec {
    ToolSpec {
        name: "get_fleet",
        description: "Collar care: for each collar its battery (0-1), the battery trend in percentage points per day over the last 7 days since its last charge (with 2+ days of data), days until empty at that trend (only when falling, with 3+ days), the mean battery of each of the last 14 days (oldest first), when its fit on the animal was last checked and when the next check is due (fit_check_days apart), when it last reported, and whether it is parked (on a shelf or charger). tag is the animal wearing it.",
        input_schema: json!({
            "type": "object",
            "properties": { "herd_id": { "type": "string", "description": "Only this herd's collars." } },
            "required": [],
            "additionalProperties": false
        }),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run: ToolSpec::run_fn(|c: ToolCall| async move { tool_run(&c.ctx, &c.args).await }),
    }
}

async fn tool_run(ctx: &Ctx, args: &Value) -> ApiResult<Value> {
    let herd = args.get("herd_id").and_then(Value::as_str).filter(|s| !s.is_empty());
    if let Some(h) = herd {
        if ctx.store().get_herd(h).await?.is_none() {
            return Err(ApiError::not_found(format!("No herd {h}.")));
        }
    }
    let collars = rows(ctx, herd, None).await?;
    Ok(json!({ "fit_check_days": settings(ctx).await?.fit_check_days, "collars": collars }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    fn day(n: i64, mean: f64) -> (NaiveDate, DayBattery) {
        let t0 = 1_790_035_200_000 + n * DAY;
        (
            crate::range::date_of(t0),
            DayBattery { min: mean - 0.02, max: mean + 0.02, mean, last: mean - 0.02, n: 24, first_t: t0, last_t: t0 + DAY - 3_600_000 },
        )
    }

    #[test]
    fn a_steady_drain() {
        let days: Vec<_> = (0..5).map(|i| day(i, 0.9 - 0.03 * i as f64)).collect();
        let today = days[4].0;
        let t = trend(&days, today, Some(0.78));
        assert_eq!(t.pct_day, Some(-3.0));
        assert_eq!(t.days_left, Some(26.0));
    }

    #[test]
    fn two_days_give_a_trend_but_no_days_left() {
        let days = vec![day(0, 0.9), day(1, 0.88)];
        let t = trend(&days, days[1].0, None);
        assert_eq!(t.pct_day, Some(-2.0));
        assert_eq!(t.days_left, None);
        assert_eq!(trend(&days[..1], days[0].0, None), Trend::default());
    }

    #[test]
    fn the_trend_starts_after_a_charge() {
        let days = vec![day(0, 0.5), day(1, 0.45), day(2, 0.95), day(3, 0.93), day(4, 0.91), day(5, 0.89)];
        let t = trend(&days, days[5].0, None);
        assert_eq!(t.pct_day, Some(-2.0));
        assert!(t.days_left.is_some());
        // A charge on the last day leaves one day: no trend yet.
        let days = vec![day(0, 0.5), day(1, 0.45), day(2, 0.95)];
        assert_eq!(trend(&days, days[2].0, None), Trend::default());
    }

    #[test]
    fn only_the_last_week_counts() {
        let mut days: Vec<_> = (0..10).map(|i| day(i, 0.95 - 0.01 * i as f64)).collect();
        // Ten days ago it dropped fast; the last seven are steady.
        days[0].1.mean = 0.99;
        days[1].1.mean = 0.97;
        let t = trend(&days, days[9].0, None);
        assert_eq!(t.pct_day, Some(-1.0));
        // Rising (solar) has no days left.
        let up: Vec<_> = (0..4).map(|i| day(i, 0.5 + 0.01 * i as f64)).collect();
        let t = trend(&up, up[3].0, None);
        assert_eq!(t.pct_day, Some(1.0));
        assert_eq!(t.days_left, None);
    }

    #[test]
    fn due_dates() {
        let added = time::from_db("2026-09-01T00:00:00Z").unwrap();
        assert_eq!(fit_due(None, added, 30), time::from_db("2026-10-01T00:00:00Z").unwrap());
        let checked = time::from_db("2026-09-20T08:00:00Z").unwrap();
        assert_eq!(fit_due(Some(checked), added, 14), time::from_db("2026-10-04T08:00:00Z").unwrap());
    }
}
