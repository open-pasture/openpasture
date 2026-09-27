//! SQL over the tables op-ingest writes: collars, boundaries, acks, fixes,
//! cues, health.

use chrono::{DateTime, Utc};
use op_core::store::{boundary_from_row, collar_from_row};
use op_core::time::to_db;
use op_core::{Boundary, BoundaryAck, Collar, DbEnum, Paddock};
use sqlx::Row;
use sqlx::SqlitePool;

pub async fn get_collar(db: &SqlitePool, id: &str) -> anyhow::Result<Option<Collar>> {
    let row = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(id).fetch_optional(db).await?;
    row.map(|r| collar_from_row(&r)).transpose()
}

pub async fn collar_by_key_hash(db: &SqlitePool, hash: &str) -> anyhow::Result<Option<Collar>> {
    let row = sqlx::query("SELECT * FROM collars WHERE key_hash = ?").bind(hash).fetch_optional(db).await?;
    row.map(|r| collar_from_row(&r)).transpose()
}

pub async fn list_collars(db: &SqlitePool, herd_id: Option<&str>) -> anyhow::Result<Vec<Collar>> {
    let rows = match herd_id {
        Some(h) => sqlx::query("SELECT * FROM collars WHERE herd_id = ? ORDER BY created_at, id").bind(h).fetch_all(db).await?,
        None => sqlx::query("SELECT * FROM collars ORDER BY created_at, id").fetch_all(db).await?,
    };
    rows.iter().map(collar_from_row).collect()
}

/// A new collar with its key hash.
pub async fn insert_collar(db: &SqlitePool, id: &str, name: &str, herd_id: &str, key_hash: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO collars (id, name, herd_id, key_hash, state, created_at) VALUES (?, ?, ?, ?, 'unknown', ?)")
        .bind(id)
        .bind(name)
        .bind(herd_id)
        .bind(key_hash)
        .bind(to_db(&op_core::time::now()))
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete_collar(db: &SqlitePool, id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query("DELETE FROM collars WHERE id = ?").bind(id).execute(db).await?.rows_affected() > 0)
}

// Boundaries

pub async fn boundaries_for_herd(db: &SqlitePool, herd_id: &str) -> anyhow::Result<Vec<Boundary>> {
    let rows = sqlx::query("SELECT * FROM boundaries WHERE herd_id = ? AND collar_id IS NULL ORDER BY version").bind(herd_id).fetch_all(db).await?;
    rows.iter().map(boundary_from_row).collect()
}

pub async fn boundary_by_id(db: &SqlitePool, id: &str) -> anyhow::Result<Option<Boundary>> {
    let row = sqlx::query("SELECT * FROM boundaries WHERE id = ?").bind(id).fetch_optional(db).await?;
    row.map(|r| boundary_from_row(&r)).transpose()
}

/// The herd's boundaries split by time: the newest one in effect, and the
/// staged ones (newer version, `effective_at` still ahead) in version order.
pub struct HerdBoundaries {
    pub active: Option<Boundary>,
    pub staged: Vec<Boundary>,
}

pub fn split_boundaries(all: Vec<Boundary>, now: DateTime<Utc>) -> HerdBoundaries {
    let mut active: Option<Boundary> = None;
    let mut future = Vec::new();
    for b in all {
        if b.effective_at.is_none_or(|t| t <= now) {
            if active.as_ref().is_none_or(|a| b.version > a.version) {
                active = Some(b);
            }
        } else {
            future.push(b);
        }
    }
    let floor = active.as_ref().map_or(0, |a| a.version);
    future.retain(|b| b.version > floor);
    future.sort_by_key(|b| b.version);
    HerdBoundaries { active, staged: future }
}

pub async fn herd_boundaries(db: &SqlitePool, herd_id: &str, now: DateTime<Utc>) -> anyhow::Result<HerdBoundaries> {
    Ok(split_boundaries(boundaries_for_herd(db, herd_id).await?, now))
}

/// Latest ack per collar still in the herd: highest version first, then the
/// most recent status for that version. A collar handed back to the herd's
/// boundary after an escape holds a copy of it; its ack shows the herd
/// version it copies.
pub async fn latest_acks(db: &SqlitePool, herd_id: &str) -> anyhow::Result<Vec<BoundaryAck>> {
    let rows = sqlx::query(
        "SELECT a.collar_id, COALESCE(b.copy_of, a.version) AS version, a.status, a.reason, a.at FROM (
             SELECT a.*, ROW_NUMBER() OVER (PARTITION BY a.collar_id ORDER BY a.version DESC, a.id DESC) AS rn
             FROM acks a JOIN collars c ON c.id = a.collar_id
             WHERE c.herd_id = ? AND a.herd_id = ?
         ) a LEFT JOIN boundaries b ON b.id = a.command_id WHERE a.rn = 1 ORDER BY a.collar_id",
    )
    .bind(herd_id)
    .bind(herd_id)
    .fetch_all(db)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(BoundaryAck {
                collar_id: r.try_get("collar_id")?,
                version: r.try_get::<i64, _>("version")? as u32,
                status: op_core::AckStatus::from_db(&r.try_get::<String, _>("status")?)?,
                reason: r.try_get("reason")?,
                at: op_core::time::from_db(&r.try_get::<String, _>("at")?)?,
            })
        })
        .collect()
}

/// The smallest paddock containing a point.
pub fn paddock_for_point<'a>(paddocks: &'a [Paddock], p: op_geo::LonLat) -> Option<&'a Paddock> {
    paddocks.iter().filter(|pad| pad.geometry.contains(p)).min_by(|a, b| a.area_ha.partial_cmp(&b.area_ha).unwrap_or(std::cmp::Ordering::Equal))
}
