//! SQL over the tables op-ingest writes: collars, boundaries, acks, fixes,
//! cues, health, collar slots, episodes and collar configs.

use chrono::{DateTime, Utc};
use op_core::store::{boundary_from_row, collar_from_row};
use op_core::time::to_db;
use op_core::{Boundary, BoundaryAck, Collar, DbEnum, Paddock};
use sqlx::Row;
use sqlx::{SqliteConnection, SqlitePool};

/// The statements on the report and download path, so tests can check their
/// query plans use indexes (`EXPLAIN QUERY PLAN`).
#[doc(hidden)]
pub mod sql {
    /// The herd boundary in effect at `?2`: the highest version activating by then.
    pub const HERD_ACTIVE: &str = "SELECT * FROM boundaries WHERE herd_id = ?1 AND collar_id IS NULL
         AND (effective_at IS NULL OR effective_at <= ?2) ORDER BY version DESC LIMIT 1";
    /// Herd boundaries above the active version: staged ones (dead ones are dropped after).
    pub const HERD_ABOVE: &str = "SELECT * FROM boundaries WHERE herd_id = ?1 AND collar_id IS NULL AND version > ?2 ORDER BY version";
    /// A collar's copies of herd boundaries (handed back after an escape) above a version.
    pub const OWN_COPIES: &str = "SELECT * FROM boundaries WHERE herd_id = ?1 AND collar_id = ?2 AND copy_of IS NOT NULL AND version > ?3 ORDER BY version";
    /// The pen of a collar's open escape.
    pub const OPEN_PEN: &str = "SELECT b.* FROM escapes e JOIN boundaries b ON b.id = e.boundary_id
         WHERE e.collar_id = ?1 AND e.status = 'returning' AND b.herd_id = ?2";
    /// Versions a collar refused for good (every code but `slots_full`; no code counts as permanent).
    pub const REJECTED: &str = "SELECT version FROM collar_slots WHERE collar_id = ?1 AND status = 'rejected'
         AND (code IS NULL OR code != 'slots_full')";
    /// How many boundaries a collar holds (applied or staged), as the server knows.
    pub const HELD: &str = "SELECT COUNT(*) FROM collar_slots WHERE collar_id = ?1 AND status != 'rejected'";
    /// The next boundary version (one sequence across herds).
    pub const NEXT_VERSION: &str = "SELECT COALESCE(MAX(version), 0) + 1 FROM boundaries";
    /// A collar's stored config.
    pub const CONFIG: &str = "SELECT body, reject_version FROM collar_config WHERE collar_id = ?1";
    /// Staged boundaries taking effect in a window (the activation watcher).
    pub const TAKING_EFFECT: &str = "SELECT * FROM boundaries WHERE effective_at > ?1 AND effective_at <= ?2 AND collar_id IS NULL ORDER BY version";
}

