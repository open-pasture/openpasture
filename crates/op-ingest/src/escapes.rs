//! Escapes: an animal that stays outside its herd's boundary gets a boundary
//! of its own (API.md "Escapes"). It is the herd's boundary joined to a pen
//! around the animal, built by the sweep planner with the herd's boundary as
//! the target, and it closes in behind the animal until it is back. The rest
//! of the herd keeps the herd's boundary, so the paddock never opens for
//! anyone to follow it out.
//!
//! The collar pulls its own boundary like any other: while an escape is
//! open, `/collar/v1/boundary` serves it instead of the herd's. When the
//! animal is back (or the farmer lets it go) the collar gets a copy of the
//! herd's boundary with a new version, since it never goes back to an older
//! one.
//!
//! Pens keep the herd boundary's holes. A collar out on an escape reports
//! and polls fast (its config gets a fast window). When the escape ends, the
//! herd boundaries staged at that moment are copied for that collar alone
//! (`collar_id` + `copy_of`), so the rest of the herd downloads nothing.
//!
//! [`advance`] is pure. [`scan`] starts, steps and ends escapes; the driver
//! runs it every 2 s.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use op_core::time::{from_db, now, opt_from_db, to_db};
use op_core::{ActivityEvent, ApiError, ApiResult, Boundary, Collar, Ctx, DbEnum, Escape, EscapeStatus, Event, FenceState, LonLat, Polygon, id};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::boundary::{NewBoundary, insert_boundary};
use crate::moves::{FRESH_FIX, STEP_EVERY, stride};
use crate::planner::{self, Frame, Plan, PlanInput};
use crate::{config, db};
use op_geo::CollarLimits;

/// Outside the herd's boundary this long, with the collar's own cue spent
/// (its outside tone stops after 10 s), and the animal gets a boundary of
/// its own.
pub const ESCAPE_AFTER: Duration = Duration::seconds(60);
/// `BoundaryStatus.escapes` keeps showing an ended escape this long.
pub const SHOW_ENDED: Duration = Duration::minutes(10);

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/collars/{id}/escape/stop", post(post_stop))
}

/// The driver's own state, kept on the escape row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pen {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Frame>,
    /// Back line of the pen in effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_step_at: Option<DateTime<Utc>>,
    /// The herd boundary the pen was built against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// Send the collar this boundary.
    Send {
        polygon: Polygon,
        remaining_m: f64,
    },
    /// The animal is back inside the herd's boundary.
    Back,
    Wait,
}

/// The escape as far as [`advance`] needs it.
pub struct EscapeState<'a> {
    /// The herd's active boundary and its version.
    pub target: &'a Polygon,
    pub target_version: u32,
    pub warn_m: f64,
    /// The collar's own boundary now; `None` before the first.
    pub current: Option<&'a Polygon>,
    pub pen: &'a Pen,
    /// What the herd's collars hold.
    pub limits: CollarLimits,
}

/// Decide the escape's next action from the animal's position. The first
/// call sends a pen at once. Later pens wait for the animal to move a stride
/// toward the herd and for 30 s since the last, except when the herd's
/// boundary changed (the pen is rebuilt against it) or the animal got out of
/// its pen too (a new pen is built where it is).
pub fn advance(e: &EscapeState, at: LonLat, now: DateTime<Utc>) -> (Next, Pen) {
    let mut pen = e.pen.clone();
    let retarget = pen.target_version != Some(e.target_version);
    if retarget {
        // Progress is measured from the target, so it starts over with it.
        pen.frame = None;
        pen.level = None;
    }
    let plan_from = |previous: Option<&Polygon>, frame: Option<Frame>| {
        planner::plan(&PlanInput { target: e.target, previous, paddock: None, animals: &[at], warn_m: e.warn_m, frame, limits: e.limits })
    };
    let mut fresh = e.current.is_none();
    let mut plan = plan_from(e.current, pen.frame);
    if !fresh && matches!(&plan, Plan::Step(s) if !s.left_out.is_empty()) {
        pen.frame = None;
        pen.level = None;
        plan = plan_from(None, None);
        fresh = true;
    }
    match plan {
        Plan::Target => (Next::Back, pen),
        Plan::Hold(why) => {
            tracing::debug!("escape holds: {why}");
            (Next::Wait, pen)
        }
        Plan::Step(s) => {
            if !s.left_out.is_empty() {
                return (Next::Wait, pen);
            }
            let jump = fresh || retarget;
            let due = jump || pen.last_step_at.is_none_or(|t| now - t >= STEP_EVERY);
            let ahead = jump || pen.level.is_none_or(|l| s.level >= l + stride(e.warn_m));
            if !(due && ahead) {
                return (Next::Wait, pen);
            }
            pen.frame = Some(s.frame);
            pen.level = Some(s.level);
            pen.last_step_at = Some(now);
            pen.target_version = Some(e.target_version);
            (Next::Send { polygon: s.polygon, remaining_m: s.remaining_m }, pen)
        }
    }
}

