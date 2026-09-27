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
const MAX_FIX_ROWS: i64 = 20_000;

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

/// Fixes for a herd in `[from, to)`, evenly sampled to at most `MAX_FIX_ROWS`,
/// each with the paddock it fell in.
pub async fn herd_fixes(ctx: &Ctx, herd_id: &str, from: DateTime<Utc>, to: DateTime<Utc>, paddocks: &[Paddock]) -> anyhow::Result<Vec<FixSample>> {
    let (f, t) = (time::unix_ms(&from), time::unix_ms(&to));
    let n: i64 =
        sqlx::query("SELECT COUNT(*) FROM fixes WHERE herd_id = ? AND t >= ? AND t < ?").bind(herd_id).bind(f).bind(t).fetch_one(ctx.db()).await?.get(0);
    let stride = (n / MAX_FIX_ROWS).max(1);
    let rows = sqlx::query("SELECT collar_id, t, lon, lat, paddock_id FROM fixes WHERE herd_id = ? AND t >= ? AND t < ? AND id % ? = 0 ORDER BY t")
        .bind(herd_id)
        .bind(f)
        .bind(t)
        .bind(stride)
        .fetch_all(ctx.db())
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let stored: Option<String> = r.get("paddock_id");
            let point = [r.get::<f64, _>("lon"), r.get::<f64, _>("lat")];
            FixSample { collar_id: r.get("collar_id"), t: r.get("t"), paddock_id: stored.or_else(|| paddock_at(paddocks, point).map(|p| p.id.clone())) }
        })
        .collect())
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
        for f in herd_fixes(ctx, &h.id, now - Duration::days(REST_LOOKBACK_DAYS), now, paddocks).await? {
            if let Some(p) = &f.paddock_id {
                bump(p, time::from_unix_ms(f.t));
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
    let measured = heights::current(ctx, i.now).await?;
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

/// A paddock's forage: a height measured in the last 21 days wins
/// (`source: "measured"`, with `height_cm` and `measured_at`); otherwise NDVI,
/// unless snow or dormant grass makes imagery meaningless, when the estimate is
/// null and `reason` says why (`"snow"` or `"dormant"`).
pub fn paddock_forage(ndvi: Option<f64>, measured: Option<&heights::Height>, withheld: Option<&'static str>) -> Value {
    let residual = calc::DEFAULT_RESIDUAL_INCHES;
    if let Some(h) = measured {
        let mut f = calc::forage_estimate(ndvi, Some(h.height_cm / 2.54), residual);
        f["source"] = json!("measured");
        f["height_cm"] = json!(h.height_cm);
        f["measured_at"] = json!(time::to_db(&h.at));
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
