//! Map feature routes (docs/API.md "Map features"): list, create, read,
//! change and delete exclusions, water, gates, shade, hazards, roads,
//! neighbour lines and the farm boundary, plus the `list_features` MCP tool.
//!
//! Types, the table, validation of a new feature and the reads live in
//! [`crate::features`]. Every change publishes [`Event::Feature`].

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::error::{ApiError, ApiJson, ApiResult};
use crate::features::{FeatureKind, MapFeature, NewFeature, check_geometry, get_feature, insert_feature, list_features};
use crate::tools::{ToolCall, ToolSpec};
use crate::{Ctx, DbEnum, Event, Role, patch, time};

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/features", get(list).post(create)).route("/api/features/{id}", get(read).patch(update).delete(remove))
}

/// `?kind=` one kind, `?paddock_id=` one paddock's (farm-wide ones have
/// none), `?active=true` those in effect now or `?active=<RFC 3339>` those in
/// effect then. Without `active`: all, whatever their window.
struct Filter {
    kind: Option<FeatureKind>,
    paddock_id: Option<String>,
    at: Option<DateTime<Utc>>,
}

impl Filter {
    fn parse(q: &HashMap<String, String>) -> ApiResult<Self> {
        let get = |k: &str| q.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
        let kind = get("kind").map(parse_kind).transpose()?;
        let at = match get("active") {
            None => None,
            Some("true" | "now") => Some(time::now()),
            Some(t) => Some(DateTime::parse_from_rfc3339(t).map_err(|_| ApiError::bad_request("active is true or an RFC 3339 time."))?.with_timezone(&Utc)),
        };
        Ok(Self { kind, paddock_id: get("paddock_id").map(str::to_owned), at })
    }
}

fn parse_kind(s: &str) -> ApiResult<FeatureKind> {
    FeatureKind::from_db(s)
        .map_err(|_| ApiError::bad_request(format!("Unknown kind {s}. Use exclusion, water, gate, shade, hazard, road, neighbour_line or farm_boundary.")))
}

async fn list(State(ctx): State<Ctx>, Query(q): Query<HashMap<String, String>>) -> ApiResult<Json<Vec<MapFeature>>> {
    let f = Filter::parse(&q)?;
    Ok(Json(list_features(&ctx, f.kind, f.paddock_id.as_deref(), f.at).await?))
}

async fn create(State(ctx): State<Ctx>, ApiJson(body): ApiJson<NewFeature>) -> ApiResult<(StatusCode, Json<MapFeature>)> {
    let feature = insert_feature(&ctx, body).await?;
    ctx.publish(Event::Feature { feature: feature.clone(), deleted: false });
    Ok((StatusCode::CREATED, Json(feature)))
}

