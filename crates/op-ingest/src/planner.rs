//! Sweep planner: the next active boundary of a move. Pure: no database, no
//! clock. See API.md "Moves: target and sweep".
//!
//! Work happens in local metres around the target's centroid (op-geo's
//! projection, the same one the collars use). A step is
//!
//! ```text
//! hull(target ∪ animals buffered by warn_m + 2 m) ∩ ahead of the back line ∩ (previous ∪ target), ∪ target
//! ```
//!
//! The back line sits `0.6 × warn_m` behind the rearmost animal, so that
//! animal is in the warning band and every other animal is clear of it.
//!
//! Steps fit the herd's collars (`limits`): the outer ring is cut down to
//! what they hold, leaving room for holes. The target's holes are in every
//! step (the step contains the target), and so is each hole of the previous
//! boundary that still lies whole inside the step with the gap a collar
//! needs; the step is checked with the collars' shape rules.

use geo::{BooleanOps, ConvexHull, Coord, InteriorPoint, LineString, MultiPoint, MultiPolygon, Point};
use op_geo::ring::{clean_ring, distance_to_segment, point_in_ring, ring_is_simple, signed_area, validate_ring};
use op_geo::shape::{self, SERVER_SLACK_M};
use op_geo::{CollarLimits, LonLat, Polygon, Projection};
use serde::{Deserialize, Serialize};

type P = [f64; 2];
type GPoly = geo::Polygon<f64>;

/// The back line sits this share of `warn_m` behind the rearmost animal.
pub const BACK_FACTOR: f64 = 0.6;
/// Animals are buffered by `warn_m` plus this for the sides of a step.
pub const SIDE_EXTRA_M: f64 = 2.0;
/// Once the back line reaches the target, animals this far inside it are in.
pub const FINISH_MARGIN_M: f64 = 1.5;
/// Sides of the polygon standing in for a buffer circle.
const DISC_SIDES: usize = 12;
/// A followed back edge (see [`Rear::Follow`]) keeps each animal this
/// share of `warn_m` from it: inside its warning band, clear of the edge by
/// more than a fix wanders.
pub const FOLLOW_FACTOR: f64 = 0.7;
/// The wedge behind each animal rises this many metres forward per metre
/// sideways (45°: an animal cued at the back walks straight ahead, off it).
pub const FOLLOW_SLOPE: f64 = 1.0;
/// Share of the corners a collar holds that the followed back edge may use
/// (64 of V0's 128); beyond that, the smallest bumps of it are straightened,
/// which only ever moves it back. Each straightened bump leaves the animals
/// under it further from the edge, and past `warn_m` they aren't cued, so
/// the edge gets most of the budget.
pub const FOLLOW_CORNERS_SHARE: f64 = 0.5;

/// How the back of a sweep step is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rear<'a> {
    /// One straight back line [`BACK_FACTOR`] × `warn_m` behind the
    /// rearmost animal (escape pens: one animal).
    Line,
    /// The back edge follows the herd's rear: a wedge behind every animal,
    /// [`FOLLOW_FACTOR`] × `warn_m` from it (plus `extra`), and the edge is
    /// the rearmost of them. Every animal at the back of the herd is in its
    /// warning band at once, and one lagging behind holds back only its own
    /// stretch of the edge. The step's `level` is the back line of the
    /// `quantile` of the animals' progress (0.1: nine in ten are ahead of
    /// it). `extra`, per animal (empty: none), is room added behind it for
    /// how uncertain and how old its position is.
    Follow { quantile: f64, extra: &'a [f64] },
}

/// How progress toward the target is measured, fixed for a whole move.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    /// Sweep along a direction: unit vector in east/north metres, from the
    /// herd toward the target. Progress is distance along it.
    Axis { axis: [f64; 2] },
    /// The herd surrounds the target: close in from every side. Progress is
    /// minus the distance outside the target (0 inside).
    Gather,
}

pub struct PlanInput<'a> {
    pub target: &'a Polygon,
    /// The herd's active boundary. `None`: `paddock`, else the target's
    /// bounding box grown around the herd.
    pub previous: Option<&'a Polygon>,
    pub paddock: Option<&'a Polygon>,
    /// Latest positions of the animals in the sweep.
    pub animals: &'a [LonLat],
    pub warn_m: f64,
    /// The move's frame once chosen; `None` picks one (herd centroid →
    /// target centroid).
    pub frame: Option<Frame>,
    /// What the herd's collars hold.
    pub limits: CollarLimits,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Send the target itself.
    Target,
    Step(Step),
    /// No usable step (shape problems); keep the current boundary.
    Hold(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub polygon: Polygon,
    pub frame: Frame,
    /// The back line: the rearmost animal's progress minus `0.6 × warn_m`.
    pub level: f64,
    /// Back line to the target's rear edge, metres.
    pub remaining_m: f64,
    /// Indices of animals the step doesn't hold with margin (outside the
    /// previous boundary, or cut off from the target).
    pub left_out: Vec<usize>,
    /// The previous boundary and the target don't touch, so the step spans
    /// their convex hull to give the herd a way across.
    pub corridor: bool,
}

/// Local metres around the target, and progress in a frame.
#[derive(Debug, Clone)]
pub struct Space {
    proj: Projection,
    target: Vec<P>,
}

impl Space {
    pub fn new(target: &Polygon) -> Option<Self> {
        let ring = target.outer_ring();
        let origin = target.centroid()?;
        let proj = Projection::new(origin);
        let target = ccw(proj.forward_ring(&ring));
        (target.len() >= 3).then_some(Self { proj, target })
    }

    pub fn local(&self, p: LonLat) -> P {
        self.proj.forward(p)
    }

    /// Signed distance to the target edge in metres, + inside.
    pub fn target_margin(&self, p: LonLat) -> f64 {
        signed_distance(self.local(p), &self.target)
    }

    pub fn progress(&self, frame: Frame, p: LonLat) -> f64 {
        match frame {
            Frame::Axis { axis } => dot(self.local(p), axis),
            Frame::Gather => self.target_margin(p).min(0.0),
        }
    }

    /// Where the back line meets the target.
    fn target_rear(&self, frame: Frame) -> f64 {
        match frame {
            Frame::Axis { axis } => self.target.iter().map(|v| dot(*v, axis)).fold(f64::INFINITY, f64::min),
            Frame::Gather => 0.0,
        }
    }
}

