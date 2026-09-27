//! What each collar holds (protocol v1 §3.7, §3.8): the `collar_slots` rows.
//!
//! A v1 collar lists every boundary it holds in each report (`applied` is the
//! one in effect, `received` the staged ones); that list replaces its rows.
//! Between reports, and for legacy collars that never list them, acks keep
//! the rows: `received` stores a version, `applied` stores it and drops every
//! lower one (the collar does the same), `rejected` stores the refusal and
//! its code so the download path can skip versions refused for good. A
//! rejected row goes once the collar holds a higher version.
//!
//! Counts per herd version (`SlotCount`) cover the herd's collars that are
//! neither parked nor out on an escape; a copy of a herd boundary handed back
//! after an escape counts for the version it copies.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::time::{from_db, opt_from_db, to_db};
use op_core::{AckStatus, ApiError, ApiResult, Ctx, DbEnum, SlotCount};
use op_geo::CollarLimits;
use op_protocol::{RejectCode, SlotReport};
use serde::Serialize;
use sqlx::{Row, SqliteConnection};

use crate::db;
use crate::shape::CollarCaps;

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/herds/{id}/slots", get(get_herd)).route("/api/collars/{id}/slots", get(get_collar))
}

/// Replace a collar's applied and received rows with the complete list from
/// its report. Rejected rows at or below the highest version it holds go:
/// those versions are never offered to it again anyway.
pub(crate) async fn replace(conn: &mut SqliteConnection, collar_id: &str, slots: &[SlotReport], at: DateTime<Utc>) -> anyhow::Result<()> {
    let top = slots.iter().map(|s| s.version).max().unwrap_or(0);
    sqlx::query("DELETE FROM collar_slots WHERE collar_id = ? AND (status != 'rejected' OR version <= ?)")
        .bind(collar_id)
        .bind(top as i64)
        .execute(&mut *conn)
        .await?;
    for s in slots {
        sqlx::query(
            "INSERT INTO collar_slots (collar_id, version, status, effective_at, reported_at, code) VALUES (?, ?, ?, ?, ?, NULL)
             ON CONFLICT(collar_id, version) DO UPDATE SET status = excluded.status, effective_at = excluded.effective_at,
                 reported_at = excluded.reported_at, code = NULL",
        )
        .bind(collar_id)
        .bind(s.version as i64)
        .bind(s.status.as_str())
        .bind(s.effective_at.as_ref().map(to_db))
        .bind(to_db(&at))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Note an ack in the collar's rows (see the module docs).
pub(crate) async fn record_ack(
    conn: &mut SqliteConnection,
    collar_id: &str,
    version: u32,
    status: AckStatus,
    code: Option<RejectCode>,
    effective_at: Option<DateTime<Utc>>,
    at: DateTime<Utc>,
) -> anyhow::Result<()> {
    if status == AckStatus::Applied {
        let (newer,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM collar_slots WHERE collar_id = ? AND version > ? AND status = 'applied'")
            .bind(collar_id)
            .bind(version as i64)
            .fetch_one(&mut *conn)
            .await?;
        if newer > 0 {
            // A late ack: the collar has applied something newer since.
            return Ok(());
        }
        sqlx::query("DELETE FROM collar_slots WHERE collar_id = ? AND version < ?").bind(collar_id).bind(version as i64).execute(&mut *conn).await?;
    }
    // A late `received` never takes an applied row back to staged.
    sqlx::query(
        "INSERT INTO collar_slots (collar_id, version, status, effective_at, reported_at, code) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(collar_id, version) DO UPDATE SET status = excluded.status, effective_at = excluded.effective_at,
             reported_at = excluded.reported_at, code = excluded.code
         WHERE NOT (collar_slots.status = 'applied' AND excluded.status = 'received')",
    )
    .bind(collar_id)
    .bind(version as i64)
    .bind(status.as_str())
    .bind(effective_at.as_ref().map(to_db))
    .bind(to_db(&at))
    .bind(code.map(|c| c.as_str()))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// How far each of `versions` (herd version, activation) has reached the
/// herd's collars that are neither parked nor out on an escape.
pub async fn counts(db: &sqlx::SqlitePool, herd_id: &str, versions: &[(u32, Option<DateTime<Utc>>)]) -> anyhow::Result<Vec<SlotCount>> {
    if versions.is_empty() {
        return Ok(vec![]);
    }
    let (collars,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM collars c WHERE c.herd_id = ? AND c.parked_at IS NULL
         AND NOT EXISTS (SELECT 1 FROM escapes e WHERE e.collar_id = c.id AND e.status = 'returning')",
    )
    .bind(herd_id)
    .fetch_one(db)
    .await?;
    let rows: Vec<(i64, String, i64)> = sqlx::query_as(
        "SELECT COALESCE(b.copy_of, s.version) AS v, s.status, COUNT(*) FROM collar_slots s
         JOIN collars c ON c.id = s.collar_id
         JOIN boundaries b ON b.version = s.version AND b.herd_id = c.herd_id
         WHERE c.herd_id = ? AND c.parked_at IS NULL
           AND NOT EXISTS (SELECT 1 FROM escapes e WHERE e.collar_id = c.id AND e.status = 'returning')
         GROUP BY v, s.status",
    )
    .bind(herd_id)
    .fetch_all(db)
    .await?;
    let mut by: HashMap<(u32, &str), u32> = HashMap::new();
    for (v, status, n) in &rows {
        *by.entry((*v as u32, status.as_str())).or_default() += *n as u32;
    }
    Ok(versions
        .iter()
        .map(|(v, at)| SlotCount {
            version: *v,
            effective_at: *at,
            applied: by.get(&(*v, "applied")).copied().unwrap_or(0),
            stored: by.get(&(*v, "received")).copied().unwrap_or(0),
            rejected: by.get(&(*v, "rejected")).copied().unwrap_or(0),
            collars: collars as u32,
        })
        .collect())
}

/// One boundary a collar holds (or refused).
#[derive(Debug, Clone, Serialize)]
pub struct HeldSlot {
    pub version: u32,
    /// The herd version this one copies (handed back after an escape).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<u32>,
    pub status: AckStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_at: Option<DateTime<Utc>>,
    pub reported_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// A collar's config as stored (unsigned view).
#[derive(Debug, Clone, Serialize)]
pub struct ConfigView {
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub report_s: u32,
    pub poll_s: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_report_s: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_poll_s: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_until: Option<DateTime<Utc>>,
    /// The collar refused this version.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub refused: bool,
}

/// What one collar is and holds.
#[derive(Debug, Clone, Serialize)]
pub struct CollarSlots {
    pub collar_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fw: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<String>,
    pub limits: CollarLimits,
    pub slots: Vec<HeldSlot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigView>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub parked: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub escaped: bool,
}

async fn load(ctx: &Ctx, herd_id: Option<&str>, collar_id: Option<&str>) -> anyhow::Result<Vec<CollarSlots>> {
    let collars = match (herd_id, collar_id) {
        (_, Some(c)) => sqlx::query("SELECT * FROM collars WHERE id = ?").bind(c).fetch_all(ctx.db()).await?,
        (Some(h), None) => sqlx::query("SELECT * FROM collars WHERE herd_id = ? ORDER BY created_at, id").bind(h).fetch_all(ctx.db()).await?,
        (None, None) => vec![],
    };
    let mut out = Vec::with_capacity(collars.len());
    let mut index = HashMap::new();
    for r in &collars {
        let id: String = r.try_get("id")?;
        let caps = CollarCaps::from_row(r)?;
        index.insert(id.clone(), out.len());
        out.push(CollarSlots {
            collar_id: id,
            fw: caps.fw,
            caps: caps.caps,
            limits: caps.limits,
            slots: vec![],
            config: None,
            parked: r.try_get::<Option<String>, _>("parked_at")?.is_some(),
            escaped: false,
        });
    }
    if out.is_empty() {
        return Ok(out);
    }
    let filter = match (herd_id, collar_id) {
        (_, Some(_)) => "c.id = ?1",
        _ => "c.herd_id = ?1",
    };
    let key = collar_id.or(herd_id).unwrap_or_default();
    let rows = sqlx::query(&format!(
        "SELECT s.*, b.copy_of FROM collar_slots s JOIN collars c ON c.id = s.collar_id
         LEFT JOIN boundaries b ON b.version = s.version WHERE {filter} ORDER BY s.collar_id, s.version"
    ))
    .bind(key)
    .fetch_all(ctx.db())
    .await?;
    for r in &rows {
        let Some(&i) = index.get(&r.try_get::<String, _>("collar_id")?) else { continue };
        out[i].slots.push(HeldSlot {
            version: r.try_get::<i64, _>("version")? as u32,
            copy_of: r.try_get::<Option<i64>, _>("copy_of")?.map(|v| v as u32),
            status: AckStatus::from_db(&r.try_get::<String, _>("status")?)?,
            effective_at: opt_from_db(r.try_get("effective_at")?)?,
            reported_at: from_db(&r.try_get::<String, _>("reported_at")?)?,
            code: r.try_get("code")?,
        });
    }
    let rows =
        sqlx::query(&format!("SELECT k.* FROM collar_config k JOIN collars c ON c.id = k.collar_id WHERE {filter}")).bind(key).fetch_all(ctx.db()).await?;
    for r in &rows {
        let Some(&i) = index.get(&r.try_get::<String, _>("collar_id")?) else { continue };
        let cmd: op_protocol::ConfigCommand = serde_json::from_str(&r.try_get::<String, _>("body")?)?;
        let refused = r.try_get::<Option<i64>, _>("reject_version")?.is_some_and(|v| v as u32 == cmd.version);
        out[i].config = Some(ConfigView {
            version: cmd.version,
            herd_id: cmd.herd_id,
            endpoint: cmd.endpoint,
            report_s: cmd.report_s,
            poll_s: cmd.poll_s,
            fast_report_s: cmd.fast_report_s,
            fast_poll_s: cmd.fast_poll_s,
            fast_until: cmd.fast_until,
            refused,
        });
    }
    let rows = sqlx::query(&format!("SELECT e.collar_id FROM escapes e JOIN collars c ON c.id = e.collar_id WHERE e.status = 'returning' AND {filter}"))
        .bind(key)
        .fetch_all(ctx.db())
        .await?;
    for r in &rows {
        if let Some(&i) = index.get(&r.try_get::<String, _>("collar_id")?) {
            out[i].escaped = true;
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
struct HerdSlots {
    counts: Vec<SlotCount>,
    collars: Vec<CollarSlots>,
}

/// The herd's slot counts for its active and staged versions, and what each collar holds.
async fn get_herd(State(ctx): State<Ctx>, Path(herd_id): Path<String>) -> ApiResult<Json<HerdSlots>> {
    if ctx.store().get_herd(&herd_id).await?.is_none() {
        return Err(ApiError::not_found("No such herd."));
    }
    let split = db::herd_boundaries(ctx.db(), &herd_id, op_core::time::now()).await?;
    let versions: Vec<(u32, Option<DateTime<Utc>>)> = split.active.iter().chain(&split.staged).map(|b| (b.version, b.effective_at)).collect();
    Ok(Json(HerdSlots { counts: counts(ctx.db(), &herd_id, &versions).await?, collars: load(&ctx, Some(&herd_id), None).await? }))
}

async fn get_collar(State(ctx): State<Ctx>, Path(collar_id): Path<String>) -> ApiResult<Json<CollarSlots>> {
    load(&ctx, None, Some(&collar_id)).await?.into_iter().next().map(Json).ok_or_else(|| ApiError::not_found("No such collar."))
}