async fn read(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<MapFeature>> {
    Ok(Json(find(&ctx, &id).await?))
}

async fn find(ctx: &Ctx, id: &str) -> ApiResult<MapFeature> {
    get_feature(ctx, id).await?.ok_or_else(|| ApiError::not_found("No such feature."))
}

async fn update(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<MapFeature>> {
    let feature = update_feature(&ctx, &id, &body).await?;
    ctx.publish(Event::Feature { feature: feature.clone(), deleted: false });
    Ok(Json(feature))
}

async fn remove(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let feature = delete_feature(&ctx, &id).await?;
    ctx.publish(Event::Feature { feature, deleted: true });
    Ok(StatusCode::NO_CONTENT)
}

/// Change a feature with a JSON merge patch over its record: `null` clears an
/// optional field (`paddock_id: null` makes it farm-wide, `active_until:
/// null` makes it lasting). `id`, `kind` and the timestamps don't change.
/// Checked as a new feature is: geometry type for the kind, paddock exists,
/// `active_until` after `active_from`. Publishing is the caller's job.
pub async fn update_feature(ctx: &Ctx, id: &str, body: &Value) -> ApiResult<MapFeature> {
    let current = find(ctx, id).await?;
    let mut f: MapFeature = patch::apply(&current, body, &["id", "kind", "created_at", "updated_at"])?;
    f.props = match f.props {
        Value::Null => Value::Object(Default::default()),
        v @ Value::Object(_) => v,
        _ => return Err(ApiError::bad_request("props must be an object.")),
    };
    f.geometry = check_geometry(f.kind, &f.geometry, &f.props)?;
    f.paddock_id = f.paddock_id.filter(|p| !p.is_empty());
    if let Some(p) = &f.paddock_id
        && ctx.store().get_paddock(p).await?.is_none()
    {
        return Err(ApiError::bad_request("No such paddock."));
    }
    if let (Some(from), Some(until)) = (f.active_from, f.active_until)
        && until <= from
    {
        return Err(ApiError::bad_request("active_until must be after active_from."));
    }
    f.name = clean_text(f.name, 200, "name")?;
    f.notes = clean_text(f.notes, 2000, "note")?;
    f.updated_at = time::now();
    let done = sqlx::query(
        "UPDATE features SET name = ?, geometry = ?, paddock_id = ?, notes = ?, props = ?, active_from = ?, active_until = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(&f.name)
    .bind(serde_json::to_string(&f.geometry).map_err(anyhow::Error::from)?)
    .bind(&f.paddock_id)
    .bind(&f.notes)
    .bind(f.props.to_string())
    .bind(f.active_from.as_ref().map(time::to_db))
    .bind(f.active_until.as_ref().map(time::to_db))
    .bind(time::to_db(&f.updated_at))
    .bind(&f.id)
    .execute(ctx.db())
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::not_found("No such feature."));
    }
    Ok(f)
}

/// Delete a feature and return what it was. Publishing is the caller's job.
pub async fn delete_feature(ctx: &Ctx, id: &str) -> ApiResult<MapFeature> {
    let feature = find(ctx, id).await?;
    let done = sqlx::query("DELETE FROM features WHERE id = ?").bind(id).execute(ctx.db()).await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::not_found("No such feature."));
    }
    Ok(feature)
}

/// Trimmed; empty is none; at most `max` characters (the same rule as a new feature's).
fn clean_text(s: Option<String>, max: usize, what: &str) -> ApiResult<Option<String>> {
    let s = s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    if s.as_ref().is_some_and(|s| s.chars().count() > max) {
        return Err(ApiError::bad_request(format!("The {what} is too long.")));
    }
    Ok(s)
}

/// `list_features`: the farm's map features for an agent or a text question.
pub fn tool() -> ToolSpec {
    ToolSpec {
        name: "list_features",
        description: "Map features: exclusions (ground kept out of every boundary sent while they are active), water, gates, shade, hazards, roads, neighbour lines and the farm boundary. Each has a kind, an optional name, GeoJSON geometry ([longitude, latitude]), the paddock it belongs to (none = farm-wide), notes, and an optional active window (active_from inclusive, active_until exclusive).",
        input_schema: json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["exclusion", "water", "gate", "shade", "hazard", "road", "neighbour_line", "farm_boundary"] },
                "paddock_id": { "type": "string", "description": "Only this paddock's features (farm-wide ones have no paddock)." },
                "active": { "type": "boolean", "description": "Only those in effect now." }
            },
            "required": [],
            "additionalProperties": false
        }),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run: ToolSpec::run_fn(|c: ToolCall| async move { list_tool(&c.ctx, &c.args).await }),
    }
}

async fn list_tool(ctx: &Ctx, args: &Value) -> ApiResult<Value> {
    let kind = args.get("kind").and_then(Value::as_str).map(parse_kind).transpose()?;
    let paddock_id = args.get("paddock_id").and_then(Value::as_str).filter(|p| !p.is_empty());
    let at = args.get("active").and_then(Value::as_bool).unwrap_or(false).then(time::now);
    let features = list_features(ctx, kind, paddock_id, at).await?;
    Ok(serde_json::to_value(features).map_err(anyhow::Error::from)?)
}
