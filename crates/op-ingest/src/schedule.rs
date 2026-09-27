//! Strip schedules on the collars (field-ready §2.14): the `schedules` and
//! `schedule_moves` tables, what each open and back-fence step looks like,
//! the queue edits, and the driver that stages the moves ahead as boundaries
//! with `effective_at`, so collars open strips on their own clocks while the
//! server is away.
//!
//! **Times.** Rows carry absolute times. Everything that works a time out
//! from the cadence takes an [`Occurrences`] function from its caller
//! (op-engine knows the farm's time zone), so this crate never reads a zone.
//!
//! **Staging** (the driver, every 5 s, and after every edit):
//! - Staged versions must rise with their times: a higher version that takes
//!   effect at or before a lower one kills it (protocol v1 §3.7). So the
//!   schedule's staged moves are always a prefix of its pending moves in time
//!   order; the first move that isn't staged alive, and everything after it,
//!   is staged again above what the herd has now.
//! - How far ahead: what the smallest `free` and `free_bytes` among the
//!   herd's reporting collars allow (parked collars and collars silent for
//!   20 minutes are left out; they are restaged when they report), never more
//!   than `limits.slots - 1`. The server doesn't keep the `free` a collar
//!   sends with each download, so it is worked out from what the collar
//!   holds (`collar_slots`): its slots less the herd versions it holds that
//!   are still alive and aren't this schedule's, counting the herd's active
//!   version whether or not it holds it yet; bytes the same way with each
//!   record's size (`CollarLimits::record_bytes`).
//! - Any immediate herd boundary (a sweep step, a farmer's draw, a reissue)
//!   drops the lower staged slots on the collars. The driver restages above
//!   it once the sequence settles: at the end of a move, or 60 s after a lone
//!   boundary.
//! - A move whose time passed without taking effect (the sequence held it
//!   up, or the server was down before it was staged) is applied at once when
//!   at most 30 minutes late, else marked `late` and never applied; later
//!   moves go ahead. A move is `done` once it took effect by the collars'
//!   rule; `applied_at` is the first collar's own apply time from its ack.
//!
//! **Edits** (skip, hold, move now, edit time, pause, end) that change what
//! is already staged reissue the herd's current strip as a new immediate
//! version, so collars drop the staged moves, then restage.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use chrono::{DateTime, Duration, Utc};
use geo::{Area, BooleanOps, Coord, LineString, MultiPolygon, Polygon as GeoPolygon};
use op_core::identity::Actor;
use op_core::time::{from_db, now, opt_from_db, to_db};
use op_core::{ApiError, ApiResult, Boundary, Ctx, DbEnum, Decision, DecisionAction, DecisionStatus, Event, Polygon, SlotCount, id};
use op_geo::shape::{SERVER_SLACK_M, fit_gap, min_gap_m};
use op_geo::{CollarLimits, Projection};
use op_protocol::wire_time::trunc_secs;
use sqlx::Row as _;
use sqlx::sqlite::SqliteRow;

pub use op_core::schedule::{BackFence, Cadence, LATE_AFTER_MIN, MoveState, SCHEDULE, Schedule, ScheduleStatus, ScheduledMove};

use crate::boundary::send_boundary;
use crate::db::{self, HerdBoundaries};
use crate::shape::{self, CollarCaps};
use crate::{SendOpts, slots};

/// Occurrence `n` of a schedule's cadence in UTC; occurrence 0 is its
/// `starts_at`. From the caller that knows the farm's time zone.
pub type Occurrences<'a> = &'a (dyn Fn(u32) -> DateTime<Utc> + Send + Sync);

/// Collars that haven't reported for this long don't size the staging.
pub const REPORTING_WITHIN: Duration = Duration::minutes(20);
/// A lone immediate boundary counts as settled after this.
pub const SETTLE_AFTER: Duration = Duration::seconds(60);
/// Most strips a schedule takes.
pub const MAX_STRIPS: usize = 200;
const TICK: std::time::Duration = std::time::Duration::from_secs(5);

// ---- rows ---------------------------------------------------------------------------------

fn schedule_from_row(r: &SqliteRow) -> anyhow::Result<Schedule> {
    Ok(Schedule {
        id: r.try_get("id")?,
        herd_id: r.try_get("herd_id")?,
        paddock_id: r.try_get("paddock_id")?,
        layout_id: r.try_get("layout_id")?,
        strips: serde_json::from_str(&r.try_get::<String, _>("strips")?)?,
        next_index: r.try_get::<i64, _>("next_index")? as u32,
        cadence: serde_json::from_str(&r.try_get::<String, _>("cadence")?)?,
        starts_at: from_db(&r.try_get::<String, _>("starts_at")?)?,
        back_fence: serde_json::from_str(&r.try_get::<String, _>("back_fence")?)?,
        status: ScheduleStatus::from_db(&r.try_get::<String, _>("status")?)?,
        created_by: serde_json::from_str(&r.try_get::<String, _>("created_by")?)?,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
        updated_at: from_db(&r.try_get::<String, _>("updated_at")?)?,
        planned_end: opt_from_db(r.try_get("planned_end")?)?,
        ended_at: opt_from_db(r.try_get("ended_at")?)?,
    })
}

/// One `schedule_moves` row. `id` 0 is a row not written yet.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    id: i64,
    strip: u32,
    step: u32,
    occurrence: Option<u32>,
    at: DateTime<Utc>,
    geometry: Polygon,
    state: MoveState,
    skipped: Option<String>,
    boundary_id: Option<String>,
    version: Option<u32>,
    applied_at: Option<DateTime<Utc>>,
}

impl Row {
    fn from_row(r: &SqliteRow) -> anyhow::Result<Self> {
        Ok(Self {
            id: r.try_get("id")?,
            strip: r.try_get::<i64, _>("strip")? as u32,
            step: r.try_get::<i64, _>("step")? as u32,
            occurrence: r.try_get::<Option<i64>, _>("occurrence")?.map(|o| o as u32),
            at: from_db(&r.try_get::<String, _>("at")?)?,
            geometry: serde_json::from_str(&r.try_get::<String, _>("geometry")?)?,
            state: MoveState::from_db(&r.try_get::<String, _>("state")?)?,
            skipped: r.try_get("skipped")?,
            boundary_id: r.try_get("boundary_id")?,
            version: r.try_get::<Option<i64>, _>("boundary_version")?.map(|v| v as u32),
            applied_at: opt_from_db(r.try_get("applied_at")?)?,
        })
    }

    fn planned(strip: u32, step: u32, occurrence: Option<u32>, at: DateTime<Utc>, geometry: Polygon) -> Self {
        Self { id: 0, strip, step, occurrence, at, geometry, state: MoveState::Planned, skipped: None, boundary_id: None, version: None, applied_at: None }
    }

    fn pending(&self) -> bool {
        matches!(self.state, MoveState::Planned | MoveState::Staged)
    }

    fn open(&self) -> bool {
        self.step == 0 && self.skipped.as_deref() != Some("held")
    }

    fn unstage(&mut self) {
        if self.state == MoveState::Staged {
            self.state = MoveState::Planned;
            self.boundary_id = None;
            self.version = None;
        }
    }

    fn skip(&mut self, why: &str) {
        self.state = MoveState::Skipped;
        self.skipped = Some(why.to_owned());
        self.boundary_id = None;
        self.version = None;
    }

    fn to_move(&self, schedule_id: &str) -> ScheduledMove {
        ScheduledMove {
            schedule_id: schedule_id.to_owned(),
            index: self.strip,
            step: self.step,
            at: self.at,
            geometry: self.geometry.clone(),
            boundary_version: self.version,
            skipped: self.skipped.clone(),
            state: self.state,
            applied_at: self.applied_at,
        }
    }
}

async fn load_rows(ctx: &Ctx, schedule_id: &str) -> anyhow::Result<Vec<Row>> {
    let rows = sqlx::query("SELECT * FROM schedule_moves WHERE schedule_id = ? ORDER BY at, strip, step, id").bind(schedule_id).fetch_all(ctx.db()).await?;
    rows.iter().map(Row::from_row).collect()
}

