//! Grazing signals for a herd and each paddock, from the record: collar fixes
//! and cues, applied decisions, paddock records and cached land reports.
//! Port of the kit's `DecisionEngine.compute_signals`. Anything without a real
//! source is left null.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Duration, Utc};
use op_core::{Ctx, DbEnum, Decision, DecisionAction, DecisionStatus, Herd, Paddock, time};
use serde_json::{Map, Value, json};
use sqlx::Row;

use crate::{calc, heights, land};

/// The decision window: signals look back one day.
pub const WINDOW_HOURS: i64 = 24;
/// How far back "last grazed" looks in the fix record.
const REST_LOOKBACK_DAYS: i64 = 120;
/// Fix rows read per query; larger ranges are sampled evenly.
pub const MAX_FIX_ROWS: i64 = 20_000;

pub struct FixSample {
    pub collar_id: String,
    pub t: i64,
    pub paddock_id: Option<String>,
}

/// Paddocks with a real mapped area (kit `has_real_geometry`).
pub fn real_area(p: &Paddock) -> Option<f64> {
    (p.area_ha >= 0.05).then_some(p.area_ha)
}

pub fn paddock_at<'a>(paddocks: &'a [Paddock], point: [f64; 2]) -> Option<&'a Paddock> {
    paddocks.iter().find(|p| p.geometry.contains(point))
}

/// Fixes for a herd in `[from, to)`, at most `MAX_FIX_ROWS`, each with the
/// paddock it fell in. A window holding no more than that is read whole; a
/// larger one (a day of 250 collars is 4.3 M fixes) is sampled evenly in time
/// per collar, each collar's first fix in each of its buckets, one index seek
/// per bucket: the window is never walked.
pub async fn herd_fixes(ctx: &Ctx, herd_id: &str, from: DateTime<Utc>, to: DateTime<Utc>, paddocks: &[Paddock]) -> anyhow::Result<Vec<FixSample>> {
    let (f, t) = (time::unix_ms(&from), time::unix_ms(&to));
    let n: i64 = sqlx::query_scalar(COUNT_UP_TO_SQL).bind(herd_id).bind(f).bind(t).bind(MAX_FIX_ROWS + 1).fetch_one(ctx.db()).await?;
    let rows = if n <= MAX_FIX_ROWS {
        sqlx::query("SELECT collar_id, t, lon, lat, paddock_id FROM fixes WHERE herd_id = ? AND t >= ? AND t < ? ORDER BY t")
            .bind(herd_id)
            .bind(f)
            .bind(t)
            .fetch_all(ctx.db())
            .await?
    } else {
        sampled_fixes(ctx, herd_id, f, t).await?
    };
    let mut out: Vec<FixSample> = rows
        .iter()
        .map(|r| {
            let stored: Option<String> = r.get("paddock_id");
            let point = [r.get::<f64, _>("lon"), r.get::<f64, _>("lat")];
            FixSample { collar_id: r.get("collar_id"), t: r.get("t"), paddock_id: stored.or_else(|| paddock_at(paddocks, point).map(|p| p.id.clone())) }
        })
        .collect();
    out.sort_by_key(|s| s.t);
    Ok(out)
}

/// How many of a herd's fixes are in `[?2, ?3)`, counting no further than `?4`.
pub const COUNT_UP_TO_SQL: &str = "SELECT COUNT(*) FROM (SELECT 1 FROM fixes WHERE herd_id = ? AND t >= ? AND t < ? LIMIT ?)";
/// The herd's next collar in its fixes after `?2` (`fixes_herd_collar_t`).
pub const NEXT_COLLAR_SQL: &str = "SELECT collar_id FROM fixes WHERE herd_id = ? AND collar_id > ? ORDER BY collar_id LIMIT 1";
/// Whether the collar has a fix of the herd in `[?3, ?4)`.
pub const COLLAR_IN_WINDOW_SQL: &str = "SELECT 1 FROM fixes WHERE herd_id = ? AND collar_id = ? AND t >= ? AND t < ? LIMIT 1";
/// A collar's first fix in each of `?4` buckets of `?5` ms from `?3`, up to `?6`
/// (`?1` herd, `?2` collar): one seek on `fixes_herd_collar_t` per bucket.
pub const BUCKETS_SQL: &str = "WITH RECURSIVE b(k) AS (SELECT 0 UNION ALL SELECT k + 1 FROM b WHERE k + 1 < ?4)
     SELECT f.collar_id, f.t, f.lon, f.lat, f.paddock_id FROM b JOIN fixes f ON f.id = (
         SELECT id FROM fixes WHERE herd_id = ?1 AND collar_id = ?2 AND t >= ?3 + b.k * ?5 AND t < min(?6, ?3 + (b.k + 1) * ?5)
         ORDER BY t, id LIMIT 1)";

