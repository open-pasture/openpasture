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
//! **The ground under the herd.** An open never takes ground away: it is the
//! ground the herd has joined to the new strip ([`opening`]); only back-fence
//! steps take ground, from the far side. Each open is planned from what the
//! move before it leaves. When the boundary in effect isn't that move (one
//! was late or couldn't be sent, a boundary came from elsewhere, move now),
//! what is still to come is planned again from the ground the herd is on
//! (`rechain`) before anything more is sent or staged.
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

/// The first two strips that share ground (a square metre or more), if any.
/// `union_geo` puts every strip in one boolean op, and `geo`'s ops fill
/// even-odd: ground two strips share cancels out there, so a strip lying
/// inside another would be staged as a hole in the boundary that opens it.
/// Strips that only share an edge (the strip cutter's) don't count.
fn overlapping(strips: &[Polygon]) -> Option<(usize, usize)> {
    use geo::BoundingRect;
    let s = snapped(strips)?;
    let boxes: Vec<_> = s.polys.iter().map(|p| p.bounding_rect()).collect();
    for i in 0..s.polys.len() {
        for j in i + 1..s.polys.len() {
            let (Some(a), Some(b)) = (boxes[i], boxes[j]) else { continue };
            if a.max().x <= b.min().x || b.max().x <= a.min().x || a.max().y <= b.min().y || b.max().y <= a.min().y {
                continue;
            }
            if s.polys[i].intersection(&s.polys[j]).unsigned_area() >= 1.0 {
                return Some((i, j));
            }
        }
    }
    None
}

/// One polygon covering these strips, or `None` when they don't join up.
pub fn union_of(strips: &[Polygon]) -> Option<Polygon> {
    let s = snapped(strips)?;
    s.local.one(&union_geo(&s.polys))
}

/// Where a back fence stands part way through closing: `region` (the whole
/// open ground) less the far `1 - keep` of the old ground's depth on each
/// side it has some (`behind` the strips kept, `ahead` of them), measured
/// along `axis` (from, toward). One intersection with a band just bigger than
/// the region, so the result keeps the region's own edges.
fn part_toward(region: &GeoPolygon, behind: Option<&MultiPolygon>, ahead: Option<&MultiPolygon>, axis: ([f64; 2], [f64; 2]), keep: f64) -> MultiPolygon {
    let (from, toward) = axis;
    let (dx, dy) = (toward[0] - from[0], toward[1] - from[1]);
    let len = dx.hypot(dy).max(1e-9);
    let (ux, uy) = (dx / len, dy / len);
    let (px, py) = (-uy, ux);
    let span =
        |cs: &mut dyn Iterator<Item = &Coord>, f: &dyn Fn(&Coord) -> f64| cs.map(f).fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(v), b.max(v)));
    let along = |c: &Coord| c.x * ux + c.y * uy;
    let across = |c: &Coord| c.x * px + c.y * py;
    let coords = |m: &MultiPolygon| m.0.iter().flat_map(|p| p.exterior().0.iter()).copied().collect::<Vec<Coord>>();
    let keep = keep.clamp(0.0, 1.0);
    let (near, far) = span(&mut region.exterior().0.iter(), &along);
    let (a0, a1) = span(&mut region.exterior().0.iter(), &across);
    let (a0, a1) = (a0 - 10.0, a1 + 10.0);
    let lower = match behind {
        Some(b) => {
            let (lo, hi) = span(&mut coords(b).iter(), &along);
            hi - keep * (hi - lo)
        }
        None => near - 10.0,
    };
    let upper = match ahead {
        Some(a) => {
            let (lo, hi) = span(&mut coords(a).iter(), &along);
            lo + keep * (hi - lo)
        }
        None => far + 10.0,
    };
    let at = |t: f64, s: f64| (ux * t + px * s, uy * t + py * s);
    let band = GeoPolygon::new(LineString::from(vec![at(lower, a0), at(upper, a0), at(upper, a1), at(lower, a1), at(lower, a0)]), vec![]);
    MultiPolygon(vec![region.clone()]).intersection(&MultiPolygon(vec![band]))
}

/// The ground a herd has before an open.
#[derive(Debug, Clone, PartialEq)]
pub enum Ground {
    /// `strips[a ..= b]`.
    Strips(usize, usize),
    /// Any other shape (a boundary drawn or swept to, a back fence part way
    /// closed), by its outer ring: every send applies exclusions again.
    Shape(Polygon),
}

