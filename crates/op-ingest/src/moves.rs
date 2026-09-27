//! Moves: a target and the sweep that brings the herd into it (API.md
//! "Moves: target and sweep"). Every step is a boundary stored through the
//! same path as any other, under the move's decision.
//!
//! [`advance`] decides what happens next from the move and what the collars
//! report; it is pure. [`drive`] loads, decides and writes for one herd under
//! a per-herd lock. The driver task runs it on fix events and every 5 s.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use op_core::time::{from_db, now, to_db};
use op_core::{ApiError, ApiResult, Boundary, Ctx, DbEnum, Event, LonLat, Move, MoveStatus, Polygon, id};
use op_geo::CollarLimits;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::boundary::{NewBoundary, announce, insert_boundary};
use crate::planner::{self, BACK_FACTOR, Frame, Plan, PlanInput, Space};
use crate::shape::{Prepared, prepare};
use crate::{SendOpts, config, db};

/// At most one step this often.
pub const STEP_EVERY: Duration = Duration::seconds(30);
/// An animal holding the sweep up this long without moving up is dropped.
pub const STRAGGLER_AFTER: Duration = Duration::minutes(5);
/// Fixes older than this don't count as a position.
pub const FRESH_FIX: Duration = Duration::minutes(10);
/// `BoundaryStatus.move` keeps showing an ended move this long.
pub const SHOW_ENDED: Duration = Duration::minutes(10);
/// Counts as moving up.
const MOVED_UP_M: f64 = 1.0;

/// The smallest advance worth a new version.
pub(crate) fn stride(warn_m: f64) -> f64 {
    (0.3 * warn_m).max(2.0)
}

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/herds/{id}/move/stop", post(post_stop))
}

/// The driver's own state, kept on the move row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sweep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Frame>,
    /// Back line of the step in effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_step_at: Option<DateTime<Utc>>,
    /// Per collar: best progress so far and when the stuck clock started.
    #[serde(default)]
    pub animals: BTreeMap<String, Track>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub best: f64,
    pub since: DateTime<Utc>,
}

/// What the server knows about the herd right now.
#[derive(Debug, Clone)]
pub struct Situation {
    pub active: Option<Polygon>,
    /// A staged boundary is waiting to take effect.
    pub pending: bool,
    pub paddock: Option<Polygon>,
    /// Fresh positions per collar.
    pub positions: Vec<(String, LonLat)>,
    /// What the herd's collars hold (the strictest limits among those that hold holes).
    pub limits: CollarLimits,
}