async fn sampled_fixes(ctx: &Ctx, herd_id: &str, from: i64, to: i64) -> anyhow::Result<Vec<sqlx::sqlite::SqliteRow>> {
    let mut collars = Vec::new();
    let mut after = String::new();
    while let Some(c) = sqlx::query_scalar::<_, String>(NEXT_COLLAR_SQL).bind(herd_id).bind(&after).fetch_optional(ctx.db()).await? {
        let here: Option<i64> = sqlx::query_scalar(COLLAR_IN_WINDOW_SQL).bind(herd_id).bind(&c).bind(from).bind(to).fetch_optional(ctx.db()).await?;
        if here.is_some() {
            collars.push(c.clone());
        }
        after = c;
    }
    let buckets = (MAX_FIX_ROWS / (collars.len() as i64).max(1)).max(1);
    let width = ((to - from) + buckets - 1).div_euclid(buckets).max(1);
    let mut rows = Vec::new();
    for c in &collars {
        rows.extend(sqlx::query(BUCKETS_SQL).bind(herd_id).bind(c).bind(from).bind(buckets).bind(width).bind(to).fetch_all(ctx.db()).await?);
    }
    Ok(rows)
}

/// (collar_id, margin_m) for a herd's cues in `[from, to)`.
pub async fn herd_cues(ctx: &Ctx, herd_id: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> anyhow::Result<Vec<(String, Option<f64>)>> {
    let rows = sqlx::query("SELECT collar_id, margin_m FROM cues WHERE herd_id = ? AND t >= ? AND t < ?")
        .bind(herd_id)
        .bind(time::unix_ms(&from))
        .bind(time::unix_ms(&to))
        .fetch_all(ctx.db())
        .await?;
    Ok(rows.iter().map(|r| (r.get("collar_id"), r.get("margin_m"))).collect())
}

pub fn fix_counts(fixes: &[FixSample]) -> BTreeMap<String, i64> {
    let mut counts = BTreeMap::new();
    for f in fixes {
        if let Some(p) = &f.paddock_id {
            *counts.entry(p.clone()).or_insert(0) += 1;
        }
    }
    counts
}

/// The paddock with most fixes, if any.
pub fn dominant(counts: &BTreeMap<String, i64>) -> Option<String> {
    counts.iter().filter(|(_, n)| **n > 0).max_by_key(|(_, n)| **n).map(|(id, _)| id.clone())
}

/// The paddock a MOVE left, recorded in `inputs.from_paddock_id`.
pub fn from_paddock(d: &Decision) -> Option<String> {
    d.inputs.get("from_paddock_id").and_then(Value::as_str).map(str::to_owned)
}

/// When each paddock was last grazed: applied moves out of it, the last collar
/// fix in it (hot fixes, plus rolled-up days and imported history from the
/// `paddock_days` view), the farmer's `grazed_until`, and now for the current paddock.
///
/// The newest hot fix per paddock is one row each in `fix_paddock_last` (a
/// trigger keeps it as fixes land), not a walk of the herd's fixes. Fixes that
/// landed outside every paddock are placed by today's shapes only when a
/// paddock was drawn or reshaped since they landed (a bounded sample).
pub async fn last_grazed(
    ctx: &Ctx,
    herd: Option<&Herd>,
    paddocks: &[Paddock],
    current: Option<&str>,
    history: &[Decision],
    now: DateTime<Utc>,
) -> anyhow::Result<BTreeMap<String, Option<DateTime<Utc>>>> {
    let mut last: BTreeMap<String, Option<DateTime<Utc>>> = paddocks.iter().map(|p| (p.id.clone(), p.grazed_until.filter(|g| *g <= now))).collect();
    let mut bump = |id: &str, at: DateTime<Utc>| {
        if let Some(slot) = last.get_mut(id)
            && slot.is_none_or(|cur| at > cur)
        {
            *slot = Some(at);
        }
    };
    for d in history {
        if d.action == Some(DecisionAction::Move)
            && d.status == DecisionStatus::Applied
            && let Some(from) = from_paddock(d)
        {
            bump(&from, d.responded_at.unwrap_or(d.created_at));
        }
    }
    if let Some(h) = herd {
        // Days no longer in `fixes`: the rollup's and imported position history.
        let days = sqlx::query("SELECT paddock_id, MAX(last_t) FROM paddock_days WHERE herd_id = ? AND paddock_id != '' AND fixes > 0 GROUP BY paddock_id")
            .bind(&h.id)
            .fetch_all(ctx.db())
            .await?;
        for r in &days {
            let at = time::from_unix_ms(r.get::<i64, _>(1));
            if at <= now {
                bump(&r.get::<String, _>(0), at);
            }
        }
        let since = now - Duration::days(REST_LOOKBACK_DAYS);
        let newest: Vec<(String, i64)> =
            sqlx::query_as("SELECT paddock_id, last_t FROM fix_paddock_last WHERE herd_id = ?").bind(&h.id).fetch_all(ctx.db()).await?;
        let mut outside = false;
        for (paddock, t) in newest {
            let at = time::from_unix_ms(t);
            if at < since {
                continue;
            }
            if paddock.is_empty() {
                outside = true;
            } else {
                // A fix stamped after `now` (a collar clock running ahead) is grazing now.
                bump(&paddock, at.min(now));
            }
        }
        if outside && let Some((from, to)) = reshaped_since(ctx, &h.id, since, now).await? {
            for f in herd_fixes(ctx, &h.id, from, to, paddocks).await? {
                if let Some(p) = &f.paddock_id {
                    bump(p, time::from_unix_ms(f.t));
                }
            }
        }
    }
    if let Some(c) = current
        && last.contains_key(c)
    {
        last.insert(c.to_owned(), Some(now));
    }
    Ok(last)
}

/// When a paddock was drawn or reshaped after some of the herd's hot fixes
/// landed: the stretch of fixes whose stored paddock predates today's shapes,
/// from the herd's oldest hot fix (or `since`) to that change.
async fn reshaped_since(ctx: &Ctx, herd_id: &str, since: DateTime<Utc>, now: DateTime<Utc>) -> anyhow::Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
    let changed: Option<String> = sqlx::query_scalar("SELECT MAX(at) FROM paddock_geometry_history WHERE source != 'deleted'").fetch_one(ctx.db()).await?;
    let Some(changed) = changed.as_deref().and_then(|c| time::from_db(c).ok()) else { return Ok(None) };
    let oldest: Option<i64> = sqlx::query_scalar("SELECT MIN(t) FROM fixes WHERE herd_id = ?").bind(herd_id).fetch_one(ctx.db()).await?;
    let Some(from) = oldest.map(time::from_unix_ms).map(|o| o.max(since)) else { return Ok(None) };
    let to = changed.min(now);
    Ok((to > from).then_some((from, to)))
}

