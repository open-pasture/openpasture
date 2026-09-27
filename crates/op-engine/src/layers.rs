//! Map layers per paddock: `GET /api/layers/paddocks`. Rest days come from the
//! grazing signals (every herd's record); NDVI, drought and flood from the
//! newest cached land report of each paddock. Nothing here fetches a report,
//! so the map never waits on a provider.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::{ApiResult, Ctx, time};
use serde::Serialize;
use serde_json::Value;

use crate::{calc, context, db, land, signals};

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/layers/paddocks", get(route))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Layers {
    pub as_of: DateTime<Utc>,
    pub paddocks: Vec<PaddockLayer>,
}

/// One paddock's values. A field is absent when nothing is known.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaddockLayer {
    pub paddock_id: String,
    /// A herd is in it now.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub grazing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rest_days: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_grazed: Option<DateTime<Utc>>,
    /// Mean NDVI of the latest imagery, and when it was captured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ndvi: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ndvi_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drought: Option<Drought>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flood: Option<Flood>,
}

/// US Drought Monitor category from the land report's climate section:
/// `D0`–`D4`, or none when the paddock is not in drought.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Drought {
    pub category: Option<String>,
}

/// The land report's floodplain, and the flood risk flag of its forecast.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Flood {
    pub in_floodplain: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// `medium` or `high` when the 3-day forecast carries a flood flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
}

async fn route(State(ctx): State<Ctx>) -> ApiResult<Json<Layers>> {
    Ok(Json(paddock_layers(&ctx, time::now()).await?))
}

/// Every paddock's layer values at `now`, from the record and cached land reports.
pub async fn paddock_layers(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<Layers> {
    let store = ctx.store();
    let paddocks = store.list_paddocks().await?;
    let herds = store.list_herds().await?;

    // Last grazed by any herd: each herd's signals, the latest wins.
    let mut last: BTreeMap<String, Option<DateTime<Utc>>> = paddocks.iter().map(|p| (p.id.clone(), None)).collect();
    let mut merge = |from: BTreeMap<String, Option<DateTime<Utc>>>| {
        for (id, at) in from {
            if let (Some(slot), Some(at)) = (last.get_mut(&id), at)
                && slot.is_none_or(|cur| at > cur)
            {
                *slot = Some(at);
            }
        }
    };
    let mut grazing = std::collections::BTreeSet::new();
    if herds.is_empty() {
        merge(signals::last_grazed(ctx, None, &paddocks, None, &[], now).await?);
    }
    for h in &herds {
        let (current, _, _, _) = context::locate(ctx, h, &paddocks).await?;
        let history = db::list(ctx, Some(&h.id), 10).await?;
        merge(signals::last_grazed(ctx, Some(h), &paddocks, current.as_deref(), &history, now).await?);
        // An empty herd left in a paddock isn't grazing it (signals::last_grazed).
        if h.count > 0 {
            grazing.extend(current);
        }
    }
    let rest = calc::rest_days(&last, now);

    let mut out = Vec::with_capacity(paddocks.len());
    for p in &paddocks {
        let report = land::latest(ctx, &p.id).await?;
        let (ndvi, ndvi_at) = report.as_ref().map(imagery).unwrap_or_default();
        out.push(PaddockLayer {
            paddock_id: p.id.clone(),
            grazing: grazing.contains(&p.id),
            rest_days: rest.get(&p.id).copied().flatten(),
            last_grazed: last.get(&p.id).copied().flatten(),
            ndvi,
            ndvi_at,
            drought: report.as_ref().and_then(drought),
            flood: report.as_ref().and_then(flood),
        });
    }
    Ok(Layers { as_of: now, paddocks: out })
}

fn imagery(report: &Value) -> (Option<f64>, Option<String>) {
    let Some(i) = land::ok_section(report, "imagery") else { return (None, None) };
    let ndvi = i.get("ndvi_stats").and_then(|s| s.get("mean")).and_then(Value::as_f64).filter(|v| v.is_finite()).map(|v| calc::round(v, 3));
    let at = i.get("latest").and_then(|l| l.get("captured_at")).and_then(Value::as_str).map(|s| s.chars().take(10).collect());
    (ndvi, ndvi.and(at))
}

fn drought(report: &Value) -> Option<Drought> {
    let d = land::ok_section(report, "climate")?.get("drought")?.as_object()?.clone();
    let category = match d.get("category") {
        Some(Value::String(s)) => Some(s.trim().to_uppercase()),
        Some(Value::Number(n)) => Some(format!("D{n}")),
        _ => None,
    }
    .filter(|c| !c.is_empty() && !matches!(c.as_str(), "NONE" | "NO DROUGHT"));
    Some(Drought { category })
}

fn flood(report: &Value) -> Option<Flood> {
    let f = land::ok_section(report, "water")?.get("floodplain")?.as_object()?.clone();
    // Same reading as the risk flags: a boolean, or any non-null value.
    let in_floodplain = f.get("in_floodplain").is_some_and(|v| v.as_bool().unwrap_or(!v.is_null()));
    let zone = f.get("zone").and_then(Value::as_str).map(str::to_owned);
    let risk = calc::risk_flags(&land::section_data(report))
        .into_iter()
        .find(|flag| flag["type"] == "flood")
        .and_then(|flag| flag["level"].as_str().map(str::to_owned));
    Some(Flood { in_floodplain, zone, risk })
}
