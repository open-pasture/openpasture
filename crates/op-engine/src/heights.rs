//! Forage heights measured in a paddock (`paddock_heights`). The latest one no
//! older than [`MAX_AGE_DAYS`] replaces the imagery estimate in the grazing
//! signals (`forage.source = "measured"`). `GET/POST /api/paddocks/{id}/heights`.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use op_core::{Actor, ApiError, ApiJson, ApiResult, Ctx, Identity, Role, time};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

/// Id prefix of a measured height.
pub const HEIGHT: &str = "hgt";
/// A measurement counts for this long; after that forage comes from imagery again.
pub const MAX_AGE_DAYS: i64 = 21;
/// Tallest height accepted, cm (about 10 ft).
const MAX_CM: f64 = 300.0;
/// How far ahead of the server's clock a measurement may be dated.
const CLOCK_SKEW_MIN: i64 = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Height {
    pub id: String,
    pub paddock_id: String,
    pub at: DateTime<Utc>,
    pub height_cm: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residual_cm: Option<f64>,
    pub by: Actor,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewHeight {
    pub height_cm: f64,
    #[serde(default)]
    pub residual_cm: Option<f64>,
    /// When it was measured; now when absent.
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
}

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/paddocks/{id}/heights", get(list_route).post(add_route))
}

fn from_row(r: &SqliteRow) -> anyhow::Result<Height> {
    Ok(Height {
        id: r.try_get("id")?,
        paddock_id: r.try_get("paddock_id")?,
        at: time::from_db(&r.try_get::<String, _>("at")?)?,
        height_cm: r.try_get("height_cm")?,
        residual_cm: r.try_get("residual_cm")?,
        by: serde_json::from_str(&r.try_get::<String, _>("by")?)?,
        created_at: time::from_db(&r.try_get::<String, _>("created_at")?)?,
    })
}

fn check_cm(v: f64, what: &str) -> ApiResult<()> {
    if v.is_finite() && v > 0.0 && v <= MAX_CM { Ok(()) } else { Err(ApiError::bad_request(format!("{what} must be between 0 and {MAX_CM} cm."))) }
}

/// Store a measurement for a paddock. 404 when there is no such paddock.
pub async fn add(ctx: &Ctx, paddock_id: &str, h: NewHeight, by: Actor) -> ApiResult<Height> {
    if ctx.store().get_paddock(paddock_id).await?.is_none() {
        return Err(ApiError::not_found("No such paddock."));
    }
    check_cm(h.height_cm, "height_cm")?;
    if let Some(r) = h.residual_cm {
        check_cm(r, "residual_cm")?;
        if r > h.height_cm {
            return Err(ApiError::bad_request("residual_cm can't be taller than height_cm."));
        }
    }
    let now = time::now();
    let at = h.at.map(|t| t.with_timezone(&Utc)).unwrap_or(now);
    if at > now + Duration::minutes(CLOCK_SKEW_MIN) {
        return Err(ApiError::bad_request("at is in the future."));
    }
    let row = Height {
        id: op_core::id::new_id(HEIGHT),
        paddock_id: paddock_id.to_owned(),
        at: time::from_unix_ms(time::unix_ms(&at)),
        height_cm: h.height_cm,
        residual_cm: h.residual_cm,
        by,
        created_at: now,
    };
    sqlx::query("INSERT INTO paddock_heights (id, paddock_id, at, height_cm, residual_cm, by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(&row.id)
        .bind(&row.paddock_id)
        .bind(time::to_db(&row.at))
        .bind(row.height_cm)
        .bind(row.residual_cm)
        .bind(serde_json::to_string(&row.by).map_err(anyhow::Error::from)?)
        .bind(time::to_db(&row.created_at))
        .execute(ctx.db())
        .await?;
    Ok(row)
}

/// A paddock's measurements, newest first.
pub async fn list(ctx: &Ctx, paddock_id: &str, limit: i64) -> anyhow::Result<Vec<Height>> {
    let rows = sqlx::query("SELECT * FROM paddock_heights WHERE paddock_id = ? ORDER BY at DESC, created_at DESC LIMIT ?")
        .bind(paddock_id)
        .bind(limit)
        .fetch_all(ctx.db())
        .await?;
    rows.iter().map(from_row).collect()
}

/// Per paddock, the newest measurement taken in the `MAX_AGE_DAYS` before `now`.
pub async fn current(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<HashMap<String, Height>> {
    let since = now - Duration::days(MAX_AGE_DAYS);
    let rows = sqlx::query("SELECT * FROM paddock_heights WHERE at >= ? AND at <= ? ORDER BY at DESC, created_at DESC")
        .bind(time::to_db(&since))
        .bind(time::to_db(&now))
        .fetch_all(ctx.db())
        .await?;
    let mut out = HashMap::new();
    for r in &rows {
        let h = from_row(r)?;
        out.entry(h.paddock_id.clone()).or_insert(h);
    }
    Ok(out)
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
}

async fn list_route(State(ctx): State<Ctx>, Path(id): Path<String>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Height>>> {
    if ctx.store().get_paddock(&id).await?.is_none() {
        return Err(ApiError::not_found("No such paddock."));
    }
    Ok(Json(list(&ctx, &id, q.limit.unwrap_or(50).clamp(1, 500)).await?))
}

async fn add_route(
    State(ctx): State<Ctx>,
    identity: Identity,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<NewHeight>,
) -> ApiResult<(StatusCode, Json<Height>)> {
    identity.require(Role::Hand)?;
    Ok((StatusCode::CREATED, Json(add(&ctx, &id, b, identity.actor()).await?)))
}
