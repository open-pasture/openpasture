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
use crate::planner::{self, Frame, Plan, PlanInput, Rear, Space};
use crate::shape::{Prepared, prepare};
use crate::{SendOpts, config, db};

/// At most one step this often: about one round of the fast config (a
/// download, a cue at the next fix, the animal walking off it, and the
/// report that shows it).
pub const STEP_EVERY: Duration = Duration::seconds(25);
/// An animal holding the sweep up this long without moving up is dropped.
pub const STRAGGLER_AFTER: Duration = Duration::minutes(5);
/// Fixes older than this don't count as a position.
pub const FRESH_FIX: Duration = Duration::minutes(10);
/// A collar heard within this long before a move started is with the herd:
/// its silence holds the sweep (see [`advance`]). One quiet for longer is
/// gone (a flat battery, a lost collar) and holds nothing.
pub const WITH_HERD: Duration = Duration::hours(24);
/// A collar with the herd this long without a fix (not counting time the
/// sweep was held for an outage) is taken as gone, a flat battery or a lost
/// collar, and listed as a straggler; until then the sweep waits for it
/// where it was last fixed.
pub const SILENT_DROP: Duration = Duration::minutes(15);
/// A sweep held for silent collars goes on this long after the silence ends,
/// so the collars that report back later (each on its own report timer)
/// rejoin it before it moves.
pub const RESUME_AFTER: Duration = Duration::minutes(2);
/// `BoundaryStatus.move` keeps showing an ended move this long.
pub const SHOW_ENDED: Duration = Duration::minutes(10);
/// Counts as moving up.
const MOVED_UP_M: f64 = 1.0;
/// A step's back line is where nine in ten of the herd are ahead of it
/// (see [`Rear::Follow`]): the p90 of the animals.
pub const REAR_QUANTILE: f64 = 0.1;
/// A sweep steps once its back line has moved up this share of `warn_m`.
pub const SWEEP_STRIDE: f64 = 0.1;
/// Where an animal is: its fixes this close before its newest one,
/// averaged by accuracy (a single fix wanders by metres).
pub const SMOOTH_WINDOW: Duration = Duration::seconds(12);
/// Room a step leaves behind an animal per metre of uncertainty in where
/// it is (the spread of its averaged fixes).
pub const NOISE_MARGIN: f64 = 0.5;
/// ... and per second since its newest fix (a grazing animal drifts).
pub const DRIFT_M_PER_S: f64 = 0.05;
/// The most room added for both, so an animal at the back stays inside its
/// warning band and is cued.
pub const EXTRA_MAX_M: f64 = 0.5;

/// Where an animal is from its latest fixes `(point, accuracy_m)`: the mean
/// weighted by `1 / accuracy²`, and its spread (`1 / sqrt(Σ 1 / accuracy²)`,
/// metres; one fix: its accuracy).
pub fn estimate(fixes: &[(LonLat, f64)]) -> Option<(LonLat, f64)> {
    let (mut w, mut x, mut y) = (0.0, 0.0, 0.0);
    for (p, acc) in fixes {
        let wi = 1.0 / acc.max(0.5).powi(2);
        w += wi;
        x += wi * p[0];
        y += wi * p[1];
    }
    (w > 0.0).then(|| ([x / w, y / w], 1.0 / w.sqrt()))
}

/// The smallest advance of an escape pen worth a new version.
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
    /// The farmer's time for the move when its first step hasn't gone yet
    /// (it waits for the collars): that step is staged for then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<DateTime<Utc>>,
    /// Held for silent collars: it goes on no sooner than this
    /// ([`RESUME_AFTER`] after the silence ended).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_until: Option<DateTime<Utc>>,
    /// When the last such hold ended: silence before it doesn't count toward [`SILENT_DROP`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_at: Option<DateTime<Utc>>,
    /// How far the back line had to go at the first step, metres: the
    /// sweep's length, for its pace once it is done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_m: Option<f64>,
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
    /// A staged step of this move is waiting to take effect (a farmer's
    /// target sent for later). Staged boundaries of anything else (a strip
    /// schedule) don't hold the sweep up; the schedule restages above it.
    pub pending: bool,
    pub paddock: Option<Polygon>,
    /// Fresh positions per collar.
    pub positions: Vec<(String, LonLat)>,
    /// When each of `positions` was fixed (the collar's own clock); a
    /// collar not here counts as fixed now.
    pub heard: HashMap<String, DateTime<Utc>>,
    /// How uncertain each of `positions` is, metres (see [`estimate`]); a
    /// collar not here counts as exact.
    pub spread: HashMap<String, f64>,
    /// Each collar's newest fix where it isn't its position (an average of
    /// its latest fixes): the collar judges its fence from this one, so a
    /// step is planned from whichever of the two is further back.
    pub newest: HashMap<String, LonLat>,
    /// Collars with the herd when the move started (heard within
    /// [`WITH_HERD`] before it) that have no fresh fix now (last fix older
    /// than [`FRESH_FIX`]): where and when each was last fixed.
    pub silent: Vec<(String, LonLat, DateTime<Utc>)>,
    /// What the herd's collars hold: the strictest limits among those that
    /// hold holes, with no more outer corners than any of its collars holds
    /// (so no collar's copy of a step is cut down behind an animal).
    pub limits: CollarLimits,
    /// The limits each of the herd's collars gets its copy of a step fitted
    /// to ([`crate::shape::CollarCaps::fit_limits`], each set once).
    pub fits: Vec<CollarLimits>,
}