fn ndvi_inputs(report: Option<&Value>) -> (Option<f64>, Vec<(DateTime<Utc>, f64)>) {
    let Some(imagery) = report.and_then(|r| land::ok_section(r, "imagery")) else { return (None, vec![]) };
    let parse = |s: &str| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc))
            .or_else(|| chrono::NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()?.and_hms_opt(0, 0, 0).map(|d| d.and_utc()))
    };
    let ndvi = imagery.get("ndvi_stats").and_then(|s| s.get("mean")).and_then(Value::as_f64);
    let mut hist = Vec::new();
    for row in imagery.get("history").and_then(Value::as_array).into_iter().flatten() {
        if let (Some(v), Some(at)) = (row.get("ndvi_mean").and_then(Value::as_f64), row.get("captured_at").and_then(Value::as_str).and_then(parse)) {
            hist.push((at, v));
        }
    }
    if let (Some(v), Some(at)) = (ndvi, imagery.get("latest").and_then(|l| l.get("captured_at")).and_then(Value::as_str).and_then(parse)) {
        hist.push((at, v));
    }
    (ndvi, hist)
}

pub struct SignalInputs<'a> {
    pub herd: Option<&'a Herd>,
    pub paddocks: &'a [Paddock],
    pub current: Option<&'a str>,
    pub reports: &'a HashMap<String, Value>,
    pub history: &'a [Decision],
    pub now: DateTime<Utc>,
}