async fn write_rows(tx: &mut sqlx::SqliteConnection, schedule_id: &str, rows: &mut [Row], at: DateTime<Utc>) -> anyhow::Result<()> {
    for r in rows.iter_mut() {
        let geometry = serde_json::to_string(&r.geometry)?;
        if r.id == 0 {
            let (id,): (i64,) = sqlx::query_as(
                "INSERT INTO schedule_moves (schedule_id, strip, step, occurrence, at, geometry, state, skipped, boundary_id, boundary_version, applied_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
            )
            .bind(schedule_id)
            .bind(r.strip as i64)
            .bind(r.step as i64)
            .bind(r.occurrence.map(i64::from))
            .bind(to_db(&r.at))
            .bind(geometry)
            .bind(r.state.as_db())
            .bind(&r.skipped)
            .bind(&r.boundary_id)
            .bind(r.version.map(i64::from))
            .bind(r.applied_at.as_ref().map(to_db))
            .bind(to_db(&at))
            .fetch_one(&mut *tx)
            .await?;
            r.id = id;
        } else {
            sqlx::query(
                "UPDATE schedule_moves SET strip = ?, step = ?, occurrence = ?, at = ?, geometry = ?, state = ?, skipped = ?, boundary_id = ?,
                     boundary_version = ?, applied_at = ?, updated_at = ? WHERE id = ?",
            )
            .bind(r.strip as i64)
            .bind(r.step as i64)
            .bind(r.occurrence.map(i64::from))
            .bind(to_db(&r.at))
            .bind(geometry)
            .bind(r.state.as_db())
            .bind(&r.skipped)
            .bind(&r.boundary_id)
            .bind(r.version.map(i64::from))
            .bind(r.applied_at.as_ref().map(to_db))
            .bind(to_db(&at))
            .bind(r.id)
            .execute(&mut *tx)
            .await?;
        }
    }
    Ok(())
}

pub async fn get(ctx: &Ctx, id: &str) -> anyhow::Result<Option<Schedule>> {
    let row = sqlx::query("SELECT * FROM schedules WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    row.map(|r| schedule_from_row(&r)).transpose()
}

/// Newest first, of one herd or all.
pub async fn list(ctx: &Ctx, herd_id: Option<&str>) -> anyhow::Result<Vec<Schedule>> {
    let rows =
        sqlx::query("SELECT * FROM schedules WHERE (?1 IS NULL OR herd_id = ?1) ORDER BY created_at DESC, id DESC").bind(herd_id).fetch_all(ctx.db()).await?;
    rows.iter().map(schedule_from_row).collect()
}

/// The herd's schedule that is running (active or paused), if any.
pub async fn running(ctx: &Ctx, herd_id: &str) -> anyhow::Result<Option<Schedule>> {
    let row = sqlx::query("SELECT * FROM schedules WHERE herd_id = ? AND status != 'done'").bind(herd_id).fetch_optional(ctx.db()).await?;
    row.map(|r| schedule_from_row(&r)).transpose()
}

/// Every open and back-fence step, in time order: history, the held and
/// skipped ones, and the queue.
pub async fn moves(ctx: &Ctx, schedule_id: &str) -> anyhow::Result<Vec<ScheduledMove>> {
    Ok(load_rows(ctx, schedule_id).await?.iter().map(|r| r.to_move(schedule_id)).collect())
}

/// The next open still to happen and how far its boundary has reached the
/// herd's collars (once staged).
pub async fn next_open(ctx: &Ctx, s: &Schedule) -> anyhow::Result<Option<(ScheduledMove, Option<SlotCount>)>> {
    let rows = load_rows(ctx, &s.id).await?;
    let Some(r) = rows.iter().find(|r| r.pending() && r.open()) else { return Ok(None) };
    let count = match r.version {
        Some(v) => slots::counts(ctx.db(), &s.herd_id, &[(v, Some(r.at))]).await?.into_iter().next(),
        None => None,
    };
    Ok(Some((r.to_move(&s.id), count)))
}

/// The strip the herd is on: the last open that took effect.
pub async fn current_strip(ctx: &Ctx, s: &Schedule) -> anyhow::Result<Option<u32>> {
    let rows = load_rows(ctx, &s.id).await?;
    Ok(rows.iter().rev().find(|r| r.step == 0 && r.state == MoveState::Done).map(|r| r.strip))
}

// ---- shapes ---------------------------------------------------------------------------------

/// Polygons in local metres about one origin, for the unions and cuts.
struct Local {
    proj: Projection,
}

impl Local {
    fn new(p: &Polygon) -> Option<Self> {
        Some(Self { proj: Projection::new(*p.outer_ring().first()?) })
    }

    fn to_geo(&self, p: &Polygon) -> Option<GeoPolygon> {
        let outer = p.outer_ring();
        if outer.len() < 3 {
            return None;
        }
        let ring = |r: &[[f64; 2]]| LineString::new(r.iter().map(|q| self.proj.forward(*q)).map(|[x, y]| Coord { x, y }).collect());
        // Counter-clockwise outside, clockwise holes: the boolean ops leave a
        // shared edge between two clockwise strips instead of joining them.
        use geo::orient::{Direction, Orient};
        Some(GeoPolygon::new(ring(&outer), p.holes().filter(|h| h.len() >= 3).map(|h| ring(&h)).collect()).orient(Direction::Default))
    }

    fn back(&self, g: &GeoPolygon) -> Option<Polygon> {
        let ring = |ls: &LineString| -> Option<Vec<[f64; 2]>> {
            let pts = simplify(&ls.0.iter().map(|c| [c.x, c.y]).collect::<Vec<_>>());
            let out = op_geo::clean_ring(&self.proj.inverse_ring(&pts));
            (out.len() >= 3).then_some(out)
        };
        Some(Polygon::from_rings(ring(g.exterior())?, g.interiors().iter().filter_map(ring)))
    }

    /// The one polygon a union makes, or `None` when it falls apart (pieces
    /// that don't touch; slivers under a square metre don't count).
    fn one(&self, m: &MultiPolygon) -> Option<Polygon> {
        let mut parts: Vec<&GeoPolygon> = m.0.iter().filter(|p| p.unsigned_area() >= 1.0).collect();
        if parts.len() != 1 {
            return None;
        }
        self.back(parts.remove(0))
    }
}

/// Drop repeated points and points within a centimetre of the straight line
/// between their neighbours (unions leave them along shared edges).
fn simplify(ring: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut pts = op_geo::clean_ring(ring);
    let mut changed = true;
    while changed && pts.len() > 3 {
        changed = false;
        let n = pts.len();
        for i in 0..n {
            let (a, b, c) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
            let cross = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
            let len = ((c[0] - a[0]).powi(2) + (c[1] - a[1]).powi(2)).sqrt();
            let between =
                (b[0] - a[0]) * (c[0] - a[0]) + (b[1] - a[1]) * (c[1] - a[1]) >= 0.0 && (b[0] - c[0]) * (a[0] - c[0]) + (b[1] - c[1]) * (a[1] - c[1]) >= 0.0;
            let dup = (b[0] - a[0]).hypot(b[1] - a[1]) < 1e-3;
            if dup || (len > 0.0 && (cross / len).abs() < 0.01 && between) {
                pts.remove(i);
                changed = true;
                break;
            }
        }
    }
    pts
}

/// Strips in one local frame, their corners snapped together: two strips
/// cut along the same line carry the same corners, but each went through its
/// own rounding to 7 decimals, so a shared corner can differ by a centimetre
/// and leave a sliver between them that no union closes.
struct Snapped {
    local: Local,
    polys: Vec<GeoPolygon>,
}

/// Corners closer than this are one corner.
const SNAP_M: f64 = 0.03;

fn snapped(strips: &[Polygon]) -> Option<Snapped> {
    let local = Local::new(strips.first()?)?;
    let mut seen: Vec<Coord> = Vec::new();
    let mut snap = |c: Coord| -> Coord {
        match seen.iter().find(|s| (s.x - c.x).hypot(s.y - c.y) < SNAP_M) {
            Some(s) => *s,
            None => {
                seen.push(c);
                c
            }
        }
    };
    let mut polys = Vec::with_capacity(strips.len());
    for s in strips {
        let g = local.to_geo(s)?;
        let ring = |ls: &LineString, snap: &mut dyn FnMut(Coord) -> Coord| LineString::new(ls.0.iter().map(|c| snap(*c)).collect());
        let ext = ring(g.exterior(), &mut snap);
        let holes = g.interiors().iter().map(|h| ring(h, &mut snap)).collect();
        polys.push(GeoPolygon::new(ext, holes));
    }
    Some(Snapped { local, polys })
}

/// Everything in one boolean op: each op puts its output on a grid of its
/// own, so a union built up one strip at a time leaves hairline gaps along
/// the edges it shares with the next strip.
fn union_geo(polys: &[GeoPolygon]) -> MultiPolygon {
    MultiPolygon(polys.to_vec()).union(&MultiPolygon(vec![]))
}

/// One polygon covering these strips, or `None` when they don't join up.
pub fn union_of(strips: &[Polygon]) -> Option<Polygon> {
    let s = snapped(strips)?;
    s.local.one(&union_geo(&s.polys))
}

/// Where a back fence stands part way through closing: `region` (the whole
/// open ground) less the far `1 - keep` of `old`'s depth, measured from the
/// side away from `toward`. One intersection with a band just bigger than
/// the region, so the result keeps the region's own edges.
fn part_toward(region: &GeoPolygon, old: &GeoPolygon, from: [f64; 2], toward: [f64; 2], keep: f64) -> MultiPolygon {
    let (dx, dy) = (toward[0] - from[0], toward[1] - from[1]);
    let len = dx.hypot(dy).max(1e-9);
    let (ux, uy) = (dx / len, dy / len);
    let (px, py) = (-uy, ux);
    let span =
        |g: &GeoPolygon, f: &dyn Fn(&Coord) -> f64| g.exterior().0.iter().map(f).fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(v), b.max(v)));
    let along = |c: &Coord| c.x * ux + c.y * uy;
    let across = |c: &Coord| c.x * px + c.y * py;
    let (lo, hi) = span(old, &along);
    let cut = hi - keep.clamp(0.0, 1.0) * (hi - lo);
    let (_, far) = span(region, &along);
    let (a0, a1) = span(region, &across);
    let (a0, a1, far) = (a0 - 10.0, a1 + 10.0, far + 10.0);
    let at = |t: f64, s: f64| (ux * t + px * s, uy * t + py * s);
    let band = GeoPolygon::new(LineString::from(vec![at(cut, a0), at(far, a0), at(far, a1), at(cut, a1), at(cut, a0)]), vec![]);
    MultiPolygon(vec![region.clone()]).intersection(&MultiPolygon(vec![band]))
}