pub fn plan(input: &PlanInput) -> Plan {
    plan_with(input, Rear::Line)
}

/// [`plan`], with the back of the step drawn as `rear` says.
pub fn plan_with(input: &PlanInput, rear_edge: Rear<'_>) -> Plan {
    let Some(space) = Space::new(input.target) else { return Plan::Hold("The target has no area.".into()) };
    let w = input.warn_m.max(0.0);
    if input.animals.is_empty() {
        return Plan::Target;
    }
    let pts: Vec<P> = input.animals.iter().map(|p| space.local(*p)).collect();
    let in_target: Vec<f64> = pts.iter().map(|p| signed_distance(*p, &space.target)).collect();
    if in_target.iter().all(|m| *m >= w + SIDE_EXTRA_M) {
        return Plan::Target;
    }
    let frame = input.frame.unwrap_or_else(|| choose_frame(&pts, &space.target));
    let progress: Vec<f64> = input.animals.iter().map(|p| space.progress(frame, *p)).collect();
    let level = match (rear_edge, frame) {
        (Rear::Follow { quantile, .. }, Frame::Axis { .. }) => quantile_of(&progress, quantile),
        _ => progress.iter().copied().fold(f64::INFINITY, f64::min),
    } - BACK_FACTOR * w;
    let rear = space.target_rear(frame);
    // A followed back edge holds every animal on its own: the target goes once all are in it.
    let back_reached = matches!(rear_edge, Rear::Follow { .. }) || level >= rear;
    if back_reached && in_target.iter().all(|m| *m >= FINISH_MARGIN_M) {
        return Plan::Target;
    }
    let remaining_m = match frame {
        Frame::Axis { .. } => (rear - level).max(0.0),
        Frame::Gather => -progress.iter().copied().fold(0.0, f64::min),
    };

    // Where the step may go: previous ∪ target.
    let target = gpoly(&space.target);
    let area_ring: Vec<P> = match input.previous {
        Some(prev) => space.proj.forward_ring(&prev.outer_ring()),
        None => {
            let pad = input.paddock.map(|p| space.proj.forward_ring(&p.outer_ring())).filter(|r| r.len() >= 3 && pts.iter().all(|p| point_in_ring(*p, r)));
            pad.unwrap_or_else(|| grown_bbox(&space.target, &pts, 2.0 * (w + SIDE_EXTRA_M) + 10.0))
        }
    };
    if area_ring.len() < 3 {
        return Plan::Hold("The current boundary has no area.".into());
    }
    let area = gpoly(&ccw(area_ring.clone()));
    let mut region = without_crumbs(area.union(&target));
    let corridor = region.0.len() != 1;
    if corridor {
        let all: MultiPoint<f64> = area_ring.iter().chain(&space.target).map(|p| Point::new(p[0], p[1])).collect();
        region = MultiPolygon(vec![all.convex_hull()]);
    }
    let region_outer: Vec<P> = region.0.first().map(|p| ring_of(p.exterior())).unwrap_or_default();

    // The herd's hull: target plus buffered animals.
    let r = match frame {
        Frame::Axis { .. } => w + SIDE_EXTRA_M,
        Frame::Gather => BACK_FACTOR * w,
    };
    let r_out = r / (std::f64::consts::PI / DISC_SIDES as f64).cos();
    let mut hull_pts: Vec<Point<f64>> = space.target.iter().map(|p| Point::new(p[0], p[1])).collect();
    for p in &pts {
        for k in 0..DISC_SIDES {
            let a = std::f64::consts::TAU * k as f64 / DISC_SIDES as f64;
            hull_pts.push(Point::new(p[0] + r_out * a.cos(), p[1] + r_out * a.sin()));
        }
    }
    let hull = MultiPoint(hull_pts).convex_hull();

    // Holes: the target's always; the previous boundary's where they still fit.
    let limits = input.limits;
    let hole_rings = |p: &Polygon| -> Vec<Vec<LonLat>> { p.holes().filter(|h| h.len() >= 3).collect() };
    let target_holes = if limits.holes > 0 { hole_rings(input.target) } else { vec![] };
    let prev_holes = match input.previous {
        Some(prev) if limits.holes > 0 => hole_rings(prev),
        _ => vec![],
    };
    let reserved: usize = target_holes.iter().map(Vec::len).sum();
    let max_outer = limits.outer.min(limits.total.saturating_sub(reserved)).max(3);

    let Some(inner) = target.interior_point() else { return Plan::Hold("The target has no area.".into()) };
    // The hull keeps the sides close. In a non-convex area it can cut the
    // herd off from the target (an L-shaped paddock); then the area itself
    // ahead of the back line is the step.
    let mut outcome = Plan::Hold("No step holds the herd.".into());
    for use_hull in [true, false] {
        let mut clipped = if use_hull { hull.intersection(&region) } else { region.clone() };
        if let Frame::Axis { axis } = frame {
            let extent = extent_of(&hull, &region) * 4.0 + 100.0;
            let behind = match rear_edge {
                Rear::Line => half_plane(axis, level, extent),
                Rear::Follow { extra, .. } => {
                    let corners = ((max_outer as f64 * FOLLOW_CORNERS_SHARE) as usize).max(8);
                    follow_edge(axis, &pts, FOLLOW_FACTOR * w, extra, corners, extent)
                }
            };
            clipped = clipped.intersection(&behind);
        }
        let joined = without_crumbs(clipped.union(&target));
        // One piece: the one holding the target.
        let Some(piece) = joined.0.into_iter().find(|p| geo::Contains::contains(p, &inner)) else {
            outcome = Plan::Hold("The step lost the target.".into());
            continue;
        };
        if !piece.interiors().is_empty() {
            outcome = Plan::Hold("The step would have a hole.".into());
            continue;
        }
        let mut ring = ccw(ring_of(piece.exterior()));
        drop_flat_corners(&mut ring);

        // How much margin each animal keeps: half the warning band, or what
        // it had in the area already if that was less.
        let need: Vec<Option<f64>> = pts
            .iter()
            .map(|p| {
                let m_region = signed_distance(*p, &region_outer);
                let m = signed_distance(*p, &ring);
                let need = (0.5 * w).min(m_region) - 0.25;
                (m_region > 0.0 && m > 0.0 && m >= need).then_some(need.max(0.01))
            })
            .collect();
        let cut_off = pts.iter().zip(&need).any(|(p, n)| n.is_none() && signed_distance(*p, &region_outer) > 0.0);
        if use_hull && cut_off {
            continue;
        }
        if !reduce(&mut ring, max_outer, &pts, &need, &space.target) {
            outcome = Plan::Hold("The step needs more corners than a collar holds.".into());
            continue;
        }
        let lonlat = space.proj.inverse_ring(&ring);
        let Ok(valid) = validate_ring(&lonlat, Some(max_outer)) else {
            outcome = Plan::Hold("The step is not a valid collar boundary.".into());
            continue;
        };
        let Some(polygon) = with_holes(valid, &target_holes, &prev_holes, &limits, w) else {
            outcome = Plan::Hold("The step is not a valid collar boundary.".into());
            continue;
        };
        // An animal in a hole is outside the step, like one the step doesn't reach.
        let in_hole = |a: &LonLat| polygon.holes().any(|h| point_in_ring(*a, &h));
        let left_out: Vec<usize> = need.iter().zip(input.animals).enumerate().filter(|(_, (n, a))| n.is_none() || in_hole(a)).map(|(i, _)| i).collect();
        return Plan::Step(Step { polygon, frame, level, remaining_m, left_out, corridor });
    }
    outcome
}

