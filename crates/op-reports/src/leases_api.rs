//! Paddock leases: the landowner of a rented paddock and the grazing rate.
//!
//! `GET /api/leases` (leases of existing paddocks), `GET/PUT/DELETE
//! /api/leases/{paddock_id}`. For `acre_season` the rate is per hectare, like
//! every area in the API; the UI shows it per acre on imperial farms.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate, Utc};
use op_core::time::{from_db, now, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Ctx};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RatePer {
    /// A flat rent per area for the season.
    AcreSeason,
    HeadDay,
    AuDay,
    /// Animal-unit months (AU-days ÷ 30.4).
    Aum,
    /// Cow-calf pairs × days ÷ 30.4.
    PairMonth,
}

impl RatePer {
    pub fn as_db(&self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub paddock_id: String,
    pub landowner: String,
    pub rate_per: RatePer,
    pub rate_amount: f64,
    pub currency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season_from: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season_to: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct LeaseBody {
    landowner: String,
    rate_per: RatePer,
    rate_amount: f64,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    season_from: Option<NaiveDate>,
    #[serde(default)]
    season_to: Option<NaiveDate>,
    #[serde(default)]
    notes: Option<String>,
}

fn from_row(r: &SqliteRow) -> anyhow::Result<Lease> {
    let rate_per: String = r.get("rate_per");
    let date = |k: &str| -> anyhow::Result<Option<NaiveDate>> { r.get::<Option<String>, _>(k).map(|s| s.parse()).transpose().map_err(Into::into) };
    Ok(Lease {
        paddock_id: r.get("paddock_id"),
        landowner: r.get("landowner"),
        rate_per: serde_json::from_value(serde_json::Value::String(rate_per))?,
        rate_amount: r.get("rate_amount"),
        currency: r.get("currency"),
        season_from: date("season_from")?,
        season_to: date("season_to")?,
        notes: r.get("notes"),
        updated_at: from_db(&r.get::<String, _>("updated_at"))?,
    })
}

/// Every lease on record, including those of deleted paddocks.
pub async fn all(ctx: &Ctx) -> anyhow::Result<Vec<Lease>> {
    let rows = sqlx::query("SELECT * FROM paddock_leases ORDER BY landowner, paddock_id").fetch_all(ctx.db()).await?;
    rows.iter().map(from_row).collect()
}

async fn get_lease(ctx: &Ctx, paddock_id: &str) -> anyhow::Result<Option<Lease>> {
    let row = sqlx::query("SELECT * FROM paddock_leases WHERE paddock_id = ?").bind(paddock_id).fetch_optional(ctx.db()).await?;
    row.map(|r| from_row(&r)).transpose()
}

async fn list(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<Lease>>> {
    let paddocks: std::collections::BTreeSet<String> = ctx.store().list_paddocks().await?.into_iter().map(|p| p.id).collect();
    Ok(Json(all(&ctx).await?.into_iter().filter(|l| paddocks.contains(&l.paddock_id)).collect()))
}

async fn get_one(State(ctx): State<Ctx>, Path(paddock_id): Path<String>) -> ApiResult<Json<Lease>> {
    get_lease(&ctx, &paddock_id).await?.map(Json).ok_or_else(|| ApiError::not_found("This paddock has no lease."))
}

async fn put(State(ctx): State<Ctx>, Path(paddock_id): Path<String>, ApiJson(b): ApiJson<LeaseBody>) -> ApiResult<Json<Lease>> {
    if ctx.store().get_paddock(&paddock_id).await?.is_none() {
        return Err(ApiError::not_found("No such paddock."));
    }
    let landowner = b.landowner.trim().to_owned();
    if landowner.is_empty() || landowner.chars().count() > 120 {
        return Err(ApiError::bad_request("The landowner needs a name under 120 characters."));
    }
    if !(b.rate_amount.is_finite() && b.rate_amount >= 0.0 && b.rate_amount <= 1_000_000.0) {
        return Err(ApiError::bad_request("The rate must be between 0 and 1,000,000."));
    }
    let currency = b.currency.map(|c| c.trim().to_uppercase()).filter(|c| !c.is_empty()).unwrap_or_else(|| "USD".into());
    if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_uppercase()) {
        return Err(ApiError::bad_request("Currency is a three-letter code, like USD."));
    }
    if let (Some(a), Some(z)) = (b.season_from, b.season_to)
        && z < a
    {
        return Err(ApiError::bad_request("The season ends before it starts."));
    }
    let notes = b.notes.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
    if notes.as_ref().is_some_and(|n| n.chars().count() > 1000) {
        return Err(ApiError::bad_request("Keep notes under 1,000 characters."));
    }
    let lease = Lease {
        paddock_id,
        landowner,
        rate_per: b.rate_per,
        rate_amount: b.rate_amount,
        currency,
        season_from: b.season_from,
        season_to: b.season_to,
        notes,
        updated_at: now(),
    };
    sqlx::query(
        "INSERT INTO paddock_leases (paddock_id, landowner, rate_per, rate_amount, currency, season_from, season_to, notes, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(paddock_id) DO UPDATE SET landowner = excluded.landowner, rate_per = excluded.rate_per, rate_amount = excluded.rate_amount,
           currency = excluded.currency, season_from = excluded.season_from, season_to = excluded.season_to, notes = excluded.notes,
           updated_at = excluded.updated_at",
    )
    .bind(&lease.paddock_id)
    .bind(&lease.landowner)
    .bind(lease.rate_per.as_db())
    .bind(lease.rate_amount)
    .bind(&lease.currency)
    .bind(lease.season_from.map(|d| d.to_string()))
    .bind(lease.season_to.map(|d| d.to_string()))
    .bind(&lease.notes)
    .bind(to_db(&lease.updated_at))
    .execute(ctx.db())
    .await?;
    Ok(Json(lease))
}

async fn delete(State(ctx): State<Ctx>, Path(paddock_id): Path<String>) -> ApiResult<StatusCode> {
    let n = sqlx::query("DELETE FROM paddock_leases WHERE paddock_id = ?").bind(&paddock_id).execute(ctx.db()).await?.rows_affected();
    if n == 0 {
        return Err(ApiError::not_found("This paddock has no lease."));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/leases", get(list)).route("/api/leases/{paddock_id}", get(get_one).put(put).delete(delete))
}