/// Kit-shaped signals: `{ as_of, herd_animal_units, rest_days, forage, recovery,
/// grazing_pressure, feed_budget_days_current, behavior, risk_flags,
/// risk_flags_by_paddock, assumptions }`.
pub async fn compute(ctx: &Ctx, i: SignalInputs<'_>) -> anyhow::Result<Value> {
    let window_days = WINDOW_HOURS as f64 / 24.0;
    let areas: BTreeMap<String, Option<f64>> = i.paddocks.iter().map(|p| (p.id.clone(), real_area(p))).collect();

    let mut forage = Map::new();
    let mut recovery = Map::new();
    let mut risks = Map::new();
    let measured = measured(ctx, i.paddocks, i.now).await?;
    for p in i.paddocks {
        let report = i.reports.get(&p.id);
        let (ndvi, hist) = ndvi_inputs(report);
        forage.insert(p.id.clone(), paddock_forage(ndvi, measured.get(&p.id), report.and_then(land::forage_withheld)));
        recovery.insert(p.id.clone(), calc::recovery_trend(&hist));
        if let Some(r) = report {
            risks.insert(p.id.clone(), json!(calc::risk_flags(&land::section_data(r))));
        }
    }

    let mut pressure = Map::new();
    let mut behavior = Value::Object(Map::new());
    if let Some(h) = i.herd {
        let since = i.now - Duration::hours(WINDOW_HOURS);
        let fixes = herd_fixes(ctx, &h.id, since, i.now, i.paddocks).await?;
        let cues = herd_cues(ctx, &h.id, since, i.now).await?;
        if !fixes.is_empty() {
            pressure = calc::grazing_pressure(&fix_counts(&fixes), h.count as i64, window_days, &areas);
        }
        let collars: Vec<String> = fixes.iter().map(|f| f.collar_id.clone()).collect();
        behavior = calc::behavior(&collars, &cues, window_days);
    }

    let units = i.herd.map(|h| calc::animal_units(&h.species.as_db(), h.count as i64, None));
    let feed_budget = match (units, i.current) {
        (Some(au), Some(cur)) if au > 0.0 => match (areas.get(cur).copied().flatten(), forage.get(cur).and_then(|f| f["available_kg_dm_per_ha"].as_f64())) {
            (Some(area), Some(avail)) => calc::feed_budget_days(avail * area, au, calc::DEFAULT_INTAKE_KG_DM_PER_AU_DAY, calc::DEFAULT_UTILIZATION, 0.0),
            _ => None,
        },
        _ => None,
    };
    let current_risks = i.current.and_then(|c| risks.get(c).cloned()).or_else(|| risks.values().next().cloned()).unwrap_or_else(|| json!([]));
    let last = last_grazed(ctx, i.herd, i.paddocks, i.current, i.history, i.now).await?;

    Ok(json!({
        "as_of": time::to_db(&i.now),
        "herd_animal_units": units,
        "rest_days": calc::rest_days(&last, i.now),
        "last_grazed": last.iter().map(|(k, v)| (k.clone(), json!(v.map(|t| time::to_db(&t))))).collect::<Map<String, Value>>(),
        "forage": forage,
        "recovery": recovery,
        "grazing_pressure": pressure,
        "feed_budget_days_current": feed_budget,
        "behavior": behavior,
        "risk_flags": current_risks,
        "risk_flags_by_paddock": risks,
        "assumptions": [
            "Forage from imagery maps NDVI 0.2-0.8 to 0-10 inches; farmer heights win when recorded.",
            "Grazing pressure assumes collared animals move like the rest of the herd.",
            "Feed budget counts 60% of standing forage above a 3 inch residual at 11.8 kg DM per animal unit per day.",
        ],
    }))
}

/// A measured height that still says how tall a paddock's grass stands.
#[derive(Debug, Clone, PartialEq)]
pub struct Measured {
    pub height: heights::Height,
    /// What stands now: the height, or after grazing the residual left.
    pub cm: f64,
}

/// A herd grazing a paddock this long after a height was taken there has
/// eaten the grass it measured.
const GRAZED_AFTER: Duration = Duration::hours(1);

/// Per paddock, the measured height that still describes its grass: the
/// newest of the last 21 days ([`heights::current`]), unless a herd grazed
/// the paddock after it was taken. Grazing is any herd's: a stay on the farm
/// record (`herd_history`) that ended after it, the farmer's `grazed_until`,
/// or collar fixes there (hot, rolled-up or imported). In a paddock a herd is
/// in now, the height stands for the grass ahead of it (`height_cm`);
/// elsewhere a `residual_cm` is what the last grazing left, and stands instead.
/// A few indexed reads per measured paddock, never a walk of the fixes.
pub async fn measured(ctx: &Ctx, paddocks: &[Paddock], now: DateTime<Utc>) -> anyhow::Result<HashMap<String, Measured>> {
    let heights = heights::current(ctx, now).await?;
    if heights.is_empty() {
        return Ok(HashMap::new());
    }
    let occupied: Vec<String> = ctx.store().list_herds().await?.into_iter().filter_map(|h| h.paddock_id).collect();
    let mut out = HashMap::new();
    for p in paddocks {
        let Some(h) = heights.get(&p.id) else { continue };
        if occupied.contains(&p.id) {
            out.insert(p.id.clone(), Measured { cm: h.height_cm, height: h.clone() });
            continue;
        }
        if grazed_after(ctx, p, h.at + GRAZED_AFTER, now).await? {
            continue;
        }
        out.insert(p.id.clone(), Measured { cm: h.residual_cm.unwrap_or(h.height_cm), height: h.clone() });
    }
    Ok(out)
}

