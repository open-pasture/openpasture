//! Strip grazing: parallel strips across a paddock with the ground and the
//! days each one holds. `POST /api/strips/preview` (docs/API.md, "Strips and
//! layouts"). The geometry comes from [`op_geo::strip`]; forage comes from the
//! paddock's grazing signals, which is why this lives in op-engine.
//!
//! Sending a strip is the farmer drawing that boundary
//! (`POST /api/herds/{id}/boundary`), so exclusion holes, fitting and checks
//! happen where every boundary goes through them. A strip here is the whole
//! band; `grazeable_ha` is that band less the exclusions in effect now.

use std::collections::HashMap;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use op_core::features::FeatureKind;
use op_core::{ApiError, ApiJson, ApiResult, Ctx, DbEnum, Herd, Paddock, Polygon, time};
use op_geo::strip::{self, StripBy};
use serde::{Deserialize, Serialize};

use crate::{calc, land, signals};

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/strips/preview", post(preview_route))
}

/// Most strips one preview or layout holds.
pub const MAX_STRIPS: usize = 200;
/// Most head a preview sizes for.
const MAX_HEAD: u32 = 100_000;

/// How to cut: an orientation and exactly one of `width_m`, `count` or `days`.
/// A layout stores these, with `width_m` filled in when `days` chose it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StripParams {
    /// Compass bearing the strips advance toward: 0 = strips run east-west, advancing north.
    pub orientation_deg: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// Days of grazing per strip; picks the width that gives that many.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<f64>,
    /// Head to size for; the herd's count when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<u32>,
    /// Warning distance the strips must leave room for; 5 m when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_m: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct PreviewRequest {
    pub paddock_id: String,
    #[serde(default)]
    pub herd_id: Option<String>,
    #[serde(flatten)]
    pub params: StripParams,
}

/// One strip and what it holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StripFacts {
    pub geometry: Polygon,
    pub area_ha: f64,
    /// The strip less the exclusions in effect now.
    pub grazeable_ha: f64,
    /// Days of grazing for the herd: forage × grazeable ÷ (AU × 11.8 kg DM/day). Absent without a forage estimate or animals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    pub paddock_id: String,
    pub strips: Vec<StripFacts>,
    /// Width each strip was cut to before thin ones merged (m).
    pub width_m: f64,
    /// Depth of the paddock along the advance direction (m).
    pub depth_m: f64,
    pub head: u32,
    pub animal_units: f64,
    /// Standing forage above the residual, kg DM/ha, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forage_kg_dm_per_ha: Option<f64>,
    /// Where the forage figure comes from (imagery, or a measured height).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forage_source: Option<String>,
    pub warn_m: f64,
}

/// Standing forage above the residual target for one paddock.
#[derive(Debug, Clone, PartialEq)]
pub struct Forage {
    pub kg_dm_per_ha: f64,
    pub source: Option<String>,
}

/// The paddock's forage as the grazing signals see it: from its latest cached
/// land report (never fetched here), with a measured height winning where one
/// is recorded. `None` when there is no estimate.
pub async fn paddock_forage(ctx: &Ctx, p: &Paddock) -> anyhow::Result<Option<Forage>> {
    let mut reports = HashMap::new();
    if let Some(r) = land::latest(ctx, &p.id).await? {
        reports.insert(p.id.clone(), r);
    }
    let sig = signals::compute(
        ctx,
        signals::SignalInputs { herd: None, paddocks: std::slice::from_ref(p), current: None, reports: &reports, history: &[], now: time::now() },
    )
    .await?;
    let f = &sig["forage"][p.id.as_str()];
    Ok(f["available_kg_dm_per_ha"].as_f64().map(|kg| Forage { kg_dm_per_ha: kg, source: f["source"].as_str().map(str::to_owned) }))
}

/// Exclusions in effect now that touch the paddock, as polygons.
pub async fn exclusions(ctx: &Ctx, p: &Paddock) -> anyhow::Result<Vec<Polygon>> {
    let found = op_core::features::exclusions_for(ctx, &p.geometry, time::now()).await?;
    Ok(found.into_iter().filter(|f| f.kind == FeatureKind::Exclusion).filter_map(|f| f.geometry.polygon()).collect())
}

/// Days a strip feeds `animal_units`: forage × grazeable ÷ (AU × 11.8 kg DM/day), to 0.1 d.
pub fn strip_days(kg_dm_per_ha: f64, grazeable_ha: f64, animal_units: f64) -> Option<f64> {
    let demand = animal_units * calc::DEFAULT_INTAKE_KG_DM_PER_AU_DAY;
    (demand > 0.0 && kg_dm_per_ha.is_finite()).then(|| calc::round((kg_dm_per_ha.max(0.0) * grazeable_ha.max(0.0) / demand).min(3650.0), 1))
}

/// Who is being fed: head (asked, else the herd's count) and their animal units.
pub fn feeding(herd: Option<&Herd>, head: Option<u32>) -> (u32, f64) {
    let head = head.or(herd.map(|h| h.count)).unwrap_or(0);
    let species = herd.map(|h| h.species.as_db()).unwrap_or_else(|| "cattle".into());
    (head, calc::animal_units(&species, head as i64, None))
}