// Storage

struct EscapeRow {
    e: Escape,
    pen: Pen,
}

const SELECT: &str = "SELECT e.*, b.geometry AS b_geometry, b.version AS b_version FROM escapes e LEFT JOIN boundaries b ON b.id = e.boundary_id";

fn escape_from_row(r: &SqliteRow) -> anyhow::Result<EscapeRow> {
    let geometry: Option<String> = r.try_get("b_geometry")?;
    let e = Escape {
        id: r.try_get("id")?,
        herd_id: r.try_get("herd_id")?,
        collar_id: r.try_get("collar_id")?,
        status: EscapeStatus::from_db(&r.try_get::<String, _>("status")?)?,
        geometry: geometry.map(|g| serde_json::from_str(&g)).transpose()?,
        version: r.try_get::<Option<i64>, _>("b_version")?.map(|v| v as u32),
        step: r.try_get::<i64, _>("step")? as u32,
        remaining_m: r.try_get("remaining_m")?,
        started_at: from_db(&r.try_get::<String, _>("started_at")?)?,
        updated_at: from_db(&r.try_get::<String, _>("updated_at")?)?,
        ended_at: opt_from_db(r.try_get("ended_at")?)?,
    };
    let pen = serde_json::from_str(&r.try_get::<String, _>("pen")?).unwrap_or_default();
    Ok(EscapeRow { e, pen })
}

/// The herd's open escapes, and those that ended in the last 10 minutes.
pub async fn current_escapes(db: &sqlx::SqlitePool, herd_id: &str, at: DateTime<Utc>) -> anyhow::Result<Vec<Escape>> {
    let rows = sqlx::query(&format!("{SELECT} WHERE e.herd_id = ? AND (e.status = 'returning' OR e.ended_at >= ?) ORDER BY e.started_at, e.id"))
        .bind(herd_id)
        .bind(to_db(&(at - SHOW_ENDED)))
        .fetch_all(db)
        .await?;
    rows.iter().map(|r| escape_from_row(r).map(|r| r.e)).collect()
}

async fn open_row(db: &sqlx::SqlitePool, collar_id: &str) -> anyhow::Result<Option<EscapeRow>> {
    let row = sqlx::query(&format!("{SELECT} WHERE e.collar_id = ? AND e.status = 'returning'")).bind(collar_id).fetch_optional(db).await?;
    row.map(|r| escape_from_row(&r)).transpose()
}

/// The boundaries a collar should hold, split like the herd's: out on an
/// escape, only its pen; otherwise its herd's, together with the copies it
/// was handed back after its last escape (each copy has a higher version
/// than the herd boundary it copies and the same activation, so it stands
/// in for it). Two index lookups for the herd, one for the copies.
pub async fn collar_boundaries(db: &sqlx::SqlitePool, collar: &Collar, at: DateTime<Utc>) -> anyhow::Result<db::HerdBoundaries> {
    let mut conn = db.acquire().await?;
    let pen = sqlx::query(db::sql::OPEN_PEN).bind(&collar.id).bind(&collar.herd_id).fetch_optional(&mut *conn).await?;
    if let Some(r) = pen {
        return Ok(db::HerdBoundaries { active: Some(op_core::store::boundary_from_row(&r)?), staged: vec![] });
    }
    let herd = db::herd_boundaries_in(&mut conn, &collar.herd_id, at).await?;
    let floor = herd.active.as_ref().map_or(0, |a| a.version);
    let copies = sqlx::query(db::sql::OWN_COPIES).bind(&collar.herd_id).bind(&collar.id).bind(floor as i64).fetch_all(&mut *conn).await?;
    if copies.is_empty() {
        return Ok(herd);
    }
    let mut all = copies.iter().map(op_core::store::boundary_from_row).collect::<anyhow::Result<Vec<_>>>()?;
    all.extend(herd.active);
    all.extend(herd.staged);
    Ok(db::split_boundaries(all, at))
}

