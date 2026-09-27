//! Device endpoints under `/collar/v1`, per `opencollar/protocol/README.md`.
//! Every request carries `Authorization: Bearer <collar key>`.

use axum::extract::{FromRequestParts, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Duration, SubsecRound};
use op_core::time::{now, to_db, unix_ms};
use op_core::{ApiError, ApiJson, ApiResult, Collar, Ctx, Event, FenceState, Fix, keys};
use op_geo::{Geofence, GeofenceConfig};
use op_protocol::{Ack, AckStatus, PositionReport, ReportResponse};
use serde::Deserialize;

use crate::{boundary, db, escapes};

pub fn router() -> Router<Ctx> {
    Router::new().route("/collar/v1/report", post(report)).route("/collar/v1/boundary", get(get_boundary)).route("/collar/v1/ack", post(ack))
}

/// The collar a request's key belongs to.
pub struct Device(pub Collar);

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
        let collar = db::collar_by_key_hash(ctx.db(), &keys::hash_key(key)).await?.ok_or_else(|| ApiError::unauthorized("Unknown collar key."))?;
        Ok(Device(collar))
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

async fn report(State(ctx): State<Ctx>, Device(mut collar): Device, ApiJson(mut rep): ApiJson<PositionReport>) -> ApiResult<Json<ReportResponse>> {
    check_collar_id(&collar, rep.collar_id.as_deref())?;
    rep.validate()?;
    let received = now();
    if rep.fixes.iter().map(|f| f.at).chain(rep.cues.iter().map(|c| c.at)).any(|t| t > received + MAX_CLOCK_AHEAD) {
        return Err(ApiError::bad_request("A fix or cue is timestamped in the future. Check the collar clock."));
    }
    // Stored times have millisecond precision.
    for f in &mut rep.fixes {
        f.at = f.at.trunc_subsecs(3);
    }
    for c in &mut rep.cues {
        c.at = c.at.trunc_subsecs(3);
    }
    rep.fixes.sort_by_key(|f| f.at);
    rep.cues.sort_by_key(|c| c.at);

    let herd_id = collar.herd_id.clone();
    // Every read happens before the write lock is taken, so a report holding
    // the lock never waits on the pool for another connection.
    let split = db::herd_boundaries(ctx.db(), &herd_id, received).await?;
    let held = escapes::collar_boundaries(ctx.db(), &collar, received).await?;
    let paddocks = ctx.store().list_paddocks().await?;
    let health = rep.health.clone().unwrap_or_default();

    let latest_version = held.staged.last().or(held.active.as_ref()).map(|b| b.version);

    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    // The collar as it is now: another report for it may have just landed.
    let row = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(&collar.id).fetch_optional(&mut *tx).await?;
    collar = match row {
        Some(r) => op_core::store::collar_from_row(&r)?,
        None => return Err(ApiError::unauthorized("Unknown collar key.")),
    };
    // A parked collar (charging, on the shelf, in repair) says nothing about
    // grazing: only its battery, health and last contact are kept.
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
        return Ok(Json(ReportResponse { latest_version, ..Default::default() }));
    }
    let fence_for = |state: FenceState| {
        let b = split.active.as_ref()?;
        let cfg = GeofenceConfig { warn_m: b.warn_m, hysteresis_m: b.hysteresis_m, ..GeofenceConfig::default() };
        let mut f = Geofence::new(cfg, &b.geometry.outer_ring(), b.version).ok()?;
        f.set_state(state);
        Some(f)
    };
    // Fixes newer than the collar's last fix move its state and position;
    // late or backfilled ones are stored with their own fence state only.
    let mut fence = fence_for(collar.state);
    let mut late_fence = fence_for(collar.state);
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
            "INSERT INTO fixes (collar_id, herd_id, animal_id, at, t, lon, lat, accuracy_m, sats, cn0, ttf_s, boundary_version, state, margin_m, paddock_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(rep.boundary_version.map(|v| v as i64))
        .bind(fix_state.as_str())
        .bind(margin)
        .bind(db::paddock_for_point(&paddocks, fix.point).map(|p| p.id.as_str()))
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
            "INSERT INTO cues (collar_id, herd_id, animal_id, at, t, level, margin_m, lon, lat, boundary_version) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(rep.boundary_version.map(|v| v as i64))
        .execute(&mut *tx)
        .await?;
        events.push(Event::Cue { collar_id: collar.id.clone(), at: cue.at, level: cue.level, margin_m: cue.margin_m, kind: None, ring: None });
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
    Ok(Json(ReportResponse { latest_version, ..Default::default() }))
}