/// What opening strip `k` stages, then each back-fence close step, when the
/// strip opened before it is `prev` (the one the herd is on).
///
/// Without a back fence: `strips[0 ..= k]`. With one: `strips[prev-lag ..= k]`
/// (the animals keep the ground they stand on), then `close_steps` steps that
/// sweep the old ground from the far side, the last being `strips[k-lag ..= k]`.
/// Skipped strips between `prev` and `k` are part of the old ground.
pub fn stage_shapes(strips: &[Polygon], prev: Option<usize>, k: usize, bf: &BackFence) -> Result<(Polygon, Vec<Polygon>), String> {
    let n = strips.len();
    if k >= n {
        return Err(format!("There is no strip {}.", k + 1));
    }
    let apart = |a: usize, b: usize| {
        if a == b {
            format!("Strip {} can't go to the collars as one boundary.", a + 1)
        } else {
            format!("Strips {} to {} don't join into one boundary.", a + 1, b + 1)
        }
    };
    if !bf.enabled {
        let open = union_of(&strips[..=k]).ok_or_else(|| apart(0, k))?;
        return Ok((open, vec![]));
    }
    let lag = bf.lag_strips as usize;
    let start = prev.map_or(k, |p| p.min(k).saturating_sub(lag));
    let end = k.saturating_sub(lag);
    // Every shape below comes from the same snapped strips.
    let sn = snapped(&strips[start..=k]).ok_or_else(|| apart(start, k))?;
    let at = |i: usize| i - start;
    let whole = union_geo(&sn.polys);
    let open = sn.local.one(&whole).ok_or_else(|| apart(start, k))?;
    if start >= end {
        return Ok((open, vec![]));
    }
    let last = sn.local.one(&union_geo(&sn.polys[at(end)..])).ok_or_else(|| apart(end, k))?;
    let one_of = |m: &MultiPolygon| {
        let big: Vec<&GeoPolygon> = m.0.iter().filter(|p| p.unsigned_area() >= 1.0).collect();
        (big.len() == 1).then(|| big[0].clone())
    };
    let (Some(region), Some(old)) = (one_of(&whole), one_of(&union_geo(&sn.polys[..at(end)]))) else { return Err(apart(start, end - 1)) };
    use geo::Centroid;
    let (Some(from), Some(toward)) = (old.centroid(), sn.polys[at(end)].centroid()) else { return Err(apart(start, k)) };
    let steps = bf.close_steps.max(1);
    let mut closes = Vec::with_capacity(steps as usize);
    for s in 1..steps {
        let keep = 1.0 - s as f64 / steps as f64;
        let part = part_toward(&region, &old, [from.x(), from.y()], [toward.x(), toward.y()], keep);
        closes.push(sn.local.one(&part).ok_or_else(|| format!("Strip {}'s back fence can't close in {steps} steps. Close it in one.", k + 1))?);
    }
    closes.push(last);
    Ok((open, closes))
}

/// The rows opening strip `k` at `at`, with its back-fence steps.
fn plan_strip(s: &Schedule, prev: Option<usize>, k: usize, at: DateTime<Utc>, occurrence: Option<u32>) -> Result<Vec<Row>, String> {
    let (open, closes) = stage_shapes(&s.strips, prev, k, &s.back_fence)?;
    let at = trunc_secs(at);
    let mut rows = vec![Row::planned(k as u32, 0, occurrence, at, open)];
    let bf = &s.back_fence;
    for (i, g) in closes.into_iter().enumerate() {
        let t = at + Duration::minutes(i64::from(bf.close_after_min) + i64::from(bf.close_every_min) * i as i64);
        rows.push(Row::planned(k as u32, i as u32 + 1, None, t, g));
    }
    Ok(rows)
}

/// Every open must come after the back fence before it has closed.
fn check_order(rows: &[Row]) -> Result<(), String> {
    let mut pending: Vec<&Row> = rows.iter().filter(|r| r.pending()).collect();
    pending.sort_by_key(|r| (r.at, r.strip, r.step));
    for w in pending.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a.strip != b.strip && (b.at <= a.at || b.strip < a.strip) {
            let (first, then) = (a.strip.min(b.strip), a.strip.max(b.strip));
            return Err(format!("Strip {} would open before strip {}'s back fence has closed. Give the back fence less time.", then + 1, first + 1));
        }
        if b.at <= a.at {
            return Err(format!("Two moves fall at the same time ({}).", to_db(&b.at)));
        }
    }
    Ok(())
}

// ---- making a schedule ----------------------------------------------------------------------

/// What `POST /api/schedules` hands over, with times already worked out.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    pub herd_id: String,
    pub paddock_id: String,
    pub layout_id: Option<String>,
    /// Copied into the schedule.
    pub strips: Vec<Polygon>,
    /// The first strip to open; `None` = the strip after the one the herd's
    /// boundary covers now.
    pub next_index: Option<u32>,
    pub cadence: Cadence,
    /// The first open (occurrence 0).
    pub starts_at: DateTime<Utc>,
    pub back_fence: BackFence,
    pub created_by: Actor,
}

/// The strip after the one the herd's boundary covers now: the strips more
/// than half inside it are the ground the herd has. `None` when it covers
/// none or several that don't make one run ending on a strip.
pub fn strip_after(strips: &[Polygon], active: &Polygon) -> Option<usize> {
    let local = Local::new(active)?;
    let a = MultiPolygon(vec![local.to_geo(active)?]);
    let covered: Vec<usize> = strips
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let g = local.to_geo(s)?;
            let area = g.unsigned_area();
            (area > 0.0 && MultiPolygon(vec![g]).intersection(&a).unsigned_area() > 0.5 * area).then_some(i)
        })
        .collect();
    let last = *covered.last()?;
    (covered.len() <= 3 && last + 1 < strips.len()).then_some(last + 1)
}

/// What [`create`] would make, checked the same way, without storing it.
pub async fn preview(ctx: &Ctx, n: NewSchedule, occ: Occurrences<'_>) -> ApiResult<(Schedule, Vec<ScheduledMove>)> {
    let (s, rows) = planned(ctx, n, occ).await?;
    let moves = rows.iter().map(|r| r.to_move(&s.id)).collect();
    Ok((s, moves))
}

/// Record a schedule and its moves, then stage what the collars have room
/// for. 400 for strips that don't make boundaries, times that don't work, or
/// a start already past; 409 when the herd already runs one or has no
/// boundary yet (a schedule starts from where the herd is fenced now).
pub async fn create(ctx: &Ctx, n: NewSchedule, occ: Occurrences<'_>) -> ApiResult<Schedule> {
    let (s, mut rows) = planned(ctx, n, occ).await?;
    let at = s.created_at;
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let inserted = sqlx::query(
        "INSERT INTO schedules (id, herd_id, paddock_id, layout_id, strips, next_index, cadence, starts_at, back_fence, status, planned_end, created_by, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&s.id)
    .bind(&s.herd_id)
    .bind(&s.paddock_id)
    .bind(&s.layout_id)
    .bind(serde_json::to_string(&s.strips).map_err(anyhow::Error::from)?)
    .bind(s.next_index as i64)
    .bind(serde_json::to_string(&s.cadence).map_err(anyhow::Error::from)?)
    .bind(to_db(&s.starts_at))
    .bind(serde_json::to_string(&s.back_fence).map_err(anyhow::Error::from)?)
    .bind(s.status.as_db())
    .bind(s.planned_end.as_ref().map(to_db))
    .bind(serde_json::to_string(&s.created_by).map_err(anyhow::Error::from)?)
    .bind(to_db(&s.created_at))
    .bind(to_db(&s.updated_at))
    .execute(&mut *tx)
    .await;
    if let Err(e) = inserted {
        if e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
            return Err(ApiError::conflict("This herd already has a schedule. End it first."));
        }
        return Err(e.into());
    }
    write_rows(&mut tx, &s.id, &mut rows, at).await?;
    tx.commit().await?;
    tracing::info!(schedule = %s.id, herd = %s.herd_id, strips = s.strips.len(), next = s.next_index, moves = rows.len(), "schedule made");
    ctx.publish(Event::Schedule { schedule: s.clone() });
    drive(ctx, &s.id, now()).await?;
    Ok(get(ctx, &s.id).await?.unwrap_or(s))
}