/// Whether the collar is out on an escape.
pub(crate) async fn on_escape(conn: &mut sqlx::SqliteConnection, collar_id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM escapes WHERE collar_id = ? AND status = 'returning')").bind(collar_id).fetch_one(conn).await?)
}

/// Collars that are out on an escape, for the herd's sweep to leave alone.
pub async fn escaped_collars(db: &sqlx::SqlitePool, herd_id: &str) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query("SELECT collar_id FROM escapes WHERE herd_id = ? AND status = 'returning'").bind(herd_id).fetch_all(db).await?;
    Ok(rows.iter().map(|r| r.get::<String, _>(0)).collect())
}

/// Escapes are driven one collar at a time.
static LOCKS: LazyLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);

async fn collar_lock(collar_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let m = LOCKS.lock().unwrap_or_else(|e| e.into_inner()).entry(collar_id.to_owned()).or_default().clone();
    m.lock_owned().await
}

fn fresh_point(c: &Collar, at: DateTime<Utc>) -> Option<LonLat> {
    c.last_fix.as_ref().filter(|f| at - f.at <= FRESH_FIX).map(|f| f.point)
}

/// One pass at time `at`: step or end every open escape, then start one for
/// every collar that has been outside its herd's boundary long enough.
pub async fn scan(ctx: &Ctx, at: DateTime<Utc>) -> anyhow::Result<()> {
    let open = sqlx::query("SELECT collar_id FROM escapes WHERE status = 'returning'").fetch_all(ctx.db()).await?;
    for r in &open {
        let collar_id: String = r.get(0);
        if let Err(e) = drive(ctx, &collar_id, at).await {
            tracing::warn!(collar = %collar_id, "escape: {e:#}");
        }
    }
    let outside = sqlx::query("SELECT id FROM collars WHERE state = ? AND id NOT IN (SELECT collar_id FROM escapes WHERE status = 'returning')")
        .bind(FenceState::Outside.as_str())
        .fetch_all(ctx.db())
        .await?;
    for r in &outside {
        let collar_id: String = r.get(0);
        if let Err(e) = maybe_start(ctx, &collar_id, at).await {
            tracing::warn!(collar = %collar_id, "escape: {e:#}");
        }
    }
    Ok(())
}

