//! Device endpoints under `/collar/v1` (protocol v1, field-ready §3; the V0
//! shape of `opencollar/protocol/README.md` is a subset). Every request
//! carries `Authorization: Bearer <collar key>`.
//!
//! - `POST /collar/v1/report`: fixes, cues, episodes, health, what the collar
//!   is (`device`) and holds (`slots`). The reply names the highest boundary
//!   version for it and, for a collar with the `config` cap, carries its
//!   signed config when it holds an older one.
//! - `GET /collar/v1/boundary?have=&free=&free_bytes=`: the boundary in effect
//!   when it is newer than `have`, else the lowest staged one above `have`
//!   while the collar has a free slot and room for it; versions it refused
//!   for good are skipped. Each command is fitted to the collar's caps.
//! - `POST /collar/v1/ack`: received, applied or rejected (with a `code`).

use axum::extract::{FromRequestParts, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, SubsecRound, Utc};
use op_core::time::{now, to_db, unix_ms};
use op_core::{ApiError, ApiJson, ApiResult, Boundary, Collar, Ctx, Event, FenceState, Fix, id, keys};
use op_geo::{CollarLimits, Geofence, GeofenceConfig};
use op_protocol::{Ack, AckStatus, PositionReport, ReportResponse, caps};
use serde::Deserialize;
use sqlx::Row;

use crate::shape::{self, CollarCaps};
use crate::{config, db, escapes, slots};

/// Id prefix of stored episodes.
pub const EPISODE: &str = "epi";

pub fn router() -> Router<Ctx> {
    Router::new().route("/collar/v1/report", post(report)).route("/collar/v1/boundary", get(get_boundary)).route("/collar/v1/ack", post(ack))
}

/// The collar a request's key belongs to, and its reported limits (JSON).
pub struct Device {
    pub collar: Collar,
    pub limits: Option<String>,
}

impl Device {
    fn caps(&self) -> CollarCaps {
        CollarCaps::of(&self.collar, self.limits.as_deref())
    }
}

impl FromRequestParts<Ctx> for Device {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, ctx: &Ctx) -> Result<Self, Self::Rejection> {
        let key = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| ApiError::unauthorized("Missing collar key."))?;
        let row = sqlx::query("SELECT * FROM collars WHERE key_hash = ?").bind(keys::hash_key(key)).fetch_optional(ctx.db()).await?;
        let row = row.ok_or_else(|| ApiError::unauthorized("Unknown collar key."))?;
        Ok(Device { collar: op_core::store::collar_from_row(&row)?, limits: row.try_get("limits")? })
    }
}

fn check_collar_id(collar: &Collar, claimed: Option<&str>) -> ApiResult<()> {
    match claimed {
        Some(c) if !c.is_empty() && c != collar.id => Err(ApiError::new(StatusCode::FORBIDDEN, format!("This key belongs to collar {}, not {c}.", collar.id))),
        _ => Ok(()),
    }
}

/// Reports may run a little ahead of the server clock, not far.
const MAX_CLOCK_AHEAD: Duration = Duration::minutes(10);

/// Big enough for any fitted shape: the server fence takes the collar's
/// shape as it is (fitting already kept it within the collar's limits).
const ANY: CollarLimits = CollarLimits { outer: 1 << 20, holes: 1 << 12, hole_vertices: 1 << 20, total: 1 << 22, slots: 1, slot_bytes: 0 };

/// The server-side fence for a collar: the herd's active boundary as that
/// collar enforces it (a legacy collar's single ring, holes for the rest).
fn fence_for(b: Option<&Boundary>, caps: &CollarCaps, state: FenceState) -> Option<Geofence> {
    let b = b?;
    let cfg = GeofenceConfig { warn_m: b.warn_m, hysteresis_m: b.hysteresis_m, ..GeofenceConfig::default() };
    let mut f = Geofence::from_polygon(cfg, &shape::fence_geometry(b, caps), b.version, &ANY).ok()?;
    f.set_state(state);
    Some(f)
}