/// What opening a strip stages, then each back-fence close step.
#[derive(Debug, Clone, PartialEq)]
pub struct Opening {
    pub open: Polygon,
    pub closes: Vec<Polygon>,
    /// The ground the herd has once the last of them is in.
    pub after: Ground,
}

/// Most back-fence steps one open takes (old ground several strips deep).
pub const MAX_CLOSE_STEPS: usize = 48;

/// Strips with at least this share of their ground inside a shape are ground it covers.
const TOUCHES: f64 = 0.01;

/// Each strip's share of its ground inside `g` (a bounding-box test first).
fn shares(strips: &[Polygon], g: &Polygon) -> Vec<f64> {
    use geo::BoundingRect;
    let outer = Polygon::from_ring(g.outer_ring());
    let (Some(local), true) = (Local::new(&outer), outer.outer_ring().len() >= 3) else { return vec![0.0; strips.len()] };
    let Some(a) = local.to_geo(&outer) else { return vec![0.0; strips.len()] };
    let abox = a.bounding_rect();
    let a = MultiPolygon(vec![a]);
    strips
        .iter()
        .map(|s| {
            let Some(p) = local.to_geo(s) else { return 0.0 };
            let (Some(pb), Some(ab)) = (p.bounding_rect(), abox) else { return 0.0 };
            if pb.max().x <= ab.min().x || ab.max().x <= pb.min().x || pb.max().y <= ab.min().y || ab.max().y <= pb.min().y {
                return 0.0;
            }
            let area = p.unsigned_area();
            if area <= 0.0 { 0.0 } else { MultiPolygon(vec![p]).intersection(&a).unsigned_area() / area }
        })
        .collect()
}

/// `g` as ground: the strips it covers when it is just those strips (more
/// than half of each inside it, one run, the same area to 1 %), else its shape.
pub fn ground_of(strips: &[Polygon], g: &Polygon) -> Ground {
    let outer = Polygon::from_ring(g.outer_ring());
    let full: Vec<usize> = shares(strips, &outer).iter().enumerate().filter(|(_, f)| **f > 0.5).map(|(i, _)| i).collect();
    if let (Some(&a), Some(&b)) = (full.first(), full.last())
        && b - a + 1 == full.len()
    {
        let theirs: f64 = strips[a..=b].iter().map(Polygon::area_ha).sum();
        let mine = outer.area_ha();
        if theirs > 0.0 && (mine - theirs).abs() <= 0.01 * theirs {
            return Ground::Strips(a, b);
        }
    }
    Ground::Shape(outer)
}

/// How much of `g` lies on the strips (0 to 1).
fn on_strips(strips: &[Polygon], g: &Polygon) -> f64 {
    let area = Polygon::from_ring(g.outer_ring()).area_ha();
    if area <= 0.0 {
        return 0.0;
    }
    shares(strips, g).iter().zip(strips).map(|(f, s)| f * s.area_ha()).sum::<f64>() / area
}