async fn maybe_start(ctx: &Ctx, collar_id: &str, at: DateTime<Utc>) -> anyhow::Result<Option<Escape>> {
    let _guard = collar_lock(collar_id).await;
    let Some(collar) = db::get_collar(ctx.db(), collar_id).await? else { return Ok(None) };
    let Some(point) = fresh_point(&collar, at).filter(|_| collar.state == FenceState::Outside) else { return Ok(None) };
    if open_row(ctx.db(), collar_id).await?.is_some() {
        return Ok(None);
    }
    // Kept current by every report (its first fix outside after the last one in).
    let Some(since) = collar.outside_since else { return Ok(None) };
    if at - since < ESCAPE_AFTER {
        return Ok(None);
    }
    // Let go by the farmer on this same trip out: leave it until it is back in.
    let (let_go,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM escapes WHERE collar_id = ? AND status = 'stopped' AND ended_at >= ?")
        .bind(collar_id)
        .bind(to_db(&since))
        .fetch_one(ctx.db())
        .await?;
    if let_go > 0 {
        return Ok(None);
    }
    let Some(active) = db::herd_boundaries(ctx.db(), &collar.herd_id, at).await?.active else { return Ok(None) };
    let limits = crate::shape::herd_limits(ctx, &collar.herd_id).await?;
    let state = EscapeState { target: &active.geometry, target_version: active.version, warn_m: active.warn_m, current: None, pen: &Pen::default(), limits };
    let (Next::Send { polygon, remaining_m }, pen) = advance(&state, point, at) else { return Ok(None) };

    let mut e = Escape {
        id: id::new_id(id::ESCAPE),
        herd_id: collar.herd_id.clone(),
        collar_id: collar.id.clone(),
        status: EscapeStatus::Returning,
        geometry: Some(polygon.clone()),
        version: None,
        step: 1,
        remaining_m: round_m(remaining_m),
        started_at: at,
        updated_at: at,
        ended_at: None,
    };
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let b = insert_own(&mut tx, &collar, &active, &polygon, None, None, at).await?;
    e.version = Some(b.version);
    sqlx::query(
        "INSERT INTO escapes (id, herd_id, collar_id, status, boundary_id, step, remaining_m, pen, started_at, updated_at)
         VALUES (?, ?, ?, 'returning', ?, ?, ?, ?, ?, ?)",
    )
    .bind(&e.id)
    .bind(&e.herd_id)
    .bind(&e.collar_id)
    .bind(&b.id)
    .bind(e.step as i64)
    .bind(e.remaining_m)
    .bind(serde_json::to_string(&pen)?)
    .bind(to_db(&at))
    .bind(to_db(&at))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(herd = %e.herd_id, collar = %e.collar_id, version = b.version, outside_s = (at - since).num_seconds(), "escape: own boundary sent");
    log_activity(ctx, &e, "escape.started", format!("{} got out; sent its own boundary", collar.name)).await;
    ctx.publish(Event::Escape { escape: e.clone() });
    // It reports and polls fast while it is walked back.
    config::refresh_quietly(ctx, config::Scope::Collar(collar_id)).await;
    Ok(Some(e))
}

/// A boundary for one collar under the decision its herd is on: a pen, or
/// (`copy_of`) a copy of a herd boundary.
async fn insert_own(
    tx: &mut sqlx::SqliteConnection,
    collar: &Collar,
    herd: &Boundary,
    geometry: &Polygon,
    copy_of: Option<u32>,
    effective_at: Option<DateTime<Utc>>,
    at: DateTime<Utc>,
) -> anyhow::Result<Boundary> {
    let nb = NewBoundary {
        herd_id: &collar.herd_id,
        geometry,
        warn_m: herd.warn_m,
        hysteresis_m: herd.hysteresis_m,
        effective_at,
        decision_id: &herd.decision_id,
        created_at: at,
        collar_id: Some(&collar.id),
        copy_of,
    };
    insert_boundary(tx, &nb).await.map_err(|e| anyhow::anyhow!(e.message))
}

/// One pass for a collar's open escape.
async fn drive(ctx: &Ctx, collar_id: &str, at: DateTime<Utc>) -> anyhow::Result<()> {
    let _guard = collar_lock(collar_id).await;
    let Some(row) = open_row(ctx.db(), collar_id).await? else { return Ok(()) };
    let collar = db::get_collar(ctx.db(), collar_id).await?;
    let Some(collar) = collar.filter(|c| c.herd_id == row.e.herd_id) else {
        // Deleted, or moved to another herd (which gets that herd's boundary).
        return end(ctx, row, None, EscapeStatus::Stopped, at).await;
    };
    let Some(active) = db::herd_boundaries(ctx.db(), &collar.herd_id, at).await?.active else { return Ok(()) };
    let Some(point) = fresh_point(&collar, at) else { return Ok(()) };
    let limits = crate::shape::herd_limits(ctx, &collar.herd_id).await?;
    let state = EscapeState {
        target: &active.geometry,
        target_version: active.version,
        warn_m: active.warn_m,
        current: row.e.geometry.as_ref(),
        pen: &row.pen,
        limits,
    };
    match advance(&state, point, at) {
        (Next::Wait, pen) => {
            if pen != row.pen {
                sqlx::query("UPDATE escapes SET pen = ? WHERE id = ? AND status = 'returning'")
                    .bind(serde_json::to_string(&pen)?)
                    .bind(&row.e.id)
                    .execute(ctx.db())
                    .await?;
            }
            Ok(())
        }
        (Next::Back, _) => end(ctx, row, Some(&collar), EscapeStatus::Back, at).await,
        (Next::Send { polygon, remaining_m }, pen) => {
            let mut e = row.e.clone();
            let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
            let b = insert_own(&mut tx, &collar, &active, &polygon, None, None, at).await?;
            e.step += 1;
            e.remaining_m = round_m(remaining_m);
            e.geometry = Some(polygon);
            e.version = Some(b.version);
            e.updated_at = at;
            let n = sqlx::query(
                "UPDATE escapes SET boundary_id = ?, step = ?, remaining_m = ?, pen = ?, updated_at = ? WHERE id = ? AND status = 'returning' AND step = ?",
            )
            .bind(&b.id)
            .bind(e.step as i64)
            .bind(e.remaining_m)
            .bind(serde_json::to_string(&pen)?)
            .bind(to_db(&at))
            .bind(&e.id)
            .bind(row.e.step as i64)
            .execute(&mut *tx)
            .await?;
            if n.rows_affected() != 1 {
                tx.rollback().await?;
                return Ok(());
            }
            tx.commit().await?;
            tracing::info!(herd = %e.herd_id, collar = %e.collar_id, step = e.step, version = b.version, remaining_m = e.remaining_m, "escape step");
            ctx.publish(Event::Escape { escape: e });
            Ok(())
        }
    }
}

/// Copies of the herd's boundary in effect and of each one it has staged,
/// for one collar alone (`collar_id` + `copy_of`: same shape, margins and
/// `effective_at`, new versions above anything the collar can hold). A
/// collar only ever asks for versions above the highest it holds, so this is
/// how it gets its herd's boundaries back when what it holds is at or above
/// them: at the end of an escape (the pen dropped its staged slots), when it
/// joins a herd holding another herd's versions, and when what it holds has
/// fallen out of step ([`out_of_step`]). Read under the caller's write
/// transaction, so the copies are of the herd's newest boundaries. The copy
/// of the one in effect comes first.
pub(crate) async fn hand_copies(tx: &mut sqlx::SqliteConnection, collar: &Collar, at: DateTime<Utc>) -> anyhow::Result<Vec<Boundary>> {
    let split = db::herd_boundaries_in(tx, &collar.herd_id, at).await?;
    let mut out = Vec::with_capacity(1 + split.staged.len());
    if let Some(active) = &split.active {
        out.push(insert_own(tx, collar, active, &active.geometry, Some(active.version), None, at).await?);
    }
    for b in &split.staged {
        out.push(insert_own(tx, collar, b, &b.geometry, Some(b.version), b.effective_at, at).await?);
    }
    Ok(out)
}

/// Whether a collar holding `held` (versions, applied or staged) can't get
/// some boundary of `set` (what it should hold, [`collar_boundaries`]) the
/// usual way: one it doesn't hold sits at or below the highest it holds, and
/// it only asks for versions above that. Versions it refused for good
/// (`rejected`) don't count: a copy would be refused too.
pub(crate) fn out_of_step(set: &db::HerdBoundaries, held: &[u32], rejected: &[u32]) -> bool {
    let Some(&top) = held.iter().max() else { return false };
    set.active.iter().chain(&set.staged).any(|b| b.version <= top && !held.contains(&b.version) && !rejected.contains(&b.version))
}

/// End an escape. A collar still in the herd gets copies of the herd's
/// boundaries ([`hand_copies`]).
async fn end(ctx: &Ctx, row: EscapeRow, collar: Option<&Collar>, status: EscapeStatus, at: DateTime<Utc>) -> anyhow::Result<()> {
    let mut e = row.e;
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let mut copy = None;
    if let Some(c) = collar {
        let copies = hand_copies(&mut tx, c, at).await?;
        copy = copies.into_iter().next().filter(|b| b.effective_at.is_none());
    }
    e.status = status;
    e.updated_at = at;
    e.ended_at = Some(at);
    if let Some(b) = &copy {
        e.geometry = Some(b.geometry.clone());
        e.version = Some(b.version);
    }
    let n = sqlx::query(
        "UPDATE escapes SET status = ?, boundary_id = COALESCE(?, boundary_id), updated_at = ?, ended_at = ? WHERE id = ? AND status = 'returning'",
    )
    .bind(status.as_db())
    .bind(copy.as_ref().map(|b| b.id.as_str()))
    .bind(to_db(&at))
    .bind(to_db(&at))
    .bind(&e.id)
    .execute(&mut *tx)
    .await?;
    if n.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(());
    }
    tx.commit().await?;
    let name = collar.map_or(e.collar_id.as_str(), |c| c.name.as_str()).to_owned();
    let (kind, title) = match status {
        EscapeStatus::Back => ("escape.back", format!("{name} is back with the herd")),
        _ => ("escape.stopped", format!("{name} let go")),
    };
    tracing::info!(herd = %e.herd_id, collar = %e.collar_id, status = %status.as_db(), version = e.version, "escape ended");
    log_activity(ctx, &e, kind, title).await;
    ctx.publish(Event::Escape { escape: e });
    Ok(())
}

