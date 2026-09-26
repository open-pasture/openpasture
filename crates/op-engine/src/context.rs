//! Decision context: everything the brain sees before deciding. Port of the
//! kit's `DecisionEngine.assemble_context`. The JSON shape is documented in
//! docs/API.md (Decision context) because op-brain's heuristic reads it.

use std::collections::HashMap;

use chrono::Duration;
use op_core::store::collar_from_row;
use op_core::{Collar, Ctx, Decision, FenceState, Herd, Paddock, time};
use serde_json::{Value, json};

use crate::{db, knowledge, land, signals};

/// Land reports fetched per cycle (kit `MAX_LAND_REPORTS_PER_CYCLE`).
pub const MAX_LAND_REPORTS: usize = 8;
const LOW_BATTERY: f64 = 0.2;

pub struct Assembled {
    pub context: Value,
    pub current_paddock_id: Option<String>,
    pub land_report_ids: Vec<String>,
    pub knowledge_ids: Vec<String>,
}

pub async fn herd_collars(ctx: &Ctx, herd_id: &str) -> anyhow::Result<Vec<Collar>> {
    let rows = sqlx::query("SELECT * FROM collars WHERE herd_id = ? ORDER BY created_at, id").bind(herd_id).fetch_all(ctx.db()).await?;
    rows.iter().map(collar_from_row).collect()
}

/// When the herd's current boundary took effect, if it has one. A move's
/// sweep steps all count from its first boundary.
pub async fn boundary_since(ctx: &Ctx, herd_id: &str, now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT MIN(COALESCE(effective_at, created_at)) FROM boundaries WHERE herd_id = ? AND decision_id = (
             SELECT decision_id FROM boundaries WHERE herd_id = ? AND COALESCE(effective_at, created_at) <= ? ORDER BY version DESC LIMIT 1)",
    )
    .bind(herd_id)
    .bind(herd_id)
    .bind(time::to_db(&now))
    .fetch_one(ctx.db())
    .await?;
    row.get::<Option<String>, _>(0).as_deref().map(time::from_db).transpose()
}

/// Where the herd is: the paddock with most collar fixes in the last day
/// (since the current boundary took effect, if later) when it holds at least
/// half of them, else the farm record.
pub async fn locate(
    ctx: &Ctx,
    herd: &Herd,
    paddocks: &[Paddock],
) -> anyhow::Result<(Option<String>, &'static str, std::collections::BTreeMap<String, i64>, usize)> {
    let now = time::now();
    let mut since = now - Duration::hours(signals::WINDOW_HOURS);
    if let Some(b) = boundary_since(ctx, &herd.id, now).await? {
        since = since.max(b);
    }
    let fixes = signals::herd_fixes(ctx, &herd.id, since, now, paddocks).await?;
    let counts = signals::fix_counts(&fixes);
    // Collars place the herd only when most fixes agree; a herd between
    // paddocks (walking a lane after a move) falls back to the record.
    let placed = signals::dominant(&counts).filter(|p| counts.get(p).is_some_and(|n| *n as usize * 2 >= fixes.len()));
    let (current, source) = match placed {
        Some(p) => (Some(p), "collar"),
        None => match herd.paddock_id.clone().filter(|id| paddocks.iter().any(|p| &p.id == id)) {
            Some(p) => (Some(p), "farm_record"),
            None => (None, "unknown"),
        },
    };
    Ok((current, source, counts, fixes.len()))
}

fn history_entry(d: &Decision) -> Value {
    let farmer = d.inputs.get("farmer_response").cloned();
    json!({
        "id": d.id,
        "created_at": d.created_at,
        "source": d.source,
        "status": d.status,
        "action": d.action,
        "from_paddock_id": signals::from_paddock(d),
        "to_paddock_id": d.to_paddock_id,
        "reasoning": d.reasoning,
        "confidence": d.confidence,
        "need": d.need,
        "farmer_response": farmer,
        "outcome": d.outcome,
    })
}