impl Default for Situation {
    fn default() -> Self {
        Self { active: None, pending: false, paddock: None, positions: vec![], limits: CollarLimits::V0 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// Send this boundary; `last` when it is the target.
    Send {
        polygon: Polygon,
        last: bool,
        remaining_m: f64,
    },
    Wait,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub next: Next,
    pub sweep: Sweep,
    pub stragglers: Vec<String>,
}

/// The move as far as [`advance`] needs it.
pub struct MoveState<'a> {
    pub target: &'a Polygon,
    pub warn_m: f64,
    pub step: u32,
    pub sweep: &'a Sweep,
    pub stragglers: &'a [String],
}

/// Decide the move's next action. The first call (step 0) sends at once:
/// the target when the herd is already in it, else the first step. Later
/// steps wait for the herd to move a stride up and for 30 s since the last.
/// Animals holding the sweep up for 5 minutes without moving up become
/// stragglers; at step 0, animals already outside the active boundary do.
pub fn advance(m: &MoveState, sit: &Situation, now: DateTime<Utc>) -> Outcome {
    let mut sweep = m.sweep.clone();
    let mut stragglers = m.stragglers.to_vec();
    let space = Space::new(m.target);
    let stride = stride(m.warn_m);
    for _ in 0..8 {
        let herd: Vec<&(String, LonLat)> = sit.positions.iter().filter(|(c, _)| !stragglers.contains(c)).collect();
        let animals: Vec<LonLat> = herd.iter().map(|(_, p)| *p).collect();
        let input = PlanInput {
            target: m.target,
            previous: sit.active.as_ref(),
            paddock: sit.paddock.as_ref(),
            animals: &animals,
            warn_m: m.warn_m,
            frame: sweep.frame,
            limits: sit.limits,
        };
        let plan = planner::plan(&input);
        let due = m.step == 0 || sweep.last_step_at.is_none_or(|t| now - t >= STEP_EVERY);
        let send = |sweep: &mut Sweep, polygon: Polygon, last: bool, remaining_m: f64, frame: Option<Frame>, level: Option<f64>| {
            sweep.last_step_at = Some(now);
            if frame.is_some() {
                sweep.frame = frame;
            }
            sweep.level = level;
            // Stuck clocks start over with every step.
            sweep.animals.clear();
            if let (Some(space), Some(f)) = (&space, sweep.frame) {
                for (c, p) in &herd {
                    sweep.animals.insert(c.clone(), Track { best: space.progress(f, *p), since: now });
                }
            }
            Next::Send { polygon, last, remaining_m }
        };
        let (left_out, frame) = match &plan {
            Plan::Target if due => {
                let level = sweep.level;
                let next = send(&mut sweep, m.target.clone(), true, 0.0, None, level);
                return Outcome { next, sweep, stragglers };
            }
            Plan::Step(s) => {
                if m.step == 0 && !s.left_out.is_empty() {
                    // Already outside the fence they hold: never cued there, so listed now.
                    let outside: Vec<String> = s
                        .left_out
                        .iter()
                        .map(|i| herd[*i])
                        .filter(|(_, p)| sit.active.as_ref().is_some_and(|a| !a.contains(*p)))
                        .map(|(c, _)| c.clone())
                        .collect();
                    if !outside.is_empty() {
                        stragglers.extend(outside);
                        continue;
                    }
                }
                let ahead = sweep.level.is_none_or(|cur| s.level >= cur + stride);
                if due && ahead && s.left_out.is_empty() {
                    let next = send(&mut sweep, s.polygon.clone(), false, s.remaining_m, Some(s.frame), Some(s.level));
                    return Outcome { next, sweep, stragglers };
                }
                (s.left_out.clone(), Some(s.frame))
            }
            Plan::Target => (vec![], None),
            Plan::Hold(why) => {
                tracing::debug!("move holds: {why}");
                (vec![], None)
            }
        };
        // Waiting. When the wait is on the animals, run their stuck clocks.
        let (Some(space), Some(frame)) = (&space, sweep.frame.or(frame)) else { break };
        if !due {
            break;
        }
        let threshold = sweep.level.map(|l| l + stride);
        let mut dropped = Vec::new();
        for (i, (c, p)) in herd.iter().enumerate() {
            let prog = space.progress(frame, *p);
            let blocking = left_out.contains(&i) || threshold.is_some_and(|t| prog - BACK_FACTOR * m.warn_m < t);
            let t = sweep.animals.entry(c.clone()).or_insert(Track { best: prog, since: now });
            if !blocking || prog > t.best + MOVED_UP_M {
                t.best = t.best.max(prog);
                t.since = now;
            } else if now - t.since >= STRAGGLER_AFTER {
                dropped.push(c.clone());
            }
        }
        if dropped.is_empty() {
            break;
        }
        for c in &dropped {
            sweep.animals.remove(c);
        }
        stragglers.extend(dropped);
    }
    Outcome { next: Next::Wait, sweep, stragglers }
}

// Storage

struct MoveRow {
    m: Move,
    warn_m: f64,
    hysteresis_m: f64,
    sweep: Sweep,
}

fn move_from_row(r: &SqliteRow) -> anyhow::Result<MoveRow> {
    let m = Move {
        id: r.try_get("id")?,
        herd_id: r.try_get("herd_id")?,
        decision_id: r.try_get("decision_id")?,
        target: serde_json::from_str(&r.try_get::<String, _>("target")?)?,
        status: MoveStatus::from_db(&r.try_get::<String, _>("status")?)?,
        step: r.try_get::<i64, _>("step")? as u32,
        remaining_m: r.try_get("remaining_m")?,
        stragglers: serde_json::from_str(&r.try_get::<String, _>("stragglers")?)?,
        started_at: from_db(&r.try_get::<String, _>("started_at")?)?,
        updated_at: from_db(&r.try_get::<String, _>("updated_at")?)?,
    };
    let sweep = serde_json::from_str(&r.try_get::<String, _>("sweep")?).unwrap_or_default();
    Ok(MoveRow { m, warn_m: r.try_get("warn_m")?, hysteresis_m: r.try_get("hysteresis_m")?, sweep })
}

/// The herd's running move, or its last one if it ended less than 10
/// minutes ago.
pub async fn current_move(db: &sqlx::SqlitePool, herd_id: &str, at: DateTime<Utc>) -> anyhow::Result<Option<Move>> {
    let row = sqlx::query("SELECT * FROM moves WHERE herd_id = ? ORDER BY (status = 'sweeping') DESC, started_at DESC, id DESC LIMIT 1")
        .bind(herd_id)
        .fetch_optional(db)
        .await?;
    let Some(m) = row.map(|r| move_from_row(&r)).transpose()?.map(|r| r.m) else { return Ok(None) };
    Ok((m.status == MoveStatus::Sweeping || at - m.updated_at <= SHOW_ENDED).then_some(m))
}

async fn sweeping_row(db: &sqlx::SqlitePool, herd_id: &str) -> anyhow::Result<Option<MoveRow>> {
    let row = sqlx::query("SELECT * FROM moves WHERE herd_id = ? AND status = 'sweeping'").bind(herd_id).fetch_optional(db).await?;
    row.map(|r| move_from_row(&r)).transpose()
}

/// Everything [`advance`] reads, gathered before any write lock is taken.
async fn situation(ctx: &Ctx, herd_id: &str, at: DateTime<Utc>) -> anyhow::Result<Situation> {
    let split = db::herd_boundaries(ctx.db(), herd_id, at).await?;
    let paddock = match ctx.store().get_herd(herd_id).await?.and_then(|h| h.paddock_id) {
        Some(p) => ctx.store().get_paddock(&p).await?.map(|p| p.geometry),
        None => None,
    };
    // Animals out on their own boundary are walked back by their escape.
    let escaped = crate::escapes::escaped_collars(ctx.db(), herd_id).await?;
    let positions = db::list_collars(ctx.db(), Some(herd_id))
        .await?
        .into_iter()
        .filter(|c| !escaped.contains(&c.id))
        .filter_map(|c| {
            let f = c.last_fix?;
            (at - f.at <= FRESH_FIX).then_some((c.id, f.point))
        })
        .collect();
    let limits = crate::shape::herd_limits(ctx, herd_id).await?;
    Ok(Situation { active: split.active.map(|b| b.geometry), pending: !split.staged.is_empty(), paddock, positions, limits })
}

/// A sweep step on its way to the collars, like every herd boundary.
async fn prepare_step(ctx: &Ctx, herd_id: &str, polygon: &Polygon, warn_m: f64, hysteresis_m: f64) -> ApiResult<Polygon> {
    let opts = SendOpts { warn_m: Some(warn_m), hysteresis_m: Some(hysteresis_m), effective_at: None };
    Ok(prepare(ctx, herd_id, polygon, &opts).await?.geometry)
}

/// Moves are driven one herd at a time.
static LOCKS: LazyLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);