fn check(params: &StripParams) -> ApiResult<f64> {
    if !params.orientation_deg.is_finite() {
        return Err(ApiError::bad_request("orientation_deg must be a number of degrees."));
    }
    let given = [params.width_m.is_some(), params.count.is_some(), params.days.is_some()].iter().filter(|g| **g).count();
    if given != 1 {
        return Err(ApiError::bad_request("Give one of width_m, count or days."));
    }
    if params.width_m.is_some_and(|w| !w.is_finite() || w <= 0.0) {
        return Err(ApiError::bad_request("width_m must be more than 0."));
    }
    if params.count.is_some_and(|n| n == 0 || n as usize > MAX_STRIPS) {
        return Err(ApiError::bad_request(format!("count must be 1 to {MAX_STRIPS}.")));
    }
    if params.days.is_some_and(|d| !d.is_finite() || d <= 0.0) {
        return Err(ApiError::bad_request("days must be more than 0."));
    }
    if params.head.is_some_and(|h| h > MAX_HEAD) {
        return Err(ApiError::bad_request("head is too many."));
    }
    let warn_m = params.warn_m.unwrap_or(op_geo::shape::DEFAULT_WARN_M);
    if !warn_m.is_finite() || !(0.0..=1000.0).contains(&warn_m) {
        return Err(ApiError::bad_request("warn_m must be between 0 and 1000 metres."));
    }
    Ok(warn_m)
}

/// Strips across `paddock` for `params` with what each holds, from the
/// paddock's forage estimate.
pub async fn preview(ctx: &Ctx, paddock: &Paddock, herd: Option<&Herd>, params: &StripParams) -> ApiResult<Preview> {
    let forage = paddock_forage(ctx, paddock).await?;
    preview_with(ctx, paddock, herd, params, forage).await
}

/// [`preview`] with the forage given: how strips are sized and counted for
/// any estimate (imagery or a measured height).
pub async fn preview_with(ctx: &Ctx, paddock: &Paddock, herd: Option<&Herd>, params: &StripParams, forage: Option<Forage>) -> ApiResult<Preview> {
    let warn_m = check(params)?;
    let (head, au) = feeding(herd, params.head);
    let excl = exclusions(ctx, paddock).await?;
    let depth = strip::depth_m(&paddock.geometry, params.orientation_deg);
    let by = match (params.width_m, params.count, params.days) {
        (Some(w), _, _) => StripBy::Width(w),
        (_, Some(n), _) => StripBy::Count(n),
        (_, _, Some(days)) => {
            let Some(f) = forage.as_ref().filter(|f| f.kg_dm_per_ha > 0.0) else {
                return Err(ApiError::bad_request("This paddock has no forage estimate, so strips can't be sized by days."));
            };
            if au <= 0.0 {
                return Err(ApiError::bad_request("Strips sized by days need a head count."));
            }
            let per_strip_ha = days * au * calc::DEFAULT_INTAKE_KG_DM_PER_AU_DAY / f.kg_dm_per_ha;
            let grazeable = strip::area_outside_ha(&paddock.geometry, &excl);
            if grazeable <= 0.0 || depth <= 0.0 {
                return Err(ApiError::bad_request("This paddock has no ground left to graze."));
            }
            StripBy::Width(depth * per_strip_ha / grazeable)
        }
        _ => unreachable!("check() requires one"),
    };
    let geometries = strip::strips(&paddock.geometry, params.orientation_deg, by, warn_m);
    if geometries.is_empty() {
        return Err(ApiError::bad_request("This paddock's shape can't be cut into strips."));
    }
    if geometries.len() > MAX_STRIPS {
        return Err(ApiError::bad_request(format!("That makes more than {MAX_STRIPS} strips. Make them wider.")));
    }
    let width_m = match by {
        StripBy::Width(w) => w,
        StripBy::Count(n) => depth / n.max(1) as f64,
    };
    Ok(Preview {
        paddock_id: paddock.id.clone(),
        strips: facts(geometries, &excl, forage.as_ref(), au),
        width_m: calc::round(width_m, 2),
        depth_m: calc::round(depth, 2),
        head,
        animal_units: au,
        forage_kg_dm_per_ha: forage.as_ref().map(|f| f.kg_dm_per_ha),
        forage_source: forage.and_then(|f| f.source),
        warn_m,
    })
}

/// Area, grazeable area and days for strips already cut.
pub fn facts(strips: Vec<Polygon>, exclusions: &[Polygon], forage: Option<&Forage>, animal_units: f64) -> Vec<StripFacts> {
    strips
        .into_iter()
        .map(|g| {
            let area_ha = round_ha(g.area_ha());
            let grazeable_ha = round_ha(strip::area_outside_ha(&g, exclusions)).min(area_ha);
            let days = forage.and_then(|f| strip_days(f.kg_dm_per_ha, grazeable_ha, animal_units));
            StripFacts { geometry: g, area_ha, grazeable_ha, days }
        })
        .collect()
}

fn round_ha(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

async fn find_herd(ctx: &Ctx, id: Option<&str>) -> ApiResult<Option<Herd>> {
    match id.filter(|s| !s.is_empty()) {
        Some(id) => Ok(Some(ctx.store().get_herd(id).await?.ok_or_else(|| ApiError::not_found("No such herd."))?)),
        None => Ok(None),
    }
}

pub(crate) async fn find_paddock(ctx: &Ctx, id: &str) -> ApiResult<Paddock> {
    ctx.store().get_paddock(id).await?.ok_or_else(|| ApiError::not_found("No such paddock."))
}

pub(crate) async fn load(ctx: &Ctx, paddock_id: &str, herd_id: Option<&str>) -> ApiResult<(Paddock, Option<Herd>)> {
    Ok((find_paddock(ctx, paddock_id).await?, find_herd(ctx, herd_id).await?))
}

async fn preview_route(State(ctx): State<Ctx>, ApiJson(b): ApiJson<PreviewRequest>) -> ApiResult<Json<Preview>> {
    let (paddock, herd) = load(&ctx, &b.paddock_id, b.herd_id.as_deref()).await?;
    Ok(Json(preview(&ctx, &paddock, herd.as_ref(), &b.params).await?))
}