async fn planned(ctx: &Ctx, n: NewSchedule, occ: Occurrences<'_>) -> ApiResult<(Schedule, Vec<Row>)> {
    if ctx.store().get_herd(&n.herd_id).await?.is_none() {
        return Err(ApiError::not_found("No such herd."));
    }
    if n.strips.is_empty() || n.strips.len() > MAX_STRIPS {
        return Err(ApiError::bad_request(format!("A schedule takes 1 to {MAX_STRIPS} strips.")));
    }
    let mut strips = Vec::with_capacity(n.strips.len());
    for (i, s) in n.strips.iter().enumerate() {
        strips.push(s.validated().map_err(|e| ApiError::bad_request(format!("Strip {}: {e}", i + 1)))?);
    }
    n.cadence.check().map_err(ApiError::bad_request)?;
    n.back_fence.check().map_err(ApiError::bad_request)?;
    let at = now();
    let starts_at = trunc_secs(n.starts_at);
    if starts_at <= at {
        return Err(ApiError::bad_request("Pick a start time that hasn't passed."));
    }
    if running(ctx, &n.herd_id).await?.is_some() {
        return Err(ApiError::conflict("This herd already has a schedule. End it first."));
    }
    let split = db::herd_boundaries(ctx.db(), &n.herd_id, at).await?;
    let Some(active) = split.active.as_ref() else {
        return Err(ApiError::conflict("Send the herd a boundary first: a schedule starts from where it is fenced now."));
    };
    let next = match n.next_index {
        Some(i) => i as usize,
        None => strip_after(&strips, &active.geometry).unwrap_or(0),
    };
    if next >= strips.len() {
        return Err(ApiError::bad_request(format!("There is no strip {}.", next + 1)));
    }
    let mut s = Schedule {
        id: id::new_id(SCHEDULE),
        herd_id: n.herd_id,
        paddock_id: n.paddock_id,
        layout_id: n.layout_id,
        strips,
        next_index: next as u32,
        cadence: n.cadence,
        starts_at,
        back_fence: n.back_fence,
        status: ScheduleStatus::Active,
        created_by: n.created_by,
        created_at: at,
        updated_at: at,
        planned_end: None,
        ended_at: None,
    };
    let first = |o: u32| if o == 0 { starts_at } else { trunc_secs(occ(o)) };
    let mut rows = Vec::new();
    for (o, k) in (next..s.strips.len()).enumerate() {
        let prev = k.checked_sub(1);
        rows.extend(plan_strip(&s, prev, k, first(o as u32), Some(o as u32)).map_err(ApiError::bad_request)?);
    }
    check_order(&rows).map_err(ApiError::bad_request)?;
    s.planned_end = Some(first((s.strips.len() - next) as u32));
    // Every move must go to the collars as it stands (checked as it will be sent).
    for r in &rows {
        let opts = SendOpts { effective_at: Some(r.at), ..Default::default() };
        shape::prepare(ctx, &s.herd_id, &r.geometry, &opts).await.map_err(|e| {
            let what = if r.step == 0 { format!("Strip {}", r.strip + 1) } else { format!("Strip {}'s back fence step {}", r.strip + 1, r.step) };
            ApiError::bad_request(format!("{what} can't go to the collars: {}", e.message))
        })?;
    }
    Ok((s, rows))
}

// ---- edits ------------------------------------------------------------------------------------

static LOCKS: LazyLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);

async fn lock(schedule_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let m = LOCKS.lock().unwrap_or_else(|e| e.into_inner()).entry(schedule_id.to_owned()).or_default().clone();
    m.lock_owned().await
}

/// A schedule and its rows, loaded for an edit.
struct Plan {
    s: Schedule,
    rows: Vec<Row>,
    /// Rows to delete (back-fence steps planned again).
    gone: Vec<i64>,
    /// This schedule's versions staged alive when loaded, at their times.
    staged: Vec<(u32, DateTime<Utc>)>,
}

impl Plan {
    fn sort(&mut self) {
        self.rows.sort_by_key(|r| (r.at, r.strip, r.step, r.id));
    }

    /// The open of `strip` still to happen.
    fn pending_open(&self, strip: u32) -> Option<usize> {
        self.rows.iter().position(|r| r.strip == strip && r.open() && r.pending())
    }

    fn first_pending_open(&self) -> Option<usize> {
        self.rows.iter().enumerate().filter(|(_, r)| r.open() && r.pending()).min_by_key(|(_, r)| r.at).map(|(i, _)| i)
    }

    /// Pending opens in time order.
    fn pending_opens(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.rows.len()).filter(|&i| self.rows[i].open() && self.rows[i].pending()).collect();
        v.sort_by_key(|&i| self.rows[i].at);
        v
    }

    /// Pending close steps of `strip`.
    fn closes(&self, strip: u32) -> Vec<usize> {
        (0..self.rows.len()).filter(|&i| self.rows[i].strip == strip && self.rows[i].step > 0 && self.rows[i].pending()).collect()
    }

    /// The strip opened last (done), else the one before the first to open.
    fn current(&self) -> Option<u32> {
        let done = self.rows.iter().filter(|r| r.open() && r.state == MoveState::Done).max_by_key(|r| r.at).map(|r| r.strip);
        done.or_else(|| self.s.next_index.checked_sub(1))
    }

    /// The strip opened before `before` (done or still to open, not skipped);
    /// before the schedule's first open, the strip the herd was on.
    fn prev_open_strip(&self, before: DateTime<Utc>) -> Option<usize> {
        let opened = self.rows.iter().filter(|r| r.open() && r.state != MoveState::Skipped && r.at < before).max_by_key(|r| r.at).map(|r| r.strip as usize);
        opened.or_else(|| self.rows.iter().filter(|r| r.open()).map(|r| r.strip as usize).min().and_then(|f| f.checked_sub(1)))
    }

    /// Re-plan strip `k`'s open shape and back-fence steps from its open (its
    /// old ground changes when a strip before it is skipped).
    fn replan(&mut self, k: u32) -> Result<(), String> {
        let Some(oi) = self.pending_open(k) else { return Ok(()) };
        let open_at = self.rows[oi].at;
        let prev = self.prev_open_strip(open_at);
        let planned = plan_strip(&self.s, prev, k as usize, open_at, self.rows[oi].occurrence)?;
        self.rows[oi].geometry = planned[0].geometry.clone();
        for i in self.closes(k).into_iter().rev() {
            let r = self.rows.remove(i);
            if r.id != 0 {
                self.gone.push(r.id);
            }
        }
        self.rows.extend(planned.into_iter().skip(1));
        Ok(())
    }

    /// Move strip `k`'s open to `at`, its pending back-fence steps with it.
    fn move_open(&mut self, oi: usize, at: DateTime<Utc>) {
        let at = trunc_secs(at);
        let delta = at - self.rows[oi].at;
        let k = self.rows[oi].strip;
        self.rows[oi].at = at;
        for i in self.closes(k) {
            self.rows[i].at += delta;
        }
    }
}

async fn load_plan(ctx: &Ctx, schedule_id: &str) -> ApiResult<Plan> {
    let s = get(ctx, schedule_id).await?.ok_or_else(|| ApiError::not_found("No such schedule."))?;
    let rows = load_rows(ctx, schedule_id).await?;
    let staged = alive_staged(ctx, &s.herd_id, &rows, now()).await?;
    Ok(Plan { s, rows, gone: vec![], staged })
}

fn require_running(s: &Schedule) -> ApiResult<()> {
    if s.status == ScheduleStatus::Done {
        return Err(ApiError::conflict("This schedule has ended."));
    }
    Ok(())
}

/// The versions this schedule has staged that are still alive at `now`.
async fn alive_staged(ctx: &Ctx, herd_id: &str, rows: &[Row], now: DateTime<Utc>) -> anyhow::Result<Vec<(u32, DateTime<Utc>)>> {
    let split = db::herd_boundaries(ctx.db(), herd_id, now).await?;
    let alive: HashSet<u32> = split.staged.iter().map(|b| b.version).collect();
    Ok(rows.iter().filter_map(|r| r.version.filter(|v| r.state == MoveState::Staged && alive.contains(v)).map(|v| (v, r.at))).collect())
}