/// What opening strip `k` stages when the herd has `ground`, then each
/// back-fence close step.
///
/// An open never takes ground away: it is the ground the herd has joined to
/// the strips up to `k` and to the strips between (the herd walks through
/// them; skipped strips are such ground). Without a back fence that is all:
/// `strips[0 ..= k]` and the ground. With one, the steps then sweep all but
/// `strips[k-lag ..= k]` from the far side, `close_steps` steps for each strip
/// of old ground, the last being `strips[k-lag ..= k]`; old ground on both
/// sides closes from both ends. `None`: nothing known, the strip alone.
pub fn opening(strips: &[Polygon], ground: Option<&Ground>, k: usize, bf: &BackFence) -> Result<Opening, String> {
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
    let lag = if bf.enabled { bf.lag_strips as usize } else { 0 };
    let end = if bf.enabled { k.saturating_sub(lag) } else { 0 };
    // The strips taken in, and the drawn ground joined to them.
    let (lo, hi, shape, touched) = match ground {
        None => (if bf.enabled { k } else { 0 }, k, None, vec![]),
        Some(Ground::Strips(a, b)) => ((*a).min(end), (*b).max(k), None, vec![]),
        Some(Ground::Shape(g)) => {
            let touched: Vec<usize> = shares(strips, g).iter().enumerate().filter(|(_, f)| **f >= TOUCHES).map(|(i, _)| i).collect();
            let below = touched.iter().copied().filter(|&i| i <= k).max();
            let above = touched.iter().copied().filter(|&i| i > k).min();
            // The strips it touches are taken in whole, so the ground joins up.
            let lo = if bf.enabled { below.map_or(end, |b| b.min(end)) } else { 0 };
            let hi = above.map_or(k, |a| a.max(k));
            (lo, hi, Some(Polygon::from_ring(g.outer_ring())), touched)
        }
    };
    let mut polys: Vec<Polygon> = strips[lo..=hi].to_vec();
    if let Some(g) = &shape {
        polys.push(g.clone());
    }
    // Every shape below comes from the same snapped polygons.
    let mut sn = snapped(&polys).ok_or_else(|| apart(lo, hi))?;
    // The drawn ground overlaps the strips: joined in an op of its own (one op
    // over overlapping shapes fills even-odd and would cut the overlap out).
    let drawn = shape.as_ref().and_then(|_| sn.polys.pop());
    let at = |i: usize| i - lo;
    let mut whole = union_geo(&sn.polys);
    if let Some(g) = drawn {
        whole = whole.union(&MultiPolygon(vec![g]));
    }
    let open = sn.local.one(&whole).ok_or_else(|| apart(lo.min(k), hi.max(k)))?;
    // Old ground: what the herd has besides strips[end ..= k].
    let old_strips: Vec<usize> =
        (lo..=hi).chain(touched.iter().copied()).filter(|&i| i < end || i > k).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let beyond = shape.as_ref().is_some_and(|_| {
        let kept = union_geo(&sn.polys[at(end)..=at(k)]);
        whole.difference(&kept).unsigned_area() >= 1.0
    });
    if !bf.enabled || (old_strips.is_empty() && !beyond) {
        let after = match &shape {
            Some(_) => ground_of(strips, &open),
            None => Ground::Strips(lo, hi),
        };
        return Ok(Opening { open, closes: vec![], after });
    }
    let kept = union_geo(&sn.polys[at(end)..=at(k)]);
    let last = sn.local.one(&kept).ok_or_else(|| apart(end, k))?;
    let one_of = |m: &MultiPolygon| {
        let big: Vec<&GeoPolygon> = m.0.iter().filter(|p| p.unsigned_area() >= 1.0).collect();
        (big.len() == 1).then(|| big[0].clone())
    };
    // The old ground behind the strips kept and ahead of them, each closed from its far side.
    use geo::Centroid;
    let centre = |p: &GeoPolygon| p.centroid().map(|c| [c.x(), c.y()]);
    let region = one_of(&whole).ok_or_else(|| apart(lo, hi))?;
    let (Some(first), Some(front)) = (centre(&sn.polys[at(end)]), centre(&sn.polys[at(k)])) else { return Err(apart(end, k)) };
    // Which way the strips advance: from one strip to the next.
    let centre_of = |i: usize| sn.local.to_geo(&strips[i]).as_ref().and_then(centre);
    let (a, b) = if k >= 1 { (k - 1, k) } else { (0, 1.min(n - 1)) };
    let step_dir = match (centre_of(a), centre_of(b)) {
        (Some(p), Some(q)) if a != b => [q[0] - p[0], q[1] - p[1]],
        _ => [0.0, 0.0],
    };
    let old = whole.difference(&kept);
    let (mut behind, mut ahead) = (Vec::new(), Vec::new());
    for p in old.0.iter().filter(|p| p.unsigned_area() >= 1.0) {
        let Some(c) = centre(p) else { continue };
        let side = (c[0] - first[0]) * step_dir[0] + (c[1] - first[1]) * step_dir[1];
        if side < 0.0 || step_dir == [0.0, 0.0] { behind.push(p.clone()) } else { ahead.push(p.clone()) }
    }
    let deep = |n: usize, any: bool| if any { n.max(1) } else { 0 };
    let depth =
        deep(old_strips.iter().filter(|&&i| i < end).count(), !behind.is_empty()).max(deep(old_strips.iter().filter(|&&i| i > k).count(), !ahead.is_empty()));
    let steps = (bf.close_steps.max(1) as usize * depth.max(1)).min(MAX_CLOSE_STEPS);
    let (behind, ahead) = ((!behind.is_empty()).then(|| MultiPolygon(behind)), (!ahead.is_empty()).then(|| MultiPolygon(ahead)));
    let axis = match (&behind, &ahead) {
        (Some(b), None) => b.centroid().map(|c| ([c.x(), c.y()], first)),
        (None, Some(a)) => a.centroid().map(|c| (front, [c.x(), c.y()])),
        (Some(b), Some(a)) => b.centroid().zip(a.centroid()).map(|(b, a)| ([b.x(), b.y()], [a.x(), a.y()])),
        (None, None) => None,
    };
    let mut closes = Vec::with_capacity(steps);
    if let Some(axis) = axis.filter(|_| steps > 1) {
        for s in 1..steps {
            let keep = 1.0 - s as f64 / steps as f64;
            let part = part_toward(&region, behind.as_ref(), ahead.as_ref(), axis, keep);
            closes.push(sn.local.one(&part).ok_or_else(|| format!("Strip {}'s back fence can't close in {steps} steps. Close it in one.", k + 1))?);
        }
    }
    closes.push(last);
    Ok(Opening { open, closes, after: Ground::Strips(end, k) })
}