pub async fn get_collar(db: &SqlitePool, id: &str) -> anyhow::Result<Option<Collar>> {
    let row = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(id).fetch_optional(db).await?;
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
    let mut tx = db.begin().await?;
    let gone = sqlx::query("DELETE FROM collars WHERE id = ?").bind(id).execute(&mut *tx).await?.rows_affected() > 0;
    for t in ["collar_slots", "collar_config"] {
        sqlx::query(&format!("DELETE FROM {t} WHERE collar_id = ?")).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(gone)
}

// Boundaries

pub async fn boundary_by_id(db: &SqlitePool, id: &str) -> anyhow::Result<Option<Boundary>> {
    let row = sqlx::query("SELECT * FROM boundaries WHERE id = ?").bind(id).fetch_optional(db).await?;
    row.map(|r| boundary_from_row(&r)).transpose()
}

/// A set of boundaries split at one moment (protocol v1 §3.7): the one in
/// effect, and the staged ones still alive, in version order.
#[derive(Debug, Clone, Default)]
pub struct HerdBoundaries {
    pub active: Option<Boundary>,
    pub staged: Vec<Boundary>,
}

impl HerdBoundaries {
    /// Highest version in the set.
    pub fn latest(&self) -> Option<&Boundary> {
        self.staged.last().or(self.active.as_ref())
    }
}

/// When a boundary takes effect: `effective_at`; one without takes effect
/// as it is received, which orders before any time a staged one names (a
/// collar applies it at once and drops every lower version).
pub fn activation(b: &Boundary) -> DateTime<Utc> {
    b.effective_at.unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// The collars' rule: in effect at `now` is the highest version activating at
/// or before then; a staged version is dead once a higher version activates
/// at or before it.
pub fn split_boundaries(all: Vec<Boundary>, now: DateTime<Utc>) -> HerdBoundaries {
    let s = op_protocol::split_by_activation(all, now, |b| (b.version, activation(b)));
    HerdBoundaries { active: s.active, staged: s.staged }
}

/// A herd's boundaries in effect and staged at `now`, read with two index
/// lookups (the active one, then those above it), never every version.
pub async fn herd_boundaries_in(conn: &mut SqliteConnection, herd_id: &str, now: DateTime<Utc>) -> anyhow::Result<HerdBoundaries> {
    let active = sqlx::query(sql::HERD_ACTIVE).bind(herd_id).bind(to_db(&now)).fetch_optional(&mut *conn).await?;
    let active = active.map(|r| boundary_from_row(&r)).transpose()?;
    let floor = active.as_ref().map_or(0, |a| a.version);
    let above = sqlx::query(sql::HERD_ABOVE).bind(herd_id).bind(floor as i64).fetch_all(&mut *conn).await?;
    let mut all = above.iter().map(boundary_from_row).collect::<anyhow::Result<Vec<_>>>()?;
    all.extend(active);
    Ok(split_boundaries(all, now))
}

pub async fn herd_boundaries(db: &SqlitePool, herd_id: &str, now: DateTime<Utc>) -> anyhow::Result<HerdBoundaries> {
    herd_boundaries_in(&mut *db.acquire().await?, herd_id, now).await
}

/// Boundaries a collar holds as far as the server knows (its report's slot
/// list, else its acks).
pub async fn held_count(db: &SqlitePool, collar_id: &str) -> anyhow::Result<usize> {
    let (n,): (i64,) = sqlx::query_as(sql::HELD).bind(collar_id).fetch_one(db).await?;
    Ok(n as usize)
}

/// Versions a collar refused for good; the server doesn't offer them again.
pub async fn rejected_versions(db: &SqlitePool, collar_id: &str) -> anyhow::Result<Vec<u32>> {
    let rows: Vec<(i64,)> = sqlx::query_as(sql::REJECTED).bind(collar_id).fetch_all(db).await?;
    Ok(rows.into_iter().map(|r| r.0 as u32).collect())
}

/// Each collar's latest boundary state, for collars still in the herd (from
/// `collar_boundary_state`, kept by the ack handler). A collar holding a copy
/// of a herd boundary (handed back after an escape) shows the herd version it
/// copies. The reason is the latest ack's, for a rejection.
pub async fn latest_acks(db: &SqlitePool, herd_id: &str) -> anyhow::Result<Vec<BoundaryAck>> {
    let rows = sqlx::query(
        "SELECT s.collar_id, COALESCE(b.copy_of, s.version) AS version, s.status, s.code, s.at,
             CASE WHEN s.status = 'rejected' THEN
                 (SELECT a.reason FROM acks a WHERE a.collar_id = s.collar_id AND a.version = s.version ORDER BY a.id DESC LIMIT 1)
             END AS reason
         FROM collar_boundary_state s JOIN collars c ON c.id = s.collar_id
         LEFT JOIN boundaries b ON b.id = s.command_id
         WHERE c.herd_id = ?1 AND s.herd_id = ?1 ORDER BY s.collar_id",
    )
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
                code: r.try_get("code")?,
            })
        })
        .collect()
}

/// The smallest paddock containing a point.
pub fn paddock_for_point<'a>(paddocks: &'a [Paddock], p: op_geo::LonLat) -> Option<&'a Paddock> {
    paddocks.iter().filter(|pad| pad.geometry.contains(p)).min_by(|a, b| a.area_ha.partial_cmp(&b.area_ha).unwrap_or(std::cmp::Ordering::Equal))
}