/// The herd's current strip again as a new immediate version, so collars
/// drop what is staged above it. The shape is the one planned for the strip
/// the herd is on when that is what's in effect (it goes through `prepare`
/// again), else the boundary in effect as it is.
async fn reissue(ctx: &Ctx, p: &Plan) -> ApiResult<Option<Boundary>> {
    let split = db::herd_boundaries(ctx.db(), &p.s.herd_id, now()).await?;
    let Some(a) = split.active else { return Ok(None) };
    let geometry =
        p.rows.iter().find(|r| r.version == Some(a.version) && r.state == MoveState::Done).map_or_else(|| a.geometry.clone(), |r| r.geometry.clone());
    let opts = SendOpts { warn_m: Some(a.warn_m), hysteresis_m: Some(a.hysteresis_m), effective_at: None };
    let b = send_boundary(ctx, &p.s.herd_id, geometry, opts, &p.s.id).await.map_err(|e| ApiError::bad_request(e.to_string()))?;
    tracing::info!(schedule = %p.s.id, herd = %p.s.herd_id, version = b.version, "current strip reissued");
    Ok(Some(b))
}

/// Write an edited plan: rows, schedule fields; reissue first when the edit
/// changed moves already staged at or after `from`. Then stage again.
async fn finish_edit(ctx: &Ctx, mut p: Plan, from: Option<DateTime<Utc>>) -> ApiResult<Schedule> {
    let at = now();
    check_order(&p.rows).map_err(ApiError::bad_request)?;
    if let Some(from) = from {
        if p.staged.iter().any(|(_, t)| *t >= from) {
            reissue(ctx, &p).await?;
            for r in &mut p.rows {
                r.unstage();
            }
        }
    }
    let gone = std::mem::take(&mut p.gone);
    p.sort();
    p.s.next_index = next_index(&p.s, &p.rows);
    p.s.updated_at = at;
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    for id in gone {
        sqlx::query("DELETE FROM schedule_moves WHERE id = ?").bind(id).execute(&mut *tx).await?;
    }
    write_rows(&mut tx, &p.s.id, &mut p.rows, at).await?;
    write_schedule(&mut tx, &p.s).await?;
    tx.commit().await?;
    ctx.publish(Event::Schedule { schedule: p.s.clone() });
    drive_locked(ctx, &p.s.id, now()).await?;
    Ok(get(ctx, &p.s.id).await?.unwrap_or(p.s))
}