/// Whether any herd grazed the paddock after `t` (up to `now`).
async fn grazed_after(ctx: &Ctx, p: &Paddock, t: DateTime<Utc>, now: DateTime<Utc>) -> anyhow::Result<bool> {
    if p.grazed_until.is_some_and(|g| g > t && g <= now) {
        return Ok(true);
    }
    let after = |ms: Option<i64>| ms.map(time::from_unix_ms).is_some_and(|at| at > t);
    let hot: Option<i64> = sqlx::query_scalar("SELECT MAX(last_t) FROM fix_paddock_last WHERE paddock_id = ?").bind(&p.id).fetch_one(ctx.db()).await?;
    if after(hot) {
        return Ok(true);
    }
    let days: Option<i64> = sqlx::query_scalar("SELECT MAX(last_t) FROM paddock_days WHERE paddock_id = ? AND date >= ? AND fixes > 0")
        .bind(&p.id)
        .bind(t.date_naive().to_string())
        .fetch_one(ctx.db())
        .await?;
    if after(days) {
        return Ok(true);
    }
    // A herd on record in the paddock that left it after `t`.
    let left: Option<String> = sqlx::query_scalar(
        "SELECT MAX(n.at) FROM herd_history h JOIN herd_history n ON n.id = (
             SELECT x.id FROM herd_history x WHERE x.herd_id = h.herd_id AND (x.at > h.at OR (x.at = h.at AND x.id > h.id)) ORDER BY x.at, x.id LIMIT 1)
         WHERE h.paddock_id = ?1 AND (n.paddock_id IS NULL OR n.paddock_id != ?1) AND n.at > ?2",
    )
    .bind(&p.id)
    .bind(time::to_db(&t))
    .fetch_one(ctx.db())
    .await?;
    Ok(left.is_some())
}

/// A paddock's forage: a measured height that still describes its grass
/// wins ([`measured`]; `source: "measured"`, with the `height_cm` it stands
/// at and `measured_at`); otherwise NDVI, unless snow or dormant grass makes
/// imagery meaningless, when the estimate is null and `reason` says why
/// (`"snow"` or `"dormant"`).
pub fn paddock_forage(ndvi: Option<f64>, measured: Option<&Measured>, withheld: Option<&'static str>) -> Value {
    let residual = calc::DEFAULT_RESIDUAL_INCHES;
    if let Some(m) = measured {
        let mut f = calc::forage_estimate(ndvi, Some(m.cm / 2.54), residual);
        f["source"] = json!("measured");
        f["height_cm"] = json!(m.cm);
        f["measured_at"] = json!(time::to_db(&m.height.at));
        return f;
    }
    match withheld {
        Some(reason) => {
            let mut f = calc::forage_estimate(None, None, residual);
            f["reason"] = json!(reason);
            f
        }
        None => calc::forage_estimate(ndvi, None, residual),
    }
}

/// Per-paddock view of the signals for `GET /api/signals`.
pub fn per_paddock(signals: &Value, paddocks: &[Paddock], current: Option<&str>) -> Value {
    let au = signals["herd_animal_units"].as_f64().filter(|a| *a > 0.0);
    let rows: Vec<Value> = paddocks
        .iter()
        .map(|p| {
            let id = p.id.as_str();
            // Days of grazing this paddock's forage gives the herd, as for the current paddock.
            let grazing_days = match (au, real_area(p), signals["forage"][id]["available_kg_dm_per_ha"].as_f64()) {
                (Some(au), Some(area), Some(avail)) => {
                    calc::feed_budget_days(avail * area, au, calc::DEFAULT_INTAKE_KG_DM_PER_AU_DAY, calc::DEFAULT_UTILIZATION, 0.0)
                }
                _ => None,
            };
            json!({
                "paddock_id": id,
                "name": p.name,
                "status": p.status,
                "area_ha": p.area_ha,
                "current": current == Some(id),
                "rest_days": signals["rest_days"][id],
                "grazing_pressure": signals["grazing_pressure"][id],
                "forage": signals["forage"][id],
                "grazing_days": grazing_days,
                "last_grazed": signals["last_grazed"][id],
                "recovery": signals["recovery"][id],
                "risk_flags": signals["risk_flags_by_paddock"].get(id).cloned().unwrap_or_else(|| json!([])),
            })
        })
        .collect();
    json!(rows)
}
