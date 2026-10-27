//! The collar console: everything a bench or field test needs about one
//! collar in one read. First contact, raw health, the latest fix with its
//! receiver quality, boundary acks with their codes, the cue log (with or
//! without an animal), and where the collar stands in firmware and in the
//! herd network. Slots and config come from `GET /api/collars/:id/slots`,
//! the wire log from `GET /api/collars/:id/wire`.

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::time::from_db;
use op_core::{ApiError, ApiResult, Collar, Ctx};
use serde::Serialize;
use sqlx::Row;

use crate::firmware::UnitFirmware;
use crate::network::Member;
use crate::units::Unit;

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/fleet/collars/{id}/console", get(console))
}

#[derive(Debug, Serialize)]
pub struct Contact {
    /// `waiting` until the first report, then `heard`.
    pub state: &'static str,
    /// When the collar was added.
    pub since: DateTime<Utc>,
    /// Where it should report, when this server has a collar address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    /// Last contact (server time) minus the newest fix (collar time): clock
    /// drift or a store-and-forward backlog.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_s: Option<f64>,
}

#[derive(Debug, Default, Serialize)]
pub struct Cell {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rsrp_dbm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rsrq_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub band: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tac: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_v: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charging: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub still_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tilt_deg: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_attempts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_ok: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell: Option<Cell>,
}

#[derive(Debug, Serialize)]
pub struct FixView {
    pub at: DateTime<Utc>,
    pub point: [f64; 2],
    pub accuracy_m: f64,
    pub sats: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cn0: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdop: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttf_s: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct AckView {
    pub at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub version: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub command_id: String,
}

#[derive(Debug, Serialize)]
pub struct CueView {
    pub at: DateTime<Utc>,
    pub kind: String,
    pub level: i64,
    pub margin_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ring: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dur_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary_version: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CollarConsole {
    pub collar: Collar,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<Unit>,
    pub contact: Contact,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<FixView>,
    pub acks: Vec<AckView>,
    pub cues: Vec<CueView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<UnitFirmware>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<Member>,
}

/// Rows kept in the ack and cue lists.
pub const LOG_ROWS: i64 = 50;

async fn console(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<CollarConsole>> {
    let row = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(&id).fetch_optional(ctx.db()).await?.ok_or_else(|| ApiError::not_found("No such collar."))?;
    let collar = op_core::store::collar_from_row(&row)?;
    let created_at = from_db(&row.try_get::<String, _>("created_at")?)?;
    let db = ctx.db();

    let fix = sqlx::query("SELECT at, lon, lat, accuracy_m, sats, cn0, ttf_s, hdop FROM fixes WHERE collar_id = ? ORDER BY t DESC LIMIT 1")
        .bind(&id)
        .fetch_optional(db)
        .await?
        .map(|r| -> anyhow::Result<FixView> {
            Ok(FixView {
                at: from_db(&r.try_get::<String, _>("at")?)?,
                point: [r.try_get("lon")?, r.try_get("lat")?],
                accuracy_m: r.try_get("accuracy_m")?,
                sats: r.try_get("sats")?,
                cn0: r.try_get("cn0")?,
                hdop: r.try_get("hdop")?,
                ttf_s: r.try_get("ttf_s")?,
            })
        })
        .transpose()?;

    let health = sqlx::query("SELECT * FROM health WHERE collar_id = ? ORDER BY t DESC LIMIT 1")
        .bind(&id)
        .fetch_optional(db)
        .await?
        .map(|r| -> anyhow::Result<Health> {
            let cell = Cell {
                rsrp_dbm: r.try_get("rsrp_dbm")?,
                rsrq_db: r.try_get("rsrq_db")?,
                snr_db: r.try_get("snr_db")?,
                mode: r.try_get("cell_mode")?,
                band: r.try_get("band")?,
                cell_id: r.try_get("cell_id")?,
                tac: r.try_get("tac")?,
            };
            let has_cell = cell.rsrp_dbm.is_some() || cell.cell_id.is_some() || cell.mode.is_some();
            Ok(Health {
                at: from_db(&r.try_get::<String, _>("at")?)?,
                battery: r.try_get("battery")?,
                battery_v: r.try_get("battery_v")?,
                charging: r.try_get::<Option<i64>, _>("charging")?.map(|c| c != 0),
                temp_c: r.try_get("temp_c")?,
                uptime_s: r.try_get("uptime_s")?,
                reset: r.try_get("reset")?,
                still_s: r.try_get("still_s")?,
                tilt_deg: r.try_get("tilt_deg")?,
                fix_attempts: r.try_get("fix_attempts")?,
                fix_ok: r.try_get("fix_ok")?,
                cell: has_cell.then_some(cell),
            })
        })
        .transpose()?;

    let acks = sqlx::query("SELECT * FROM acks WHERE collar_id = ? ORDER BY id DESC LIMIT ?")
        .bind(&id)
        .bind(LOG_ROWS)
        .fetch_all(db)
        .await?
        .iter()
        .map(|r| -> anyhow::Result<AckView> {
            Ok(AckView {
                at: from_db(&r.try_get::<String, _>("at")?)?,
                received_at: from_db(&r.try_get::<String, _>("received_at")?)?,
                version: r.try_get("version")?,
                status: r.try_get("status")?,
                code: r.try_get("code")?,
                reason: r.try_get("reason")?,
                command_id: r.try_get("command_id")?,
            })
        })
        .collect::<anyhow::Result<_>>()?;

    let cues = sqlx::query(
        "SELECT at, COALESCE(kind, CASE WHEN margin_m < 0 THEN 'outside' ELSE 'warn' END) AS kind, level, margin_m, ring, dur_ms, boundary_version
         FROM cues WHERE collar_id = ? ORDER BY t DESC LIMIT ?",
    )
    .bind(&id)
    .bind(LOG_ROWS)
    .fetch_all(db)
    .await?
    .iter()
    .map(|r| -> anyhow::Result<CueView> {
        Ok(CueView {
            at: from_db(&r.try_get::<String, _>("at")?)?,
            kind: r.try_get("kind")?,
            level: r.try_get("level")?,
            margin_m: r.try_get("margin_m")?,
            ring: r.try_get("ring")?,
            dur_ms: r.try_get("dur_ms")?,
            boundary_version: r.try_get("boundary_version")?,
        })
    })
    .collect::<anyhow::Result<_>>()?;

    let endpoint = crate::provision::collar_endpoint(&ctx).await?;
    let latency_s = match (collar.last_seen, &fix) {
        (Some(seen), Some(f)) => Some((seen - f.at).num_milliseconds() as f64 / 1000.0),
        _ => None,
    };
    let contact = Contact {
        state: if collar.last_seen.is_some() { "heard" } else { "waiting" },
        since: created_at,
        why: endpoint.is_none().then(|| crate::provision::NO_COLLAR_ADDRESS.to_owned()),
        endpoint,
        last_seen: collar.last_seen,
        latency_s,
    };
    let unit = crate::units::for_collar(&ctx, &id).await?;
    let firmware = crate::firmware::status_for_collar(&ctx, &id).await?;
    let network = crate::network::member(&ctx, &collar, unit.as_ref()).await?;
    Ok(Json(CollarConsole { collar, unit, contact, health, fix, acks, cues, firmware, network: Some(network) }))
}