async fn write_schedule(tx: &mut sqlx::SqliteConnection, s: &Schedule) -> anyhow::Result<()> {
    sqlx::query("UPDATE schedules SET next_index = ?, status = ?, updated_at = ?, ended_at = ? WHERE id = ?")
        .bind(s.next_index as i64)
        .bind(s.status.as_db())
        .bind(to_db(&s.updated_at))
        .bind(s.ended_at.as_ref().map(to_db))
        .bind(&s.id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

fn next_index(s: &Schedule, rows: &[Row]) -> u32 {
    rows.iter().filter(|r| r.open() && r.pending()).min_by_key(|r| r.at).map_or(s.strips.len() as u32, |r| r.strip)
}

/// Skip strip `strip`: it doesn't open; the strips after it move up one
/// occurrence each (the next one opens when this one would have, and so on).
/// The herd walks through the skipped ground on the next open.
pub async fn skip(ctx: &Ctx, schedule_id: &str, strip: u32) -> ApiResult<Schedule> {
    let _g = lock(schedule_id).await;
    let mut p = load_plan(ctx, schedule_id).await?;
    require_running(&p.s)?;
    let oi = p.pending_open(strip).ok_or_else(|| ApiError::conflict(format!("Strip {} isn't waiting to open.", strip + 1)))?;
    let from = p.rows[oi].at;
    let opens = p.pending_opens();
    let later: Vec<usize> = opens.iter().copied().filter(|&i| p.rows[i].at > from).collect();
    // Each later open takes the time and place in the cadence of the one before it.
    let mut slot = (p.rows[oi].at, p.rows[oi].occurrence);
    for &i in &later {
        let here = (p.rows[i].at, p.rows[i].occurrence);
        p.move_open(i, slot.0);
        p.rows[i].occurrence = slot.1;
        slot = here;
    }
    for i in p.closes(strip) {
        p.rows[i].skip("skipped");
    }
    p.rows[oi].skip("skipped");
    if let Some(&first) = later.first() {
        let k = p.rows[first].strip;
        p.replan(k).map_err(ApiError::bad_request)?;
    }
    tracing::info!(schedule = %schedule_id, strip, "strip skipped");
    finish_edit(ctx, p, Some(from)).await
}

/// Hold: the herd stays on today's strip. The next open and everything after
/// it move one occurrence later; a `held` row keeps the place where the next
/// open was. Collars drop the staged open (the current strip is reissued).
pub async fn hold(ctx: &Ctx, schedule_id: &str, occ: Occurrences<'_>) -> ApiResult<Schedule> {
    let _g = lock(schedule_id).await;
    let mut p = load_plan(ctx, schedule_id).await?;
    require_running(&p.s)?;
    let oi = p.first_pending_open().ok_or_else(|| ApiError::conflict("Every strip has opened; there is nothing to hold."))?;
    let from = p.rows[oi].at;
    let current = p.current().unwrap_or(p.rows[oi].strip);
    let here = db::herd_boundaries(ctx.db(), &p.s.herd_id, now()).await?.active.map(|b| b.geometry);
    let mut held = Row::planned(current, 0, p.rows[oi].occurrence, from, here.unwrap_or_else(|| p.rows[oi].geometry.clone()));
    held.skip("held");
    for i in p.pending_opens() {
        let (at, o) = (p.rows[i].at, p.rows[i].occurrence);
        let Some(o) = o else { continue };
        let step = occ(o + 1) - occ(o);
        p.move_open(i, at + step);
        p.rows[i].occurrence = Some(o + 1);
    }
    p.rows.push(held);
    tracing::info!(schedule = %schedule_id, strip = current, "held");
    finish_edit(ctx, p, Some(from)).await
}

/// Open the next strip now, as an immediate boundary. Later opens keep their
/// times; its back-fence steps follow from now, squeezed to close before the
/// next open when they would run past it.
pub async fn move_now(ctx: &Ctx, schedule_id: &str) -> ApiResult<Schedule> {
    let _g = lock(schedule_id).await;
    let mut p = load_plan(ctx, schedule_id).await?;
    require_running(&p.s)?;
    let oi = p.first_pending_open().ok_or_else(|| ApiError::conflict("Every strip has opened."))?;
    let at = trunc_secs(now());
    let k = p.rows[oi].strip;
    let next_at = p.pending_opens().into_iter().map(|i| p.rows[i].at).find(|t| *t > p.rows[oi].at);
    let b =
        send_boundary(ctx, &p.s.herd_id, p.rows[oi].geometry.clone(), SendOpts::default(), &p.s.id).await.map_err(|e| ApiError::bad_request(e.to_string()))?;
    let r = &mut p.rows[oi];
    r.at = at;
    r.state = MoveState::Done;
    r.boundary_id = Some(b.id.clone());
    r.version = Some(b.version);
    let mut closes = p.closes(k);
    closes.sort_by_key(|&i| p.rows[i].step);
    let bf = p.s.back_fence;
    let n = closes.len() as i64;
    let squeeze = next_at.filter(|next| at + Duration::minutes(i64::from(bf.span_min())) >= *next - Duration::minutes(1));
    for (j, i) in closes.into_iter().enumerate() {
        let t = match squeeze {
            // Evenly between now and a minute before the next open.
            Some(next) => at + Duration::seconds((next - Duration::minutes(1) - at).num_seconds() * (j as i64 + 1) / (n + 1)),
            None => at + Duration::minutes(i64::from(bf.close_after_min) + i64::from(bf.close_every_min) * j as i64),
        };
        p.rows[i].at = trunc_secs(t);
    }
    tracing::info!(schedule = %schedule_id, strip = k, version = b.version, "strip opened now");
    finish_edit(ctx, p, None).await
}

/// Move strip `strip`'s open to `at` (its back-fence steps with it). It must
/// still fall after the move before it and close before the next open.
pub async fn set_time(ctx: &Ctx, schedule_id: &str, strip: u32, at: DateTime<Utc>) -> ApiResult<Schedule> {
    let _g = lock(schedule_id).await;
    let mut p = load_plan(ctx, schedule_id).await?;
    require_running(&p.s)?;
    let oi = p.pending_open(strip).ok_or_else(|| ApiError::conflict(format!("Strip {} isn't waiting to open.", strip + 1)))?;
    let at = trunc_secs(at);
    if at <= now() + Duration::seconds(30) {
        return Err(ApiError::bad_request("Pick a time that hasn't passed."));
    }
    let from = p.rows[oi].at.min(at);
    p.move_open(oi, at);
    finish_edit(ctx, p, Some(from)).await
}

/// Stop staging; collars drop what is staged. Times are kept.
pub async fn pause(ctx: &Ctx, schedule_id: &str) -> ApiResult<Schedule> {
    let _g = lock(schedule_id).await;
    let mut p = load_plan(ctx, schedule_id).await?;
    if p.s.status != ScheduleStatus::Active {
        return Err(ApiError::conflict(format!("This schedule is {}.", p.s.status.as_db())));
    }
    p.s.status = ScheduleStatus::Paused;
    let from = p.rows.iter().filter(|r| r.pending()).map(|r| r.at).min();
    finish_edit(ctx, p, from.or(Some(DateTime::<Utc>::MIN_UTC))).await
}

/// Start staging again. When the next open has passed meanwhile, every open
/// still to come moves on by whole occurrences to the first one ahead.
pub async fn resume(ctx: &Ctx, schedule_id: &str, occ: Occurrences<'_>) -> ApiResult<Schedule> {
    let _g = lock(schedule_id).await;
    let mut p = load_plan(ctx, schedule_id).await?;
    if p.s.status != ScheduleStatus::Paused {
        return Err(ApiError::conflict(format!("This schedule is {}.", p.s.status.as_db())));
    }
    p.s.status = ScheduleStatus::Active;
    let soon = now() + Duration::minutes(1);
    if let Some(oi) = p.first_pending_open()
        && p.rows[oi].at <= soon
        && let Some(o) = p.rows[oi].occurrence
    {
        let shift = (1..=10_000u32).find(|d| occ(o + d) > soon).unwrap_or(1);
        for i in p.pending_opens() {
            let Some(o) = p.rows[i].occurrence else { continue };
            let at = p.rows[i].at + (occ(o + shift) - occ(o));
            p.move_open(i, at);
            p.rows[i].occurrence = Some(o + shift);
        }
    }
    finish_edit(ctx, p, None).await
}

/// End the schedule: nothing more opens. Collars drop what is staged.
pub async fn end(ctx: &Ctx, schedule_id: &str) -> ApiResult<Schedule> {
    let _g = lock(schedule_id).await;
    let p = load_plan(ctx, schedule_id).await?;
    require_running(&p.s)?;
    end_plan(ctx, p, true).await
}

async fn end_plan(ctx: &Ctx, mut p: Plan, reissue_staged: bool) -> ApiResult<Schedule> {
    let from = p.rows.iter().filter(|r| r.pending()).map(|r| r.at).min();
    for r in &mut p.rows {
        if r.pending() {
            r.skip("skipped");
        }
    }
    p.s.status = ScheduleStatus::Done;
    p.s.ended_at = Some(now());
    tracing::info!(schedule = %p.s.id, herd = %p.s.herd_id, "schedule ended");
    let from = if reissue_staged { from } else { None };
    let s = finish_edit(ctx, p, from).await?;
    Ok(s)
}

/// A MOVE to another paddock ends the herd's schedule when it applies (the
/// move's own boundaries replace the staged strips). Idempotent.
pub async fn on_decision(ctx: &Ctx, d: &Decision) -> anyhow::Result<Option<Schedule>> {
    if d.status != DecisionStatus::Applied || d.action != Some(DecisionAction::Move) {
        return Ok(None);
    }
    let Some(s) = running(ctx, &d.herd_id).await? else { return Ok(None) };
    match d.to_paddock_id.as_deref() {
        Some(p) if p != s.paddock_id => {}
        _ => return Ok(None),
    }
    let _g = lock(&s.id).await;
    let p = load_plan(ctx, &s.id).await.map_err(|e| anyhow::anyhow!(e.message))?;
    if p.s.status == ScheduleStatus::Done {
        return Ok(None);
    }
    tracing::info!(schedule = %s.id, decision = %d.id, "a move to another paddock ends the schedule");
    Ok(Some(end_plan(ctx, p, false).await.map_err(|e| anyhow::anyhow!(e.message))?))
}

// ---- how far ahead ------------------------------------------------------------------------------

/// Room on the herd's collars for this schedule's staged moves.
#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    /// Staged moves the tightest collar takes.
    pub count: usize,
    /// Per kind of collar (by the limits its boundaries are fitted to): the
    /// tightest collar's free slot bytes (`None` = no byte limit).
    pub groups: Vec<(CollarCaps, Option<usize>)>,
    /// Collars that sized it.
    pub collars: usize,
}

impl Budget {
    fn bytes_for(&self, g: &Polygon, warn_m: f64) -> Vec<usize> {
        let gap = min_gap_m(warn_m) + SERVER_SLACK_M;
        self.groups.iter().map(|(caps, _)| CollarLimits::record_bytes(fit_gap(g, &caps.fit_limits(), gap).total_vertices())).collect()
    }

    /// Whether one more move of these sizes (per group) fits after `used`.
    fn fits(&self, used: &[usize], size: &[usize]) -> bool {
        self.groups.iter().zip(used).zip(size).all(|(((_, room), u), s)| room.is_none_or(|r| u + s <= r))
    }
}

/// What the herd's reporting collars have room for, leaving out `ours` (this
/// schedule's alive staged versions, which the budget is for).
pub async fn budget(ctx: &Ctx, herd_id: &str, split: &HerdBoundaries, ours: &HashSet<u32>, at: DateTime<Utc>) -> anyhow::Result<Budget> {
    let rows = sqlx::query("SELECT id, fw, caps, limits, parked_at, last_seen FROM collars WHERE herd_id = ? ORDER BY created_at, id")
        .bind(herd_id)
        .fetch_all(ctx.db())
        .await?;
    let mut all = Vec::with_capacity(rows.len());
    for r in &rows {
        let parked: Option<String> = r.try_get("parked_at")?;
        if parked.is_some() {
            continue;
        }
        let seen = opt_from_db(r.try_get("last_seen")?)?;
        all.push((r.try_get::<String, _>("id")?, CollarCaps::from_row(r)?, seen.is_some_and(|t| at - t <= REPORTING_WITHIN)));
    }
    let reporting: Vec<&(String, CollarCaps, bool)> = all.iter().filter(|c| c.2).collect();
    let used: Vec<&(String, CollarCaps, bool)> = if reporting.is_empty() { all.iter().collect() } else { reporting };
    let alive: HashMap<u32, &Boundary> = split.active.iter().chain(&split.staged).map(|b| (b.version, b)).collect();
    let active = split.active.as_ref();
    let size = |b: &Boundary, caps: &CollarCaps| shape::record_bytes(b, caps).unwrap_or(CollarLimits::record_bytes(b.geometry.total_vertices()));
    if used.is_empty() {
        let caps = CollarCaps { fw: None, caps: vec!["holes".into(), "slots".into()], limits: shape::herd_limits(ctx, herd_id).await? };
        let base: Vec<&Boundary> = alive.values().copied().filter(|b| !ours.contains(&b.version)).collect();
        let n = base.len().max(1);
        let bytes = base.iter().map(|b| size(b, &caps)).sum::<usize>();
        let room = (caps.limits.slot_bytes > 0).then(|| caps.limits.slot_bytes.saturating_sub(bytes));
        return Ok(Budget { count: caps.limits.slots.saturating_sub(n), groups: vec![(caps, room)], collars: 0 });
    }
    // What each collar holds that stays: alive herd versions (or its copies of them) not ours.
    let held = sqlx::query(
        "SELECT s.collar_id, s.version, b.copy_of FROM collar_slots s JOIN collars c ON c.id = s.collar_id
         LEFT JOIN boundaries b ON b.version = s.version AND b.herd_id = c.herd_id
         WHERE c.herd_id = ? AND s.status != 'rejected'",
    )
    .bind(herd_id)
    .fetch_all(ctx.db())
    .await?;
    let mut holds: HashMap<String, HashSet<u32>> = HashMap::new();
    for r in &held {
        let v = r.try_get::<Option<i64>, _>("copy_of")?.unwrap_or(r.try_get::<i64, _>("version")?) as u32;
        if alive.contains_key(&v) && !ours.contains(&v) {
            holds.entry(r.try_get("collar_id")?).or_default().insert(v);
        }
    }
    let mut count = usize::MAX;
    let mut groups: Vec<(CollarCaps, Option<usize>)> = Vec::new();
    for (id, caps, _) in used {
        let mut base = holds.remove(id).unwrap_or_default();
        // It takes the herd's boundary in effect whether or not it has it yet.
        if let Some(a) = active {
            base.insert(a.version);
        }
        let n = base.len().max(1);
        let free = caps.limits.slots.saturating_sub(n);
        let room = (caps.limits.slot_bytes > 0).then(|| {
            let bytes: usize = base.iter().filter_map(|v| alive.get(v)).map(|b| size(b, caps)).sum();
            caps.limits.slot_bytes.saturating_sub(bytes)
        });
        count = count.min(free).min(caps.limits.slots.saturating_sub(1));
        let fit = caps.fit_limits();
        match groups.iter_mut().find(|(c, _)| c.fit_limits() == fit) {
            Some((_, r)) => {
                *r = match (*r, room) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            }
            None => groups.push(((*caps).clone(), room)),
        }
    }
    Ok(Budget { count, groups, collars: all.iter().filter(|c| c.2).count() })
}

// ---- the driver ---------------------------------------------------------------------------------

/// No immediate sequence is running for the herd: no sweeping move, and the
/// newest immediate boundary that isn't this schedule's belongs to a move
/// that has ended, or is a minute old.
async fn settled(ctx: &Ctx, s: &Schedule, at: DateTime<Utc>) -> anyhow::Result<bool> {
    let sweeping: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM moves WHERE herd_id = ? AND status = 'sweeping')").bind(&s.herd_id).fetch_one(ctx.db()).await?;
    if sweeping {
        return Ok(false);
    }
    let foreign: Option<String> = sqlx::query_scalar(
        "SELECT created_at FROM boundaries WHERE herd_id = ? AND collar_id IS NULL AND effective_at IS NULL AND decision_id != ? ORDER BY version DESC LIMIT 1",
    )
    .bind(&s.herd_id)
    .bind(&s.id)
    .fetch_optional(ctx.db())
    .await?;
    let Some(created) = foreign.as_deref().map(from_db).transpose()? else { return Ok(true) };
    if at - created >= SETTLE_AFTER {
        return Ok(true);
    }
    let ended: Option<String> = sqlx::query_scalar("SELECT updated_at FROM moves WHERE herd_id = ? ORDER BY started_at DESC, id DESC LIMIT 1")
        .bind(&s.herd_id)
        .fetch_optional(ctx.db())
        .await?;
    Ok(ended.as_deref().map(from_db).transpose()?.is_some_and(|u| u >= created))
}