/// The step ring with the target's holes, plus each of the previous
/// boundary's holes that still passes the collars' shape rules with it
/// (whole inside the step, the gap from every other ring, within the vertex
/// budget). `None` when even the target's holes don't pass.
fn with_holes(outer: Vec<LonLat>, target_holes: &[Vec<LonLat>], prev_holes: &[Vec<LonLat>], limits: &CollarLimits, warn_m: f64) -> Option<Polygon> {
    let ok = |p: &Polygon| shape::check(p, limits, warn_m, 0.0, SERVER_SLACK_M).is_ok();
    let mut polygon = Polygon::from_rings(outer, target_holes.iter().cloned());
    if !ok(&polygon) {
        return None;
    }
    for h in prev_holes {
        if target_holes.contains(h) {
            continue;
        }
        let mut next = polygon.clone();
        next.coordinates.push(Polygon::from_ring(h.clone()).coordinates.remove(0));
        if ok(&next) {
            polygon = next;
        }
    }
    Some(polygon)
}

/// Axis from the herd's centroid toward the target's centroid (the local
/// origin), unless the herd sits around the target: then gather.
fn choose_frame(pts: &[P], target: &[P]) -> Frame {
    let n = pts.len() as f64;
    let c = [pts.iter().map(|p| p[0]).sum::<f64>() / n, pts.iter().map(|p| p[1]).sum::<f64>() / n];
    let spread = (pts.iter().map(|p| (p[0] - c[0]).powi(2) + (p[1] - c[1]).powi(2)).sum::<f64>() / n).sqrt();
    let d = c[0].hypot(c[1]);
    if d < 1.0_f64.max(0.25 * spread) || (pts.len() > 1 && point_in_ring(c, target)) {
        return Frame::Gather;
    }
    Frame::Axis { axis: [-c[0] / d, -c[1] / d] }
}

/// Remove convex corners until the ring fits the collar. Cutting a convex
/// corner only shrinks the shape, so it stays inside the area. A cut is kept
/// only if the ring stays simple, the target stays whole and every held
/// animal keeps its margin. Smallest cuts first.
fn reduce(ring: &mut Vec<P>, max: usize, pts: &[P], need: &[Option<f64>], target: &[P]) -> bool {
    while ring.len() > max {
        let n = ring.len();
        let mut cands: Vec<(f64, usize)> = (0..n)
            .filter_map(|i| {
                let (a, b, c) = (ring[(i + n - 1) % n], ring[i], ring[(i + 1) % n]);
                let cross = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
                let on_target = signed_distance(b, target) > -0.05;
                (cross > 0.0 && !on_target).then_some((cross.abs(), i))
            })
            .collect();
        cands.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut done = false;
        for (_, i) in cands {
            let mut next = ring.clone();
            next.remove(i);
            if next.len() < 3 || signed_area(&next) <= 0.0 || !ring_is_simple(&next) {
                continue;
            }
            let (a, c) = (ring[(i + n - 1) % n], ring[(i + 1) % n]);
            let cuts_target = target.iter().any(|v| signed_distance(*v, &next) < -0.01)
                || (0..target.len()).any(|k| op_geo::ring::segments_cross(a, c, target[k], target[(k + 1) % target.len()]));
            if cuts_target {
                continue;
            }
            if pts.iter().zip(need).all(|(p, n)| n.is_none_or(|n| signed_distance(*p, &next) >= n)) {
                *ring = next;
                done = true;
                break;
            }
        }
        if !done {
            return false;
        }
    }
    true
}

/// Drop repeated and collinear corners (they don't change the shape).
fn drop_flat_corners(ring: &mut Vec<P>) {
    let mut i = 0;
    while ring.len() > 3 && i < ring.len() {
        let n = ring.len();
        let (a, b, c) = (ring[(i + n - 1) % n], ring[i], ring[(i + 1) % n]);
        let twice_area = ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).abs();
        let short = (b[0] - a[0]).hypot(b[1] - a[1]) < 0.05;
        if short || twice_area < 0.02 {
            ring.remove(i);
            i = i.saturating_sub(1);
        } else {
            i += 1;
        }
    }
}

/// Holes smaller than this (m²) in the ground a step may use are crumbs,
/// not ground to keep the herd off: where the previous boundary's edge
/// runs back and forth across the target's (the notches of a followed back
/// edge, with a metre between two strips), the two together enclose bits
/// of neither. Kept, they are a hole no step may have, and the move holds.
const CRUMB_M2: f64 = 10.0;

/// `m` without the holes smaller than [`CRUMB_M2`].
fn without_crumbs(m: MultiPolygon<f64>) -> MultiPolygon<f64> {
    use geo::Area;
    MultiPolygon(
        m.0.into_iter()
            .map(|p| {
                let (outer, holes) = p.into_inner();
                GPoly::new(outer, holes.into_iter().filter(|h| GPoly::new(h.clone(), vec![]).unsigned_area() >= CRUMB_M2).collect())
            })
            .collect(),
    )
}

