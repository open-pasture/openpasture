//! The feed log: supplemental dry matter given to a herd, per farm-local day.
//! The organic report subtracts it from demand.
//!
//! `GET /api/feed-log?herd_id=&from=&to=` (newest first), `POST /api/feed-log`
//! (hands and up), `PATCH/DELETE /api/feed-log/{id}` (managers and up).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate, Utc};
use op_core::time::{from_db, now, to_db};
use op_core::{Actor, ApiError, ApiJson, ApiResult, Ctx, Identity, id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

/// `fed_…`
pub const FEED: &str = "fed";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedEntry {
    pub id: String,
    pub herd_id: String,
    pub date: NaiveDate,
    pub kg_dm: f64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Actor>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct NewEntry {
    herd_id: String,
    date: NaiveDate,
    kg_dm: f64,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Filter {
    pub herd_id: Option<String>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
}

fn from_row(r: &SqliteRow) -> anyhow::Result<FeedEntry> {
    let date: String = r.get("date");
    let created_by: String = r.get("created_by");
    Ok(FeedEntry {
        id: r.get("id"),
        herd_id: r.get("herd_id"),
        date: date.parse()?,
        kg_dm: r.get("kg_dm"),
        kind: r.get("kind"),
        note: r.get("note"),
        created_by: serde_json::from_str(&created_by).ok(),
        created_at: from_db(&r.get::<String, _>("created_at"))?,
    })
}

/// Entries by date (oldest first) for the filter.
pub async fn list(ctx: &Ctx, f: &Filter) -> anyhow::Result<Vec<FeedEntry>> {
    let rows = sqlx::query(
        "SELECT * FROM feed_log WHERE (?1 IS NULL OR herd_id = ?1) AND (?2 IS NULL OR date >= ?2) AND (?3 IS NULL OR date <= ?3)
         ORDER BY date, created_at, id",
    )
    .bind(&f.herd_id)
    .bind(f.from.map(|d| d.to_string()))
    .bind(f.to.map(|d| d.to_string()))
    .fetch_all(ctx.db())
    .await?;
    rows.iter().map(from_row).collect()
}

async fn get_one(ctx: &Ctx, id: &str) -> ApiResult<FeedEntry> {
    let row = sqlx::query("SELECT * FROM feed_log WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    Ok(row.map(|r| from_row(&r)).transpose()?.ok_or_else(|| ApiError::not_found("No such feed entry."))?)
}

fn check(e: &FeedEntry) -> ApiResult<()> {
    if !(e.kg_dm.is_finite() && e.kg_dm >= 0.0 && e.kg_dm <= 1_000_000.0) {
        return Err(ApiError::bad_request("Dry matter must be between 0 and 1,000,000 kg."));
    }
    if e.kind.is_empty() || e.kind.chars().count() > 40 {
        return Err(ApiError::bad_request("Kind must be 1 to 40 characters."));
    }
    if e.note.as_ref().is_some_and(|n| n.chars().count() > 500) {
        return Err(ApiError::bad_request("Keep the note under 500 characters."));
    }
    Ok(())
}

async fn check_herd(ctx: &Ctx, herd_id: &str) -> ApiResult<()> {
    if ctx.store().get_herd(herd_id).await?.is_none() {
        return Err(ApiError::bad_request("No such herd."));
    }
    Ok(())
}

fn tidy(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

async fn list_entries(State(ctx): State<Ctx>, Query(f): Query<Filter>) -> ApiResult<Json<Vec<FeedEntry>>> {
    let mut out = list(&ctx, &f).await?;
    out.reverse();
    Ok(Json(out))
}

async fn create(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<NewEntry>) -> ApiResult<(StatusCode, Json<FeedEntry>)> {
    check_herd(&ctx, &b.herd_id).await?;
    let e = FeedEntry {
        id: id::new_id(FEED),
        herd_id: b.herd_id,
        date: b.date,
        kg_dm: b.kg_dm,
        kind: tidy(b.kind).unwrap_or_else(|| "hay".into()),
        note: tidy(b.note),
        created_by: Some(identity.actor()),
        created_at: now(),
    };
    check(&e)?;
    sqlx::query("INSERT INTO feed_log (id, herd_id, date, kg_dm, kind, note, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(&e.id)
        .bind(&e.herd_id)
        .bind(e.date.to_string())
        .bind(e.kg_dm)
        .bind(&e.kind)
        .bind(&e.note)
        .bind(serde_json::to_string(&e.created_by).map_err(anyhow::Error::from)?)
        .bind(to_db(&e.created_at))
        .execute(ctx.db())
        .await?;
    Ok((StatusCode::CREATED, Json(e)))
}

async fn update(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<FeedEntry>> {
    let current = get_one(&ctx, &id).await?;
    let mut e: FeedEntry = op_core::patch::apply(&current, &body, &["id", "created_by", "created_at"])?;
    e.note = tidy(e.note);
    e.kind = e.kind.trim().to_owned();
    if e.herd_id != current.herd_id {
        check_herd(&ctx, &e.herd_id).await?;
    }
    check(&e)?;
    sqlx::query("UPDATE feed_log SET herd_id = ?, date = ?, kg_dm = ?, kind = ?, note = ? WHERE id = ?")
        .bind(&e.herd_id)
        .bind(e.date.to_string())
        .bind(e.kg_dm)
        .bind(&e.kind)
        .bind(&e.note)
        .bind(&e.id)
        .execute(ctx.db())
        .await?;
    Ok(Json(e))
}

async fn delete(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let n = sqlx::query("DELETE FROM feed_log WHERE id = ?").bind(&id).execute(ctx.db()).await?.rows_affected();
    if n == 0 {
        return Err(ApiError::not_found("No such feed entry."));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/feed-log", get(list_entries).post(create)).route("/api/feed-log/{id}", patch(update).delete(delete))
}