/// What opening strip `k` stages, then each back-fence close step, when the
/// strip opened before it is `prev` (the herd is on `strips[prev-lag ..= prev]`
/// once its back fence closed): [`opening`] from that ground.
pub fn stage_shapes(strips: &[Polygon], prev: Option<usize>, k: usize, bf: &BackFence) -> Result<(Polygon, Vec<Polygon>), String> {
    let lag = if bf.enabled { bf.lag_strips as usize } else { 0 };
    let ground = prev.map(|p| if bf.enabled { Ground::Strips(p.saturating_sub(lag), p) } else { Ground::Strips(0, p) });
    let o = opening(strips, ground.as_ref(), k, bf)?;
    Ok((o.open, o.closes))
}

/// Times for `n` back-fence steps from `first`, `every` apart; pressed evenly
/// into the time before a minute ahead of `next` when they'd run into it and
/// `press` allows (never closer than a second apart).
fn spaced(first: DateTime<Utc>, n: usize, every: Duration, next: Option<DateTime<Utc>>, press: bool) -> Vec<DateTime<Utc>> {
    let mut step = every;
    if press
        && n > 1
        && let Some(next) = next
    {
        let limit = next - Duration::minutes(1);
        let room = (limit - first).num_seconds();
        if first + every * (n as i32 - 1) >= limit && room >= n as i64 - 1 {
            step = Duration::seconds(room / (n as i64 - 1));
        }
    }
    (0..n).map(|i| trunc_secs(first + step * i as i32)).collect()
}

/// The rows opening strip `k` at `at` from `ground`, with its back-fence steps.
/// Steps beyond the back fence's own count (old ground several strips deep)
/// are pressed in before `next` (the next open) when they'd run into it.
fn plan_strip(
    s: &Schedule,
    ground: Option<&Ground>,
    k: usize,
    at: DateTime<Utc>,
    occurrence: Option<u32>,
    next: Option<DateTime<Utc>>,
) -> Result<(Vec<Row>, Ground), String> {
    let o = opening(&s.strips, ground, k, &s.back_fence)?;
    let at = trunc_secs(at);
    let mut rows = vec![Row::planned(k as u32, 0, occurrence, at, o.open)];
    let bf = &s.back_fence;
    let press = o.closes.len() > bf.close_steps.max(1) as usize;
    let times = spaced(at + Duration::minutes(i64::from(bf.close_after_min)), o.closes.len(), Duration::minutes(i64::from(bf.close_every_min)), next, press);
    for (i, (g, t)) in o.closes.into_iter().zip(times).enumerate() {
        rows.push(Row::planned(k as u32, i as u32 + 1, None, t, g));
    }
    Ok((rows, o.after))
}