/// The `q` quantile of `v` (0: the least), by nearest rank.
fn quantile_of(v: &[f64], q: f64) -> f64 {
    let mut s: Vec<f64> = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let i = ((s.len() as f64 - 1.0) * q.clamp(0.0, 1.0)).floor() as usize;
    s.get(i).copied().unwrap_or(f64::NEG_INFINITY)
}

/// Everything ahead of a back edge that follows the animals (see
/// [`Rear::Follow`]), as a big polygon: the lower envelope of a wedge
/// behind each animal, its apex placed so the animal is `margin` (plus its
/// `extra`) from both sides. In (sideways, forward) coordinates the wedge
/// of an animal at `(s, u)` is `u' = u - d + k |s' - s|`, `d = margin·√(1+k²)`.
fn follow_edge(axis: [f64; 2], pts: &[P], margin: f64, extra: &[f64], corners: usize, extent: f64) -> GPoly {
    let k = FOLLOW_SLOPE;
    let sec = (1.0 + k * k).sqrt();
    let side = [-axis[1], axis[0]];
    let mut v: Vec<(f64, f64)> =
        pts.iter().enumerate().map(|(i, p)| (dot(*p, side), dot(*p, axis) - (margin + extra.get(i).copied().unwrap_or(0.0).max(0.0)) * sec)).collect();
    v.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.total_cmp(&y.1)));
    // Apexes no other wedge is below.
    let n = v.len();
    let mut keep = vec![true; n];
    let mut best = f64::INFINITY; // min over the left of (a - k s)
    for i in 0..n {
        if best + k * v[i].0 <= v[i].1 {
            keep[i] = false;
        }
        best = best.min(v[i].1 - k * v[i].0);
    }
    let mut best = f64::INFINITY; // min over the right of (a + k s)
    for i in (0..n).rev() {
        if best - k * v[i].0 < v[i].1 {
            keep[i] = false;
        }
        best = best.min(v[i].1 + k * v[i].0);
    }
    let apex: Vec<(f64, f64)> = v.into_iter().zip(keep).filter(|(_, k)| *k).map(|(a, _)| a).collect();
    let (Some(first), Some(last)) = (apex.first().copied(), apex.last().copied()) else {
        return gpoly(&[]);
    };
    let (s_lo, s_hi) = (first.0 - extent, last.0 + extent);
    let mut line: Vec<(f64, f64)> = vec![(s_lo, first.1 + k * (first.0 - s_lo))];
    for (i, &(s, a)) in apex.iter().enumerate() {
        line.push((s, a));
        if let Some(&(s2, a2)) = apex.get(i + 1) {
            let sp = (a2 - a + k * (s + s2)) / (2.0 * k);
            line.push((sp, a + k * (sp - s)));
        }
    }
    line.push((s_hi, last.1 + k * (s_hi - last.0)));
    // Straighten the smallest bumps until the corners fit: a corner above
    // the line between its neighbours goes, which only moves the edge back.
    while line.len() > corners + 2 {
        let bump = |j: usize| {
            let ((s0, u0), (s1, u1), (s2, u2)) = (line[j - 1], line[j], line[j + 1]);
            let chord = if s2 > s0 { u0 + (u2 - u0) * (s1 - s0) / (s2 - s0) } else { u0.min(u2) };
            u1 - chord
        };
        let Some((j, _)) = (1..line.len() - 1).map(|j| (j, bump(j))).filter(|(_, b)| *b >= 0.0).min_by(|a, b| a.1.total_cmp(&b.1)) else { break };
        line.remove(j);
    }
    let top = line.iter().map(|x| x.1).fold(f64::NEG_INFINITY, f64::max) + extent;
    line.push((s_hi, top));
    line.push((s_lo, top));
    let ring: Vec<P> = line.iter().map(|&(s, u)| [axis[0] * u + side[0] * s, axis[1] * u + side[1] * s]).collect();
    gpoly(&ccw(ring))
}

/// Everything with progress ≥ `level` along `axis`, as a big rectangle.
fn half_plane(axis: [f64; 2], level: f64, extent: f64) -> GPoly {
    let n = [-axis[1], axis[0]];
    let at = |s: f64, t: f64| [axis[0] * s + n[0] * t, axis[1] * s + n[1] * t];
    gpoly(&ccw(vec![at(level, -extent), at(level + extent, -extent), at(level + extent, extent), at(level, extent)]))
}

fn extent_of(a: &GPoly, b: &MultiPolygon<f64>) -> f64 {
    let mut m: f64 = 0.0;
    for c in a.exterior().coords().chain(b.0.iter().flat_map(|p| p.exterior().coords())) {
        m = m.max(c.x.abs()).max(c.y.abs());
    }
    m
}

fn grown_bbox(target: &[P], pts: &[P], grow: f64) -> Vec<P> {
    let all = target.iter().chain(pts);
    let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for p in all {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    vec![[x0 - grow, y0 - grow], [x1 + grow, y0 - grow], [x1 + grow, y1 + grow], [x0 - grow, y1 + grow]]
}

/// Signed distance from a point to a ring's edge, + inside.
pub fn signed_distance(p: P, ring: &[P]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return f64::NEG_INFINITY;
    }
    let d = (0..n).map(|i| distance_to_segment(p, ring[i], ring[(i + 1) % n])).fold(f64::INFINITY, f64::min);
    if point_in_ring(p, ring) { d } else { -d }
}

fn dot(a: P, b: P) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}

fn ccw(mut ring: Vec<P>) -> Vec<P> {
    ring = clean_ring(&ring);
    if signed_area(&ring) < 0.0 {
        ring.reverse();
    }
    ring
}

fn gpoly(ring: &[P]) -> GPoly {
    let mut coords: Vec<Coord<f64>> = ring.iter().map(|p| Coord { x: p[0], y: p[1] }).collect();
    if let Some(first) = coords.first().copied() {
        coords.push(first);
    }
    GPoly::new(LineString(coords), vec![])
}

fn ring_of(ls: &LineString<f64>) -> Vec<P> {
    clean_ring(&ls.coords().map(|c| [c.x, c.y]).collect::<Vec<_>>())
}