/// Whether `version` was the herd's boundary in effect at `at`, as the
/// boundaries stored by then say.
async fn took_effect(ctx: &Ctx, herd_id: &str, version: u32, at: DateTime<Utc>) -> anyhow::Result<bool> {
    let v: Option<i64> = sqlx::query_scalar(
        "SELECT version FROM boundaries WHERE herd_id = ?1 AND collar_id IS NULL AND created_at <= ?2
             AND (effective_at IS NULL OR effective_at <= ?2) ORDER BY version DESC LIMIT 1",
    )
    .bind(herd_id)
    .bind(to_db(&at))
    .fetch_optional(ctx.db())
    .await?;
    Ok(v == Some(i64::from(version)))
}

/// One pass for one schedule at `at`: mark what took effect, apply or give up
/// on what is late, stage what the collars have room for. Returns whether
/// anything changed (a `schedule` event went out).
pub async fn drive(ctx: &Ctx, schedule_id: &str, at: DateTime<Utc>) -> anyhow::Result<bool> {
    let _g = lock(schedule_id).await;
    drive_locked(ctx, schedule_id, at).await
}

async fn drive_locked(ctx: &Ctx, schedule_id: &str, at: DateTime<Utc>) -> anyhow::Result<bool> {
    let Some(mut s) = get(ctx, schedule_id).await? else { return Ok(false) };
    let mut rows = load_rows(ctx, schedule_id).await?;
    let before = rows.clone();
    let mut changed = false;
    if s.status == ScheduleStatus::Active {
        if settled(ctx, &s, at).await? {
            settle_due(ctx, &s, &mut rows, at).await?;
            stage(ctx, &s, &mut rows, at).await?;
            if !rows.iter().any(Row::pending) {
                s.status = ScheduleStatus::Done;
                s.ended_at = Some(at);
                tracing::info!(schedule = %s.id, "every strip has opened; schedule done");
                changed = true;
            }
        } else {
            // A sequence is running: what it dropped isn't staged any more.
            let alive: HashSet<u32> = db::herd_boundaries(ctx.db(), &s.herd_id, at).await?.staged.iter().map(|b| b.version).collect();
            for r in rows.iter_mut().filter(|r| r.at > at && r.version.is_some_and(|v| !alive.contains(&v))) {
                r.unstage();
            }
        }
    }
    fill_applied(ctx, &s, &mut rows, at).await?;
    let next = next_index(&s, &rows);
    if next != s.next_index {
        s.next_index = next;
        changed = true;
    }
    let dirty: Vec<usize> = (0..rows.len()).filter(|&i| before.get(i) != Some(&rows[i])).collect();
    if dirty.is_empty() && !changed {
        return Ok(false);
    }
    s.updated_at = now();
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let mut write: Vec<Row> = dirty.iter().map(|&i| rows[i].clone()).collect();
    write_rows(&mut tx, &s.id, &mut write, s.updated_at).await?;
    write_schedule(&mut tx, &s).await?;
    tx.commit().await?;
    ctx.publish(Event::Schedule { schedule: s });
    Ok(true)
}

/// `applied_at` from the first `applied` ack of each move that took effect
/// in the last two days and has none yet.
async fn fill_applied(ctx: &Ctx, s: &Schedule, rows: &mut [Row], at: DateTime<Utc>) -> anyhow::Result<()> {
    for r in rows.iter_mut().filter(|r| r.state == MoveState::Done && r.applied_at.is_none() && at - r.at < Duration::days(2)) {
        let Some(v) = r.version else { continue };
        let first: Option<String> = sqlx::query_scalar("SELECT MIN(at) FROM acks WHERE herd_id = ? AND version = ? AND status = 'applied'")
            .bind(&s.herd_id)
            .bind(i64::from(v))
            .fetch_one(ctx.db())
            .await?;
        r.applied_at = first.as_deref().map(from_db).transpose()?;
    }
    Ok(())
}

/// Moves whose time has come: done when they took effect, else applied at
/// once when at most 30 minutes late, else `late` (with the back-fence steps
/// of an open that never happened).
async fn settle_due(ctx: &Ctx, s: &Schedule, rows: &mut [Row], at: DateTime<Utc>) -> anyhow::Result<()> {
    let mut order: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].pending() && rows[i].at <= at).collect();
    order.sort_by_key(|&i| (rows[i].at, rows[i].strip, rows[i].step));
    for i in order {
        if !rows[i].pending() {
            continue;
        }
        if let Some(v) = rows[i].version.filter(|_| rows[i].state == MoveState::Staged)
            && took_effect(ctx, &s.herd_id, v, rows[i].at).await?
        {
            rows[i].state = MoveState::Done;
            tracing::info!(schedule = %s.id, strip = rows[i].strip, step = rows[i].step, version = v, "scheduled move took effect");
            continue;
        }
        if at - rows[i].at <= Duration::minutes(LATE_AFTER_MIN) {
            match send_boundary(ctx, &s.herd_id, rows[i].geometry.clone(), SendOpts::default(), &s.id).await {
                Ok(b) => {
                    tracing::info!(schedule = %s.id, strip = rows[i].strip, step = rows[i].step, version = b.version, late_s = (at - rows[i].at).num_seconds(), "late move applied now");
                    let r = &mut rows[i];
                    r.state = MoveState::Done;
                    r.boundary_id = Some(b.id);
                    r.version = Some(b.version);
                }
                // It can't go to the collars as it stands (an exclusion, the herd's collars changed).
                Err(e) => {
                    tracing::warn!(schedule = %s.id, strip = rows[i].strip, step = rows[i].step, "scheduled move not sent: {e:#}");
                    rows[i].skip("skipped");
                }
            }
        } else {
            let (strip, open) = (rows[i].strip, rows[i].step == 0);
            tracing::info!(schedule = %s.id, strip, step = rows[i].step, "move too late; not applied");
            rows[i].skip("late");
            if open {
                for r in rows.iter_mut().filter(|r| r.strip == strip && r.step > 0 && r.pending()) {
                    r.skip("late");
                }
            }
        }
    }
    Ok(())
}