/// Give strip `k`'s back-fence steps still to come these shapes: in place,
/// keeping their times, when there are as many; else as new rows at
/// `times(n)`. Rows whose shape changes are unstaged. Whether anything changed.
fn set_closes(rows: &mut Vec<Row>, gone: &mut Vec<i64>, k: u32, shapes: Vec<Polygon>, times: &dyn Fn(usize) -> Vec<DateTime<Utc>>) -> bool {
    let mut mine: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].strip == k && rows[i].step > 0 && rows[i].pending()).collect();
    mine.sort_by_key(|&i| rows[i].step);
    if mine.len() == shapes.len() {
        let mut changed = false;
        for (i, g) in mine.into_iter().zip(shapes) {
            if rows[i].geometry != g {
                rows[i].geometry = g;
                rows[i].unstage();
                changed = true;
            }
        }
        return changed;
    }
    let first_step = mine.first().map_or(1, |&i| rows[i].step);
    for i in mine.into_iter().rev() {
        let r = rows.remove(i);
        if r.id != 0 {
            gone.push(r.id);
        }
    }
    let n = shapes.len();
    for (j, (g, t)) in shapes.into_iter().zip(times(n)).enumerate() {
        rows.push(Row::planned(k, first_step + j as u32, None, t, g));
    }
    true
}

/// The ground the herd is on with `active` in effect: the shape planned for
/// it when it is one of this schedule's moves, else its own shape.
fn ground_now(s: &Schedule, rows: &[Row], active: &Boundary) -> Ground {
    let g = rows.iter().find(|r| r.version == Some(active.version)).map_or(&active.geometry, |r| &r.geometry);
    ground_of(&s.strips, g)
}