impl Default for Situation {
    fn default() -> Self {
        Self {
            active: None,
            pending: false,
            paddock: None,
            positions: vec![],
            heard: HashMap::new(),
            spread: HashMap::new(),
            newest: HashMap::new(),
            silent: vec![],
            limits: CollarLimits::V0,
            fits: vec![],
        }
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
/// steps wait [`STEP_EVERY`] since the last and for the herd's back line
/// (where nine in ten of its animals are ahead, [`REAR_QUANTILE`]) to move
/// up [`SWEEP_STRIDE`] × `warn_m`. Each step's back edge follows the rear of
/// the herd ([`Rear::Follow`]): every animal is inside it, those at the back
/// in their warning band, with room for how uncertain and how old its
/// position is, so one lagging animal holds back only its own stretch of
/// it. Animals whose own fixes show them at the very back (within a stride
/// of the rearmost) for 5 minutes without moving up become stragglers, a
/// few at a time, never the rear tenth at once; at step 0,
/// animals already outside the active boundary do.
///
/// A sweep only moves on what the collars say. While half or more of the
/// collars with the herd are silent (no fix for [`FRESH_FIX`]: the farm's
/// link or the cell is down), it holds, from its first step on: no step, no
/// target, nobody dropped, and it goes on [`RESUME_AFTER`] after the silence
/// ends, with the stuck clocks started over. Fewer silent ones hold it up
/// where they were last fixed, never stepped past unseen, and are never
/// dropped for being stuck (their fixes don't say so). Only one silent for
/// [`SILENT_DROP`] outside such a hold is taken as gone and listed. An
/// outage reaches the collars one report at a time, and so does the link
/// coming back, so nobody is dropped on the way in or out of one.
pub fn advance(m: &MoveState, sit: &Situation, now: DateTime<Utc>) -> Outcome {
    let mut sweep = m.sweep.clone();
    let mut stragglers = m.stragglers.to_vec();
    let silent: Vec<&(String, LonLat, DateTime<Utc>)> = sit.silent.iter().filter(|(c, _, _)| !stragglers.contains(c)).collect();
    let with_it = sit.positions.iter().filter(|(c, _)| !stragglers.contains(c)).count() + silent.len();
    let outage = !silent.is_empty() && silent.len() * 2 >= with_it;
    if outage {
        sweep.held_until = Some(now + RESUME_AFTER);
    }
    if outage || sweep.held_until.is_some_and(|t| now < t) {
        for t in sweep.animals.values_mut() {
            t.since = now;
        }
        return Outcome { next: Next::Wait, sweep, stragglers };
    }
    if let Some(t) = sweep.held_until.take() {
        sweep.resumed_at = Some(t);
    }
    let quiet_since = |at: DateTime<Utc>| sweep.resumed_at.map_or(at, |r| r.max(at));
    let (gone, waited): (Vec<_>, Vec<_>) = silent.into_iter().partition(|(_, _, at)| now - quiet_since(*at) >= SILENT_DROP);
    stragglers.extend(gone.into_iter().map(|(c, _, _)| c.clone()));
    let space = Space::new(m.target);
    // Where each animal is, or its newest fix if that is further back: an
    // animal walking back against the sweep is behind the average of its
    // fixes, and its collar judges the fence from the newest one.
    let mut positions: Vec<(String, LonLat)> = sit
        .positions
        .iter()
        .map(|(c, p)| {
            let q = match (sit.newest.get(c), &space, sweep.frame) {
                (Some(n), Some(space), Some(f)) if space.progress(f, *n) < space.progress(f, *p) => *n,
                (Some(n), _, None) => *n,
                _ => *p,
            };
            (c.clone(), q)
        })
        .collect();
    // The rest are with the herd where they were last fixed.
    positions.extend(waited.iter().map(|(c, p, _)| (c.clone(), *p)));
    let last_fix: HashMap<&str, DateTime<Utc>> = waited.iter().map(|(c, _, at)| (c.as_str(), *at)).collect();
    let fixed_at = |c: &str| sit.heard.get(c).or_else(|| last_fix.get(c)).copied().unwrap_or(now);
    let stride = SWEEP_STRIDE * m.warn_m;
    for _ in 0..8 {
        let herd: Vec<&(String, LonLat)> = positions.iter().filter(|(c, _)| !stragglers.contains(c)).collect();
        let animals: Vec<LonLat> = herd.iter().map(|(_, p)| *p).collect();
        // Room for how uncertain and how old each position is.
        let extra: Vec<f64> = herd
            .iter()
            .map(|(c, _)| {
                let age = (now - fixed_at(c)).num_milliseconds().max(0) as f64 / 1000.0;
                (NOISE_MARGIN * sit.spread.get(c).copied().unwrap_or(0.0) + DRIFT_M_PER_S * age).min(EXTRA_MAX_M)
            })
            .collect();
        let input = PlanInput {
            target: m.target,
            previous: sit.active.as_ref(),
            paddock: sit.paddock.as_ref(),
            animals: &animals,
            warn_m: m.warn_m,
            frame: sweep.frame,
            limits: sit.limits,
        };
        let plan = planner::plan_with(&input, Rear::Follow { quantile: REAR_QUANTILE, extra: &extra });
        let due = m.step == 0 || sweep.last_step_at.is_none_or(|t| now - t >= STEP_EVERY);
        let send = |sweep: &mut Sweep, polygon: Polygon, last: bool, remaining_m: f64, frame: Option<Frame>, level: Option<f64>| {
            sweep.last_step_at = Some(now);
            sweep.start_m.get_or_insert(remaining_m);
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
                // Its back line moved up, or its back edge did where it cues animals.
                let ahead = sweep.level.is_none_or(|cur| s.level >= cur + stride)
                    || sit.active.as_ref().is_some_and(|a| edge_moved_up(&space, a, &s.polygon, &animals, m.warn_m) >= stride);
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
        // Holding it up: the few at the very back (within a stride of the
        // rearmost), not everyone behind the step's p10 back line: dropping
        // the rear tenth at once would fence them all out together.
        let progress: Vec<f64> = herd.iter().map(|(_, p)| space.progress(frame, *p)).collect();
        let rearmost = progress.iter().copied().fold(f64::INFINITY, f64::min);
        let threshold = sweep.level.map(|_| rearmost + stride);
        let mut dropped = Vec::new();
        for (i, (c, _)) in herd.iter().enumerate() {
            let prog = progress[i];
            let blocking = left_out.contains(&i) || threshold.is_some_and(|t| prog < t);
            let t = sweep.animals.entry(c.clone()).or_insert(Track { best: prog, since: now });
            if !blocking || prog > t.best + MOVED_UP_M {
                t.best = t.best.max(prog);
                t.since = now;
            } else if fixed_at(c) - t.since >= STRAGGLER_AFTER {
                // Its fixes, not its silence, say it stayed put.
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
/// `decision_id` is the move's: only its own staged step holds it up.
/// `since`: when the move started; collars heard within [`WITH_HERD`]
/// before then and silent now are [`Situation::silent`] (`None`: nobody is).
pub(crate) async fn situation(
    ctx: &Ctx,
    herd_id: &str,
    at: DateTime<Utc>,
    decision_id: Option<&str>,
    since: Option<DateTime<Utc>>,
) -> anyhow::Result<Situation> {
    let split = db::herd_boundaries(ctx.db(), herd_id, at).await?;
    let paddock = match ctx.store().get_herd(herd_id).await?.and_then(|h| h.paddock_id) {
        Some(p) => ctx.store().get_paddock(&p).await?.map(|p| p.geometry),
        None => None,
    };
    // Animals out on their own boundary are walked back by their escape.
    let escaped = crate::escapes::escaped_collars(ctx.db(), herd_id).await?;
    let recent = recent_fixes(ctx, herd_id, at).await?;
    let (mut positions, mut heard, mut spread, mut newest, mut silent) = (Vec::new(), HashMap::new(), HashMap::new(), HashMap::new(), Vec::new());
    // Parked collars are off duty; animals out on their own boundary are walked back by their escape.
    for c in db::list_collars(ctx.db(), Some(herd_id)).await?.into_iter().filter(|c| c.parked_at.is_none() && !escaped.contains(&c.id)) {
        let Some(f) = c.last_fix else { continue };
        if at - f.at <= FRESH_FIX {
            let near: Vec<(LonLat, f64)> = recent.get(&c.id).map(|v| near_fixes(v, &f)).unwrap_or_default();
            let (point, s) = estimate(&near).unwrap_or((f.point, f.accuracy_m));
            heard.insert(c.id.clone(), f.at);
            spread.insert(c.id.clone(), s);
            if point != f.point {
                newest.insert(c.id.clone(), f.point);
            }
            positions.push((c.id, point));
        } else if since.is_some_and(|s| f.at >= s - WITH_HERD) {
            silent.push((c.id, f.point, f.at));
        }
    }
    let collars = crate::shape::herd_collars(ctx.db(), herd_id).await?;
    let mut limits = crate::shape::strictest(collars.iter().map(|c| &c.caps));
    let mut fits: Vec<CollarLimits> = Vec::new();
    for l in collars.iter().filter(|c| c.parked.is_none()).map(|c| c.caps.fit_limits()) {
        if !fits.contains(&l) {
            limits.outer = limits.outer.min(l.outer);
            fits.push(l);
        }
    }
    // @S: the freeze covers the move's own staged step only.
    let pending = split.staged.iter().any(|b| decision_id.is_some_and(|d| b.decision_id == d));
    Ok(Situation { active: split.active.map(|b| b.geometry), pending, paddock, positions, heard, spread, newest, silent, limits, fits })
}

/// The fixes [`estimate`] averages for a collar whose newest fix is
/// `newest`: those within [`SMOOTH_WINDOW`] before it that agree with it
/// (within twice the accuracy of either), so a walk isn't averaged away.
pub fn near_fixes(recent: &[(DateTime<Utc>, LonLat, f64)], newest: &op_core::Fix) -> Vec<(LonLat, f64)> {
    let proj = op_geo::Projection::new(newest.point);
    let mut out: Vec<(LonLat, f64)> = recent
        .iter()
        .filter(|(t, p, acc)| {
            let q = proj.forward(*p);
            *t < newest.at && newest.at - *t <= SMOOTH_WINDOW && q[0].hypot(q[1]) <= 2.0 * acc.max(newest.accuracy_m)
        })
        .map(|(_, p, a)| (*p, *a))
        .collect();
    out.push((newest.point, newest.accuracy_m));
    out
}

/// How far back [`recent_fixes`] reads: a collar reporting every minute
/// still has its newest few fixes in it.
const RECENT: Duration = Duration::seconds(90);

/// The herd's fixes of the last [`RECENT`], per collar: `(time, point, accuracy_m)`.
async fn recent_fixes(ctx: &Ctx, herd_id: &str, at: DateTime<Utc>) -> anyhow::Result<HashMap<String, Vec<(DateTime<Utc>, LonLat, f64)>>> {
    let rows: Vec<(String, i64, f64, f64, f64)> = sqlx::query_as("SELECT collar_id, t, lon, lat, accuracy_m FROM fixes WHERE herd_id = ? AND t >= ?")
        .bind(herd_id)
        .bind((at - RECENT).timestamp_millis())
        .fetch_all(ctx.db())
        .await?;
    let mut out: HashMap<String, Vec<(DateTime<Utc>, LonLat, f64)>> = HashMap::new();
    for (c, t, lon, lat, acc) in rows {
        out.entry(c).or_default().push((op_core::time::from_unix_ms(t), [lon, lat], acc));
    }
    Ok(out)
}

/// How far a step's edge moves up on the animals it cues: over those
/// inside the new step's warning band and not yet in the target, the median
/// of how much closer its edge is to them than the current boundary's (0
/// when there are none). The target's own edges never move, so animals
/// along them say nothing about the sweep.
fn edge_moved_up(space: &Option<Space>, current: &Polygon, next: &Polygon, animals: &[LonLat], warn_m: f64) -> f64 {
    let Some(space) = space else { return 0.0 };
    let ring = |p: &Polygon| -> Vec<[f64; 2]> { p.outer_ring().iter().map(|q| space.local(*q)).collect() };
    let (cur, new) = (ring(current), ring(next));
    let mut moved: Vec<f64> = animals
        .iter()
        .filter(|a| space.target_margin(**a) < planner::FINISH_MARGIN_M)
        .filter_map(|a| {
            let q = space.local(*a);
            let m = planner::signed_distance(q, &new);
            (m >= 0.0 && m < warn_m).then(|| planner::signed_distance(q, &cur) - m)
        })
        .collect();
    if moved.is_empty() {
        return 0.0;
    }
    moved.sort_by(|a, b| a.total_cmp(b));
    moved[moved.len() / 2]
}

/// A pass of the planner slower than this is logged as a warning.
pub const PLAN_SLOW_MS: f64 = 150.0;

/// [`advance`] on a blocking thread (a pass plans the whole herd), timed.
/// Gives `sit` back with the outcome and the milliseconds it took.
async fn advance_off_thread(m: &MoveState<'_>, sit: Situation, now: DateTime<Utc>) -> anyhow::Result<(Outcome, Situation, f64)> {
    let (target, warn_m, step, sweep, stragglers) = (m.target.clone(), m.warn_m, m.step, m.sweep.clone(), m.stragglers.to_vec());
    let (out, sit, ms) = tokio::task::spawn_blocking(move || {
        let t = std::time::Instant::now();
        let out = advance(&MoveState { target: &target, warn_m, step, sweep: &sweep, stragglers: &stragglers }, &sit, now);
        (out, sit, t.elapsed().as_secs_f64() * 1000.0)
    })
    .await?;
    if ms > PLAN_SLOW_MS {
        tracing::warn!(animals = sit.positions.len(), ms = format!("{ms:.1}"), "sweep planning is slow");
    } else {
        tracing::debug!(animals = sit.positions.len(), ms = format!("{ms:.1}"), "sweep planned");
    }
    Ok((out, sit, ms))
}

/// A sweep step on its way to the collars, like every herd boundary.
async fn prepare_step(ctx: &Ctx, herd_id: &str, polygon: &Polygon, warn_m: f64, hysteresis_m: f64) -> ApiResult<Prepared> {
    let opts = SendOpts { warn_m: Some(warn_m), hysteresis_m: Some(hysteresis_m), effective_at: None };
    prepare(ctx, herd_id, polygon, &opts).await
}

/// How many of `animals` a prepared step leaves without the room the plan
/// gave them in one of the fences the herd's collars enforce (the stored
/// step, and its copy fitted to each collar's `fits`). The room is judged
/// in `shaped`, the step with the exclusions in effect: an animal in an
/// exclusion is its to cue out, not the fit's. Half the warning band less a
/// quarter metre, or what it had if that was less, as the planner gives.
pub fn short_of_room(shaped: &Polygon, fitted: &Polygon, fits: &[CollarLimits], animals: &[LonLat], warn_m: f64) -> usize {
    use op_geo::ring::point_in_ring;
    let Some(origin) = shaped.centroid() else { return 0 };
    let proj = op_geo::Projection::new(origin);
    let reference = proj.forward_ring(&shaped.outer_ring());
    let mut fences = vec![proj.forward_ring(&fitted.outer_ring())];
    fences.extend(fits.iter().map(|l| proj.forward_ring(&crate::shape::fit_for(fitted, l, warn_m).outer_ring())));
    animals
        .iter()
        .filter(|a| {
            if shaped.holes().any(|h| point_in_ring(**a, &h)) {
                return false;
            }
            let q = proj.forward(**a);
            let m = planner::signed_distance(q, &reference);
            let need = ((0.5 * warn_m).min(m) - 0.25).max(0.0);
            m > 0.0 && fences.iter().any(|f| planner::signed_distance(q, f) < need - 1e-3)
        })
        .count()
}

/// Tries at planning a step whose fences keep every animal's room (see [`next_step`]).
const FIT_TRIES: usize = 4;

/// [`advance`], with a step prepared for the collars ([`prepare_step`]).
/// Exclusions notched into a step add corners, and fitting it down to what
/// a collar holds cuts its smallest corners, the apexes of a followed edge
/// a few metres behind single animals. So a step goes only if every animal
/// keeps its room in each collar's fence ([`short_of_room`]); else it is
/// planned again with fewer corners (room for the ones the exclusions
/// add); `None` when after [`FIT_TRIES`] none does (nothing is sent this pass).
async fn next_step(
    ctx: &Ctx,
    herd_id: &str,
    m: &MoveState<'_>,
    mut sit: Situation,
    now: DateTime<Utc>,
    hysteresis_m: f64,
) -> ApiResult<Option<(Outcome, Situation, f64)>> {
    let mut total_ms = 0.0;
    for _ in 0..FIT_TRIES {
        let (mut out, back, ms) = advance_off_thread(m, sit, now).await?;
        sit = back;
        total_ms += ms;
        let Next::Send { polygon, last: false, .. } = &mut out.next else { return Ok(Some((out, sit, total_ms))) };
        let prepared = prepare_step(ctx, herd_id, polygon, m.warn_m, hysteresis_m).await?;
        let animals: Vec<LonLat> = sit
            .positions
            .iter()
            .filter(|(c, _)| !out.stragglers.contains(c))
            .flat_map(|(c, p)| std::iter::once(*p).chain(sit.newest.get(c).copied()))
            .collect();
        let short = short_of_room(&prepared.excluded, &prepared.geometry, &sit.fits, &animals, m.warn_m);
        if short == 0 {
            *polygon = prepared.geometry;
            return Ok(Some((out, sit, total_ms)));
        }
        let planned = polygon.outer_ring().len();
        let added = prepared.excluded.outer_ring().len().saturating_sub(planned);
        let outer = sit.limits.outer.min(planned).saturating_sub(added.max(4)).max(8);
        tracing::debug!(herd = %herd_id, short, planned, outer, "sweep step planned again with fewer corners");
        if outer >= sit.limits.outer {
            break;
        }
        sit.limits.outer = outer;
    }
    Ok(None)
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
    let sit = situation(ctx, herd_id, at, None, Some(at)).await?;
    let positions = sit.positions.len();
    let first = MoveState { target: &target, warn_m, step: 0, sweep: &Sweep::default(), stragglers: &[] };
    let (mut out, plan_ms) = match next_step(ctx, herd_id, &first, sit, at, hysteresis_m).await? {
        Some((out, _, ms)) => (out, ms),
        // No first step keeps every animal inside its collar's fence yet: the driver tries again as the herd moves.
        None => {
            tracing::warn!(herd = %herd_id, "first sweep step not sent: no step keeps every animal inside the fence its collar holds");
            (Outcome { next: Next::Wait, sweep: Sweep::default(), stragglers: vec![] }, 0.0)
        }
    };
    // Nothing goes yet (the collars are silent): the first step keeps the farmer's time.
    if out.next == Next::Wait {
        out.sweep.not_before = effective_at;
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
    log_move(&m, boundary.as_ref(), positions, plan_ms);
    ctx.publish(Event::Move { r#move: m.clone() });
    // A sweep: the herd's collars report and poll fast until it is over.
    if m.status == MoveStatus::Sweeping {
        config::refresh_quietly(ctx, config::Scope::Herd(herd_id)).await;
    }
    Ok(Started { r#move: m, boundary })
}

fn log_move(m: &Move, b: Option<&Boundary>, tracked: usize, plan_ms: f64) {
    tracing::info!(
        herd = %m.herd_id,
        r#move = %m.id,
        status = %m.status.as_db(),
        step = m.step,
        version = b.map(|b| b.version),
        remaining_m = m.remaining_m,
        tracked,
        stragglers = m.stragglers.len(),
        plan_ms = format!("{plan_ms:.1}"),
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
    log_move(&m, None, 0, 0.0);
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
    let sit = situation(ctx, herd_id, at, Some(&row.m.decision_id), Some(row.m.started_at)).await?;
    if sit.pending {
        return Ok(None);
    }
    let mut m = row.m.clone();
    let state = MoveState { target: &m.target, warn_m: row.warn_m, step: m.step, sweep: &row.sweep, stragglers: &m.stragglers };
    let (mut out, sit, plan_ms) = match next_step(ctx, herd_id, &state, sit, at, row.hysteresis_m).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            tracing::warn!(herd = %herd_id, r#move = %m.id, "sweep step not sent: no step keeps every animal inside the fence its collar holds");
            return Ok(None);
        }
        Err(e) => {
            tracing::warn!(herd = %herd_id, r#move = %m.id, "sweep step not sent: {}", e.message);
            return Ok(None);
        }
    };
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
        // A first step held for the collars keeps the farmer's time if it is still ahead.
        let effective_at = out.sweep.not_before.take().filter(|t| *t > at);
        let nb = NewBoundary {
            herd_id,
            geometry: polygon,
            warn_m: row.warn_m,
            hysteresis_m: row.hysteresis_m,
            effective_at,
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
        log_move(&m, boundary.as_ref(), sit.positions.len(), plan_ms);
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
    fn steps_wait_for_the_herd_and_for_25_seconds() {
        let mut r = Run::new(&[[50.0, 50.0], [80.0, 60.0], [100.0, 90.0]]);
        let Next::Send { last: false, remaining_m, .. } = r.tick(t0()) else { panic!("first step") };
        assert!(remaining_m > 100.0);
        // Nobody moved: wait.
        assert_eq!(r.tick(t0() + Duration::seconds(40)), Next::Wait);
        // Everyone moved up, but it's only been 10 s since the step.
        r.shift(20.0, 12.0);
        assert_eq!(r.tick(t0() + Duration::seconds(10)), Next::Wait);
        assert!(matches!(r.tick(t0() + STEP_EVERY + Duration::seconds(1)), Next::Send { last: false, .. }));
        // Less than a stride (half a metre at 5 m): wait.
        r.shift(0.2, 0.12);
        assert_eq!(r.tick(t0() + Duration::seconds(70)), Next::Wait);
    }

    #[test]
    fn one_animal_lagging_at_the_back_does_not_hold_the_sweep_up() {
        // Twenty in a bunch and one 25 m behind them that stays put.
        let mut pts: Vec<[f64; 2]> = (0..20).map(|i| [80.0 + 3.0 * (i % 5) as f64, 60.0 + 4.0 * (i / 5) as f64]).collect();
        pts.push([55.0, 70.0]);
        let mut r = Run::new(&pts);
        let lag = at(55.0, 70.0);
        assert!(matches!(r.tick(t0()), Next::Send { last: false, .. }));
        let mut sent = 0;
        for k in 1..=8 {
            let p = Projection::new(ORIGIN);
            for (c, q) in &mut r.sit.positions {
                if c != "col_20" {
                    let l = p.forward(*q);
                    *q = p.inverse([l[0] + 2.4, l[1] + 1.6]);
                }
            }
            if let Next::Send { polygon, .. } = r.tick(t0() + STEP_EVERY * k) {
                sent += 1;
                assert!(polygon.contains(lag), "step {k} still holds the one behind");
            }
        }
        assert!(sent >= 6, "the herd kept being swept: {sent} steps");
        assert!(r.stragglers.is_empty(), "not dropped before its 5 minutes");
    }

    #[test]
    fn where_an_animal_is_comes_from_its_latest_fixes_by_accuracy() {
        let (p, spread) = estimate(&[([0.0, 0.0], 2.0), ([4.0, 0.0], 2.0)]).unwrap();
        assert!((p[0] - 2.0).abs() < 1e-12 && p[1] == 0.0);
        assert!((spread - 2.0 / 2f64.sqrt()).abs() < 1e-12);
        // A sharper fix counts for more.
        let (p, spread) = estimate(&[([0.0, 0.0], 1.0), ([3.0, 0.0], 3.0)]).unwrap();
        assert!((p[0] - 0.3).abs() < 1e-12, "{p:?}");
        assert!(spread < 1.0);
        assert_eq!(estimate(&[([1.0, 2.0], 3.5)]), Some(([1.0, 2.0], 3.5)));
        assert_eq!(estimate(&[]), None);
    }

    #[test]
    fn an_uncertain_or_old_position_gets_more_room_behind_it() {
        let pts = [[60.0, 50.0], [60.0, 90.0], [60.0, 130.0]];
        let margin_of = |spread: f64, age_s: i64| {
            let mut r = Run::new(&pts);
            r.sit.spread.insert("col_1".into(), spread);
            r.sit.heard.insert("col_1".into(), t0() - Duration::seconds(age_s));
            let Next::Send { polygon, .. } = r.tick(t0()) else { panic!("a step") };
            let p = Projection::new(ORIGIN);
            crate::planner::signed_distance(p.forward(at(60.0, 90.0)), &p.forward_ring(&polygon.outer_ring()))
        };
        let exact = margin_of(0.0, 0);
        assert!((exact - planner::FOLLOW_FACTOR * W).abs() < 0.05, "{exact}");
        assert!((margin_of(1.0, 0) - exact - NOISE_MARGIN).abs() < 0.05);
        assert!((margin_of(0.0, 10) - exact - 10.0 * DRIFT_M_PER_S).abs() < 0.05);
        // Never so much that the animal leaves its warning band.
        assert!((margin_of(9.0, 600) - exact - EXTRA_MAX_M).abs() < 0.05);
    }

    /// Fixes 5 s apart at 5 m accuracy all agree while a cow walks back at
    /// 0.9 m/s: their average is 4.5 m ahead of her newest fix, the one her
    /// collar judges the fence from. The step is planned behind that one.
    #[test]
    fn an_animal_walking_back_against_the_sweep_is_planned_from_its_newest_fix() {
        let mut r = Run::new(&[[60.0, 50.0], [60.0, 90.0], [60.0, 130.0]]);
        assert!(matches!(r.tick(t0()), Next::Send { .. }));
        let Some(Frame::Axis { axis }) = r.sweep.frame else { panic!("an axis") };
        r.shift(20.0, 12.0);
        let p = Projection::new(ORIGIN);
        let avg = p.forward(r.sit.positions[1].1);
        let newest = p.inverse([avg[0] - 4.5 * axis[0], avg[1] - 4.5 * axis[1]]);
        r.sit.newest.insert("col_1".into(), newest);
        r.sit.spread.insert("col_1".into(), 5.0 / 3f64.sqrt());
        let Next::Send { polygon, .. } = r.tick(t0() + STEP_EVERY) else { panic!("a step") };
        let margin = crate::planner::signed_distance(p.forward(newest), &p.forward_ring(&polygon.outer_ring()));
        assert!(margin >= planner::FOLLOW_FACTOR * W - 0.05, "her newest fix is {margin:.2} m inside the step");
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

    /// 250 head scattered over the back of the paddock that stop answering
    /// cues after the first step: only the few at the very back hold the
    /// sweep up, so only they are dropped every five minutes, not the whole
    /// rear tenth of the herd at once.
    #[test]
    fn a_stalled_sweep_of_250_drops_only_the_rearmost_few_each_5_minutes() {
        let mut seed: u64 = 7;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let pts: Vec<[f64; 2]> = (0..250).map(|_| [10.0 + 120.0 * rnd(), 10.0 + 144.0 * rnd()]).collect();
        let mut r = Run::new(&pts);
        assert!(matches!(r.tick(t0()), Next::Send { last: false, .. }));
        let mut dropped_before = 0;
        for k in 1..=48 {
            let now = t0() + Duration::seconds(25 * k);
            r.sit.heard = r.sit.positions.iter().map(|(c, _)| (c.clone(), now)).collect();
            if let Next::Send { polygon, .. } = r.tick(now) {
                let outside = r.sit.positions.iter().filter(|(_, p)| !polygon.contains(*p)).count();
                assert!(outside <= 3, "at {} s a step leaves {outside} collars outside it at once", 25 * k);
            }
            if k % 12 == 0 {
                let new = r.stragglers.len() - dropped_before;
                assert!(new <= 3, "{new} stragglers dropped in the 5 minutes to {} s", 25 * k);
                dropped_before = r.stragglers.len();
            }
        }
    }

    #[test]
    fn a_fit_that_cuts_the_corner_behind_an_animal_leaves_it_short_of_room() {
        // A followed edge behind ten animals 12 m apart: each one its own corner.
        let pts: Vec<[f64; 2]> = (0..10).map(|i| [20.0 + 12.0 * i as f64, 60.0]).collect();
        let mut r = Run::new(&pts);
        r.target = rect(0.0, 150.0, 300.0, 200.0);
        let Next::Send { polygon, .. } = r.tick(t0()) else { panic!("a step") };
        let animals: Vec<LonLat> = r.sit.positions.iter().map(|(_, p)| *p).collect();
        assert_eq!(short_of_room(&polygon, &polygon, &[], &animals, W), 0);
        let corners = polygon.outer_ring().len();
        let small = CollarLimits { outer: corners - 6, holes: 0, hole_vertices: 0, total: corners - 6, ..CollarLimits::LEGACY };
        assert!(short_of_room(&polygon, &polygon, &[small], &animals, W) > 0, "fitting {corners} corners down to {} cuts behind animals", corners - 6);
        // An animal in an exclusion is the exclusion's to cue out, not the fit's.
        let p = Projection::new(ORIGIN);
        let hole: Vec<LonLat> = [[16.0, 56.0], [24.0, 56.0], [24.0, 64.0], [16.0, 64.0]].iter().map(|q| p.inverse(*q)).collect();
        let mut shaped = polygon.clone();
        shaped.coordinates.push(Polygon::from_ring(hole).coordinates.remove(0));
        assert_eq!(short_of_room(&shaped, &polygon, &[], &animals[..1], W), 0);
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

    #[test]
    fn a_collar_outage_holds_the_sweep_and_drops_nobody() {
        let mut r = Run::new(&[[50.0, 50.0], [80.0, 60.0], [100.0, 90.0]]);
        assert!(matches!(r.tick(t0()), Next::Send { last: false, .. }));
        // The farm's link goes down: every collar's last fix goes stale.
        let herd = std::mem::take(&mut r.sit.positions);
        r.sit.silent = herd.iter().map(|(c, p)| (c.clone(), *p, t0())).collect();
        for m in [1, 5, 11, 30, 90] {
            assert_eq!(r.tick(t0() + Duration::minutes(m)), Next::Wait, "at {m} min");
            assert!(r.stragglers.is_empty(), "at {m} min");
        }
        // Back, where they were: nobody is dropped for the time the link was down.
        r.sit.silent.clear();
        r.sit.positions = herd;
        let back = t0() + Duration::minutes(91);
        r.sit.heard = r.sit.positions.iter().map(|(c, _)| (c.clone(), back)).collect();
        assert_eq!(r.tick(back), Next::Wait);
        assert!(r.stragglers.is_empty());
        // Walking up, the sweep goes on once the rest have had time to report.
        r.shift(20.0, 12.0);
        assert_eq!(r.tick(back + Duration::seconds(40)), Next::Wait);
        assert!(matches!(r.tick(back + RESUME_AFTER), Next::Send { .. }));
        assert!(r.stragglers.is_empty());
    }

    #[test]
    fn a_silent_collar_is_not_stuck_for_its_silence() {
        let mut r = Run::new(&[[20.0, 20.0], [120.0, 90.0]]);
        assert!(matches!(r.tick(t0()), Next::Send { .. }));
        // Its last fix is from the step: no fix says it stayed put for five minutes.
        r.sit.heard = r.sit.positions.iter().map(|(c, _)| (c.clone(), t0())).collect();
        assert_eq!(r.tick(t0() + Duration::minutes(6)), Next::Wait);
        assert!(r.stragglers.is_empty());
        // Its fixes say so: it is.
        r.sit.heard.insert("col_0".into(), t0() + Duration::minutes(6));
        r.tick(t0() + Duration::minutes(6) + Duration::seconds(5));
        assert_eq!(r.stragglers, vec!["col_0".to_owned()]);
    }

    #[test]
    fn a_lone_silent_collar_holds_the_sweep_where_it_was_until_it_is_gone() {
        let mut r = Run::new(&[[50.0, 50.0], [80.0, 60.0], [100.0, 90.0], [60.0, 70.0]]);
        assert!(matches!(r.tick(t0()), Next::Send { .. }));
        // One of four goes quiet at the back: the sweep isn't stepped past it unseen.
        let (c, p) = r.sit.positions.remove(0);
        r.sit.silent = vec![(c.clone(), p, t0())];
        r.shift(20.0, 12.0);
        for m in [1, 5, 14] {
            let t = t0() + Duration::minutes(m);
            r.sit.heard = r.sit.positions.iter().map(|(c, _)| (c.clone(), t)).collect();
            assert_eq!(r.tick(t), Next::Wait, "at {m} min");
            assert!(r.stragglers.is_empty(), "at {m} min: its silence isn't being stuck");
        }
        // Quiet for a quarter of an hour: gone (a flat battery). Listed, and the sweep goes on.
        let t = t0() + SILENT_DROP + Duration::seconds(1);
        r.sit.heard = r.sit.positions.iter().map(|(c, _)| (c.clone(), t)).collect();
        let Next::Send { polygon, .. } = r.tick(t) else { panic!("goes on without it") };
        assert_eq!(r.stragglers, vec![c]);
        assert!(!polygon.contains(p), "it is left behind only now");
    }

    /// An outage reaches the collars one report at a time, and so does the
    /// link coming back: at no point is anyone dropped for it.
    #[test]
    fn an_outage_that_comes_and_goes_collar_by_collar_drops_nobody() {
        let pts: Vec<[f64; 2]> = (0..10).map(|i| [40.0 + 6.0 * i as f64, 40.0 + 5.0 * (i % 3) as f64]).collect();
        let mut r = Run::new(&pts);
        assert!(matches!(r.tick(t0()), Next::Send { last: false, .. }));
        let herd = r.sit.positions.clone();
        let mut t = t0() + Duration::minutes(10);
        // Their last fixes pass ten minutes old a few seconds apart.
        for c in &herd {
            r.sit.positions.retain(|(id, _)| id != &c.0);
            r.sit.silent.push((c.0.clone(), c.1, t - FRESH_FIX));
            t += Duration::seconds(6);
            assert_eq!(r.tick(t), Next::Wait, "{} silent", r.sit.silent.len());
            assert!(r.stragglers.is_empty(), "{} silent: {:?}", r.sit.silent.len(), r.stragglers);
        }
        t += Duration::minutes(30);
        assert_eq!(r.tick(t), Next::Wait);
        // Back, walked up a little, one report at a time.
        for c in &herd {
            r.sit.silent.retain(|(id, _, _)| id != &c.0);
            r.sit.positions.push(c.clone());
            r.sit.heard.insert(c.0.clone(), t);
            t += Duration::seconds(6);
            assert_eq!(r.tick(t), Next::Wait, "{} back", r.sit.positions.len());
            assert!(r.stragglers.is_empty(), "{} back: {:?}", r.sit.positions.len(), r.stragglers);
        }
        // Once everyone has had time to report, the sweep goes on with all of them.
        r.shift(20.0, 12.0);
        let later = t + RESUME_AFTER;
        r.sit.heard.values_mut().for_each(|h| *h = later);
        assert!(matches!(r.tick(later), Next::Send { .. }));
        assert!(r.stragglers.is_empty());
    }
}
