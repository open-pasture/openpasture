//! `/api/reports*`, `/api/feed-log*`, `/api/leases*` and the `get_report` tool.
//!
//! `GET /api/reports` → `[{id, title}]`;
//! `GET /api/reports/{id}?from=&to=&herd_id=&format=json|csv` → `ReportDoc` or
//! one CSV file. `from`/`to` are farm-local days (default: this year to today).

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{Datelike, NaiveDate};
use op_core::tools::{ToolCall, ToolSpec};
use op_core::{ApiError, ApiResult, Ctx, Role};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::history::Farm;
use crate::{ReportDoc, ReportParams, csv, feed_log, leases_api, report, reports, settings};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportInfo {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Default, Deserialize)]
struct ReportQuery {
    from: Option<String>,
    to: Option<String>,
    herd_id: Option<String>,
    format: Option<String>,
}

fn day(s: Option<&str>, what: &str) -> ApiResult<Option<NaiveDate>> {
    match s.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => s.parse::<NaiveDate>().map(Some).map_err(|_| ApiError::bad_request(format!("{what} must be a date, YYYY-MM-DD."))),
    }
}

/// Parameters from a query or tool call; default this year to today, farm time.
async fn params(ctx: &Ctx, from: Option<&str>, to: Option<&str>, herd_id: Option<&str>) -> ApiResult<ReportParams> {
    let today = Farm::load(ctx).await?.today();
    let to = day(to, "to")?.unwrap_or(today);
    let from = day(from, "from")?.unwrap_or_else(|| NaiveDate::from_ymd_opt(to.year(), 1, 1).unwrap_or(to));
    if from > to {
        return Err(ApiError::bad_request("from is after to."));
    }
    if (to - from).num_days() > 3700 {
        return Err(ApiError::bad_request("Keep a report to ten years or less."));
    }
    let herd_id = herd_id.map(str::trim).filter(|h| !h.is_empty()).map(str::to_owned);
    if let Some(h) = &herd_id {
        let known = ctx.store().get_herd(h).await?.is_some()
            || sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM herd_history WHERE herd_id = ?").bind(h).fetch_one(ctx.db()).await? > 0;
        if !known {
            return Err(ApiError::not_found("No such herd."));
        }
    }
    Ok(ReportParams { from, to, herd_id })
}

/// Build report `id`, or 404.
pub async fn build(ctx: &Ctx, id: &str, p: &ReportParams) -> ApiResult<ReportDoc> {
    let r = report(id).ok_or_else(|| ApiError::not_found("No such report."))?;
    Ok(r.build(ctx, p).await?)
}

async fn list() -> Json<Vec<ReportInfo>> {
    Json(reports().iter().map(|r| ReportInfo { id: r.id().into(), title: r.title().into() }).collect())
}

async fn get_report(State(ctx): State<Ctx>, Path(id): Path<String>, Query(q): Query<ReportQuery>) -> ApiResult<Response> {
    if report(&id).is_none() {
        return Err(ApiError::not_found("No such report."));
    }
    let p = params(&ctx, q.from.as_deref(), q.to.as_deref(), q.herd_id.as_deref()).await?;
    let doc = build(&ctx, &id, &p).await?;
    match q.format.as_deref().unwrap_or("json") {
        "json" => Ok(Json(doc).into_response()),
        "csv" => {
            let body = csv::write(&doc)?;
            let name = format!("{}-{}-{}.csv", doc.id, doc.from, doc.to);
            Ok((
                [(header::CONTENT_TYPE, "text/csv; charset=utf-8".to_owned()), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""))],
                body,
            )
                .into_response())
        }
        _ => Err(ApiError::bad_request("format is json or csv.")),
    }
}

pub fn router() -> Router<Ctx> {
    let mut app = Router::new().route("/api/reports", get(list)).route("/api/reports/{id}", get(get_report));
    for part in [settings::router(), feed_log::router(), leases_api::router()] {
        app = app.merge(part);
    }
    app
}

/// MCP `get_report` (read).
pub fn get_report_tool() -> ToolSpec {
    let ids: Vec<&'static str> = reports().iter().map(|r| r.id()).collect();
    ToolSpec {
        name: "get_report",
        description: "A farm report as tables in the farm's units, with its header, method notes and signature lines: paddock_record (every grazing event with head-days, AU-days, stocking density, rest), nrcs_528 (NRCS prescribed grazing record), organic_season (days on pasture, dry matter from pasture), lease_head_days (grazing and amounts per landowner), welfare (cues, tone, episodes and learning status per animal). Dates are farm-local YYYY-MM-DD; the default is this year to today.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "enum": ids },
                "from": { "type": "string", "description": "First day, YYYY-MM-DD." },
                "to": { "type": "string", "description": "Last day, YYYY-MM-DD." },
                "herd_id": { "type": "string", "description": "Only this herd." }
            },
            "required": ["id"],
            "additionalProperties": false
        }),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run: ToolSpec::run_fn(|c: ToolCall| async move {
            let arg = |k: &str| c.args.get(k).and_then(Value::as_str).map(str::to_owned);
            let id = arg("id").ok_or_else(|| ApiError::bad_request("id is required."))?;
            if report(&id).is_none() {
                return Err(ApiError::bad_request("No such report."));
            }
            let p = params(&c.ctx, arg("from").as_deref(), arg("to").as_deref(), arg("herd_id").as_deref()).await?;
            let doc = build(&c.ctx, &id, &p).await?;
            Ok(serde_json::to_value(doc).map_err(anyhow::Error::from)?)
        }),
    }
}