/// Area in m² of `a` outside `b`, both in the same local metres.
#[cfg(test)]
fn area_outside(a: &[P], b: &[P]) -> f64 {
    use geo::Area;
    gpoly(&ccw(a.to_vec())).difference(&gpoly(&ccw(b.to_vec()))).unsigned_area()
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Area;

    const ORIGIN: LonLat = [-79.25, 38.1];
    const W: f64 = 5.0;

    fn proj() -> Projection {
        Projection::new(ORIGIN)
    }
    fn at(x: f64, y: f64) -> LonLat {
        proj().inverse([x, y])
    }
    fn poly(pts: &[[f64; 2]]) -> Polygon {
        Polygon::from_ring(pts.iter().map(|p| at(p[0], p[1])).collect::<Vec<_>>())
    }
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon {
        poly(&[[x0, y0], [x1, y0], [x1, y1], [x0, y1]])
    }
    fn local(p: &Polygon) -> Vec<P> {
        proj().forward_ring(&p.outer_ring())
    }
    fn animals(pts: &[[f64; 2]]) -> Vec<LonLat> {
        pts.iter().map(|p| at(p[0], p[1])).collect()
    }
    /// A deterministic scatter of `n` points in a rectangle.
    fn scatter(n: usize, x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<[f64; 2]> {
        let mut s: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f64 / (1u64 << 53) as f64
        };
        (0..n).map(|_| [x0 + (x1 - x0) * next(), y0 + (y1 - y0) * next()]).collect()
    }

    /// The invariants every step keeps.
    fn check_step(input: &PlanInput, step: &Step) {
        let ring = local(&step.polygon);
        assert!(ring.len() >= 3 && ring.len() <= input.limits.outer, "{} corners", ring.len());
        shape::check(&step.polygon, &input.limits, W, 1.0, SERVER_SLACK_M).unwrap();
        assert!(ring_is_simple(&ring), "not simple");
        assert!(step.polygon.is_valid());
        let target = local(input.target);
        let area = match input.previous {
            Some(p) => local(p),
            None => input.paddock.map(local).unwrap(),
        };
        if !step.corridor {
            // Never outside previous ∪ target (a centimetre for rounding).
            let union = gpoly(&ccw(area.clone())).union(&gpoly(&ccw(target.clone())));
            let out = gpoly(&ccw(ring.clone())).difference(&union).unsigned_area();
            assert!(out < 0.5, "{out} m² outside previous ∪ target");
        }
        // The target is held.
        assert!(area_outside(&target, &ring) < 0.5, "step cuts the target");
        // Every animal inside, with margin.
        let region = gpoly(&ccw(area.clone())).union(&gpoly(&ccw(target.clone())));
        let region_ring = ring_of(region.0[0].exterior());
        for (i, a) in input.animals.iter().enumerate() {
            let p = proj().forward(*a);
            let m = signed_distance(p, &ring);
            let m_region = signed_distance(p, &region_ring);
            if step.left_out.contains(&i) {
                continue;
            }
            assert!(m > 0.0 && m >= (0.5 * W).min(m_region) - 0.3, "animal {i} margin {m:.2} (area {m_region:.2})");
        }
    }

    fn step_of(input: &PlanInput) -> Step {
        match plan(input) {
            Plan::Step(s) => {
                check_step(input, &s);
                s
            }
            other => panic!("expected a step, got {other:?}"),
        }
    }

    #[test]
    fn herd_already_inside_gets_the_target() {
        let target = rect(0.0, 0.0, 100.0, 100.0);
        let paddock = rect(-50.0, -50.0, 200.0, 200.0);
        let herd = animals(&scatter(12, 20.0, 20.0, 80.0, 80.0));
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        assert_eq!(plan(&input), Plan::Target);
        // No animals: nothing to sweep.
        let input = PlanInput { animals: &[], ..input };
        assert_eq!(plan(&input), Plan::Target);
    }

    #[test]
    fn dispersed_herd_to_a_corner_target() {
        let paddock = rect(0.0, 0.0, 300.0, 200.0);
        let target = rect(250.0, 150.0, 300.0, 200.0);
        let herd = animals(&scatter(12, 10.0, 10.0, 240.0, 190.0));
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let s = step_of(&input);
        let Frame::Axis { axis } = s.frame else { panic!("expected an axis, got {:?}", s.frame) };
        assert!(axis[0] > 0.3 && axis[1] > 0.1, "toward the NE corner: {axis:?}");
        assert!(s.left_out.is_empty());
        assert!(s.remaining_m > 100.0, "{}", s.remaining_m);
        // Exactly one animal (the rearmost) sits in the back warning band.
        let space = Space::new(&target).unwrap();
        let prog: Vec<f64> = herd.iter().map(|a| space.progress(s.frame, *a)).collect();
        let rear = prog.iter().copied().fold(f64::INFINITY, f64::min);
        assert!((rear - BACK_FACTOR * W - s.level).abs() < 1e-9);
        // The step is smaller than the paddock: the corner behind the herd is cut.
        assert!(s.polygon.area_ha() < paddock.area_ha());
    }

    #[test]
    fn stepping_forward_shrinks_and_ends_with_the_target() {
        // Idealised herd: each step, everyone walks 15 m along the axis,
        // staying inside the current step. The sweep must finish.
        let paddock = rect(0.0, 0.0, 300.0, 200.0);
        let target = rect(250.0, 150.0, 300.0, 200.0);
        let mut pts = scatter(12, 10.0, 10.0, 240.0, 190.0);
        let mut prev = paddock.clone();
        let mut frame = None;
        let mut last_level = f64::NEG_INFINITY;
        for step in 0..60 {
            let herd = animals(&pts);
            let input = PlanInput { target: &target, previous: Some(&prev), paddock: None, animals: &herd, warn_m: W, frame, limits: CollarLimits::V0 };
            match plan(&input) {
                Plan::Target => {
                    assert!(step > 3, "finished too soon");
                    let space = Space::new(&target).unwrap();
                    assert!(herd.iter().all(|a| space.target_margin(*a) >= FINISH_MARGIN_M));
                    return;
                }
                Plan::Step(s) => {
                    check_step(&input, &s);
                    assert!(s.level > last_level);
                    last_level = s.level;
                    let Frame::Axis { axis } = s.frame else { panic!() };
                    frame = Some(s.frame);
                    let ring = local(&s.polygon);
                    let target_l = local(&target);
                    for p in pts.iter_mut() {
                        // Walk toward the target, staying 3 m inside the step,
                        // and finally into the target.
                        let goal = [275.0, 175.0];
                        let d = [goal[0] - p[0], goal[1] - p[1]];
                        let len = d[0].hypot(d[1]);
                        let dir = if signed_distance(*p, &target_l) > 0.0 && len < 15.0 { [0.0, 0.0] } else { [d[0] / len, d[1] / len] };
                        let mut q = [p[0] + (dir[0] * 0.5 + axis[0] * 0.5) * 15.0, p[1] + (dir[1] * 0.5 + axis[1] * 0.5) * 15.0];
                        if signed_distance(q, &ring) < 3.0 {
                            q = [p[0] + dir[0] * 15.0, p[1] + dir[1] * 15.0];
                        }
                        if signed_distance(q, &ring) >= 3.0 {
                            *p = q;
                        }
                    }
                    prev = s.polygon;
                }
                Plan::Hold(why) => panic!("hold: {why}"),
            }
        }
        panic!("the sweep never finished");
    }

    #[test]
    fn target_on_the_far_side_of_the_paddock() {
        let paddock = rect(0.0, 0.0, 400.0, 120.0);
        let target = rect(340.0, 10.0, 395.0, 110.0);
        let herd = animals(&scatter(10, 5.0, 5.0, 80.0, 115.0));
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let s = step_of(&input);
        let Frame::Axis { axis } = s.frame else { panic!() };
        assert!(axis[0] > 0.95, "{axis:?}");
        assert!(s.remaining_m > 250.0);
    }

    #[test]
    fn l_shaped_paddock() {
        // An L: a bottom leg 0..300 x 0..80 and a left leg 0..80 x 0..300.
        let paddock = poly(&[[0.0, 0.0], [300.0, 0.0], [300.0, 80.0], [80.0, 80.0], [80.0, 300.0], [0.0, 300.0]]);
        let target = rect(5.0, 250.0, 75.0, 295.0);
        let herd = animals(&scatter(12, 150.0, 10.0, 290.0, 70.0));
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let s = step_of(&input);
        assert!(s.left_out.is_empty(), "{:?}", s.left_out);
        assert!(!s.corridor);
        // The inner corner of the L stays outside the step.
        assert!(!s.polygon.contains(at(150.0, 150.0)));
    }

    #[test]
    fn stragglers_left_out_of_the_input_can_end_up_outside() {
        let paddock = rect(0.0, 0.0, 300.0, 200.0);
        let target = rect(250.0, 150.0, 300.0, 200.0);
        let herd_pts = scatter(10, 120.0, 60.0, 240.0, 190.0);
        let straggler = at(15.0, 15.0);
        let herd = animals(&herd_pts);
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let s = step_of(&input);
        assert!(!s.polygon.contains(straggler), "the straggler is behind the back line");
        let mut with = herd.clone();
        with.push(straggler);
        let input = PlanInput { animals: &with, ..input };
        let s = step_of(&input);
        assert!(s.polygon.contains(straggler), "in the sweep it is held");
    }

    #[test]
    fn one_animal() {
        let paddock = rect(0.0, 0.0, 200.0, 200.0);
        let target = rect(150.0, 150.0, 200.0, 200.0);
        let herd = animals(&[[30.0, 40.0]]);
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let s = step_of(&input);
        assert!(matches!(s.frame, Frame::Axis { .. }));
    }

    #[test]
    fn collinear_animals() {
        let paddock = rect(0.0, 0.0, 200.0, 200.0);
        let target = rect(150.0, 150.0, 200.0, 200.0);
        let herd = animals(&(0..8).map(|i| [20.0 + 10.0 * i as f64, 30.0]).collect::<Vec<_>>());
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        step_of(&input);
        // Collinear along the axis too.
        let herd = animals(&(0..8).map(|i| [20.0 + 10.0 * i as f64, 20.0 + 10.0 * i as f64]).collect::<Vec<_>>());
        let input = PlanInput { animals: &herd, ..input };
        step_of(&input);
    }

    #[test]
    fn target_inside_the_herd_hull_gathers() {
        let paddock = rect(0.0, 0.0, 300.0, 300.0);
        let target = rect(130.0, 130.0, 170.0, 170.0);
        let herd = animals(&[[40.0, 40.0], [260.0, 40.0], [260.0, 260.0], [40.0, 260.0], [150.0, 150.0], [100.0, 200.0]]);
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let s = step_of(&input);
        assert_eq!(s.frame, Frame::Gather);
        // The corner animals are in their warning band, not outside.
        for a in &herd[..4] {
            let m = signed_distance(proj().forward(*a), &local(&s.polygon));
            assert!(m > 0.0 && m <= W, "{m}");
        }
        assert!(s.remaining_m > 100.0);
    }

    #[test]
    fn no_previous_boundary_uses_the_paddock_or_a_box() {
        let paddock = rect(0.0, 0.0, 300.0, 200.0);
        let target = rect(250.0, 150.0, 300.0, 200.0);
        let herd = animals(&scatter(8, 10.0, 10.0, 200.0, 190.0));
        let input = PlanInput { target: &target, previous: None, paddock: Some(&paddock), animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        step_of(&input);
        // Herd outside the paddock: a box around target and herd.
        let herd = animals(&scatter(8, -200.0, -100.0, -100.0, -20.0));
        let input = PlanInput { animals: &herd, ..input };
        match plan(&input) {
            Plan::Step(s) => {
                assert!(s.left_out.is_empty());
                assert!(herd.iter().all(|a| s.polygon.contains(*a)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn animal_outside_the_current_boundary_is_left_out() {
        let paddock = rect(0.0, 0.0, 300.0, 200.0);
        let target = rect(250.0, 150.0, 300.0, 200.0);
        let herd = animals(&[[50.0, 50.0], [100.0, 80.0], [-20.0, 60.0]]);
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let s = step_of(&input);
        assert_eq!(s.left_out, vec![2]);
    }

    #[test]
    fn many_corners_are_reduced_to_what_a_collar_holds() {
        // A 60-corner round paddock and a 40-corner round target.
        let circle = |cx: f64, cy: f64, r: f64, n: usize| {
            poly(
                &(0..n)
                    .map(|i| {
                        let a = std::f64::consts::TAU * i as f64 / n as f64;
                        [cx + r * a.cos(), cy + r * a.sin()]
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let paddock = circle(0.0, 0.0, 200.0, 60);
        let target = circle(140.0, 0.0, 40.0, 40);
        let herd = animals(&scatter(30, -150.0, -100.0, 0.0, 100.0));
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let s = step_of(&input);
        assert!(s.polygon.outer_ring().len() <= CollarLimits::V0.outer);
        // Collars that hold fewer corners get fewer.
        let input = PlanInput { limits: CollarLimits { outer: 24, ..CollarLimits::V0 }, ..input };
        let s = step_of(&input);
        assert!(s.polygon.outer_ring().len() <= 24);
    }

    #[test]
    fn the_targets_holes_are_in_every_step() {
        let paddock = rect(0.0, 0.0, 300.0, 200.0);
        // The target's hole (a pond), and a hole in the paddock the herd is leaving.
        let pond = poly(&[[260.0, 160.0], [280.0, 160.0], [280.0, 180.0], [260.0, 180.0]]).coordinates.remove(0);
        let target = Polygon::from_rings(rect(230.0, 130.0, 300.0, 200.0).coordinates.remove(0), [pond.clone()]);
        let barn = poly(&[[150.0, 100.0], [170.0, 100.0], [170.0, 120.0], [150.0, 120.0]]).coordinates.remove(0);
        let previous = Polygon::from_rings(paddock.coordinates[0].clone(), [barn.clone()]);
        let herd = animals(&scatter(10, 10.0, 10.0, 120.0, 90.0));
        let input = PlanInput { target: &target, previous: Some(&previous), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let s = step_of(&input);
        let holes: Vec<Vec<LonLat>> = s.polygon.holes().collect();
        assert!(holes.contains(&clean_ring(&pond)), "the target's hole");
        assert!(holes.contains(&clean_ring(&barn)), "the barn is still inside the step");
        assert!(!s.polygon.contains(at(270.0, 170.0)) && !s.polygon.contains(at(160.0, 110.0)));
        // Legacy-only limits carry no holes (the collar can't hold them).
        let input = PlanInput { limits: CollarLimits::LEGACY, ..input };
        match plan(&input) {
            Plan::Step(s) => assert_eq!(s.polygon.coordinates.len(), 1),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn disjoint_paddocks_use_a_corridor() {
        let paddock = rect(0.0, 0.0, 100.0, 100.0);
        let target = rect(103.0, 0.0, 200.0, 100.0);
        let herd = animals(&scatter(6, 10.0, 10.0, 90.0, 90.0));
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let s = step_of(&input);
        assert!(s.corridor);
        assert!(s.left_out.is_empty());
    }

    fn follow(input: &PlanInput) -> Step {
        match plan_with(input, Rear::Follow { quantile: 0.1, extra: &[] }) {
            Plan::Step(s) => {
                check_step(input, &s);
                s
            }
            other => panic!("expected a step, got {other:?}"),
        }
    }

    /// Distance from an animal to the step's edge, metres.
    fn margin_in(step: &Step, a: LonLat) -> f64 {
        signed_distance(proj().forward(a), &local(&step.polygon))
    }

    #[test]
    fn a_followed_back_edge_holds_every_animal_and_cues_the_whole_rear() {
        let paddock = rect(0.0, 0.0, 300.0, 200.0);
        let target = rect(250.0, 0.0, 300.0, 200.0);
        let herd = animals(&scatter(60, 40.0, 60.0, 110.0, 140.0));
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let line = step_of(&input);
        let s = follow(&input);
        assert!(s.left_out.is_empty());
        // Nobody closer to the edge than the followed margin (a centimetre for rounding).
        let margins: Vec<f64> = herd.iter().map(|a| margin_in(&s, *a)).collect();
        assert!(margins.iter().all(|m| *m >= FOLLOW_FACTOR * W - 0.01), "{margins:?}");
        // The back of the herd is in its warning band at once, not one animal.
        let cued = margins.iter().filter(|m| **m < W).count();
        let cued_by_line = herd.iter().filter(|a| margin_in(&line, **a) < W).count();
        assert!(cued >= 5 && cued > 2 * cued_by_line, "followed {cued}, line {cued_by_line}");
        // Its level: where nine in ten are ahead.
        let space = Space::new(&target).unwrap();
        let mut prog: Vec<f64> = herd.iter().map(|a| space.progress(s.frame, *a)).collect();
        prog.sort_by(|a, b| a.total_cmp(b));
        assert!((s.level - (prog[5] - BACK_FACTOR * W)).abs() < 1e-9, "{} vs {}", s.level, prog[5]);
    }

    #[test]
    fn a_lagging_animal_holds_back_only_its_own_stretch_of_the_edge() {
        let paddock = rect(0.0, 0.0, 300.0, 200.0);
        let target = rect(250.0, 0.0, 300.0, 200.0);
        // A herd across y 40..160 at x 100..130, and one animal 30 m behind its middle.
        let mut pts = scatter(40, 100.0, 40.0, 130.0, 160.0);
        pts.push([70.0, 100.0]);
        let herd = animals(&pts);
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let line = step_of(&input);
        let s = follow(&input);
        let laggard = *herd.last().unwrap();
        assert!(margin_in(&s, laggard) >= FOLLOW_FACTOR * W - 0.01, "the laggard is held");
        // Either side of it the straight line sits back with the laggard; the followed edge doesn't.
        for behind in [at(85.0, 70.0), at(85.0, 130.0)] {
            assert!(line.polygon.contains(behind));
            assert!(!s.polygon.contains(behind));
        }
        assert!(s.polygon.area_ha() < line.polygon.area_ha());
    }

    #[test]
    fn room_for_an_uncertain_position_moves_the_edge_back_behind_that_animal_only() {
        let paddock = rect(0.0, 0.0, 300.0, 200.0);
        let target = rect(250.0, 0.0, 300.0, 200.0);
        let herd = animals(&[[100.0, 60.0], [100.0, 100.0], [100.0, 140.0], [120.0, 100.0]]);
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let plain = follow(&input);
        let extra = [0.0, 1.0, 0.0, 0.0];
        let Plan::Step(roomy) = plan_with(&input, Rear::Follow { quantile: 0.1, extra: &extra }) else { panic!("a step") };
        check_step(&input, &roomy);
        let (m0, m1) = (margin_in(&plain, herd[1]), margin_in(&roomy, herd[1]));
        assert!((m1 - m0 - 1.0).abs() < 0.05, "{m0} -> {m1}");
        for i in [0, 2] {
            assert!((margin_in(&plain, herd[i]) - margin_in(&roomy, herd[i])).abs() < 0.05, "animal {i} keeps its margin");
        }
    }

    #[test]
    fn a_followed_edge_fits_what_the_collars_hold() {
        let paddock = rect(0.0, 0.0, 400.0, 400.0);
        let target = rect(300.0, 0.0, 400.0, 400.0);
        let herd = animals(&scatter(250, 60.0, 80.0, 180.0, 320.0));
        for limits in [CollarLimits::V0, CollarLimits::LEGACY] {
            let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits };
            let s = follow(&input);
            assert!(s.polygon.outer_ring().len() <= limits.outer, "{} corners for {}", s.polygon.outer_ring().len(), limits.outer);
            assert!(s.left_out.is_empty());
        }
    }

    #[test]
    fn stepping_forward_with_a_followed_edge_ends_with_the_target() {
        let paddock = rect(0.0, 0.0, 300.0, 200.0);
        let target = rect(250.0, 150.0, 300.0, 200.0);
        let mut pts = scatter(30, 10.0, 10.0, 240.0, 190.0);
        let mut prev = paddock.clone();
        let mut frame = None;
        let mut last_level = f64::NEG_INFINITY;
        for step in 0..120 {
            let herd = animals(&pts);
            let input = PlanInput { target: &target, previous: Some(&prev), paddock: None, animals: &herd, warn_m: W, frame, limits: CollarLimits::V0 };
            match plan_with(&input, Rear::Follow { quantile: 0.1, extra: &[] }) {
                Plan::Target => {
                    assert!(step > 3, "finished too soon");
                    let space = Space::new(&target).unwrap();
                    assert!(herd.iter().all(|a| space.target_margin(*a) >= FINISH_MARGIN_M));
                    return;
                }
                Plan::Step(s) => {
                    check_step(&input, &s);
                    assert!(s.level >= last_level - 1e-9);
                    last_level = s.level;
                    frame = Some(s.frame);
                    let ring = local(&s.polygon);
                    // Animals in the warning band walk off it toward the target, staying inside.
                    let goal = [275.0, 175.0];
                    for p in pts.iter_mut() {
                        if signed_distance(*p, &ring) >= W {
                            continue;
                        }
                        let d = [goal[0] - p[0], goal[1] - p[1]];
                        let len = d[0].hypot(d[1]).max(1e-9);
                        for k in (1..=8).rev() {
                            let q = [p[0] + d[0] / len * k as f64, p[1] + d[1] / len * k as f64];
                            if signed_distance(q, &ring) >= 1.0 {
                                *p = q;
                                break;
                            }
                        }
                    }
                    prev = s.polygon;
                }
                Plan::Hold(why) => panic!("hold: {why}"),
            }
        }
        panic!("the sweep never finished");
    }

    /// Found by stream L's soak: a sweep ended with its followed back edge
    /// notched, one notch reaching over the metre between two strips into
    /// the next target and its edge dipping back across the target's. The
    /// two together enclose a crumb of ground in neither, and the next move
    /// held ("The step would have a hole") for its whole half hour.
    #[test]
    fn a_notched_edge_across_the_next_target_leaves_no_crumb_to_hold_on() {
        let previous =
            poly(&[[0.0, 0.0], [100.0, 0.0], [100.0, 90.0], [102.5, 92.0], [100.8, 94.0], [102.4, 96.0], [100.0, 98.0], [100.0, 200.0], [0.0, 200.0]]);
        let target = rect(101.0, 0.0, 200.0, 200.0);
        let paddock = rect(0.0, 0.0, 200.0, 200.0);
        let herd = animals(&scatter(40, 60.0, 40.0, 95.0, 160.0));
        let input =
            PlanInput { target: &target, previous: Some(&previous), paddock: Some(&paddock), animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        for rear in [Rear::Line, Rear::Follow { quantile: 0.1, extra: &[] }] {
            match plan_with(&input, rear) {
                Plan::Step(s) => {
                    check_step(&input, &s);
                    assert!(s.left_out.is_empty(), "{rear:?}: {:?}", s.left_out);
                }
                other => panic!("{rear:?}: expected a step, got {other:?}"),
            }
        }
    }

    /// The sweep driver plans on every fix while a herd moves: 250 animals
    /// must stay far inside a report's budget, even in a debug build.
    #[test]
    fn planning_a_step_for_250_animals_is_fast() {
        let paddock = rect(0.0, 0.0, 400.0, 400.0);
        let target = rect(300.0, 0.0, 400.0, 400.0);
        let herd = animals(&scatter(250, 60.0, 80.0, 180.0, 320.0));
        let input = PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: None, limits: CollarLimits::V0 };
        let extra = vec![0.5; herd.len()];
        let mut ms: Vec<f64> = (0..20)
            .map(|_| {
                let t = std::time::Instant::now();
                assert!(matches!(plan_with(&input, Rear::Follow { quantile: 0.1, extra: &extra }), Plan::Step(_)));
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        ms.sort_by(|a, b| a.total_cmp(b));
        let p95 = ms[18];
        eprintln!("plan for 250 animals: p50 {:.1} ms, p95 {p95:.1} ms", ms[10]);
        assert!(p95 < 150.0, "p95 {p95:.1} ms");
    }

    #[test]
    fn a_fixed_frame_is_kept() {
        let paddock = rect(0.0, 0.0, 300.0, 200.0);
        let target = rect(250.0, 150.0, 300.0, 200.0);
        let herd = animals(&scatter(8, 10.0, 10.0, 200.0, 190.0));
        let frame = Frame::Axis { axis: [1.0, 0.0] };
        let input =
            PlanInput { target: &target, previous: Some(&paddock), paddock: None, animals: &herd, warn_m: W, frame: Some(frame), limits: CollarLimits::V0 };
        assert_eq!(step_of(&input).frame, frame);
    }
}