/// Bring the moves still to come in line with the ground the herd is on
/// (`active` in effect). Nothing to do when the move before the next one
/// took effect and is what is in effect: the rest was planned from it. When
/// it isn't (a move marked late or not sent, a boundary from elsewhere), each
/// strip still to come is planned again from the ground the one before
/// leaves, until one comes out as it was: an open keeps the ground under the
/// herd and its back fence closes it; the strip the herd is on closes from
/// where it is; an open that never happened takes its back-fence steps with
/// it. Rows that change are unstaged. Whether any did.
fn rechain(s: &Schedule, rows: &mut Vec<Row>, gone: &mut Vec<i64>, active: &Boundary) -> Result<bool, String> {
    rows.sort_by_key(|r| (r.at, r.strip, r.step, r.id));
    let Some(first) = rows.iter().position(Row::pending) else { return Ok(false) };
    let before = rows[..first].iter().rev().find(|r| r.skipped.as_deref() != Some("held"));
    if before.is_some_and(|b| b.state == MoveState::Done && b.version == Some(active.version)) {
        return Ok(false);
    }
    let mut ground = ground_now(s, rows, active);
    let mut order: Vec<u32> = Vec::new();
    for r in rows.iter().filter(|r| r.pending()) {
        if !order.contains(&r.strip) {
            order.push(r.strip);
        }
    }
    let bf = s.back_fence;
    let every = Duration::minutes(i64::from(bf.close_every_min));
    let mut changed = false;
    for k in order {
        let Some(oi) = rows.iter().position(|r| r.strip == k && r.open()) else { continue };
        if rows[oi].state == MoveState::Skipped {
            for r in rows.iter_mut().filter(|r| r.strip == k && r.step > 0 && r.pending()) {
                r.skip("skipped");
                changed = true;
            }
            continue;
        }
        let o = opening(&s.strips, Some(&ground), k as usize, &bf)
            .or_else(|_| opening(&s.strips, Some(&ground), k as usize, &BackFence { close_steps: 1, ..bf }))?;
        let open_at = rows[oi].at;
        let next = rows.iter().filter(|r| r.open() && r.pending() && r.strip != k && r.at > open_at).map(|r| r.at).min();
        let mut this = false;
        if rows[oi].pending() {
            if rows[oi].geometry != o.open {
                rows[oi].geometry = o.open.clone();
                rows[oi].unstage();
                this = true;
            }
            let first = open_at + Duration::minutes(i64::from(bf.close_after_min));
            this |= set_closes(rows, gone, k, o.closes, &|n| spaced(first, n, every, next, true));
        } else {
            // The strip the herd is on: what is left of its back fence closes from where the herd is.
            let Some(first) = rows.iter().filter(|r| r.strip == k && r.step > 0 && r.pending()).map(|r| r.at).min() else { continue };
            this |= set_closes(rows, gone, k, o.closes, &|n| spaced(first, n, every, next, true));
        }
        ground = o.after;
        changed |= this;
        if !this {
            break;
        }
    }
    rows.sort_by_key(|r| (r.at, r.strip, r.step, r.id));
    Ok(changed)
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
    if let Some((a, b)) = overlapping(&strips) {
        return Err(ApiError::bad_request(format!("Strips {} and {} overlap. Each piece of ground belongs to one strip.", a + 1, b + 1)));
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
    // The first open keeps the ground the herd is fenced to now; each later one
    // the ground the open before it leaves.
    let mut ground = Some(ground_of(&s.strips, &active.geometry));
    let count = s.strips.len() - next;
    for (o, k) in (next..s.strips.len()).enumerate() {
        let then = (o + 1 < count).then(|| first(o as u32 + 1));
        let (planned, after) = plan_strip(&s, ground.as_ref(), k, first(o as u32), Some(o as u32), then).map_err(ApiError::bad_request)?;
        rows.extend(planned);
        ground = Some(after);
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

    /// Re-plan strip `k`'s open shape and back-fence steps from its open, the
    /// herd having `ground` before it (it changes when a strip before is skipped).
    fn replan(&mut self, k: u32, ground: &Ground) -> Result<(), String> {
        let Some(oi) = self.pending_open(k) else { return Ok(()) };
        let open_at = self.rows[oi].at;
        let next = self.pending_opens().into_iter().map(|i| self.rows[i].at).find(|t| *t > open_at);
        let (planned, _) = plan_strip(&self.s, Some(ground), k as usize, open_at, self.rows[oi].occurrence, next)?;
        self.rows[oi].geometry = planned[0].geometry.clone();
        let closes: Vec<Row> = planned.into_iter().skip(1).collect();
        let times: Vec<DateTime<Utc>> = closes.iter().map(|r| r.at).collect();
        set_closes(&mut self.rows, &mut self.gone, k, closes.into_iter().map(|r| r.geometry).collect(), &|_| times.clone());
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
        // The herd walks through the skipped ground: the open after it keeps
        // what the move before it leaves (or the ground the herd is on now).
        let at = p.rows[first].at;
        let before = p.rows.iter().filter(|r| r.pending() && r.at < at).max_by_key(|r| (r.at, r.step)).map(|r| ground_of(&p.s.strips, &r.geometry));
        let ground = match before {
            Some(g) => Some(g),
            None => db::herd_boundaries(ctx.db(), &p.s.herd_id, now()).await?.active.map(|a| ground_now(&p.s, &p.rows, &a)),
        };
        if let Some(g) = ground {
            p.replan(k, &g).map_err(ApiError::bad_request)?;
        }
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
    let open_at = p.rows[oi].at;
    let next_at = p.pending_opens().into_iter().map(|i| p.rows[i].at).find(|t| *t > open_at);
    // What is left of the back fence before it would cut the herd off the
    // strip it moves into: the open keeps that ground and its own back fence closes it.
    for r in p.rows.iter_mut().filter(|r| r.pending() && r.step > 0 && r.strip != k && r.at <= open_at) {
        r.skip("skipped");
    }
    let bf = p.s.back_fence;
    let ground = db::herd_boundaries(ctx.db(), &p.s.herd_id, now()).await?.active.map(|a| ground_now(&p.s, &p.rows, &a));
    let o = opening(&p.s.strips, ground.as_ref(), k as usize, &bf).map_err(ApiError::bad_request)?;
    let b = send_boundary(ctx, &p.s.herd_id, o.open.clone(), SendOpts::default(), &p.s.id).await.map_err(|e| ApiError::bad_request(e.to_string()))?;
    let r = &mut p.rows[oi];
    r.at = at;
    r.geometry = o.open;
    r.state = MoveState::Done;
    r.boundary_id = Some(b.id.clone());
    r.version = Some(b.version);
    let times = |n: usize| -> Vec<DateTime<Utc>> {
        let span = Duration::minutes(i64::from(bf.close_after_min) + i64::from(bf.close_every_min) * (n as i64 - 1).max(0));
        match next_at.filter(|next| at + span >= *next - Duration::minutes(1)) {
            // Evenly between now and a minute before the next open.
            Some(next) => {
                (0..n).map(|j| trunc_secs(at + Duration::seconds((next - Duration::minutes(1) - at).num_seconds() * (j as i64 + 1) / (n as i64 + 1)))).collect()
            }
            None => (0..n).map(|j| trunc_secs(at + Duration::minutes(i64::from(bf.close_after_min) + i64::from(bf.close_every_min) * j as i64))).collect(),
        }
    };
    // Its back fence follows from now.
    let shapes: Vec<Polygon> = o.closes;
    let n = shapes.len();
    for i in p.closes(k).into_iter().rev() {
        let r = p.rows.remove(i);
        if r.id != 0 {
            p.gone.push(r.id);
        }
    }
    for (j, (g, t)) in shapes.into_iter().zip(times(n)).enumerate() {
        p.rows.push(Row::planned(k, j as u32 + 1, None, t, g));
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

/// A MOVE off the schedule's paddock ends the herd's schedule when it
/// applies (the move's own boundaries replace the staged strips): to another
/// paddock, or to ground in no mapped paddock (`to_paddock_id` none). Idempotent.
pub async fn on_decision(ctx: &Ctx, d: &Decision) -> anyhow::Result<Option<Schedule>> {
    if d.status != DecisionStatus::Applied || d.action != Some(DecisionAction::Move) {
        return Ok(None);
    }
    let Some(s) = running(ctx, &d.herd_id).await? else { return Ok(None) };
    if d.to_paddock_id.as_deref() == Some(s.paddock_id.as_str()) {
        return Ok(None);
    }
    let _g = lock(&s.id).await;
    let p = load_plan(ctx, &s.id).await.map_err(|e| anyhow::anyhow!(e.message))?;
    if p.s.status == ScheduleStatus::Done {
        return Ok(None);
    }
    tracing::info!(schedule = %s.id, decision = %d.id, to = ?d.to_paddock_id, "a move off the paddock ends the schedule");
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
    let before: HashMap<i64, Row> = rows.iter().map(|r| (r.id, r.clone())).collect();
    let mut gone = Vec::new();
    let mut changed = false;
    if s.status == ScheduleStatus::Active {
        if settled(ctx, &s, at).await? {
            settle_due(ctx, &s, &mut rows, &mut gone, at).await?;
            // What is still to come starts from the ground the herd is on. A
            // herd moved mostly off the strips (a draw or MOVE elsewhere whose
            // decision this schedule never saw) isn't dragged back onto them.
            if let Some(active) = db::herd_boundaries(ctx.db(), &s.herd_id, at).await?.active {
                if active.decision_id != s.id && on_strips(&s.strips, &active.geometry) < 0.5 {
                    tracing::warn!(schedule = %s.id, version = active.version, "the herd was moved off the schedule's strips");
                    return give_up(ctx, s, rows, gone, &active).await;
                }
                if let Err(e) = rechain(&s, &mut rows, &mut gone, &active) {
                    tracing::warn!(schedule = %s.id, "the herd's ground doesn't join the strips still to open: {e}");
                    return give_up(ctx, s, rows, gone, &active).await;
                }
            }
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
    let mut write: Vec<Row> = rows.iter().filter(|r| r.id == 0 || before.get(&r.id) != Some(*r)).cloned().collect();
    if write.is_empty() && gone.is_empty() && !changed {
        return Ok(false);
    }
    s.updated_at = now();
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    for id in &gone {
        sqlx::query("DELETE FROM schedule_moves WHERE id = ?").bind(id).execute(&mut *tx).await?;
    }
    write_rows(&mut tx, &s.id, &mut write, s.updated_at).await?;
    write_schedule(&mut tx, &s).await?;
    tx.commit().await?;
    ctx.publish(Event::Schedule { schedule: s });
    Ok(true)
}

/// The herd is on ground the strips still to open don't join (it was moved
/// off them): nothing more opens. The boundary it is on goes again as a new
/// immediate version, so collars drop the strips staged above it.
async fn give_up(ctx: &Ctx, mut s: Schedule, mut rows: Vec<Row>, gone: Vec<i64>, active: &Boundary) -> anyhow::Result<bool> {
    if rows.iter().any(|r| r.state == MoveState::Staged) {
        let opts = SendOpts { warn_m: Some(active.warn_m), hysteresis_m: Some(active.hysteresis_m), effective_at: None };
        send_boundary(ctx, &s.herd_id, active.geometry.clone(), opts, &s.id).await?;
    }
    for r in rows.iter_mut().filter(|r| r.pending()) {
        r.skip("skipped");
    }
    let at = now();
    s.status = ScheduleStatus::Done;
    s.ended_at = Some(at);
    s.next_index = next_index(&s, &rows);
    s.updated_at = at;
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    for id in &gone {
        sqlx::query("DELETE FROM schedule_moves WHERE id = ?").bind(id).execute(&mut *tx).await?;
    }
    write_rows(&mut tx, &s.id, &mut rows, at).await?;
    write_schedule(&mut tx, &s).await?;
    tx.commit().await?;
    tracing::info!(schedule = %s.id, herd = %s.herd_id, "schedule ended: the herd is off its strips");
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

/// Moves whose time has come, in time order: done when they took effect,
/// else applied at once when at most 30 minutes late, else `late`. An open
/// that is late or can't be sent takes its back-fence steps with it. What is
/// sent late is first brought in line with the ground the herd is on
/// ([`rechain`]): the move before it may never have happened.
async fn settle_due(ctx: &Ctx, s: &Schedule, rows: &mut Vec<Row>, gone: &mut Vec<i64>, at: DateTime<Utc>) -> anyhow::Result<()> {
    let mut lined_up = false;
    loop {
        rows.sort_by_key(|r| (r.at, r.strip, r.step, r.id));
        let Some(i) = rows.iter().position(|r| r.pending() && r.at <= at) else { break };
        if let Some(v) = rows[i].version.filter(|_| rows[i].state == MoveState::Staged)
            && took_effect(ctx, &s.herd_id, v, rows[i].at).await?
        {
            rows[i].state = MoveState::Done;
            lined_up = false;
            tracing::info!(schedule = %s.id, strip = rows[i].strip, step = rows[i].step, version = v, "scheduled move took effect");
            continue;
        }
        let (strip, open) = (rows[i].strip, rows[i].step == 0);
        let skip_with_steps = |rows: &mut Vec<Row>, i: usize, why: &str| {
            rows[i].skip(why);
            if open {
                for r in rows.iter_mut().filter(|r| r.strip == strip && r.step > 0 && r.pending()) {
                    r.skip(why);
                }
            }
        };
        if at - rows[i].at > Duration::minutes(LATE_AFTER_MIN) {
            tracing::info!(schedule = %s.id, strip, step = rows[i].step, "move too late; not applied");
            skip_with_steps(rows, i, "late");
            lined_up = false;
            continue;
        }
        if !lined_up {
            lined_up = true;
            if let Some(active) = db::herd_boundaries(ctx.db(), &s.herd_id, at).await?.active {
                match rechain(s, rows, gone, &active) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(schedule = %s.id, strip, step = rows[i].step, "scheduled move doesn't join the herd's ground: {e}");
                        skip_with_steps(rows, i, "skipped");
                        continue;
                    }
                }
            }
        }
        lined_up = false;
        match send_boundary(ctx, &s.herd_id, rows[i].geometry.clone(), SendOpts::default(), &s.id).await {
            Ok(b) => {
                tracing::info!(schedule = %s.id, strip, step = rows[i].step, version = b.version, late_s = (at - rows[i].at).num_seconds(), "late move applied now");
                let r = &mut rows[i];
                r.state = MoveState::Done;
                r.boundary_id = Some(b.id);
                r.version = Some(b.version);
            }
            // It can't go to the collars as it stands (an exclusion, the herd's collars changed).
            Err(e) => {
                tracing::warn!(schedule = %s.id, strip, step = rows[i].step, "scheduled move not sent: {e:#}");
                skip_with_steps(rows, i, "skipped");
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
        // Cut strips share only edges, so they are never taken for overlapping ones.
        for deg in [0.0, 17.0, 30.0, 45.0, 72.0, 90.0, 135.0, 161.0] {
            for by in [op_geo::strip::StripBy::Count(8), op_geo::strip::StripBy::Width(23.0)] {
                let s = op_geo::strip::strips(&paddock, deg, by, 5.0);
                assert_eq!(overlapping(&s), None, "{deg}°");
            }
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