async fn herd_lock(herd_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let m = LOCKS.lock().unwrap_or_else(|e| e.into_inner()).entry(herd_id.to_owned()).or_default().clone();
    m.lock_owned().await
}

/// The farmer decision recorded with a move started from the map.
pub(crate) struct FarmerDecision<'a> {
    pub to_paddock_id: Option<&'a str>,
    pub reasoning: &'a str,
}

pub struct Started {
    pub r#move: Move,
    /// The first boundary sent, if the move could send one at once.
    pub boundary: Option<Boundary>,
}

/// Start a move toward `target` under `decision_id`, replacing any running
/// move for the herd (it is marked stopped). Sends the target at once when
/// the herd is already in it, else the first sweep step.
pub async fn start_move(ctx: &Ctx, herd_id: &str, target: Polygon, opts: SendOpts, decision_id: &str) -> anyhow::Result<Started> {
    let go = async {
        if ctx.store().get_herd(herd_id).await?.is_none() {
            return Err(ApiError::not_found("No such herd."));
        }
        let prepared = prepare(ctx, herd_id, &target, &opts).await?;
        begin(ctx, herd_id, prepared, opts.effective_at, decision_id, None).await
    };
    go.await.map_err(|e| anyhow::anyhow!(e.message))
}

/// Start a move toward a prepared target (see [`start_move`]).
pub(crate) async fn begin(
    ctx: &Ctx,
    herd_id: &str,
    target: Prepared,
    effective_at: Option<DateTime<Utc>>,
    decision_id: &str,
    farmer: Option<FarmerDecision<'_>>,
) -> ApiResult<Started> {
    if ctx.store().get_herd(herd_id).await?.is_none() {
        return Err(ApiError::not_found("No such herd."));
    }
    let Prepared { geometry: target, warn_m, hysteresis_m, .. } = target;
    let _guard = herd_lock(herd_id).await;
    let at = now();
    let effective_at = effective_at.map(op_protocol::wire_time::trunc_secs).filter(|t| *t > at);
    let sit = situation(ctx, herd_id, at).await?;
    let mut out = advance(&MoveState { target: &target, warn_m, step: 0, sweep: &Sweep::default(), stragglers: &[] }, &sit, at);
    if let Next::Send { polygon, last: false, .. } = &mut out.next {
        *polygon = prepare_step(ctx, herd_id, polygon, warn_m, hysteresis_m).await?;
    }

    let mut m = Move {
        id: id::new_id(id::MOVE),
        herd_id: herd_id.to_owned(),
        decision_id: decision_id.to_owned(),
        target: target.clone(),
        status: MoveStatus::Sweeping,
        step: 0,
        remaining_m: 0.0,
        stragglers: out.stragglers.clone(),
        started_at: at,
        updated_at: at,
    };
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    if let Some(f) = &farmer {
        sqlx::query(
            "INSERT INTO decisions (id, herd_id, source, status, action, to_paddock_id, geometry, reasoning, inputs, created_at, responded_at)
             VALUES (?, ?, 'farmer', 'applied', 'MOVE', ?, ?, ?, '{}', ?, ?)",
        )
        .bind(decision_id)
        .bind(herd_id)
        .bind(f.to_paddock_id)
        .bind(serde_json::to_string(&target).map_err(anyhow::Error::from)?)
        .bind(f.reasoning)
        .bind(to_db(&at))
        .bind(to_db(&at))
        .execute(&mut *tx)
        .await?;
    }
    let rows = sqlx::query("UPDATE moves SET status = 'stopped', updated_at = ? WHERE herd_id = ? AND status = 'sweeping' RETURNING *")
        .bind(to_db(&at))
        .bind(herd_id)
        .fetch_all(&mut *tx)
        .await?;
    let replaced = rows.iter().map(|r| move_from_row(r).map(|r| r.m)).collect::<anyhow::Result<Vec<_>>>()?;
    let mut boundary = None;
    if let Next::Send { polygon, last, remaining_m } = &out.next {
        let nb = NewBoundary { herd_id, geometry: polygon, warn_m, hysteresis_m, effective_at, decision_id, created_at: at, collar_id: None, copy_of: None };
        let b = insert_boundary(&mut tx, &nb).await?;
        sqlx::query("UPDATE decisions SET boundary_id = ? WHERE id = ?").bind(&b.id).bind(decision_id).execute(&mut *tx).await?;
        m.step = 1;
        m.remaining_m = round_m(*remaining_m);
        if *last {
            m.status = MoveStatus::Done;
        }
        boundary = Some(b);
    }
    sqlx::query(
        "INSERT INTO moves (id, herd_id, decision_id, target, status, step, remaining_m, stragglers, warn_m, hysteresis_m, sweep, started_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&m.id)
    .bind(herd_id)
    .bind(decision_id)
    .bind(serde_json::to_string(&m.target).map_err(anyhow::Error::from)?)
    .bind(m.status.as_db())
    .bind(m.step as i64)
    .bind(m.remaining_m)
    .bind(serde_json::to_string(&m.stragglers).map_err(anyhow::Error::from)?)
    .bind(warn_m)
    .bind(hysteresis_m)
    .bind(serde_json::to_string(&out.sweep).map_err(anyhow::Error::from)?)
    .bind(to_db(&at))
    .bind(to_db(&at))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    for old in replaced {
        tracing::info!(herd = %herd_id, r#move = %old.id, "move replaced by a new target");
        ctx.publish(Event::Move { r#move: old });
    }
    if let Some(b) = &boundary {
        announce(ctx, b).await;
    }
    log_move(&m, boundary.as_ref(), sit.positions.len());
    ctx.publish(Event::Move { r#move: m.clone() });
    // A sweep: the herd's collars report and poll fast until it is over.
    if m.status == MoveStatus::Sweeping {
        config::refresh_quietly(ctx, config::Scope::Herd(herd_id)).await;
    }
    Ok(Started { r#move: m, boundary })
}

fn log_move(m: &Move, b: Option<&Boundary>, tracked: usize) {
    tracing::info!(
        herd = %m.herd_id,
        r#move = %m.id,
        status = %m.status.as_db(),
        step = m.step,
        version = b.map(|b| b.version),
        remaining_m = m.remaining_m,
        tracked,
        stragglers = m.stragglers.len(),
        "move"
    );
}

fn round_m(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Stop the herd's running move. The active boundary stays.
pub async fn stop_move(ctx: &Ctx, herd_id: &str) -> ApiResult<Move> {
    let _guard = herd_lock(herd_id).await;
    let row = sqlx::query("UPDATE moves SET status = 'stopped', updated_at = ? WHERE herd_id = ? AND status = 'sweeping' RETURNING *")
        .bind(to_db(&now()))
        .bind(herd_id)
        .fetch_optional(ctx.db())
        .await?;
    let Some(m) = row.map(|r| move_from_row(&r)).transpose()?.map(|r| r.m) else {
        return Err(ApiError::conflict("No move is running for this herd."));
    };
    log_move(&m, None, 0);
    ctx.publish(Event::Move { r#move: m.clone() });
    Ok(m)
}

async fn post_stop(State(ctx): State<Ctx>, Path(herd_id): Path<String>) -> ApiResult<Json<Move>> {
    if ctx.store().get_herd(&herd_id).await?.is_none() {
        return Err(ApiError::not_found("No such herd."));
    }
    Ok(Json(stop_move(&ctx, &herd_id).await?))
}

/// One pass of the driver for a herd at time `at`: load its sweeping move,
/// decide, and store the step (boundary and move in one transaction, only
/// if the move is still at the step it was read at). Returns the move when
/// something visible changed.
pub async fn drive(ctx: &Ctx, herd_id: &str, at: DateTime<Utc>) -> anyhow::Result<Option<Move>> {
    let _guard = herd_lock(herd_id).await;
    let Some(row) = sweeping_row(ctx.db(), herd_id).await? else { return Ok(None) };
    if ctx.store().get_herd(herd_id).await?.is_none() {
        sqlx::query("UPDATE moves SET status = 'stopped', updated_at = ? WHERE id = ? AND status = 'sweeping'")
            .bind(to_db(&at))
            .bind(&row.m.id)
            .execute(ctx.db())
            .await?;
        return Ok(None);
    }
    let sit = situation(ctx, herd_id, at).await?;
    if sit.pending {
        return Ok(None);
    }
    let mut m = row.m.clone();
    let state = MoveState { target: &m.target, warn_m: row.warn_m, step: m.step, sweep: &row.sweep, stragglers: &m.stragglers };
    let mut out = advance(&state, &sit, at);
    if let Next::Send { polygon, last: false, .. } = &mut out.next {
        match prepare_step(ctx, herd_id, polygon, row.warn_m, row.hysteresis_m).await {
            Ok(p) => *polygon = p,
            Err(e) => {
                tracing::warn!(herd = %herd_id, r#move = %m.id, "sweep step not sent: {}", e.message);
                return Ok(None);
            }
        }
    }
    let visible = out.stragglers != m.stragglers || matches!(out.next, Next::Send { .. });
    if !visible && out.sweep == row.sweep {
        return Ok(None);
    }
    let new_stragglers: Vec<String> = out.stragglers.iter().filter(|c| !m.stragglers.contains(c)).cloned().collect();
    m.stragglers = out.stragglers.clone();
    if visible {
        m.updated_at = at;
    }

    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let mut boundary = None;
    if let Next::Send { polygon, last, remaining_m } = &out.next {
        m.step += 1;
        m.remaining_m = round_m(*remaining_m);
        if *last {
            m.status = MoveStatus::Done;
        }
        let nb = NewBoundary {
            herd_id,
            geometry: polygon,
            warn_m: row.warn_m,
            hysteresis_m: row.hysteresis_m,
            effective_at: None,
            decision_id: &m.decision_id,
            created_at: at,
            collar_id: None,
            copy_of: None,
        };
        let b = insert_boundary(&mut tx, &nb).await.map_err(|e| anyhow::anyhow!(e.message))?;
        sqlx::query("UPDATE decisions SET boundary_id = ? WHERE id = ?").bind(&b.id).bind(&m.decision_id).execute(&mut *tx).await?;
        boundary = Some(b);
    }
    let n = sqlx::query(
        "UPDATE moves SET status = ?, step = ?, remaining_m = ?, stragglers = ?, sweep = ?, updated_at = ?
         WHERE id = ? AND status = 'sweeping' AND step = ?",
    )
    .bind(m.status.as_db())
    .bind(m.step as i64)
    .bind(m.remaining_m)
    .bind(serde_json::to_string(&m.stragglers)?)
    .bind(serde_json::to_string(&out.sweep)?)
    .bind(to_db(&m.updated_at))
    .bind(&m.id)
    .bind(row.m.step as i64)
    .execute(&mut *tx)
    .await?;
    if n.rows_affected() != 1 {
        // Stopped or replaced meanwhile: nothing is sent.
        tx.rollback().await?;
        return Ok(None);
    }
    tx.commit().await?;
    for c in &new_stragglers {
        tracing::info!(herd = %herd_id, r#move = %m.id, collar = %c, "straggler dropped from the sweep");
    }
    if let Some(b) = &boundary {
        announce(ctx, b).await;
    }
    if visible {
        log_move(&m, boundary.as_ref(), sit.positions.len());
        ctx.publish(Event::Move { r#move: m.clone() });
        return Ok(Some(m));
    }
    Ok(None)
}

async fn sweeping_herds(ctx: &Ctx) -> anyhow::Result<HashSet<String>> {
    let rows = sqlx::query("SELECT herd_id FROM moves WHERE status = 'sweeping'").fetch_all(ctx.db()).await?;
    Ok(rows.iter().map(|r| r.get::<String, _>(0)).collect())
}

/// After a restart, stuck clocks start over: the time the server was down
/// says nothing about the animals.
async fn restart_clocks(ctx: &Ctx) -> anyhow::Result<()> {
    let at = now();
    let rows = sqlx::query("SELECT * FROM moves WHERE status = 'sweeping'").fetch_all(ctx.db()).await?;
    for r in rows {
        let mut row = move_from_row(&r)?;
        for t in row.sweep.animals.values_mut() {
            t.since = at;
        }
        sqlx::query("UPDATE moves SET sweep = ? WHERE id = ? AND status = 'sweeping'")
            .bind(serde_json::to_string(&row.sweep)?)
            .bind(&row.m.id)
            .execute(ctx.db())
            .await?;
        tracing::info!(herd = %row.m.herd_id, r#move = %row.m.id, step = row.m.step, "resuming move");
    }
    Ok(())
}

/// The driver: reacts to fixes from herds with a sweeping move (coalesced
/// to once a second per herd) and looks at every sweeping move every 5 s.
/// Picks up moves left sweeping by a restart.
pub fn spawn_driver(ctx: Ctx) {
    tokio::spawn(async move {
        if let Err(e) = restart_clocks(&ctx).await {
            tracing::warn!("resuming moves: {e:#}");
        }
        let mut rx = ctx.subscribe();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut sweeping = HashSet::new();
        let mut dirty: HashSet<String> = HashSet::new();
        let mut n: u64 = 0;
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                ev = rx.recv() => match ev {
                    Ok(Event::Fix { herd_id, .. }) if sweeping.contains(&herd_id) => { dirty.insert(herd_id); }
                    Ok(Event::Move { r#move }) if r#move.status == MoveStatus::Sweeping => { sweeping.insert(r#move.herd_id); }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => dirty.extend(sweeping.iter().cloned()),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = tick.tick() => {
                    if n % 5 == 0 {
                        match sweeping_herds(&ctx).await {
                            Ok(s) => sweeping = s,
                            Err(e) => tracing::warn!("move driver: {e:#}"),
                        }
                        dirty.extend(sweeping.iter().cloned());
                    }
                    n += 1;
                    for h in std::mem::take(&mut dirty) {
                        if let Err(e) = drive(&ctx, &h, now()).await {
                            tracing::warn!(herd = %h, "move driver: {e:#}");
                        }
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
    fn herd(pts: &[[f64; 2]]) -> Vec<(String, LonLat)> {
        pts.iter().enumerate().map(|(i, p)| (format!("col_{i}"), at(p[0], p[1]))).collect()
    }

    struct Run {
        target: Polygon,
        step: u32,
        sweep: Sweep,
        stragglers: Vec<String>,
        sit: Situation,
    }

    impl Run {
        fn new(pts: &[[f64; 2]]) -> Self {
            Self {
                target: rect(250.0, 150.0, 300.0, 200.0),
                step: 0,
                sweep: Sweep::default(),
                stragglers: vec![],
                sit: Situation { active: Some(rect(0.0, 0.0, 300.0, 200.0)), positions: herd(pts), ..Default::default() },
            }
        }
        fn tick(&mut self, now: DateTime<Utc>) -> Next {
            let out =
                advance(&MoveState { target: &self.target, warn_m: W, step: self.step, sweep: &self.sweep, stragglers: &self.stragglers }, &self.sit, now);
            if let Next::Send { polygon, .. } = &out.next {
                self.step += 1;
                self.sit.active = Some(polygon.clone());
            }
            self.sweep = out.sweep;
            self.stragglers = out.stragglers;
            out.next
        }
        fn shift(&mut self, dx: f64, dy: f64) {
            let p = Projection::new(ORIGIN);
            for (_, q) in &mut self.sit.positions {
                let l = p.forward(*q);
                *q = p.inverse([l[0] + dx, l[1] + dy]);
            }
        }
    }

    #[test]
    fn herd_inside_gets_the_target_at_once() {
        let mut r = Run::new(&[[270.0, 170.0], [280.0, 180.0]]);
        assert!(matches!(r.tick(t0()), Next::Send { last: true, .. }));
    }

    #[test]
    fn steps_wait_for_the_herd_and_for_30_seconds() {
        let mut r = Run::new(&[[50.0, 50.0], [80.0, 60.0], [100.0, 90.0]]);
        let Next::Send { last: false, remaining_m, .. } = r.tick(t0()) else { panic!("first step") };
        assert!(remaining_m > 100.0);
        // Nobody moved: wait.
        assert_eq!(r.tick(t0() + Duration::seconds(40)), Next::Wait);
        // Everyone moved up, but it's only been 10 s since the step.
        r.shift(20.0, 12.0);
        assert_eq!(r.tick(t0() + Duration::seconds(10)), Next::Wait);
        assert!(matches!(r.tick(t0() + Duration::seconds(31)), Next::Send { last: false, .. }));
        // Less than a stride: wait.
        r.shift(0.5, 0.3);
        assert_eq!(r.tick(t0() + Duration::seconds(70)), Next::Wait);
    }

    #[test]
    fn a_stuck_animal_becomes_a_straggler_after_5_minutes() {
        let mut r = Run::new(&[[20.0, 20.0], [120.0, 90.0], [140.0, 110.0]]);
        assert!(matches!(r.tick(t0()), Next::Send { .. }));
        // The two in front move on; col_0 stays put.
        let p = Projection::new(ORIGIN);
        for (c, q) in &mut r.sit.positions {
            if c != "col_0" {
                let l = p.forward(*q);
                *q = p.inverse([l[0] + 30.0, l[1] + 20.0]);
            }
        }
        for s in [40, 100, 200, 299] {
            assert_eq!(r.tick(t0() + Duration::seconds(s)), Next::Wait, "at {s} s");
            assert!(r.stragglers.is_empty());
        }
        // Past 5 minutes of holding the sweep up (the clock started with the step).
        let next = r.tick(t0() + Duration::seconds(345));
        assert_eq!(r.stragglers, vec!["col_0".to_owned()]);
        let Next::Send { polygon, .. } = next else { panic!("the sweep goes on without it") };
        assert!(!polygon.contains(at(20.0, 20.0)));
    }

    #[test]
    fn an_animal_that_keeps_moving_up_is_not_a_straggler() {
        let mut r = Run::new(&[[20.0, 20.0], [120.0, 90.0]]);
        assert!(matches!(r.tick(t0()), Next::Send { .. }));
        let p = Projection::new(ORIGIN);
        for k in 1..=12 {
            // col_0 creeps up 1.5 m a minute: not enough for a step, but moving.
            for (c, q) in &mut r.sit.positions {
                if c == "col_0" {
                    let l = p.forward(*q);
                    *q = p.inverse([l[0] + 1.2, l[1] + 0.9]);
                }
            }
            r.tick(t0() + Duration::seconds(60 * k));
            assert!(r.stragglers.is_empty(), "minute {k}");
        }
    }

    #[test]
    fn animals_outside_the_fence_at_the_start_are_listed() {
        let mut r = Run::new(&[[50.0, 50.0], [80.0, 60.0]]);
        r.sit.positions.push(("col_out".into(), at(-30.0, 40.0)));
        assert!(matches!(r.tick(t0()), Next::Send { last: false, .. }));
        assert_eq!(r.stragglers, vec!["col_out".to_owned()]);
    }

    #[test]
    fn the_sweep_ends_with_the_target_when_everyone_is_in() {
        let mut r = Run::new(&[[200.0, 120.0], [220.0, 140.0]]);
        assert!(matches!(r.tick(t0()), Next::Send { last: false, .. }));
        r.shift(60.0, 45.0);
        assert!(matches!(r.tick(t0() + Duration::seconds(20)), Next::Wait));
        assert!(matches!(r.tick(t0() + Duration::seconds(30)), Next::Send { last: true, .. }));
    }

    #[test]
    fn no_positions_sends_the_target() {
        let mut r = Run::new(&[]);
        assert!(matches!(r.tick(t0()), Next::Send { last: true, .. }));
    }
}
