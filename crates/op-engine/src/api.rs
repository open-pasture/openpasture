//! REST routes (docs/API.md, "Decisions and brains") and the views they share
//! with the MCP tools.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{any, get, post};
use axum::{Json, Router};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Decision, Polygon};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cycle::{self, Response};
use crate::{context, db, knowledge, land, mcp, signals};

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/decisions", get(list_decisions))
        .route("/api/decisions/{id}", get(get_decision))
        .route("/api/decisions/{id}/respond", post(respond))
        .route("/api/herds/{id}/decide", post(decide))
        .route("/api/knowledge", get(search_knowledge))
        .route("/api/land/{paddock_id}", get(land_report))
        .route("/api/signals", get(get_signals))
        .route("/mcp", any(mcp::handler))
}

#[derive(Deserialize)]
struct ListQuery {
    herd_id: Option<String>,
    limit: Option<i64>,
}

async fn list_decisions(State(ctx): State<Ctx>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Decision>>> {
    Ok(Json(db::list(&ctx, q.herd_id.as_deref(), q.limit.unwrap_or(50).clamp(1, 500)).await?))
}

async fn get_decision(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<Decision>> {
    Ok(Json(db::get(&ctx, &id).await?.ok_or_else(|| ApiError::not_found("No such decision."))?))
}

async fn decide(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<(StatusCode, Json<Decision>)> {
    Ok((StatusCode::ACCEPTED, Json(cycle::start_decision(&ctx, &id).await?)))
}

#[derive(Deserialize)]
struct RespondBody {
    action: Response,
    geometry: Option<Polygon>,
    note: Option<String>,
}

async fn respond(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(b): ApiJson<RespondBody>) -> ApiResult<Json<Decision>> {
    Ok(Json(cycle::respond(&ctx, &id, b.action, b.geometry, b.note).await?))
}

#[derive(Deserialize)]
struct KnowledgeQuery {
    q: Option<String>,
    limit: Option<usize>,
}

async fn search_knowledge(State(ctx): State<Ctx>, Query(q): Query<KnowledgeQuery>) -> ApiResult<Json<Vec<knowledge::Entry>>> {
    Ok(Json(knowledge::search(&ctx, q.q.as_deref().unwrap_or(""), q.limit.unwrap_or(10)).await?))
}

#[derive(Deserialize)]
struct LandQuery {
    #[serde(default)]
    refresh: bool,
}

async fn land_report(State(ctx): State<Ctx>, Path(pid): Path<String>, Query(q): Query<LandQuery>) -> ApiResult<Json<Value>> {
    Ok(Json(land_view(&ctx, &pid, q.refresh).await?))
}

/// The paddock's land report (cached for six hours) with a one-line summary per section.
pub async fn land_view(ctx: &Ctx, paddock_id: &str, refresh: bool) -> ApiResult<Value> {
    let p = ctx.store().get_paddock(paddock_id).await?.ok_or_else(|| ApiError::not_found("No such paddock."))?;
    let (r, cached) = land::report_for_paddock(ctx, &p, refresh).await?;
    Ok(json!({
        "paddock_id": p.id,
        "report_id": r["report_id"],
        "source": r["source"],
        "as_of": r["as_of"],
        "cached": cached,
        "summary": land::summary(&r),
        "sections": r["sections"],
    }))
}

#[derive(Deserialize)]
struct SignalsQuery {
    herd_id: Option<String>,
}

async fn get_signals(State(ctx): State<Ctx>, Query(q): Query<SignalsQuery>) -> ApiResult<Json<Value>> {
    Ok(Json(signals_view(&ctx, q.herd_id.as_deref()).await?))
}

/// Grazing signals per paddock for a herd (default: the first herd), from
/// cached land reports only so it stays quick.
pub async fn signals_view(ctx: &Ctx, herd_id: Option<&str>) -> ApiResult<Value> {
    let store = ctx.store();
    let herd = match herd_id {
        Some(id) => Some(store.get_herd(id).await?.ok_or_else(|| ApiError::not_found("No such herd."))?),
        None => store.list_herds().await?.into_iter().next(),
    };
    let paddocks = store.list_paddocks().await?;
    let (current, source) = match &herd {
        Some(h) => {
            let (c, s, _, _) = context::locate(ctx, h, &paddocks).await?;
            (c, s)
        }
        None => (None, "unknown"),
    };
    let mut reports = HashMap::new();
    for p in &paddocks {
        if let Some(r) = land::latest(ctx, &p.id).await? {
            reports.insert(p.id.clone(), r);
        }
    }
    let history = match &herd {
        Some(h) => db::list(ctx, Some(&h.id), 10).await?,
        None => vec![],
    };
    let sig = signals::compute(
        ctx,
        signals::SignalInputs {
            herd: herd.as_ref(),
            paddocks: &paddocks,
            current: current.as_deref(),
            reports: &reports,
            history: &history,
            now: op_core::time::now(),
        },
    )
    .await?;
    Ok(json!({
        "as_of": sig["as_of"],
        "herd_id": herd.as_ref().map(|h| &h.id),
        "current_paddock_id": current,
        "position_source": source,
        "herd_animal_units": sig["herd_animal_units"],
        "feed_budget_days_current": sig["feed_budget_days_current"],
        "behavior": sig["behavior"],
        "risk_flags": sig["risk_flags"],
        "assumptions": sig["assumptions"],
        "paddocks": signals::per_paddock(&sig, &paddocks, current.as_deref()),
    }))
}