/// Stop a collar's escape: it gets its herd's boundary back, which leaves it
/// outside and so uncued. It isn't given another until it has been back in.
pub async fn stop_escape(ctx: &Ctx, collar_id: &str) -> ApiResult<Escape> {
    let at = now();
    {
        let _guard = collar_lock(collar_id).await;
        let Some(row) = open_row(ctx.db(), collar_id).await? else {
            return Err(ApiError::conflict("This collar isn't out on its own boundary."));
        };
        let collar = db::get_collar(ctx.db(), collar_id).await?.filter(|c| c.herd_id == row.e.herd_id);
        end(ctx, row, collar.as_ref(), EscapeStatus::Stopped, at).await?;
    }
    let row = sqlx::query(&format!("{SELECT} WHERE e.collar_id = ? ORDER BY e.started_at DESC, e.id DESC LIMIT 1")).bind(collar_id).fetch_one(ctx.db()).await?;
    Ok(escape_from_row(&row)?.e)
}

async fn post_stop(State(ctx): State<Ctx>, Path(collar_id): Path<String>) -> ApiResult<Json<Escape>> {
    if db::get_collar(ctx.db(), &collar_id).await?.is_none() {
        return Err(ApiError::not_found("No such collar."));
    }
    Ok(Json(stop_escape(&ctx, &collar_id).await?))
}