/// Stage pending moves ahead, in time order, as far as the collars have room.
async fn stage(ctx: &Ctx, s: &Schedule, rows: &mut [Row], at: DateTime<Utc>) -> anyhow::Result<()> {
    let mut order: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].pending() && rows[i].at > at).collect();
    if order.is_empty() {
        return Ok(());
    }
    order.sort_by_key(|&i| (rows[i].at, rows[i].strip, rows[i].step));
    let split = db::herd_boundaries(ctx.db(), &s.herd_id, at).await?;
    let alive: HashMap<u32, &Boundary> = split.staged.iter().map(|b| (b.version, b)).collect();
    // The prefix still staged alive, in rising versions and at the times planned.
    let mut prefix = 0;
    let mut last_version = split.active.as_ref().map_or(0, |b| b.version);
    for &i in &order {
        let ok = rows[i].state == MoveState::Staged
            && rows[i].version.and_then(|v| alive.get(&v)).is_some_and(|b| b.version > last_version && b.effective_at == Some(rows[i].at));
        if !ok {
            break;
        }
        last_version = rows[i].version.unwrap_or(last_version);
        prefix += 1;
    }
    for &i in &order[prefix..] {
        rows[i].unstage();
    }
    if prefix == order.len() {
        return Ok(());
    }
    let ours: HashSet<u32> = split.staged.iter().filter(|b| b.decision_id == s.id).map(|b| b.version).collect();
    let room = budget(ctx, &s.herd_id, &split, &ours, at).await?;
    let (warn, hyst) = crate::margins::default_margins(ctx, &s.herd_id).await?;
    let mut used = vec![0usize; room.groups.len()];
    for &i in &order[..prefix] {
        if let Some(b) = rows[i].version.and_then(|v| alive.get(&v)) {
            for (u, (caps, _)) in used.iter_mut().zip(&room.groups) {
                *u += shape::record_bytes(b, caps).unwrap_or(0);
            }
        }
    }
    let mut count = prefix;
    for &i in &order[prefix..] {
        if count >= room.count {
            break;
        }
        let opts = SendOpts { warn_m: Some(warn), hysteresis_m: Some(hyst), effective_at: Some(rows[i].at) };
        let prepared = match shape::prepare(ctx, &s.herd_id, &rows[i].geometry, &opts).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(schedule = %s.id, strip = rows[i].strip, step = rows[i].step, "scheduled move can't be staged: {}", e.message);
                break;
            }
        };
        let size = room.bytes_for(&prepared.geometry, prepared.warn_m);
        if !room.fits(&used, &size) {
            break;
        }
        let b = send_boundary(ctx, &s.herd_id, rows[i].geometry.clone(), opts, &s.id).await?;
        if b.effective_at != Some(rows[i].at) {
            // Its time passed while it was being sent: it went as immediate.
            let r = &mut rows[i];
            r.state = MoveState::Done;
            r.boundary_id = Some(b.id);
            r.version = Some(b.version);
            break;
        }
        for (u, s) in used.iter_mut().zip(&size) {
            *u += s;
        }
        count += 1;
        let r = &mut rows[i];
        r.state = MoveState::Staged;
        r.boundary_id = Some(b.id);
        r.version = Some(b.version);
    }
    if count > prefix {
        tracing::info!(schedule = %s.id, staged = count, new = count - prefix, room = room.count, collars = room.collars, "schedule staged ahead");
    }
    Ok(())
}

async fn active_ids(ctx: &Ctx) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT id FROM schedules WHERE status = 'active' ORDER BY created_at").fetch_all(ctx.db()).await?)
}

/// The driver: every 5 s a pass over each active schedule; a MOVE to another
/// paddock ends the herd's schedule as soon as it applies.
pub fn spawn_driver(ctx: Ctx) {
    tokio::spawn(async move {
        let mut rx = ctx.subscribe();
        let mut tick = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                ev = rx.recv() => match ev {
                    Ok(Event::Decision { decision }) => {
                        if let Err(e) = on_decision(&ctx, &decision).await {
                            tracing::warn!("schedule: {e:#}");
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                },
                _ = tick.tick() => {
                    match active_ids(&ctx).await {
                        Ok(ids) => for id in ids {
                            if let Err(e) = drive(&ctx, &id, now()).await {
                                tracing::warn!(schedule = %id, "schedule driver: {e:#}");
                            }
                        },
                        Err(e) => tracing::warn!("schedule driver: {e:#}"),
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const O: [f64; 2] = [-93.6225, 42.0318];

    fn at(x: f64, y: f64) -> [f64; 2] {
        Projection::new(O).inverse([x, y])
    }
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon {
        Polygon::from_ring(vec![at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1)])
    }
    /// Four 50 m strips advancing north across a 200 m square.
    fn strips() -> Vec<Polygon> {
        (0..4).map(|i| rect(0.0, 50.0 * i as f64, 200.0, 50.0 * (i + 1) as f64)).collect()
    }
    /// Area in m², to 0.5 % of the flat rectangles the tests draw.
    fn area(p: &Polygon) -> f64 {
        p.area_ha() * 10_000.0
    }
    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() <= 0.005 * b
    }
    fn bf() -> BackFence {
        BackFence { close_steps: 3, ..BackFence::default() }
    }

    #[test]
    fn adjacent_strips_union_into_one_ring() {
        let u = union_of(&strips()[0..3]).unwrap();
        assert_eq!(u.coordinates.len(), 1);
        assert_eq!(u.outer_ring().len(), 4, "a rectangle: {:?}", u.outer_ring());
        assert!(near(area(&u), 200.0 * 150.0));
        // Two that don't touch aren't one boundary.
        assert!(union_of(&[strips()[0].clone(), strips()[2].clone()]).is_none());
    }

    #[test]
    fn without_a_back_fence_every_strip_so_far_stays_open() {
        let (open, closes) = stage_shapes(&strips(), Some(1), 2, &BackFence { enabled: false, ..bf() }).unwrap();
        assert!(closes.is_empty());
        assert!(near(area(&open), 200.0 * 150.0));
    }

    #[test]
    fn the_back_fence_sweeps_the_old_strip_in_steps() {
        let (open, closes) = stage_shapes(&strips(), Some(1), 2, &bf()).unwrap();
        // Open: strips 2 and 3 (the one they stand on and the new one).
        assert!(near(area(&open), 200.0 * 100.0));
        assert_eq!(closes.len(), 3);
        let areas: Vec<f64> = closes.iter().map(area).collect();
        // Two thirds, one third, then none of the old strip is left.
        for (a, want) in areas.iter().zip([200.0 * (50.0 + 100.0 / 3.0), 200.0 * (50.0 + 50.0 / 3.0), 200.0 * 50.0]) {
            assert!(near(*a, want), "{a} vs {want}");
        }
        // The fence comes from the far side: the south edge of the old strip goes first.
        assert!(!closes[0].contains(at(100.0, 55.0)) && closes[0].contains(at(100.0, 95.0)));
        assert!(closes[2].contains(at(100.0, 101.0)) && !closes[2].contains(at(100.0, 99.0)));
    }

    #[test]
    fn a_lag_keeps_strips_behind_the_herd() {
        let b = BackFence { lag_strips: 1, ..bf() };
        let (open, closes) = stage_shapes(&strips(), Some(2), 3, &b).unwrap();
        assert!(near(area(&open), 200.0 * 150.0), "strips 2-4");
        assert!(near(area(closes.last().unwrap()), 200.0 * 100.0), "closes to strips 3-4");
        // The first strip of a schedule has no ground behind it to close.
        let (open, closes) = stage_shapes(&strips(), None, 0, &bf()).unwrap();
        assert!(closes.is_empty() && near(area(&open), 200.0 * 50.0));
    }

    #[test]
    fn a_skipped_strip_is_old_ground_for_the_next_open() {
        let (open, closes) = stage_shapes(&strips(), Some(0), 2, &bf()).unwrap();
        assert!(near(area(&open), 200.0 * 150.0), "strips 1-3");
        assert!(near(area(closes.last().unwrap()), 200.0 * 50.0), "closes to strip 3 alone");
    }

    #[test]
    fn the_strip_after_the_herds_boundary() {
        let s = strips();
        assert_eq!(strip_after(&s, &s[1]), Some(2));
        assert_eq!(strip_after(&s, &union_of(&s[0..2]).unwrap()), Some(2));
        // The whole paddock isn't a strip to start from.
        assert_eq!(strip_after(&s, &union_of(&s).unwrap()), None);
        assert_eq!(strip_after(&s, &rect(500.0, 500.0, 600.0, 600.0)), None);
    }

    #[test]
    fn strips_from_the_strip_cutter_join_at_any_angle() {
        // Clockwise rings rounded to 7 decimals, as op_geo::strip makes them.
        let paddock = Polygon::from_ring(vec![[-93.625, 42.03], [-93.625, 42.0336], [-93.62, 42.0336], [-93.62, 42.03]]);
        for deg in [0.0, 30.0, 90.0, 135.0] {
            let s = op_geo::strip::strips(&paddock, deg, op_geo::strip::StripBy::Count(8), 5.0);
            let whole = union_of(&s).unwrap_or_else(|| panic!("{deg}°: the strips make the paddock"));
            assert!(near(area(&whole), area(&paddock)), "{deg}°");
            let (open, closes) = stage_shapes(&s, Some(3), 4, &bf()).unwrap_or_else(|e| panic!("{deg}°: {e}"));
            assert!(near(area(&open), area(&s[3]) + area(&s[4])), "{deg}°");
            assert!(near(area(closes.last().unwrap()), area(&s[4])), "{deg}°");
            let mid = area(&closes[0]);
            assert!(mid < area(&open) && mid > area(&s[4]), "{deg}°: part way");
        }
    }

    #[test]
    fn opens_must_wait_for_the_back_fence() {
        let t = |m: i64| DateTime::<Utc>::from_timestamp(1_800_000_000 + m * 60, 0).unwrap();
        let g = strips()[0].clone();
        let ok = [Row::planned(1, 0, Some(0), t(0), g.clone()), Row::planned(1, 1, None, t(240), g.clone()), Row::planned(2, 0, Some(1), t(1440), g.clone())];
        assert!(check_order(&ok).is_ok());
        let bad = [Row::planned(1, 0, Some(0), t(0), g.clone()), Row::planned(1, 1, None, t(300), g.clone()), Row::planned(2, 0, Some(1), t(200), g)];
        assert!(check_order(&bad).unwrap_err().contains("back fence"));
    }
}