async fn report(State(ctx): State<Ctx>, device: Device, ApiJson(mut rep): ApiJson<PositionReport>) -> ApiResult<Json<ReportResponse>> {
    let mut collar = device.collar.clone();
    check_collar_id(&collar, rep.collar_id.as_deref())?;
    rep.validate()?;
    let received = now();
    let times = rep.fixes.iter().map(|f| f.at).chain(rep.cues.iter().map(|c| c.at)).chain(rep.episodes.iter().map(|e| e.end));
    if times.max().is_some_and(|t| t > received + MAX_CLOCK_AHEAD) {
        return Err(ApiError::bad_request("A fix or cue is timestamped in the future. Check the collar clock."));
    }
    // Stored times have millisecond precision.
    for f in &mut rep.fixes {
        f.at = f.at.trunc_subsecs(3);
    }
    for c in &mut rep.cues {
        c.at = c.at.trunc_subsecs(3);
    }
    for e in &mut rep.episodes {
        e.start = e.start.trunc_subsecs(3);
        e.end = e.end.trunc_subsecs(3);
    }
    rep.fixes.sort_by_key(|f| f.at);
    rep.cues.sort_by_key(|c| c.at);

    // What the collar is: as this report says, else as it said before.
    let caps = match &rep.device {
        Some(d) => CollarCaps { fw: d.fw.clone(), caps: d.caps.clone(), limits: d.limits_or_default() },
        None => device.caps(),
    };
    let herd_id = collar.herd_id.clone();
    // Every read happens before the write lock is taken, so a report holding
    // the lock never waits on the pool for another connection.
    let split = db::herd_boundaries(ctx.db(), &herd_id, received).await?;
    let held = escapes::collar_boundaries(ctx.db(), &collar, received).await?;
    let rejected = db::rejected_versions(ctx.db(), &collar.id).await?;
    let latest_version = held.active.iter().chain(&held.staged).map(|b| b.version).filter(|v| !rejected.contains(v)).max();
    let paddocks = ctx.store().list_paddocks().await?;
    let health = rep.health.clone().unwrap_or_default();
    let desired = if caps.has(caps::CONFIG) { Some(config::desired(&ctx, &collar.id, &herd_id, received).await?) } else { None };
    let config_version = rep.device.as_ref().and_then(|d| d.config_version);
    let config_reject = rep.device.as_ref().and_then(|d| d.config_reject.clone());

    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    // The collar as it is now: another report for it may have just landed.
    let row = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(&collar.id).fetch_optional(&mut *tx).await?;
    collar = match row {
        Some(r) => op_core::store::collar_from_row(&r)?,
        None => return Err(ApiError::unauthorized("Unknown collar key.")),
    };
    if let Some(d) = &rep.device {
        let caps_json = (!d.caps.is_empty()).then(|| serde_json::to_string(&d.caps)).transpose().map_err(anyhow::Error::from)?;
        let limits_json = d.limits.as_ref().map(serde_json::to_string).transpose().map_err(anyhow::Error::from)?;
        sqlx::query("UPDATE collars SET fw = ?, caps = ?, limits = ? WHERE id = ?")
            .bind(&d.fw)
            .bind(caps_json)
            .bind(limits_json)
            .bind(&collar.id)
            .execute(&mut *tx)
            .await?;
        collar.fw = d.fw.clone();
        collar.caps = d.caps.clone();
    }
    if let Some(list) = &rep.slots {
        slots::replace(&mut tx, &collar.id, list, received).await?;
    }
    let config = match &desired {
        Some(d) => config::sync(&mut tx, &ctx, d, config_version, config_reject.as_ref(), received).await?,
        None => None,
    };
    // A parked collar (charging, on the shelf, in repair) says nothing about
    // grazing: only who it is, what it holds, its battery, health and last
    // contact are kept.
    if collar.parked_at.is_some() {
        insert_health(&mut tx, &collar.id, &herd_id, received, &rep, &health).await?;
        collar.last_seen = Some(received);
        if rep.battery.is_some() {
            collar.battery = rep.battery;
        }
        sqlx::query("UPDATE collars SET last_seen = ?, battery = ? WHERE id = ?")
            .bind(collar.last_seen.as_ref().map(to_db))
            .bind(collar.battery)
            .bind(&collar.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        ctx.publish(Event::Collar { collar });
        return Ok(Json(ReportResponse { latest_version, config }));
    }
    // Fixes newer than the collar's last fix move its state and position;
    // late or backfilled ones are stored with their own fence state only.
    let mut fence = fence_for(split.active.as_ref(), &caps, collar.state);
    let mut late_fence = fence_for(split.active.as_ref(), &caps, collar.state);
    let last_at = collar.last_fix.as_ref().map(|f| f.at);

    let mut events = Vec::with_capacity(rep.cues.len() + 2);
    let mut state = if fence.is_some() { collar.state } else { FenceState::Unknown };
    // Since the first fix outside after the last one in (as escapes count it).
    let mut outside_since = if collar.state == FenceState::Outside { collar.outside_since } else { None };
    let mut latest_fix = collar.last_fix.clone();
    let mut latest_event = None;
    for wf in &rep.fixes {
        let newer = last_at.is_none_or(|t| wf.at > t);
        let f = if newer { fence.as_mut() } else { late_fence.as_mut() };
        let (fix_state, margin) = match f {
            Some(f) => {
                let r = f.update(wf.point, wf.accuracy_m);
                (r.state, Some(r.margin_m))
            }
            None => (FenceState::Unknown, None),
        };
        if newer {
            state = fix_state;
            if fix_state != FenceState::Outside {
                outside_since = None;
            } else if outside_since.is_none() {
                outside_since = Some(wf.at);
            }
        }
        let fix = Fix {
            at: wf.at,
            point: wf.point,
            accuracy_m: wf.accuracy_m,
            sats: wf.sats.or(health.sats).unwrap_or(0),
            cn0: wf.cn0.or(health.cn0),
            ttf_s: wf.ttf_s.or(health.ttf_s),
        };
        sqlx::query(
            "INSERT INTO fixes (collar_id, herd_id, animal_id, at, t, lon, lat, accuracy_m, sats, cn0, ttf_s, boundary_version, state, margin_m, paddock_id, hdop)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&collar.id)
        .bind(&herd_id)
        .bind(&collar.animal_id)
        .bind(to_db(&fix.at))
        .bind(unix_ms(&fix.at))
        .bind(fix.point[0])
        .bind(fix.point[1])
        .bind(fix.accuracy_m)
        .bind(fix.sats as i64)
        .bind(fix.cn0)
        .bind(fix.ttf_s)
        .bind(wf.boundary_version.or(rep.boundary_version).map(i64::from))
        .bind(fix_state.as_str())
        .bind(margin)
        .bind(db::paddock_for_point(&paddocks, fix.point).map(|p| p.id.as_str()))
        .bind(wf.hdop)
        .execute(&mut *tx)
        .await?;
        if newer && latest_fix.as_ref().is_none_or(|l| fix.at >= l.at) {
            latest_fix = Some(fix.clone());
            latest_event = Some((fix, fix_state));
        }
    }
    // One fix event per report (the newest), so a burst of reports can't
    // flood the live feed.
    if let Some((fix, fix_state)) = latest_event {
        events.push(Event::Fix { collar_id: collar.id.clone(), animal_id: collar.animal_id.clone(), herd_id: herd_id.clone(), fix, state: fix_state });
    }
    for cue in &rep.cues {
        sqlx::query(
            "INSERT INTO cues (collar_id, herd_id, animal_id, at, t, level, margin_m, lon, lat, boundary_version, kind, ring, dur_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&collar.id)
        .bind(&herd_id)
        .bind(&collar.animal_id)
        .bind(to_db(&cue.at))
        .bind(unix_ms(&cue.at))
        .bind(cue.level as i64)
        .bind(cue.margin_m)
        .bind(cue.point.map(|p| p[0]))
        .bind(cue.point.map(|p| p[1]))
        .bind(cue.boundary_version.or(rep.boundary_version).map(i64::from))
        .bind(cue.kind.map(|k| k.as_str()))
        .bind(cue.ring.map(i64::from))
        .bind(cue.dur_ms.map(i64::from))
        .execute(&mut *tx)
        .await?;
        // Firmware 0.1 sends no kind: outside when past the line, else warn.
        let kind = cue.kind.map_or(if cue.margin_m < 0.0 { "outside" } else { "warn" }, |k| k.as_str());
        events.push(Event::Cue { collar_id: collar.id.clone(), at: cue.at, level: cue.level, margin_m: cue.margin_m, kind: Some(kind.into()), ring: cue.ring });
    }
    for e in &rep.episodes {
        insert_episode(&mut tx, &collar, &herd_id, e, rep.boundary_version).await?;
    }
    insert_health(&mut tx, &collar.id, &herd_id, received, &rep, &health).await?;

    collar.last_seen = Some(received);
    if rep.battery.is_some() {
        collar.battery = rep.battery;
    }
    if rep.boundary_version.is_some() {
        collar.boundary_version = rep.boundary_version;
    }
    collar.state = state;
    collar.outside_since = if state == FenceState::Outside { outside_since } else { None };
    collar.last_fix = latest_fix;
    sqlx::query("UPDATE collars SET last_seen = ?, battery = ?, boundary_version = ?, state = ?, last_fix = ?, outside_since = ? WHERE id = ?")
        .bind(collar.last_seen.as_ref().map(to_db))
        .bind(collar.battery)
        .bind(collar.boundary_version.map(|v| v as i64))
        .bind(collar.state.as_str())
        .bind(collar.last_fix.as_ref().map(serde_json::to_string).transpose().map_err(anyhow::Error::from)?)
        .bind(collar.outside_since.as_ref().map(to_db))
        .bind(&collar.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    for e in events {
        ctx.publish(e);
    }
    ctx.publish(Event::Collar { collar });
    Ok(Json(ReportResponse { latest_version, config }))
}

/// One episode, stored once however often the collar resends it.
async fn insert_episode(
    tx: &mut sqlx::SqliteConnection,
    collar: &Collar,
    herd_id: &str,
    e: &op_protocol::WireEpisode,
    report_version: Option<u32>,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO episodes (id, collar_id, herd_id, animal_id, start_t, end_t, start_at, end_at, boundary_version, ring, cues, max_level, min_margin_m, outcome)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(collar_id, start_t) DO NOTHING",
    )
    .bind(id::new_id(EPISODE))
    .bind(&collar.id)
    .bind(herd_id)
    .bind(&collar.animal_id)
    .bind(unix_ms(&e.start))
    .bind(unix_ms(&e.end))
    .bind(to_db(&e.start))
    .bind(to_db(&e.end))
    .bind(e.boundary_version.or(report_version).map(i64::from))
    .bind(e.ring as i64)
    .bind(e.cues as i64)
    .bind(e.max_level as i64)
    .bind(e.min_margin_m)
    .bind(e.outcome.as_str())
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// One `health` row per report: battery and receiver, cell, motion and power
/// health over time.
async fn insert_health(
    tx: &mut sqlx::SqliteConnection,
    collar_id: &str,
    herd_id: &str,
    received: DateTime<Utc>,
    rep: &PositionReport,
    health: &op_protocol::Health,
) -> ApiResult<()> {
    let cell = health.cell.clone().unwrap_or_default();
    sqlx::query(
        "INSERT INTO health (collar_id, herd_id, at, t, battery, sats, cn0, ttf_s, fixes, cues, boundary_version,
             fix_attempts, fix_ok, rsrp_dbm, rsrq_db, snr_db, cell_mode, band, cell_id, tac, still_s, tilt_deg, temp_c, battery_v, charging, uptime_s, reset)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(collar_id)
    .bind(herd_id)
    .bind(to_db(&received))
    .bind(unix_ms(&received))
    .bind(rep.battery)
    .bind(health.sats.map(|s| s as i64))
    .bind(health.cn0)
    .bind(health.ttf_s)
    .bind(rep.fixes.len() as i64)
    .bind(rep.cues.len() as i64)
    .bind(rep.boundary_version.map(|v| v as i64))
    .bind(health.fix_attempts.map(i64::from))
    .bind(health.fix_ok.map(i64::from))
    .bind(cell.rsrp_dbm)
    .bind(cell.rsrq_db)
    .bind(cell.snr_db)
    .bind(cell.mode)
    .bind(cell.band.map(i64::from))
    .bind(cell.cell_id)
    .bind(cell.tac.map(i64::from))
    .bind(health.still_s.map(f64::from))
    .bind(health.tilt_deg)
    .bind(health.temp_c)
    .bind(health.battery_v)
    .bind(health.charging)
    .bind(health.uptime_s.map(|s| s.min(i64::MAX as u64) as i64))
    .bind(&health.reset)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
struct Want {
    /// Highest version held, applied or staged.
    have: Option<u32>,
    /// Slots left; absent (a legacy collar) counts as 1.
    free: Option<usize>,
    /// Slot bytes left; absent means no byte limit.
    free_bytes: Option<usize>,
}

/// 204 when the collar is current. Otherwise the boundary in effect if it is
/// newer than `have`, else the lowest staged one above `have` when the collar
/// has a free slot and the command fits its free bytes. Versions the collar
/// refused for good are left out. A collar out on an escape gets its own
/// boundary instead of the herd's.
async fn get_boundary(State(ctx): State<Ctx>, device: Device, Query(q): Query<Want>) -> ApiResult<Response> {
    let have = q.have.unwrap_or(0);
    let caps = device.caps();
    let split = escapes::collar_boundaries(ctx.db(), &device.collar, now()).await?;
    let rejected = db::rejected_versions(ctx.db(), &device.collar.id).await?;
    let wanted = |b: &&Boundary| b.version > have && !rejected.contains(&b.version);
    if let Some(a) = split.active.as_ref().filter(wanted) {
        return Ok(Json(shape::command_for(&ctx, a, &caps)?).into_response());
    }
    if q.free.unwrap_or(1) > 0
        && let Some(s) = split.staged.iter().find(wanted)
        && q.free_bytes.is_none_or(|room| shape::record_bytes(s, &caps).is_ok_and(|n| n <= room))
    {
        return Ok(Json(shape::command_for(&ctx, s, &caps)?).into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn ack(State(ctx): State<Ctx>, device: Device, ApiJson(a): ApiJson<Ack>) -> ApiResult<StatusCode> {
    let mut collar = device.collar;
    check_collar_id(&collar, a.collar_id.as_deref())?;
    if a.reason.as_ref().is_some_and(|r| r.len() > 1000) {
        return Err(ApiError::bad_request("reason is too long."));
    }
    let b = db::boundary_by_id(ctx.db(), &a.command_id)
        .await?
        .filter(|b| b.herd_id == collar.herd_id && b.collar_id.as_ref().is_none_or(|c| *c == collar.id))
        .ok_or_else(|| ApiError::not_found("Unknown command_id."))?;
    if b.version != a.version {
        return Err(ApiError::bad_request(format!("Command {} is version {}, not {}.", b.id, b.version, a.version)));
    }
    // A code only goes with a rejection.
    let code = a.code.filter(|_| a.status == AckStatus::Rejected);
    // Collars may repeat an ack; keep one row per change of status (or code).
    let last: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT status, code FROM acks WHERE collar_id = ? AND version = ? AND herd_id = ? ORDER BY id DESC LIMIT 1")
            .bind(&collar.id)
            .bind(a.version as i64)
            .bind(&collar.herd_id)
            .fetch_optional(ctx.db())
            .await?;
    if last.is_some_and(|(s, c)| s == a.status.as_str() && c.as_deref() == code.map(|c| c.as_str())) {
        return Ok(StatusCode::NO_CONTENT);
    }
    let received = now();
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    sqlx::query("INSERT INTO acks (collar_id, herd_id, command_id, version, status, reason, at, received_at, code) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(&collar.id)
        .bind(&collar.herd_id)
        .bind(&a.command_id)
        .bind(a.version as i64)
        .bind(a.status.as_str())
        .bind(&a.reason)
        .bind(to_db(&a.at))
        .bind(to_db(&received))
        .bind(code.map(|c| c.as_str()))
        .execute(&mut *tx)
        .await?;
    // The collar's latest boundary state: the highest version it acked, and
    // that version's latest status. A late ack for a lower version changes nothing.
    sqlx::query(
        "INSERT INTO collar_boundary_state (collar_id, herd_id, version, status, code, command_id, at) VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(collar_id) DO UPDATE SET herd_id = excluded.herd_id, version = excluded.version, status = excluded.status,
             code = excluded.code, command_id = excluded.command_id, at = excluded.at
         WHERE excluded.version >= collar_boundary_state.version",
    )
    .bind(&collar.id)
    .bind(&collar.herd_id)
    .bind(a.version as i64)
    .bind(a.status.as_str())
    .bind(code.map(|c| c.as_str()))
    .bind(&a.command_id)
    .bind(to_db(&a.at))
    .execute(&mut *tx)
    .await?;
    slots::record_ack(&mut tx, &collar.id, a.version, a.status, code, b.effective_at, received).await?;
    if a.status == AckStatus::Applied && collar.boundary_version.is_none_or(|v| a.version > v) {
        collar.boundary_version = Some(a.version);
        sqlx::query("UPDATE collars SET boundary_version = ? WHERE id = ?").bind(a.version as i64).bind(&collar.id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    tracing::info!(collar = %collar.id, version = a.version, status = a.status.as_str(), code = code.map(|c| c.as_str()), "boundary ack");
    ctx.publish(Event::Ack { collar_id: collar.id.clone(), herd_id: collar.herd_id.clone(), version: a.version, status: a.status, reason: a.reason });
    ctx.publish(Event::Collar { collar });
    Ok(StatusCode::NO_CONTENT)
}