async fn log_activity(ctx: &Ctx, e: &Escape, kind: &str, title: String) {
    let at = e.updated_at;
    let _ = ctx
        .store()
        .record_event(&ActivityEvent {
            id: id::new_id(id::EVENT),
            kind: kind.into(),
            source: "system".into(),
            occurred_at: at,
            recorded_at: at,
            title,
            body: None,
            payload: serde_json::json!({ "escape_id": e.id, "collar_id": e.collar_id, "version": e.version }),
            targets: vec![("herd".into(), e.herd_id.clone()), ("collar".into(), e.collar_id.clone())],
        })
        .await
        .inspect_err(|err| tracing::warn!("activity log: {err:#}"));
}

fn round_m(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// The driver: every 2 s.
pub fn spawn_driver(ctx: Ctx) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tick.tick() => {
                    if let Err(e) = scan(&ctx, now()).await {
                        tracing::warn!("escape driver: {e:#}");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use op_geo::Projection;

    const ORIGIN: LonLat = [-79.25, 38.1];
    const W: f64 = 5.0;

    fn at(x: f64, y: f64) -> LonLat {
        Projection::new(ORIGIN).inverse([x, y])
    }
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon {
        Polygon::from_ring(vec![at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1)])
    }
    fn t0() -> DateTime<Utc> {
        from_db("2026-09-26T12:00:00.000Z").unwrap()
    }
    fn margin(p: &Polygon, pt: LonLat) -> f64 {
        let proj = Projection::new(ORIGIN);
        planner::signed_distance(proj.forward(pt), &proj.forward_ring(&p.outer_ring()))
    }

    struct Run {
        paddock: Polygon,
        version: u32,
        current: Option<Polygon>,
        pen: Pen,
    }

    impl Run {
        fn new() -> Self {
            Self { paddock: rect(0.0, 0.0, 100.0, 100.0), version: 1, current: None, pen: Pen::default() }
        }
        fn tick(&mut self, pt: LonLat, now: DateTime<Utc>) -> Next {
            let st = EscapeState {
                target: &self.paddock,
                target_version: self.version,
                warn_m: W,
                current: self.current.as_ref(),
                pen: &self.pen,
                limits: CollarLimits::V0,
            };
            let (next, pen) = advance(&st, pt, now);
            self.pen = pen;
            if let Next::Send { polygon, .. } = &next {
                self.current = Some(polygon.clone());
            }
            next
        }
    }

    #[test]
    fn the_first_pen_holds_the_animal_and_the_paddock() {
        let mut r = Run::new();
        let cow = at(50.0, 130.0);
        let Next::Send { polygon, remaining_m } = r.tick(cow, t0()) else { panic!("a pen") };
        // The animal is inside, in the warning band at the back.
        let m = margin(&polygon, cow);
        assert!(m > 0.0 && m < W, "margin {m}");
        assert!(remaining_m > 20.0);
        // The whole paddock is in it, so the herd's side of the line is unchanged.
        for c in [at(0.5, 0.5), at(99.5, 0.5), at(99.5, 99.5), at(0.5, 99.5)] {
            assert!(polygon.contains(c));
        }
        // Nothing much beyond the animal: it can't wander further off.
        assert!(!polygon.contains(at(50.0, 140.0)));
        assert!(!polygon.contains(at(0.0, 130.0)));
    }

    #[test]
    fn the_pen_closes_in_as_the_animal_comes_back() {
        let mut r = Run::new();
        assert!(matches!(r.tick(at(50.0, 130.0), t0()), Next::Send { .. }));
        let first = r.current.clone().unwrap();
        // Not moved: nothing new, however long.
        assert_eq!(r.tick(at(50.0, 130.0), t0() + Duration::seconds(90)), Next::Wait);
        // Moved 10 m toward the paddock, but too soon after the last pen.
        assert_eq!(r.tick(at(50.0, 120.0), t0() + Duration::seconds(10)), Next::Wait);
        let Next::Send { polygon, .. } = r.tick(at(50.0, 120.0), t0() + Duration::seconds(40)) else { panic!("a closer pen") };
        assert!(first.contains(at(50.0, 128.0)) && !polygon.contains(at(50.0, 128.0)));
        // Well inside: back.
        assert_eq!(r.tick(at(50.0, 90.0), t0() + Duration::seconds(80)), Next::Back);
    }

    #[test]
    fn a_new_herd_boundary_rebuilds_the_pen_at_once() {
        let mut r = Run::new();
        assert!(matches!(r.tick(at(50.0, 130.0), t0()), Next::Send { .. }));
        r.paddock = rect(0.0, -100.0, 100.0, 60.0);
        r.version = 2;
        let Next::Send { polygon, .. } = r.tick(at(50.0, 130.0), t0() + Duration::seconds(3)) else { panic!("rebuilt") };
        assert!(polygon.contains(at(50.0, 130.0)));
        assert!(polygon.contains(at(50.0, -99.0)));
        assert_eq!(r.pen.target_version, Some(2));
    }

    #[test]
    fn out_of_its_pen_too_gets_a_new_pen_where_it_is() {
        let mut r = Run::new();
        assert!(matches!(r.tick(at(50.0, 130.0), t0()), Next::Send { .. }));
        let far = at(160.0, 170.0);
        assert!(!r.current.as_ref().unwrap().contains(far));
        let Next::Send { polygon, .. } = r.tick(far, t0() + Duration::seconds(5)) else { panic!("a new pen") };
        assert!(margin(&polygon, far) > 0.0);
        assert!(polygon.contains(at(50.0, 50.0)));
    }
}