/// Assemble the context for one herd. With `fetch_land`, land reports older
/// than six hours are refreshed; otherwise only cached reports are used.
/// `log` receives one line per step.
pub async fn assemble(ctx: &Ctx, herd: &Herd, fetch_land: bool, log: &(dyn Fn(String) + Send + Sync)) -> anyhow::Result<Assembled> {
    let now = time::now();
    let store = ctx.store();
    let farm = store.get_farm().await?.ok_or_else(|| anyhow::anyhow!("Set up the farm first."))?;
    let paddocks = store.list_paddocks().await?;

    let (current, position_source, counts, fixes_24h) = locate(ctx, herd, &paddocks).await?;
    log(match &current {
        Some(p) => format!("Herd is in {} ({position_source}).", name_of(&paddocks, p)),
        None => "Herd position is unknown.".into(),
    });
    let collars = herd_collars(ctx, &herd.id).await?;
    let positions = op_ingest::latest_positions(ctx, &herd.id).await?;
    let boundary = op_ingest::boundary_status(ctx, &herd.id).await?;
    if let Some(m) = boundary.r#move.as_ref().filter(|m| m.status == op_core::MoveStatus::Sweeping) {
        log(format!("A move is sweeping the herd toward its target: step {}, {:.0} m to go, {} stragglers.", m.step, m.remaining_m, m.stragglers.len()));
    }

    let since = now - Duration::hours(signals::WINDOW_HOURS);
    let cues = signals::herd_cues(ctx, &herd.id, since, now).await?;
    let reporting: Vec<&Collar> = collars.iter().filter(|c| c.last_seen.is_some_and(|t| t >= since)).collect();
    let mut states = json!({ "inside": 0, "warning": 0, "outside": 0, "unknown": 0 });
    for c in &collars {
        let k = match c.state {
            FenceState::Inside => "inside",
            FenceState::Warning => "warning",
            FenceState::Outside => "outside",
            _ => "unknown",
        };
        states[k] = json!(states[k].as_i64().unwrap_or(0) + 1);
    }
    let collar_summary = json!({
        "count": collars.len(),
        "reporting_24h": reporting.len(),
        "quiet": collars.iter().filter(|c| !reporting.iter().any(|r| r.id == c.id)).map(|c| &c.id).collect::<Vec<_>>(),
        "low_battery": collars.iter().filter(|c| c.battery.is_some_and(|b| b < LOW_BATTERY)).map(|c| &c.id).collect::<Vec<_>>(),
        "states": states,
        "fixes_24h": fixes_24h,
        "cues_24h": cues.len(),
        "dominant_paddock_id": signals::dominant(&counts),
        "paddock_fix_counts": counts,
    });
    log(format!("{} of {} collars reported in the last day, {} cues.", reporting.len(), collars.len(), cues.len()));

    // Land reports: current paddock first, then candidates.
    let mut wanted: Vec<&Paddock> = paddocks.iter().filter(|p| Some(&p.id) == current.as_ref()).collect();
    wanted.extend(paddocks.iter().filter(|p| Some(&p.id) != current.as_ref()));
    let mut reports: HashMap<String, Value> = HashMap::new();
    let mut notes = serde_json::Map::new();
    for p in wanted.into_iter().take(MAX_LAND_REPORTS) {
        if signals::real_area(p).is_none() {
            notes.insert(p.id.clone(), json!("No mapped boundary yet, so no land report."));
            continue;
        }
        let res = if fetch_land { land::report_for_paddock(ctx, p, false).await.map(|(r, _)| Some(r)) } else { land::latest(ctx, &p.id).await };
        match res {
            Ok(Some(r)) => {
                reports.insert(p.id.clone(), r);
            }
            Ok(None) => {
                notes.insert(p.id.clone(), json!("No land report yet."));
            }
            Err(e) => {
                tracing::warn!("land report for {}: {e:#}", p.id);
                notes.insert(p.id.clone(), json!(format!("Land report failed: {e}")));
            }
        }
    }
    if !reports.is_empty() {
        let source = reports.values().next().and_then(|r| r["source"].as_str()).unwrap_or("land");
        log(format!("Land reports for {} paddocks ({source}).", reports.len()));
    }

    let history = db::list(ctx, Some(&herd.id), 10).await?;
    let observations = observations(ctx, &paddocks, now).await?;
    let sig = signals::compute(
        ctx,
        signals::SignalInputs { herd: Some(herd), paddocks: &paddocks, current: current.as_deref(), reports: &reports, history: &history, now },
    )
    .await?;

    // Knowledge query as in the kit: the situation plus paddock notes and risk flags.
    let flags: Vec<&str> = sig["risk_flags"].as_array().into_iter().flatten().filter_map(|f| f["type"].as_str()).collect();
    let notes_text: Vec<&str> = paddocks.iter().filter_map(|p| p.notes.as_deref()).collect();
    let query = format!("movement decision {} recovery residual weather {}", notes_text.join(" "), flags.join(" "));
    let found = knowledge::search(ctx, &query, 3).await.unwrap_or_else(|e| {
        tracing::warn!("knowledge search: {e:#}");
        vec![]
    });

    let paddock_rows: Vec<Value> = paddocks
        .iter()
        .map(|p| {
            let mut v = json!({
                "id": p.id, "name": p.name, "status": p.status, "area_ha": p.area_ha, "geometry": p.geometry,
                "rest_days": sig["rest_days"][&p.id],
                "last_grazed": sig["last_grazed"][&p.id],
            });
            if let Some(n) = &p.notes {
                v["notes"] = json!(n);
            }
            if let Some(g) = &p.grazed_until {
                v["grazed_until"] = json!(g);
            }
            v
        })
        .collect();
    let positions_json: Vec<Value> = positions
        .iter()
        .map(|p| {
            json!({
                "collar_id": p.collar_id, "animal_id": p.animal_id, "point": p.fix.point, "at": p.fix.at, "state": p.state,
                "paddock_id": signals::paddock_at(&paddocks, p.fix.point).map(|x| &x.id),
            })
        })
        .collect();
    let mut herd_json = serde_json::to_value(herd)?;
    herd_json["animal_units"] = sig["herd_animal_units"].clone();
    let land_json: serde_json::Map<String, Value> = reports
        .iter()
        .map(|(k, r)| (k.clone(), json!({ "report_id": r["report_id"], "source": r["source"], "as_of": r["as_of"], "sections": r["sections"] })))
        .collect();

    let context = json!({
        "as_of": time::to_db(&now),
        "farm": farm,
        "herd": herd_json,
        "autonomy": { "mode": herd.autonomy, "timer_minutes": herd.timer_minutes },
        "current_paddock_id": current,
        "position_source": position_source,
        "paddocks": paddock_rows,
        "candidate_paddock_ids": paddocks.iter().filter(|p| Some(&p.id) != current.as_ref()).map(|p| &p.id).collect::<Vec<_>>(),
        "boundary": boundary,
        "collars": collar_summary,
        "positions": positions_json,
        "signals": sig,
        "land_reports": land_json,
        "land_report_notes": notes,
        "knowledge": found,
        "observations": observations,
        "history": history.iter().map(history_entry).collect::<Vec<_>>(),
        "units": ctx.settings().await?.units,
    });
    Ok(Assembled {
        context,
        current_paddock_id: current,
        land_report_ids: reports.values().filter_map(|r| r["report_id"].as_str().map(str::to_owned)).collect(),
        knowledge_ids: found.iter().map(|e| e.id.clone()).collect(),
    })
}

/// Farmer observations: notes given when answering decisions in the last
/// seven days (field notes, dated, per paddock), then each paddock's standing
/// notes (undated, so not counted as field observations).
pub async fn observations(ctx: &Ctx, paddocks: &[Paddock], now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<Vec<Value>> {
    let since = time::to_db(&(now - Duration::days(7)));
    let rows = sqlx::query("SELECT body, paddock_id, created_at FROM lessons WHERE kind = 'farmer' AND created_at >= ? ORDER BY created_at DESC LIMIT 20")
        .bind(since)
        .fetch_all(ctx.db())
        .await?;
    let mut out: Vec<Value> = rows
        .iter()
        .map(|r| {
            use sqlx::Row;
            json!({ "content": r.get::<String, _>(0), "paddock_id": r.get::<Option<String>, _>(1), "at": r.get::<String, _>(2), "source": "farmer-note" })
        })
        .collect();
    out.extend(paddocks.iter().filter_map(|p| p.notes.as_ref().map(|n| json!({ "content": n, "paddock_id": p.id, "source": "paddock-note" }))));
    Ok(out)
}

pub fn name_of(paddocks: &[Paddock], id: &str) -> String {
    paddocks.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_else(|| id.to_owned())
}