/// One `health` row per report: battery and receiver health over time.
async fn insert_health(
    tx: &mut sqlx::SqliteConnection,
    collar_id: &str,
    herd_id: &str,
    received: chrono::DateTime<chrono::Utc>,
    rep: &PositionReport,
    health: &op_protocol::Health,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO health (collar_id, herd_id, at, t, battery, sats, cn0, ttf_s, fixes, cues, boundary_version) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .execute(&mut *tx)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
struct Have {
    have: Option<u32>,
}

/// 204 when the collar is current. Otherwise the boundary in effect if it is
/// newer than `have`, else the next staged one. A collar out on an escape
/// gets its own boundary instead of the herd's.
async fn get_boundary(State(ctx): State<Ctx>, Device(collar): Device, Query(q): Query<Have>) -> ApiResult<Response> {
    let have = q.have.unwrap_or(0);
    let split = escapes::collar_boundaries(ctx.db(), &collar, now()).await?;
    let next = split.active.filter(|a| a.version > have).or_else(|| split.staged.into_iter().find(|b| b.version > have));
    match next {
        Some(b) => Ok(Json(boundary::command_for(&ctx, &b)?).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

async fn ack(State(ctx): State<Ctx>, Device(mut collar): Device, ApiJson(a): ApiJson<Ack>) -> ApiResult<StatusCode> {
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
    // Collars may repeat an ack; keep one row per status change.
    let last: Option<(String,)> = sqlx::query_as("SELECT status FROM acks WHERE collar_id = ? AND version = ? AND herd_id = ? ORDER BY id DESC LIMIT 1")
        .bind(&collar.id)
        .bind(a.version as i64)
        .bind(&collar.herd_id)
        .fetch_optional(ctx.db())
        .await?;
    if last.is_some_and(|(s,)| s == a.status.as_str()) {
        return Ok(StatusCode::NO_CONTENT);
    }
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    sqlx::query("INSERT INTO acks (collar_id, herd_id, command_id, version, status, reason, at, received_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(&collar.id)
        .bind(&collar.herd_id)
        .bind(&a.command_id)
        .bind(a.version as i64)
        .bind(a.status.as_str())
        .bind(&a.reason)
        .bind(to_db(&a.at))
        .bind(to_db(&now()))
        .execute(&mut *tx)
        .await?;
    // The collar's latest boundary state: the highest version it acked, and
    // that version's latest status. A late ack for a lower version changes nothing.
    sqlx::query(
        "INSERT INTO collar_boundary_state (collar_id, herd_id, version, status, code, command_id, at) VALUES (?, ?, ?, ?, NULL, ?, ?)
         ON CONFLICT(collar_id) DO UPDATE SET herd_id = excluded.herd_id, version = excluded.version, status = excluded.status,
             code = excluded.code, command_id = excluded.command_id, at = excluded.at
         WHERE excluded.version >= collar_boundary_state.version",
    )
    .bind(&collar.id)
    .bind(&collar.herd_id)
    .bind(a.version as i64)
    .bind(a.status.as_str())
    .bind(&a.command_id)
    .bind(to_db(&a.at))
    .execute(&mut *tx)
    .await?;
    if a.status == AckStatus::Applied && collar.boundary_version.is_none_or(|v| a.version > v) {
        collar.boundary_version = Some(a.version);
        sqlx::query("UPDATE collars SET boundary_version = ? WHERE id = ?").bind(a.version as i64).bind(&collar.id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    tracing::info!(collar = %collar.id, version = a.version, status = a.status.as_str(), "boundary ack");
    ctx.publish(Event::Ack { collar_id: collar.id.clone(), herd_id: collar.herd_id.clone(), version: a.version, status: a.status, reason: a.reason });
    ctx.publish(Event::Collar { collar });
    Ok(StatusCode::NO_CONTENT)
}
