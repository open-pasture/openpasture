//! Shape rules for boundaries sent to collars (protocol v1, §3.5), and fitting
//! a shape to a collar's limits.
//!
//! # [`check`]: what a collar accepts
//!
//! The firmware and the server run the same rules in the same order; the first
//! rule that fails is the rejection code:
//!
//! 1. `bad_margins`: `warn_m` or `hysteresis_m` not finite, or outside 0-1000.
//! 2. `too_many_holes`: more holes than `limits.holes`.
//! 3. `out_of_range`: a coordinate not finite, |lon| > 180 or |lat| > 90.
//! 4. `too_few_vertices`: a ring with fewer than 3 vertices once converted to
//!    e7 integers (`round(deg × 1e7)`, ties away from zero), with consecutive
//!    duplicates and a closing vertex equal to the first removed. A missing
//!    outer ring has 0. Every count below is this count.
//! 5. `too_many_vertices`: the outer ring over `limits.outer`, a hole over
//!    `limits.hole_vertices`, or every ring together over `limits.total`.
//! 6. `self_intersecting`: two edges of one ring meet anywhere other than the
//!    vertex neighbouring edges share. A ring that touches itself at a vertex,
//!    or folds back along itself, counts.
//! 7. `rings_cross`: an edge of one ring meets an edge of another ring,
//!    touching included.
//! 8. `hole_outside`: a hole's first vertex is not inside the outer ring.
//! 9. `holes_overlap`: a hole's first vertex is inside another hole.
//! 10. `zero_area`: the outer ring encloses less than 1 m².
//! 11. `hole_too_small`: a hole encloses less than 100 m².
//! 12. `hole_too_close`: two rings are closer than [`min_gap_m`]`(warn_m)` +
//!     `slack_m` (the collar passes 0, the server [`SERVER_SLACK_M`]).
//!
//! **Topology (6-9) is exact** on the e7 integers. An orientation sign
//! compares two int64 products, `(bx-ax)(cy-ay)` against `(by-ay)(cx-ax)`;
//! each is a longitude difference (≤ 3.6e9) times a latitude difference
//! (≤ 1.8e9), under 2^63, so nothing overflows and nothing is subtracted.
//! Segments meet when the usual orientation test says so, collinear overlaps
//! and touching ends included. Point in ring is the even-odd rule toward +x,
//! with the crossing test done as the same product comparison.
//!
//! **Areas and gaps (10-12) are single precision.** Every vertex is projected
//! about the outer ring's first vertex `(lon0, lat0)`:
//! `kx = (float)(M_PER_DEG_LAT · cos(lat0_deg · π/180) · 1e-7)`,
//! `ky = (float)(M_PER_DEG_LAT · 1e-7)` (both computed in double, then
//! rounded), `x = (float)(lon − lon0) · kx`, `y = (float)(lat − lat0) · ky`,
//! with `lat0_deg = lat0 / 1e7` and `M_PER_DEG_LAT` = 6 371 008.8 · π / 180.
//! A ring's area is the fan sum from its first vertex,
//! `|Σ_{i=1}^{n-2} (v_i − v_0) × (v_{i+1} − v_0)| / 2`, summed in index order.
//! The distance between two rings is the least distance from any vertex of
//! one to any edge of the other (the rings don't meet by then), each
//! point-segment distance computed as in [`crate::ring::distance_to_segment`]
//! in float. The gap comparison is `(double)d < gap`. A ring pair whose
//! bounding boxes are at least the gap apart may be skipped: it can't fail.
//!
//! # [`fit`]: make a shape fit
//!
//! Only ever makes the fenced area smaller: outer rings only shrink, holes
//! only grow. Corners are cut (a convex corner of the grazeable area removed)
//! or edges collapsed (two concave corners replaced by the meeting point of
//! their neighbouring edges), cheapest area first, never creating a crossing
//! and, where it can, never bringing two rings closer than they were (below
//! the gap it keeps). Too many holes merge into their convex hulls; a hole
//! that ends up touching the outer ring is cut out of it. With `holes == 0`
//! (LEGACY) holes are dropped: a legacy collar can't enforce them, which the
//! pre-send check reports as `collars_no_holes`.

use serde::{Deserialize, Serialize};

use crate::LonLat;
use crate::limits::CollarLimits;
use crate::polygon::{Polygon, PolygonType};
use crate::projection::{M_PER_DEG_LAT, Projection, round7};
use crate::ring::clean_ring;

/// Why a collar refuses a shape. Each maps one to one to a protocol rejection code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeCode {
    BadMargins,
    TooManyHoles,
    OutOfRange,
    TooFewVertices,
    TooManyVertices,
    SelfIntersecting,
    RingsCross,
    HoleOutside,
    HolesOverlap,
    ZeroArea,
    HoleTooSmall,
    HoleTooClose,
}

impl ShapeCode {
    /// Every code, in check order.
    pub const ALL: [ShapeCode; 12] = [
        Self::BadMargins,
        Self::TooManyHoles,
        Self::OutOfRange,
        Self::TooFewVertices,
        Self::TooManyVertices,
        Self::SelfIntersecting,
        Self::RingsCross,
        Self::HoleOutside,
        Self::HolesOverlap,
        Self::ZeroArea,
        Self::HoleTooSmall,
        Self::HoleTooClose,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadMargins => "bad_margins",
            Self::TooManyHoles => "too_many_holes",
            Self::OutOfRange => "out_of_range",
            Self::TooFewVertices => "too_few_vertices",
            Self::TooManyVertices => "too_many_vertices",
            Self::SelfIntersecting => "self_intersecting",
            Self::RingsCross => "rings_cross",
            Self::HoleOutside => "hole_outside",
            Self::HolesOverlap => "holes_overlap",
            Self::ZeroArea => "zero_area",
            Self::HoleTooSmall => "hole_too_small",
            Self::HoleTooClose => "hole_too_close",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// One short sentence for people.
    pub fn message(self) -> &'static str {
        match self {
            Self::BadMargins => "The warning distance or hysteresis is out of range.",
            Self::TooManyHoles => "The shape has more holes than the collar holds.",
            Self::OutOfRange => "The shape has a coordinate out of range.",
            Self::TooFewVertices => "A ring needs at least 3 corners.",
            Self::TooManyVertices => "The shape has more corners than the collar holds.",
            Self::SelfIntersecting => "A ring crosses itself.",
            Self::RingsCross => "Two rings cross or touch.",
            Self::HoleOutside => "A hole is outside the boundary.",
            Self::HolesOverlap => "Two holes overlap.",
            Self::ZeroArea => "The shape has no area.",
            Self::HoleTooSmall => "A hole is smaller than 100 m².",
            Self::HoleTooClose => "Two rings are too close together.",
        }
    }
}

impl std::fmt::Display for ShapeCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Largest `warn_m` or `hysteresis_m` a collar accepts.
pub const MAX_MARGIN_M: f64 = 1000.0;
/// Smallest outer ring a collar accepts.
pub const MIN_AREA_M2: f32 = 1.0;
/// Smallest hole a collar accepts.
pub const MIN_HOLE_AREA_M2: f32 = 100.0;
/// What the server adds to the gap so rounding never makes a collar reject a
/// shape the server accepted.
pub const SERVER_SLACK_M: f64 = 0.5;
/// The firmware's default warning distance, used when a caller has none.
pub const DEFAULT_WARN_M: f64 = 5.0;

/// Least distance between any two rings a collar accepts: two warning zones
/// plus 2 m, so no corridor between rings is all warning zone.
pub fn min_gap_m(warn_m: f64) -> f64 {
    2.0 * warn_m + 2.0
}

/// Check a polygon (GeoJSON rings, closed or not) as a collar would.
pub fn check(p: &Polygon, limits: &CollarLimits, warn_m: f64, hysteresis_m: f64, slack_m: f64) -> Result<(), ShapeCode> {
    let outer = p.coordinates.first().map(Vec::as_slice).unwrap_or(&[]);
    let holes = p.coordinates.get(1..).unwrap_or(&[]);
    check_rings(outer, holes, limits, warn_m, hysteresis_m, slack_m)
}

/// [`check`] on the wire form: the outer ring and the holes, unclosed.
pub fn check_rings<H: AsRef<[LonLat]>>(
    outer: &[LonLat],
    holes: &[H],
    limits: &CollarLimits,
    warn_m: f64,
    hysteresis_m: f64,
    slack_m: f64,
) -> Result<(), ShapeCode> {
    for v in [warn_m, hysteresis_m] {
        if !v.is_finite() || !(0.0..=MAX_MARGIN_M).contains(&v) {
            return Err(ShapeCode::BadMargins);
        }
    }
    if holes.len() > limits.holes {
        return Err(ShapeCode::TooManyHoles);
    }
    let raw: Vec<&[LonLat]> = std::iter::once(outer).chain(holes.iter().map(|h| h.as_ref())).collect();
    if raw.iter().flat_map(|r| r.iter()).any(|p| !in_range(*p)) {
        return Err(ShapeCode::OutOfRange);
    }
    let rings: Vec<Vec<E7>> = raw.iter().map(|r| e7_ring(r)).collect();
    if rings.iter().any(|r| r.len() < 3) {
        return Err(ShapeCode::TooFewVertices);
    }
    let total: usize = rings.iter().map(Vec::len).sum();
    if rings[0].len() > limits.outer || rings[1..].iter().any(|h| h.len() > limits.hole_vertices) || total > limits.total {
        return Err(ShapeCode::TooManyVertices);
    }
    if rings.iter().any(|r| !exact::simple(r)) {
        return Err(ShapeCode::SelfIntersecting);
    }
    let boxes: Vec<[i64; 4]> = rings.iter().map(|r| exact::bbox(r)).collect();
    for i in 0..rings.len() {
        for j in i + 1..rings.len() {
            if exact::boxes_meet(&boxes[i], &boxes[j]) && exact::rings_meet(&rings[i], &rings[j]) {
                return Err(ShapeCode::RingsCross);
            }
        }
    }
    if rings[1..].iter().any(|h| !exact::inside(h[0], &rings[0])) {
        return Err(ShapeCode::HoleOutside);
    }
    for i in 1..rings.len() {
        for j in i + 1..rings.len() {
            if exact::inside(rings[j][0], &rings[i]) || exact::inside(rings[i][0], &rings[j]) {
                return Err(ShapeCode::HolesOverlap);
            }
        }
    }
    let m = Metric::new(rings[0][0]);
    let xy: Vec<Vec<[f32; 2]>> = rings.iter().map(|r| r.iter().map(|p| m.xy(*p)).collect()).collect();
    if metric::area(&xy[0]) < MIN_AREA_M2 {
        return Err(ShapeCode::ZeroArea);
    }
    if xy[1..].iter().any(|h| metric::area(h) < MIN_HOLE_AREA_M2) {
        return Err(ShapeCode::HoleTooSmall);
    }
    let gap = min_gap_m(warn_m) + slack_m;
    let fboxes: Vec<[f32; 4]> = xy.iter().map(|r| metric::bbox(r)).collect();
    for i in 0..xy.len() {
        for j in i + 1..xy.len() {
            if (metric::box_distance(&fboxes[i], &fboxes[j]) as f64) < gap && (metric::ring_distance(&xy[i], &xy[j]) as f64) < gap {
                return Err(ShapeCode::HoleTooClose);
            }
        }
    }
    Ok(())
}

fn in_range(p: LonLat) -> bool {
    p[0].is_finite() && p[1].is_finite() && p[0].abs() <= 180.0 && p[1].abs() <= 90.0
}

/// `[lon, lat]` × 1e7.
pub type E7 = [i64; 2];

/// Degrees to e7 integers, rounded half away from zero.
pub fn to_e7(deg: f64) -> i64 {
    (deg * 1e7).round() as i64
}

/// A ring as e7 integers: consecutive duplicates and a closing vertex removed.
pub fn e7_ring(ring: &[LonLat]) -> Vec<E7> {
    let mut out: Vec<E7> = Vec::with_capacity(ring.len());
    for p in ring {
        let q = [to_e7(p[0]), to_e7(p[1])];
        if out.last() != Some(&q) {
            out.push(q);
        }
    }
    while out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    out
}

/// Whether a lon/lat ring is simple once rounded to e7 integers, by the
/// same exact test [`check`] uses.
pub(crate) fn simple_after_rounding(ring: &[LonLat]) -> bool {
    let r = e7_ring(ring);
    r.len() >= 3 && exact::simple(&r)
}

/// Exact predicates on e7 integers.
mod exact {
    use super::E7;
    use std::cmp::Ordering;

    /// Sign of `(b - a) × (c - a)`: 1 left turn, -1 right, 0 collinear.
    pub fn orient(a: E7, b: E7, c: E7) -> i8 {
        let l = (b[0] - a[0]) * (c[1] - a[1]);
        let r = (b[1] - a[1]) * (c[0] - a[0]);
        match l.cmp(&r) {
            Ordering::Greater => 1,
            Ordering::Less => -1,
            Ordering::Equal => 0,
        }
    }

    /// `q` inside the bounding box of `p`-`r` (for collinear points: on the segment).
    fn on_segment(p: E7, q: E7, r: E7) -> bool {
        q[0] >= p[0].min(r[0]) && q[0] <= p[0].max(r[0]) && q[1] >= p[1].min(r[1]) && q[1] <= p[1].max(r[1])
    }

    /// Closed segments `p1p2` and `q1q2` share at least one point.
    pub fn segments_meet(p1: E7, p2: E7, q1: E7, q2: E7) -> bool {
        let o1 = orient(p1, p2, q1);
        let o2 = orient(p1, p2, q2);
        let o3 = orient(q1, q2, p1);
        let o4 = orient(q1, q2, p2);
        if o1 != o2 && o3 != o4 {
            return true;
        }
        (o1 == 0 && on_segment(p1, q1, p2)) || (o2 == 0 && on_segment(p1, q2, p2)) || (o3 == 0 && on_segment(q1, p1, q2)) || (o4 == 0 && on_segment(q1, p2, q2))
    }

    /// Neighbouring edges `a-b` and `b-c` overlap beyond `b`: collinear with
    /// `c` on the same side of `b` as `a`.
    fn folds(a: E7, b: E7, c: E7) -> bool {
        orient(a, b, c) == 0 && (c[0] - b[0]).signum() == (a[0] - b[0]).signum() && (c[1] - b[1]).signum() == (a[1] - b[1]).signum()
    }

    /// No two edges of the ring meet except neighbours at their shared vertex.
    pub fn simple(r: &[E7]) -> bool {
        let n = r.len();
        for i in 0..n {
            let (a1, a2) = (r[i], r[(i + 1) % n]);
            for j in i + 1..n {
                let (b1, b2) = (r[j], r[(j + 1) % n]);
                if j == i + 1 {
                    if folds(a1, a2, b2) {
                        return false;
                    }
                } else if i == 0 && j == n - 1 {
                    if folds(b1, b2, a2) {
                        return false;
                    }
                } else if segments_meet(a1, a2, b1, b2) {
                    return false;
                }
            }
        }
        true
    }

    pub fn bbox(r: &[E7]) -> [i64; 4] {
        r.iter().fold([i64::MAX, i64::MAX, i64::MIN, i64::MIN], |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])])
    }

    pub fn boxes_meet(a: &[i64; 4], b: &[i64; 4]) -> bool {
        a[0] <= b[2] && b[0] <= a[2] && a[1] <= b[3] && b[1] <= a[3]
    }

    /// Any edge of `a` meets any edge of `b`.
    pub fn rings_meet(a: &[E7], b: &[E7]) -> bool {
        let (n, m) = (a.len(), b.len());
        (0..n).any(|i| (0..m).any(|j| segments_meet(a[i], a[(i + 1) % n], b[j], b[(j + 1) % m])))
    }

    /// Even-odd rule toward +x. `p` must not lie on the ring.
    pub fn inside(p: E7, r: &[E7]) -> bool {
        let n = r.len();
        let mut c = false;
        let mut j = n - 1;
        for i in 0..n {
            let (a, b) = (r[i], r[j]);
            if (a[1] > p[1]) != (b[1] > p[1]) {
                // The crossing is right of p: p.x < a.x + (p.y - a.y)(b.x - a.x)/(b.y - a.y).
                let lhs = (p[0] - a[0]) * (b[1] - a[1]);
                let rhs = (p[1] - a[1]) * (b[0] - a[0]);
                if if b[1] > a[1] { lhs < rhs } else { lhs > rhs } {
                    c = !c;
                }
            }
            j = i;
        }
        c
    }
}

/// The collar's single-precision projection.
struct Metric {
    lon0: i64,
    lat0: i64,
    kx: f32,
    ky: f32,
}

impl Metric {
    fn new(origin: E7) -> Self {
        let lat0_deg = origin[1] as f64 / 1e7;
        let m_per_deg_lon = M_PER_DEG_LAT * lat0_deg.to_radians().cos();
        Self { lon0: origin[0], lat0: origin[1], kx: (m_per_deg_lon * 1e-7) as f32, ky: (M_PER_DEG_LAT * 1e-7) as f32 }
    }

    fn xy(&self, p: E7) -> [f32; 2] {
        [(p[0] - self.lon0) as f32 * self.kx, (p[1] - self.lat0) as f32 * self.ky]
    }
}

/// Single-precision areas and distances.
mod metric {
    type F = [f32; 2];

    pub fn area(r: &[F]) -> f32 {
        let o = r[0];
        let mut s = 0.0f32;
        for i in 1..r.len() - 1 {
            let (ax, ay) = (r[i][0] - o[0], r[i][1] - o[1]);
            let (bx, by) = (r[i + 1][0] - o[0], r[i + 1][1] - o[1]);
            s += ax * by - ay * bx;
        }
        (s * 0.5).abs()
    }

    pub fn segment_distance(p: F, a: F, b: F) -> f32 {
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let len2 = dx * dx + dy * dy;
        let mut t = 0.0f32;
        if len2 > 0.0 {
            t = (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0);
        }
        let cx = a[0] + t * dx - p[0];
        let cy = a[1] + t * dy - p[1];
        (cx * cx + cy * cy).sqrt()
    }

    fn point_ring(p: F, r: &[F]) -> f32 {
        let n = r.len();
        (0..n).map(|i| segment_distance(p, r[i], r[(i + 1) % n])).fold(f32::INFINITY, f32::min)
    }

    /// Least distance between two rings that don't meet.
    pub fn ring_distance(a: &[F], b: &[F]) -> f32 {
        let ab = a.iter().map(|p| point_ring(*p, b)).fold(f32::INFINITY, f32::min);
        let ba = b.iter().map(|p| point_ring(*p, a)).fold(f32::INFINITY, f32::min);
        ab.min(ba)
    }

    pub fn bbox(r: &[F]) -> [f32; 4] {
        r.iter()
            .fold([f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY], |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])])
    }

    pub fn box_distance(a: &[f32; 4], b: &[f32; 4]) -> f32 {
        let dx = (b[0] - a[2]).max(a[0] - b[2]).max(0.0);
        let dy = (b[1] - a[3]).max(a[1] - b[3]).max(0.0);
        (dx * dx + dy * dy).sqrt()
    }
}

/// Fit a shape to a collar's limits, keeping rings at least 12.5 m apart where
/// they were (the firmware default warning distance plus the server slack).
/// See [`fit_gap`].
pub fn fit(p: &Polygon, limits: &CollarLimits) -> Polygon {
    fit_gap(p, limits, min_gap_m(DEFAULT_WARN_M) + SERVER_SLACK_M)
}

/// Fit a shape to a collar's limits. Outer rings only shrink and holes only
/// grow, so every point a collar treats as inside was inside the input.
/// Simplification prefers steps that don't bring two rings closer than
/// `keep_gap_m` (or than they already were, if closer); if none is left it
/// takes any step that keeps the rings from crossing. The result is closed
/// and rounded to 7 decimals. Run [`check`] on it: a shape that was invalid
/// going in, or that can't be simplified without breaking a rule, still fails.
pub fn fit_gap(p: &Polygon, limits: &CollarLimits, keep_gap_m: f64) -> Polygon {
    let rounded: Vec<Vec<LonLat>> = p.coordinates.iter().map(|r| clean_ring(&r.iter().map(|q| [round7(q[0]), round7(q[1])]).collect::<Vec<_>>())).collect();
    let Some(outer_ll) = rounded.first().filter(|r| r.len() >= 3) else {
        return close_all(rounded);
    };
    let holes_ll: Vec<&Vec<LonLat>> = if limits.holes == 0 { Vec::new() } else { rounded[1..].iter().filter(|r| r.len() >= 3).collect() };
    let counts: Vec<usize> = holes_ll.iter().map(|h| h.len()).collect();
    if limits.fits(outer_ll.len(), &counts) && holes_ll.len() + 1 == rounded.len() {
        return close_all(rounded);
    }

    let proj = Projection::new(outer_ll[0]);
    let outer_ccw = local::signed_area(&proj.forward_ring(outer_ll)) >= 0.0;
    let holes_ccw = holes_ll.first().is_some_and(|h| local::signed_area(&proj.forward_ring(h)) >= 0.0);
    let mut shape = local::Shape { outer: local::ccw(proj.forward_ring(outer_ll)), holes: holes_ll.iter().map(|h| local::cw(proj.forward_ring(h))).collect() };
    shape.settle();
    shape.reduce(limits, keep_gap_m);

    let mut out = Vec::with_capacity(shape.holes.len() + 1);
    let mut outer = shape.outer;
    if !outer_ccw {
        outer.reverse();
    }
    out.push(clean_ring(&proj.inverse_ring(&outer)));
    for mut h in shape.holes {
        if holes_ccw {
            h.reverse();
        }
        out.push(clean_ring(&proj.inverse_ring(&h)));
    }
    close_all(out)
}

fn close_all(rings: Vec<Vec<LonLat>>) -> Polygon {
    let coordinates = rings
        .into_iter()
        .map(|mut r| {
            if let (Some(first), Some(last)) = (r.first().copied(), r.last().copied())
                && first != last
            {
                r.push(first);
            }
            r
        })
        .collect();
    Polygon { kind: PolygonType::Polygon, coordinates }
}

/// Geometry in local metres (f64), shared by [`fit`] and [`crate::exclude`].
pub(crate) mod local {
    use crate::clip::subtract_pieces;
    use crate::limits::CollarLimits;
    use crate::ring::{distance_to_segment, point_in_ring};

    pub type P = [f64; 2];

    const EPS: f64 = 1e-9;

    pub fn signed_area(r: &[P]) -> f64 {
        crate::ring::signed_area(r)
    }

    pub fn area(r: &[P]) -> f64 {
        signed_area(r).abs()
    }

    pub fn ccw(mut r: Vec<P>) -> Vec<P> {
        if signed_area(&r) < 0.0 {
            r.reverse();
        }
        r
    }

    pub fn cw(mut r: Vec<P>) -> Vec<P> {
        if signed_area(&r) > 0.0 {
            r.reverse();
        }
        r
    }

    /// `(b - a) × (c - a)`.
    pub fn cross(a: P, b: P, c: P) -> f64 {
        (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
    }

    fn sign(v: f64) -> i8 {
        if v > EPS {
            1
        } else if v < -EPS {
            -1
        } else {
            0
        }
    }

    fn on_segment(p: P, q: P, r: P) -> bool {
        q[0] >= p[0].min(r[0]) - EPS && q[0] <= p[0].max(r[0]) + EPS && q[1] >= p[1].min(r[1]) - EPS && q[1] <= p[1].max(r[1]) + EPS
    }

    /// Closed segments share a point (within a nanometre).
    pub fn segments_meet(p1: P, p2: P, q1: P, q2: P) -> bool {
        let o1 = sign(cross(p1, p2, q1));
        let o2 = sign(cross(p1, p2, q2));
        let o3 = sign(cross(q1, q2, p1));
        let o4 = sign(cross(q1, q2, p2));
        if o1 != o2 && o3 != o4 {
            return true;
        }
        (o1 == 0 && on_segment(p1, q1, p2)) || (o2 == 0 && on_segment(p1, q2, p2)) || (o3 == 0 && on_segment(q1, p1, q2)) || (o4 == 0 && on_segment(q1, p2, q2))
    }

    pub fn segment_distance(a1: P, a2: P, b1: P, b2: P) -> f64 {
        if segments_meet(a1, a2, b1, b2) {
            return 0.0;
        }
        distance_to_segment(a1, b1, b2).min(distance_to_segment(a2, b1, b2)).min(distance_to_segment(b1, a1, a2)).min(distance_to_segment(b2, a1, a2))
    }

    pub fn edges(r: &[P]) -> impl Iterator<Item = (P, P)> + '_ {
        (0..r.len()).map(move |i| (r[i], r[(i + 1) % r.len()]))
    }

    /// Least distance from a segment to a ring.
    pub fn segment_ring_distance(a: P, b: P, r: &[P]) -> f64 {
        edges(r).map(|(c, d)| segment_distance(a, b, c, d)).fold(f64::INFINITY, f64::min)
    }

    pub fn ring_distance(a: &[P], b: &[P]) -> f64 {
        edges(a).map(|(p, q)| segment_ring_distance(p, q, b)).fold(f64::INFINITY, f64::min)
    }

    pub fn rings_meet(a: &[P], b: &[P]) -> bool {
        edges(a).any(|(p, q)| edges(b).any(|(c, d)| segments_meet(p, q, c, d)))
    }

    pub fn inside(p: P, r: &[P]) -> bool {
        point_in_ring(p, r)
    }

    /// How ring `b` sits against ring `a`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Relation {
        Crosses,
        Inside,
        Covers,
        Apart,
    }

    pub fn relate(a: &[P], b: &[P]) -> Relation {
        if rings_meet(a, b) {
            Relation::Crosses
        } else if inside(b[0], a) {
            Relation::Inside
        } else if inside(a[0], b) {
            Relation::Covers
        } else {
            Relation::Apart
        }
    }

    /// Convex hull, counter-clockwise (Andrew's monotone chain).
    pub fn convex_hull(points: impl IntoIterator<Item = P>) -> Vec<P> {
        let mut pts: Vec<P> = points.into_iter().collect();
        pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
        pts.dedup();
        if pts.len() < 3 {
            return pts;
        }
        let mut hull: Vec<P> = Vec::with_capacity(pts.len() * 2);
        for pass in 0..2 {
            let start = hull.len();
            let iter: Box<dyn Iterator<Item = &P>> = if pass == 0 { Box::new(pts.iter()) } else { Box::new(pts.iter().rev()) };
            for &p in iter {
                while hull.len() >= start + 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
                    hull.pop();
                }
                hull.push(p);
            }
            hull.pop();
        }
        hull
    }

    pub fn centroid(r: &[P]) -> P {
        let a = signed_area(r);
        if a.abs() < 1e-12 {
            let n = r.len() as f64;
            return [r.iter().map(|p| p[0]).sum::<f64>() / n, r.iter().map(|p| p[1]).sum::<f64>() / n];
        }
        let (mut cx, mut cy) = (0.0, 0.0);
        for (p, q) in edges(r) {
            let f = p[0] * q[1] - q[0] * p[1];
            cx += (p[0] + q[0]) * f;
            cy += (p[1] + q[1]) * f;
        }
        [cx / (6.0 * a), cy / (6.0 * a)]
    }

    /// Scale a ring about its centroid until it encloses at least `min_m2`.
    pub fn grow_to_area(r: Vec<P>, min_m2: f64) -> Vec<P> {
        let a = area(&r);
        if a >= min_m2 || a <= 0.0 {
            return r;
        }
        let k = (min_m2 / a).sqrt();
        let c = centroid(&r);
        r.into_iter().map(|p| [c[0] + (p[0] - c[0]) * k, c[1] + (p[1] - c[1]) * k]).collect()
    }

    /// `subject` minus `clip`: the largest piece left, counter-clockwise and
    /// tidied. `None` when nothing is left or the shapes can't be combined.
    pub fn cut(subject: &[P], clip: &[P]) -> Option<Vec<P>> {
        let pieces = subtract_pieces(subject, clip).ok()?;
        pieces.into_iter().max_by(|a, b| area(a).total_cmp(&area(b))).map(|r| tidy(ccw(r))).filter(|r| r.len() >= 3)
    }

    /// Vertices closer than 5 cm to the previous one merge, and spikes that
    /// fold straight back go: clipping leaves such slivers where edges cross
    /// almost at one point, and 7-decimal rounding (about 1 cm) would turn
    /// them into self-crossings. Moves the ring by at most a few centimetres.
    pub fn tidy(mut r: Vec<P>) -> Vec<P> {
        const MERGE_M: f64 = 0.05;
        loop {
            let n = r.len();
            if n < 4 {
                return r;
            }
            let close = (0..n).find(|&i| {
                let (a, b) = (r[i], r[(i + 1) % n]);
                (a[0] - b[0]).hypot(a[1] - b[1]) < MERGE_M
            });
            if let Some(i) = close {
                r.remove((i + 1) % n);
                continue;
            }
            let spike = (0..n).find(|&i| {
                let (a, b, c) = (r[(i + n - 1) % n], r[i], r[(i + 1) % n]);
                let (u, v) = ([b[0] - a[0], b[1] - a[1]], [c[0] - b[0], c[1] - b[1]]);
                let (lu, lv) = (u[0].hypot(u[1]), v[0].hypot(v[1]));
                let sin = (u[0] * v[1] - u[1] * v[0]) / (lu * lv);
                let dot = u[0] * v[0] + u[1] * v[1];
                dot < 0.0 && sin.abs() < 1e-3
            });
            match spike {
                Some(i) => {
                    r.remove(i);
                }
                None => return r,
            }
        }
    }

    /// An outer ring (counter-clockwise) and holes (clockwise), so the
    /// fenced area is always on the left of every edge.
    #[derive(Debug, Clone)]
    pub struct Shape {
        pub outer: Vec<P>,
        pub holes: Vec<Vec<P>>,
    }

    /// One simplification step on ring `ring` (0 = outer, k = hole k-1).
    #[derive(Debug, Clone, Copy)]
    struct Step {
        ring: usize,
        i: usize,
        /// Collapse edge i → i+1 into this point; `None` removes vertex i.
        to: Option<P>,
        cost: f64,
    }

    impl Shape {
        fn ring(&self, k: usize) -> &Vec<P> {
            if k == 0 { &self.outer } else { &self.holes[k - 1] }
        }

        fn ring_mut(&mut self, k: usize) -> &mut Vec<P> {
            if k == 0 { &mut self.outer } else { &mut self.holes[k - 1] }
        }

        fn rings(&self) -> usize {
            self.holes.len() + 1
        }

        /// Holes that meet the outer ring are cut out of it, holes outside it
        /// go, and holes that meet or nest merge into their convex hull.
        pub fn settle(&mut self) {
            'again: loop {
                for k in 0..self.holes.len() {
                    let rel = relate(&self.outer, &self.holes[k]);
                    if rel == Relation::Crosses {
                        let hole = self.holes.remove(k);
                        if let Some(outer) = cut(&self.outer, &hole) {
                            self.outer = outer;
                        }
                        continue 'again;
                    }
                    if rel != Relation::Inside {
                        self.holes.remove(k);
                        continue 'again;
                    }
                }
                for i in 0..self.holes.len() {
                    for j in i + 1..self.holes.len() {
                        if relate(&self.holes[i], &self.holes[j]) != Relation::Apart {
                            let b = self.holes.remove(j);
                            let a = self.holes.remove(i);
                            self.holes.push(cw(convex_hull(a.into_iter().chain(b))));
                            continue 'again;
                        }
                    }
                }
                return;
            }
        }

        /// Merge the two holes closest together into their convex hull.
        fn merge_closest_holes(&mut self) {
            let mut best = (f64::INFINITY, 0, 1);
            for i in 0..self.holes.len() {
                for j in i + 1..self.holes.len() {
                    let d = ring_distance(&self.holes[i], &self.holes[j]);
                    if d < best.0 {
                        best = (d, i, j);
                    }
                }
            }
            let b = self.holes.remove(best.2);
            let a = self.holes.remove(best.1);
            self.holes.push(cw(convex_hull(a.into_iter().chain(b))));
            self.settle();
        }

        fn over(&self, k: usize, limits: &CollarLimits) -> bool {
            let n = self.ring(k).len();
            if k == 0 { n > limits.outer } else { n > limits.hole_vertices }
        }

        fn total(&self) -> usize {
            self.outer.len() + self.holes.iter().map(Vec::len).sum::<usize>()
        }

        pub fn reduce(&mut self, limits: &CollarLimits, keep_gap_m: f64) {
            while self.holes.len() > limits.holes {
                self.merge_closest_holes();
            }
            let mut budget = 10 * self.total() + 100;
            loop {
                budget -= 1;
                let rings: Vec<usize> = {
                    let over: Vec<usize> = (0..self.rings()).filter(|k| self.over(*k, limits)).collect();
                    if !over.is_empty() {
                        over
                    } else if self.total() > limits.total {
                        (0..self.rings()).filter(|k| self.ring(*k).len() > 3).collect()
                    } else {
                        return;
                    }
                };
                if budget == 0 || rings.is_empty() {
                    return;
                }
                // Far over: many separate steps in one pass.
                let most = rings.iter().copied().filter(|k| self.over(*k, limits)).max_by_key(|k| self.ring(*k).len());
                if let Some(k) = most {
                    let n = self.ring(k).len();
                    let limit = if k == 0 { limits.outer } else { limits.hole_vertices };
                    if n > limit + 8 && self.batch(k, (n - limit).min(n / 4), keep_gap_m) {
                        continue;
                    }
                }
                let mut steps: Vec<Step> = rings.iter().flat_map(|k| self.steps(*k)).collect();
                steps.sort_by(|a, b| a.cost.total_cmp(&b.cost));
                let boxes = self.boxes();
                let chosen =
                    steps.iter().find(|s| self.allowed(s, Some(keep_gap_m), &boxes)).or_else(|| steps.iter().find(|s| self.allowed(s, None, &boxes))).copied();
                match chosen {
                    Some(s) => {
                        let next = apply(self.ring(s.ring), &s);
                        *self.ring_mut(s.ring) = next;
                    }
                    None => {
                        // Stuck: a hole becomes its convex hull, or merges with its nearest ring.
                        let Some(k) = rings.iter().copied().filter(|k| *k > 0).max_by_key(|k| self.ring(*k).len()) else {
                            return;
                        };
                        let hole = self.holes[k - 1].clone();
                        let hull = cw(convex_hull(hole.iter().copied()));
                        if hull.len() < hole.len() || (area(&hull) - area(&hole)).abs() > 1e-6 {
                            self.holes[k - 1] = hull;
                        } else {
                            let to_outer = ring_distance(&hull, &self.outer);
                            let nearest = (0..self.holes.len())
                                .filter(|j| *j != k - 1)
                                .map(|j| (ring_distance(&hull, &self.holes[j]), j))
                                .min_by(|a, b| a.0.total_cmp(&b.0));
                            match nearest {
                                Some((d, j)) if d < to_outer => {
                                    let other = self.holes[j].clone();
                                    let (hi, lo) = if j > k - 1 { (j, k - 1) } else { (k - 1, j) };
                                    self.holes.remove(hi);
                                    self.holes.remove(lo);
                                    self.holes.push(cw(convex_hull(hull.into_iter().chain(other))));
                                }
                                _ => {
                                    self.holes.remove(k - 1);
                                    let bridge = cw(convex_hull(hull.iter().copied().chain(nearest_points_outside(&self.outer, &hull))));
                                    match cut(&self.outer, &bridge) {
                                        Some(o) => self.outer = o,
                                        None => {
                                            self.holes.insert(k - 1, hull);
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                        self.settle();
                    }
                }
            }
        }

        /// Up to `want` of the cheapest allowed steps on ring `k` whose
        /// neighbourhoods are apart and whose new edges don't meet, applied
        /// together. False when none is allowed.
        fn batch(&mut self, k: usize, want: usize, keep_gap_m: f64) -> bool {
            let n = self.ring(k).len();
            let mut steps = self.steps(k);
            steps.sort_by(|a, b| a.cost.total_cmp(&b.cost));
            let boxes = self.boxes();
            let mut busy = vec![false; n];
            let mut taken: Vec<(Step, Vec<(P, P)>, [P; 3])> = Vec::new();
            for s in steps {
                if taken.len() >= want {
                    break;
                }
                let reach: isize = if s.to.is_some() { 3 } else { 2 };
                let span: Vec<usize> = (-2..=reach).map(|d| (s.i as isize + d).rem_euclid(n as isize) as usize).collect();
                if span.iter().any(|j| busy[*j]) || !self.allowed(&s, Some(keep_gap_m), &boxes) {
                    continue;
                }
                let (edges, tri) = self.change(&s);
                let clash = taken.iter().any(|(_, e2, t2)| {
                    edges.iter().any(|(p, q)| e2.iter().any(|(c, d)| segments_meet(*p, *q, *c, *d)))
                        || t2.iter().any(|v| in_triangle(*v, tri))
                        || tri.iter().any(|v| in_triangle(*v, *t2))
                });
                if clash {
                    continue;
                }
                for j in &span[1..span.len() - 1] {
                    busy[*j] = true;
                }
                taken.push((s, edges, tri));
            }
            if taken.is_empty() {
                return false;
            }
            let r = self.ring(k);
            let mut drop = vec![false; n];
            let mut moved: Vec<Option<P>> = vec![None; n];
            for (s, _, _) in &taken {
                match s.to {
                    None => drop[s.i] = true,
                    Some(x) => {
                        moved[s.i] = Some(x);
                        drop[(s.i + 1) % n] = true;
                    }
                }
            }
            let next: Vec<P> = (0..n).filter(|j| !drop[*j]).map(|j| moved[j].unwrap_or(r[j])).collect();
            *self.ring_mut(k) = next;
            true
        }

        /// A step's new edges and the triangle it adds to or takes from the fenced area.
        fn change(&self, s: &Step) -> (Vec<(P, P)>, [P; 3]) {
            let r = self.ring(s.ring);
            let n = r.len();
            let idx = |d: isize| (s.i as isize + d).rem_euclid(n as isize) as usize;
            match s.to {
                None => (vec![(r[idx(-1)], r[idx(1)])], [r[idx(-1)], r[idx(0)], r[idx(1)]]),
                Some(x) => (vec![(r[idx(-1)], x), (x, r[idx(2)])], [r[idx(0)], x, r[idx(1)]]),
            }
        }

        /// Steps that shrink the fenced area on ring `k`.
        fn steps(&self, k: usize) -> Vec<Step> {
            let r = self.ring(k);
            let n = r.len();
            let mut out = Vec::new();
            if n <= 3 {
                return out;
            }
            for i in 0..n {
                let a = r[(i + n - 1) % n];
                let b = r[i];
                let c = r[(i + 1) % n];
                let d = r[(i + 2) % n];
                let turn_b = cross(a, b, c);
                if turn_b >= 0.0 {
                    out.push(Step { ring: k, i, to: None, cost: turn_b / 2.0 });
                }
                let turn_c = cross(b, c, d);
                if n > 4 && turn_b < 0.0 && turn_c < 0.0 {
                    // Lines a→b and d→c, both extended past b and c.
                    let (rv, qv) = ([b[0] - a[0], b[1] - a[1]], [c[0] - d[0], c[1] - d[1]]);
                    let den = rv[0] * qv[1] - rv[1] * qv[0];
                    if den.abs() < 1e-12 {
                        continue;
                    }
                    let w = [c[0] - b[0], c[1] - b[1]];
                    let s = (w[0] * qv[1] - w[1] * qv[0]) / den;
                    let t = (w[0] * rv[1] - w[1] * rv[0]) / den;
                    if s <= 0.0 || t <= 0.0 || s > 1e4 || t > 1e4 {
                        continue;
                    }
                    let x = [b[0] + s * rv[0], b[1] + s * rv[1]];
                    let delta = cross(b, x, c);
                    if delta < 0.0 {
                        out.push(Step { ring: k, i, to: Some(x), cost: -delta / 2.0 });
                    }
                }
            }
            out
        }

        /// Bounding boxes of every ring, in ring order.
        fn boxes(&self) -> Vec<[f64; 4]> {
            (0..self.rings()).map(|k| bbox(self.ring(k).iter().copied())).collect()
        }

        /// The step keeps every ring simple and apart, swallows no vertex,
        /// and (with `gap`) brings no ring closer than `min(gap, before)`.
        /// `boxes` are the rings' bounding boxes.
        fn allowed(&self, s: &Step, gap: Option<f64>, boxes: &[[f64; 4]]) -> bool {
            let r = self.ring(s.ring);
            let n = r.len();
            let idx = |d: isize| (s.i as isize + d).rem_euclid(n as isize) as usize;
            let (new_edges, old_edges, tri, own): (Vec<(P, P)>, Vec<(P, P)>, [P; 3], Vec<usize>) = match s.to {
                None => {
                    let (a, b, c) = (r[idx(-1)], r[idx(0)], r[idx(1)]);
                    (vec![(a, c)], vec![(a, b), (b, c)], [a, b, c], vec![idx(-1), idx(0), idx(1)])
                }
                Some(x) => {
                    let (a, b, c, d) = (r[idx(-1)], r[idx(0)], r[idx(1)], r[idx(2)]);
                    (vec![(a, x), (x, d)], vec![(a, b), (b, c), (c, d)], [b, x, c], vec![idx(-1), idx(0), idx(1), idx(2)])
                }
            };
            // Everything the step touches lies in this box.
            let area_box = bbox(new_edges.iter().flat_map(|(p, q)| [*p, *q]).chain(tri));
            // Edges of this ring the step replaces or that touch its ends.
            let skip: Vec<usize> = match s.to {
                None => vec![idx(-2), idx(-1), idx(0), idx(1)],
                Some(_) => vec![idx(-2), idx(-1), idx(0), idx(1), idx(2)],
            };
            for (k, rb) in boxes.iter().enumerate() {
                if !boxes_near(&area_box, rb, 0.0) {
                    continue;
                }
                let other = self.ring(k);
                let m = other.len();
                for j in 0..m {
                    if k == s.ring && skip.contains(&j) {
                        continue;
                    }
                    let (c, d) = (other[j], other[(j + 1) % m]);
                    if !boxes_near(&area_box, &bbox([c, d]), 0.0) {
                        continue;
                    }
                    if new_edges.iter().any(|(p, q)| segments_meet(*p, *q, c, d)) {
                        return false;
                    }
                }
                for (j, v) in other.iter().enumerate() {
                    if k == s.ring && own.contains(&j) {
                        continue;
                    }
                    if v[0] >= area_box[0] - EPS && v[0] <= area_box[2] + EPS && v[1] >= area_box[1] - EPS && v[1] <= area_box[3] + EPS && in_triangle(*v, tri)
                    {
                        return false;
                    }
                }
            }
            if let Some(g) = gap {
                for k in (0..self.rings()).filter(|k| *k != s.ring) {
                    // A ring at least `g` away stays at least `g` away.
                    if !boxes_near(&area_box, &boxes[k], g) {
                        continue;
                    }
                    let other = self.ring(k);
                    let before = old_edges.iter().map(|(p, q)| segment_ring_distance(*p, *q, other)).fold(f64::INFINITY, f64::min);
                    let after = new_edges.iter().map(|(p, q)| segment_ring_distance(*p, *q, other)).fold(f64::INFINITY, f64::min);
                    if after + 1e-9 < before.min(g) {
                        return false;
                    }
                }
            }
            true
        }
    }

    pub fn bbox(points: impl IntoIterator<Item = P>) -> [f64; 4] {
        points
            .into_iter()
            .fold([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY], |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])])
    }

    /// The boxes are at most `margin` apart.
    fn boxes_near(a: &[f64; 4], b: &[f64; 4], margin: f64) -> bool {
        a[0] <= b[2] + margin + EPS && b[0] <= a[2] + margin + EPS && a[1] <= b[3] + margin + EPS && b[1] <= a[3] + margin + EPS
    }

    fn apply(r: &[P], s: &Step) -> Vec<P> {
        let n = r.len();
        match s.to {
            None => r.iter().enumerate().filter(|(k, _)| *k != s.i).map(|(_, p)| *p).collect(),
            Some(x) => {
                let drop = (s.i + 1) % n;
                r.iter().enumerate().filter(|(k, _)| *k != drop).map(|(k, p)| if k == s.i { x } else { *p }).collect()
            }
        }
    }

    /// Closed triangle test.
    fn in_triangle(p: P, t: [P; 3]) -> bool {
        let d1 = sign(cross(t[0], t[1], p));
        let d2 = sign(cross(t[1], t[2], p));
        let d3 = sign(cross(t[2], t[0], p));
        let has_neg = d1 < 0 || d2 < 0 || d3 < 0;
        let has_pos = d1 > 0 || d2 > 0 || d3 > 0;
        !(has_neg && has_pos)
    }

    /// Two points 1 m outside `outer`, across from `inner` on its nearest
    /// edge and spanning `inner`'s width along that edge. With `inner`, their
    /// convex hull is the notch that joins `inner` to the edge.
    pub fn nearest_points_outside(outer: &[P], inner: &[P]) -> [P; 2] {
        let mut best = (f64::INFINITY, 0usize);
        for (i, (a, b)) in edges(outer).enumerate() {
            let d = segment_ring_distance(a, b, inner);
            if d < best.0 {
                best = (d, i);
            }
        }
        let a = outer[best.1];
        let b = outer[(best.1 + 1) % outer.len()];
        let len = (b[0] - a[0]).hypot(b[1] - a[1]).max(1e-9);
        let u = [(b[0] - a[0]) / len, (b[1] - a[1]) / len];
        // Counter-clockwise outer ring: outside is to the right.
        let out = [u[1], -u[0]];
        let proj = |p: P| (p[0] - a[0]) * u[0] + (p[1] - a[1]) * u[1];
        let (lo, hi) = inner.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| (lo.min(proj(*p)), hi.max(proj(*p))));
        let at = |s: f64| [a[0] + u[0] * s + out[0], a[1] + u[1] * s + out[1]];
        [at(lo), at(hi)]
    }

    /// The closest points of `outer`'s edges and `inner`: `(on outer, on inner)`.
    pub fn closest_pair(outer: &[P], inner: &[P]) -> (P, P) {
        let mut best = (f64::INFINITY, outer[0], inner[0]);
        let nearest_on = |p: P, a: P, b: P| {
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let len2 = dx * dx + dy * dy;
            let t = if len2 > 0.0 { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
            [a[0] + t * dx, a[1] + t * dy]
        };
        for (a, b) in edges(outer) {
            for &e in inner {
                let t = nearest_on(e, a, b);
                let d = (t[0] - e[0]).hypot(t[1] - e[1]);
                if d < best.0 {
                    best = (d, t, e);
                }
            }
        }
        for (a, b) in edges(inner) {
            for &t in outer {
                let e = nearest_on(t, a, b);
                let d = (t[0] - e[0]).hypot(t[1] - e[1]);
                if d < best.0 {
                    best = (d, t, e);
                }
            }
        }
        (best.1, best.2)
    }

    /// Candidate notches joining `inner` to the outside of `outer`, widest
    /// first: across to the nearest edge; along the shortest way out, as wide
    /// as `inner`; the same, 5 m past the edge; a wedge to one point just
    /// outside. Each contains `inner`.
    pub fn notches(outer: &[P], inner: &[P]) -> Vec<Vec<P>> {
        let hull = |extra: &[P]| convex_hull(inner.iter().copied().chain(extra.iter().copied()));
        let mut out = vec![hull(&nearest_points_outside(outer, inner))];
        let (t, e) = closest_pair(outer, inner);
        let len = (t[0] - e[0]).hypot(t[1] - e[1]);
        if len > 1e-9 {
            let dir = [(t[0] - e[0]) / len, (t[1] - e[1]) / len];
            let perp = [-dir[1], dir[0]];
            let across = |p: P| (p[0] - t[0]) * perp[0] + (p[1] - t[1]) * perp[1];
            let (lo, hi) = inner.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| (lo.min(across(*p)), hi.max(across(*p))));
            for ext in [1.0, 5.0] {
                let at = |s: f64| [t[0] + dir[0] * ext + perp[0] * s, t[1] + dir[1] * ext + perp[1] * s];
                out.push(hull(&[at(lo), at(hi)]));
            }
            out.push(hull(&[[t[0] + dir[0], t[1] + dir[1]]]));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const O: LonLat = [-93.62, 42.03];

    /// A ring from metre offsets about O.
    fn ring(pts: &[[f64; 2]]) -> Vec<LonLat> {
        let p = Projection::new(O);
        pts.iter().map(|q| p.offset(q[0], q[1])).map(|q| [round7(q[0]), round7(q[1])]).collect()
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Vec<LonLat> {
        ring(&[[x, y], [x + w, y], [x + w, y + h], [x, y + h]])
    }

    fn poly(rings: Vec<Vec<LonLat>>) -> Polygon {
        Polygon { kind: PolygonType::Polygon, coordinates: rings }
    }

    fn circle(cx: f64, cy: f64, r: f64, n: usize) -> Vec<LonLat> {
        let pts: Vec<[f64; 2]> = (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / n as f64;
                [cx + r * a.cos(), cy + r * a.sin()]
            })
            .collect();
        ring(&pts)
    }

    fn v0(p: &Polygon) -> Result<(), ShapeCode> {
        check(p, &CollarLimits::V0, 5.0, 1.0, 0.0)
    }

    fn field() -> Vec<LonLat> {
        // First vertex at O so the check's projection matches the offsets.
        rect(0.0, 0.0, 200.0, 200.0)
    }

    #[test]
    fn codes_round_trip() {
        for c in ShapeCode::ALL {
            assert_eq!(ShapeCode::parse(c.as_str()), Some(c));
            assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
            assert!(!c.message().is_empty());
        }
    }

    #[test]
    fn a_square_with_a_hole_passes_in_both_windings() {
        let mut outer = field();
        let mut hole = rect(80.0, 80.0, 40.0, 40.0);
        v0(&poly(vec![outer.clone(), hole.clone()])).unwrap();
        outer.reverse();
        v0(&poly(vec![outer.clone(), hole.clone()])).unwrap();
        hole.reverse();
        v0(&poly(vec![outer, hole])).unwrap();
    }

    #[test]
    fn margins_are_checked_first() {
        let p = poly(vec![vec![[0.0, 0.0]]]);
        assert_eq!(check(&p, &CollarLimits::V0, -1.0, 1.0, 0.0), Err(ShapeCode::BadMargins));
        assert_eq!(check(&p, &CollarLimits::V0, 5.0, 1000.5, 0.0), Err(ShapeCode::BadMargins));
        assert_eq!(check(&p, &CollarLimits::V0, f64::NAN, 1.0, 0.0), Err(ShapeCode::BadMargins));
        assert_eq!(check(&poly(vec![field()]), &CollarLimits::V0, 1000.0, 0.0, 0.0), Ok(()));
    }

    #[test]
    fn counts() {
        let holes: Vec<Vec<LonLat>> = (0..17).map(|i| rect(20.0 + (i % 5) as f64 * 35.0, 20.0 + (i / 5) as f64 * 35.0, 11.0, 11.0)).collect();
        let mut rings = vec![field()];
        rings.extend(holes.iter().take(16).cloned());
        v0(&poly(rings.clone())).unwrap();
        rings.push(holes[16].clone());
        assert_eq!(v0(&poly(rings)), Err(ShapeCode::TooManyHoles));
        assert_eq!(check(&poly(vec![field(), rect(80.0, 80.0, 40.0, 40.0)]), &CollarLimits::LEGACY, 5.0, 1.0, 0.0), Err(ShapeCode::TooManyHoles));

        let c128 = circle(100.0, 100.0, 90.0, 128);
        v0(&poly(vec![c128])).unwrap();
        assert_eq!(v0(&poly(vec![circle(100.0, 100.0, 90.0, 129)])), Err(ShapeCode::TooManyVertices));
        assert_eq!(check(&poly(vec![circle(100.0, 100.0, 90.0, 65)]), &CollarLimits::LEGACY, 5.0, 1.0, 0.0), Err(ShapeCode::TooManyVertices));
        check(&poly(vec![circle(100.0, 100.0, 90.0, 64)]), &CollarLimits::LEGACY, 5.0, 1.0, 0.0).unwrap();
        v0(&poly(vec![field(), circle(100.0, 100.0, 30.0, 32)])).unwrap();
        assert_eq!(v0(&poly(vec![field(), circle(100.0, 100.0, 30.0, 33)])), Err(ShapeCode::TooManyVertices));
    }

    #[test]
    fn range_and_few_vertices() {
        assert_eq!(v0(&poly(vec![vec![[180.0000001, 0.0], [179.9, 0.0], [179.9, 0.1]]])), Err(ShapeCode::OutOfRange));
        assert_eq!(v0(&poly(vec![vec![[0.0, 90.5], [0.1, 89.9], [0.2, 89.9]]])), Err(ShapeCode::OutOfRange));
        assert_eq!(v0(&poly(vec![vec![[f64::INFINITY, 0.0], [0.1, 0.0], [0.2, 0.1]]])), Err(ShapeCode::OutOfRange));
        v0(&poly(vec![vec![[179.999, 0.0], [180.0, 0.0], [180.0, 0.001]]])).unwrap();
        let sq = field();
        assert_eq!(v0(&poly(vec![vec![sq[0], sq[1], sq[1], sq[0]]])), Err(ShapeCode::TooFewVertices), "duplicates and closing vertex don't count");
        assert_eq!(v0(&poly(vec![])), Err(ShapeCode::TooFewVertices));
        assert_eq!(v0(&poly(vec![sq.clone(), vec![sq[0], sq[1]]])), Err(ShapeCode::TooFewVertices));
    }

    #[test]
    fn topology_codes() {
        let bowtie = ring(&[[0.0, 0.0], [100.0, 100.0], [100.0, 0.0], [0.0, 100.0]]);
        assert_eq!(v0(&poly(vec![bowtie])), Err(ShapeCode::SelfIntersecting));
        // Touches itself at a vertex (two squares joined at a corner).
        let pinch = ring(&[[0.0, 0.0], [50.0, 0.0], [50.0, 50.0], [100.0, 50.0], [100.0, 100.0], [50.0, 100.0], [50.0, 50.0], [0.0, 50.0]]);
        assert_eq!(v0(&poly(vec![pinch])), Err(ShapeCode::SelfIntersecting));
        // A spike folding back along an edge.
        let spike = ring(&[[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [100.0, 50.0], [0.0, 100.0]]);
        assert_eq!(v0(&poly(vec![spike])), Err(ShapeCode::SelfIntersecting));
        let line = ring(&[[0.0, 0.0], [50.0, 0.0], [100.0, 0.0]]);
        assert_eq!(v0(&poly(vec![line])), Err(ShapeCode::SelfIntersecting));

        assert_eq!(v0(&poly(vec![field(), rect(150.0, 80.0, 100.0, 40.0)])), Err(ShapeCode::RingsCross));
        // A hole touching the outer ring at one vertex.
        assert_eq!(v0(&poly(vec![field(), ring(&[[200.0, 100.0], [150.0, 80.0], [150.0, 120.0]])])), Err(ShapeCode::RingsCross));
        assert_eq!(v0(&poly(vec![field(), rect(40.0, 40.0, 40.0, 40.0), rect(60.0, 60.0, 40.0, 40.0)])), Err(ShapeCode::RingsCross));
        assert_eq!(v0(&poly(vec![field(), rect(300.0, 80.0, 40.0, 40.0)])), Err(ShapeCode::HoleOutside));
        assert_eq!(v0(&poly(vec![field(), rect(40.0, 40.0, 120.0, 120.0), rect(80.0, 80.0, 20.0, 20.0)])), Err(ShapeCode::HolesOverlap));
    }

    #[test]
    fn areas_and_gaps() {
        // Right triangle of 1.4 m legs: 0.98 m².
        assert_eq!(v0(&poly(vec![ring(&[[0.0, 0.0], [1.4, 0.0], [0.0, 1.4]])])), Err(ShapeCode::ZeroArea));
        v0(&poly(vec![ring(&[[0.0, 0.0], [1.5, 0.0], [0.0, 1.5]])])).unwrap();
        assert_eq!(v0(&poly(vec![field(), rect(80.0, 80.0, 9.9, 10.0)])), Err(ShapeCode::HoleTooSmall));
        v0(&poly(vec![field(), rect(80.0, 80.0, 10.1, 10.0)])).unwrap();

        // Gap 12 m at warn 5: hole to outer, and hole to hole.
        v0(&poly(vec![field(), rect(12.1, 80.0, 20.0, 20.0)])).unwrap();
        assert_eq!(v0(&poly(vec![field(), rect(11.9, 80.0, 20.0, 20.0)])), Err(ShapeCode::HoleTooClose));
        v0(&poly(vec![field(), rect(50.0, 80.0, 20.0, 20.0), rect(82.1, 80.0, 20.0, 20.0)])).unwrap();
        assert_eq!(v0(&poly(vec![field(), rect(50.0, 80.0, 20.0, 20.0), rect(81.9, 80.0, 20.0, 20.0)])), Err(ShapeCode::HoleTooClose));
        // The server's slack widens the gap by 0.5 m.
        let p = poly(vec![field(), rect(12.3, 80.0, 20.0, 20.0)]);
        check(&p, &CollarLimits::V0, 5.0, 1.0, 0.0).unwrap();
        assert_eq!(check(&p, &CollarLimits::V0, 5.0, 1.0, SERVER_SLACK_M), Err(ShapeCode::HoleTooClose));
        // A wider warning zone needs a wider gap.
        assert_eq!(check(&poly(vec![field(), rect(20.0, 80.0, 20.0, 20.0)]), &CollarLimits::V0, 10.0, 1.0, 0.0), Err(ShapeCode::HoleTooClose));
        assert_eq!(min_gap_m(5.0), 12.0);
    }

    #[test]
    fn extreme_coordinates_do_not_overflow() {
        // Rings spanning the whole globe: every product stays inside int64
        // (debug builds panic on overflow), and the answer is a code.
        let huge = vec![[-180.0, -90.0], [180.0, -90.0], [180.0, 90.0], [-180.0, 90.0]];
        let across = vec![[179.0, 89.0], [-179.0, -89.0], [179.0, -89.0]];
        // Topology passes; at the pole a degree of longitude has no width.
        assert_eq!(v0(&poly(vec![huge.clone(), across])), Err(ShapeCode::ZeroArea));
        let crossing = vec![[179.0, 89.0], [-179.0, -89.0], [180.0, 0.0]];
        assert_eq!(v0(&poly(vec![huge, crossing])), Err(ShapeCode::RingsCross));
        let wide = vec![[-180.0, 0.0], [180.0, 0.0], [180.0, 89.0], [-180.0, 89.0]];
        assert_eq!(v0(&poly(vec![wide, vec![[-10.0, 10.0], [10.0, 10.0], [0.0, 20.0]]])), Ok(()));
    }

    fn total(p: &Polygon) -> usize {
        p.coordinates.iter().map(|r| r.len() - 1).sum()
    }

    fn covered_by(inner: &Polygon, outer: &Polygon, samples: &[LonLat]) -> bool {
        samples.iter().all(|s| !inner.contains(*s) || outer.contains(*s))
    }

    fn grid(n: usize, span: f64) -> Vec<LonLat> {
        let p = Projection::new(O);
        (0..n * n).map(|k| p.offset(span * (k % n) as f64 / n as f64 - span * 0.1, span * (k / n) as f64 / n as f64 - span * 0.1)).collect()
    }

    #[test]
    fn fit_leaves_a_fitting_shape_alone() {
        let p = poly(vec![field(), rect(80.0, 80.0, 40.0, 40.0)]);
        let f = fit(&p, &CollarLimits::V0);
        assert_eq!(f.coordinates.len(), 2);
        assert_eq!(f, p.validated().unwrap());
        assert_eq!(fit(&f, &CollarLimits::V0), f, "idempotent");
    }

    #[test]
    fn fit_shrinks_the_outer_ring_only() {
        let p = poly(vec![circle(100.0, 100.0, 95.0, 400)]);
        let f = fit(&p, &CollarLimits::LEGACY);
        assert!(f.coordinates[0].len() - 1 <= 64);
        v0(&f).unwrap();
        assert!(covered_by(&f, &p, &grid(60, 240.0)), "never grows");
        assert!(f.area_ha() > p.area_ha() * 0.98, "still close to the circle: {} vs {}", f.area_ha(), p.area_ha());
    }

    #[test]
    fn fit_grows_holes_and_keeps_gaps() {
        let p = poly(vec![circle(100.0, 100.0, 95.0, 200), circle(100.0, 100.0, 30.0, 90), circle(40.0, 100.0, 12.0, 60)]);
        let f = fit(&p, &CollarLimits::V0);
        assert!(f.coordinates[0].len() - 1 <= 128);
        assert!(f.coordinates[1..].iter().all(|h| h.len() - 1 <= 32));
        assert!(total(&f) <= 384);
        v0(&f).unwrap();
        assert!(covered_by(&f, &p, &grid(80, 240.0)), "fenced area only shrinks");
        // Holes contain the originals.
        let hole = |k: usize| Polygon::from_ring(p.coordinates[k].clone());
        let fhole = |k: usize| Polygon::from_ring(f.coordinates[k].clone());
        assert!(covered_by(&hole(1), &fhole(1), &grid(80, 240.0)));
        assert!(covered_by(&hole(2), &fhole(2), &grid(80, 240.0)));
    }

    #[test]
    fn fit_shrinks_and_grows_the_right_rings_in_every_winding() {
        // Reversed about the first vertex, so the projection origin stays put.
        let rev = |r: &[LonLat]| -> Vec<LonLat> { std::iter::once(r[0]).chain(r[1..].iter().rev().copied()).collect() };
        let outer = circle(100.0, 100.0, 95.0, 200);
        let holes = [circle(100.0, 100.0, 30.0, 90), circle(40.0, 100.0, 12.0, 60)];
        let probe = grid(80, 240.0);
        let mut areas = Vec::new();
        for flip_outer in [false, true] {
            for flips in [[false, false], [true, false], [false, true], [true, true]] {
                let mut rings = vec![if flip_outer { rev(&outer) } else { outer.clone() }];
                rings.extend(holes.iter().zip(flips).map(|(h, f)| if f { rev(h) } else { h.clone() }));
                let p = poly(rings);
                let f = fit(&p, &CollarLimits::V0);
                v0(&f).unwrap();
                assert_eq!(f.coordinates.len(), 3);
                assert!(covered_by(&f, &p, &probe), "fenced area only shrinks (outer flipped {flip_outer}, holes {flips:?})");
                for k in 1..3 {
                    assert!(
                        covered_by(&Polygon::from_ring(p.coordinates[k].clone()), &Polygon::from_ring(f.coordinates[k].clone()), &probe),
                        "hole {k} only grows"
                    );
                }
                assert!(f.area_ha() <= p.area_ha() && f.area_ha() > p.area_ha() * 0.97, "{} of {} ha", f.area_ha(), p.area_ha());
                areas.push(f.area_ha());
            }
        }
        let (lo, hi) = areas.iter().fold((f64::INFINITY, 0.0f64), |(lo, hi), a| (lo.min(*a), hi.max(*a)));
        assert!(hi - lo < 0.01 * hi, "fitted areas agree across windings: {areas:?}");
    }

    #[test]
    fn fit_for_legacy_drops_holes() {
        let p = poly(vec![field(), rect(80.0, 80.0, 40.0, 40.0)]);
        let f = fit(&p, &CollarLimits::LEGACY);
        assert_eq!(f.coordinates.len(), 1);
        assert_eq!(f.coordinates[0], p.validated().unwrap().coordinates[0]);
    }

    #[test]
    fn fit_merges_extra_holes() {
        let limits = CollarLimits { holes: 2, ..CollarLimits::V0 };
        let holes = [rect(30.0, 30.0, 20.0, 20.0), rect(70.0, 30.0, 20.0, 20.0), rect(140.0, 140.0, 20.0, 20.0)];
        let p = poly(vec![field(), holes[0].clone(), holes[1].clone(), holes[2].clone()]);
        let f = fit(&p, &limits);
        assert_eq!(f.coordinates.len(), 3);
        check(&f, &limits, 5.0, 1.0, 0.0).unwrap();
        assert!(covered_by(&f, &p, &grid(60, 240.0)));
        // The two close holes became one hull covering the ground between them.
        let between = Projection::new(O).offset(60.0, 40.0);
        assert!(p.contains(between) && !f.contains(between));
    }

    #[test]
    fn fit_reduces_total_vertices() {
        let limits = CollarLimits { total: 60, ..CollarLimits::V0 };
        let p = poly(vec![circle(100.0, 100.0, 95.0, 40), circle(100.0, 100.0, 30.0, 30)]);
        let f = fit(&p, &limits);
        assert!(total(&f) <= 60, "{}", total(&f));
        check(&f, &limits, 5.0, 1.0, 0.0).unwrap();
        assert!(covered_by(&f, &p, &grid(60, 240.0)));
    }

    #[test]
    fn fit_handles_a_concave_outer_ring() {
        // A comb: teeth pointing north, 150 vertices.
        let mut pts = vec![[0.0, 0.0], [300.0, 0.0]];
        for k in (0..74).rev() {
            let x = 4.0 * k as f64;
            pts.push([x + 4.0, 100.0 + (k % 2) as f64 * 20.0]);
        }
        let p = poly(vec![ring(&pts)]);
        let f = fit(&p, &CollarLimits::LEGACY);
        assert!(f.coordinates[0].len() - 1 <= 64);
        assert_eq!(v0(&f), Ok(()));
        assert!(covered_by(&f, &p, &grid(80, 360.0)));
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    #[test]
    fn random_shapes_fit_safely() {
        let mut rng = Lcg(7);
        let probe = grid(36, 260.0);
        for round in 0..24 {
            // A jagged star of 150-400 vertices and up to 20 detailed holes.
            let n = 150 + (rng.next() * 250.0) as usize;
            let outer: Vec<[f64; 2]> = (0..n)
                .map(|i| {
                    let a = std::f64::consts::TAU * i as f64 / n as f64;
                    let r = 95.0 + rng.next() * 10.0 * (1.0 + (a * 5.0).sin());
                    [110.0 + r * a.cos(), 110.0 + r * a.sin()]
                })
                .collect();
            let mut rings = vec![ring(&outer)];
            let mut centres: Vec<[f64; 2]> = Vec::new();
            for _ in 0..(rng.next() * 20.0) as usize {
                let c = [60.0 + rng.next() * 100.0, 60.0 + rng.next() * 100.0];
                let r = 6.0 + rng.next() * 6.0;
                if centres.iter().any(|d| (d[0] - c[0]).hypot(d[1] - c[1]) < 2.0 * 12.0 + 14.0) {
                    continue;
                }
                centres.push(c);
                let m = 36 + (rng.next() * 30.0) as usize;
                rings.push(circle(c[0], c[1], r, m));
            }
            let p = poly(rings);
            assert_eq!(v0(&p).err().filter(|c| !matches!(c, ShapeCode::TooManyVertices | ShapeCode::TooManyHoles)), None, "round {round}: input");
            // Points within 5 cm of an edge can flip either way with 7-decimal rounding.
            let clear: Vec<LonLat> = probe.iter().copied().filter(|q| p.coordinates.iter().all(|r| edge_distance(*q, r) > 0.05)).collect();
            for limits in [CollarLimits::V0, CollarLimits::LEGACY, CollarLimits { holes: 4, total: 200, ..CollarLimits::V0 }] {
                let f = fit(&p, &limits);
                assert_eq!(check(&f, &limits, 5.0, 1.0, SERVER_SLACK_M), Ok(()), "round {round}, {limits:?}");
                // LEGACY drops holes by design: compare with the outer ring alone.
                let reference = if limits.holes == 0 { poly(vec![p.coordinates[0].clone()]) } else { p.clone() };
                for q in &clear {
                    if f.contains(*q) && !reference.contains(*q) {
                        let pr = Projection::new(O);
                        let xy = pr.forward(*q);
                        let d: Vec<f64> = p.coordinates.iter().map(|r| edge_distance(*q, r)).collect();
                        panic!("round {round} {limits:?}: grew at {xy:?}, edge distances {d:?}, rings in {} out {}", p.coordinates.len(), f.coordinates.len());
                    }
                }
            }
        }
    }

    fn edge_distance(p: LonLat, ring: &[LonLat]) -> f64 {
        let proj = Projection::new(p);
        let r = proj.forward_ring(&clean_ring(ring));
        (0..r.len()).map(|i| crate::ring::distance_to_segment([0.0, 0.0], r[i], r[(i + 1) % r.len()])).fold(f64::INFINITY, f64::min)
    }

    #[test]
    #[ignore]
    fn bench_fit() {
        let n = 2000;
        let outer: Vec<[f64; 2]> = (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / n as f64;
                let r = 300.0 + 20.0 * (a * 37.0).sin();
                [310.0 + r * a.cos(), 310.0 + r * a.sin()]
            })
            .collect();
        let mut rings = vec![ring(&outer)];
        for k in 0..16 {
            rings.push(circle(175.0 + 90.0 * (k % 4) as f64, 175.0 + 90.0 * (k / 4) as f64, 15.0, 64));
        }
        let p = poly(rings);
        let t = std::time::Instant::now();
        let f = fit(&p, &CollarLimits::V0);
        eprintln!("V0 fit of {} vertices: {:?}", p.total_vertices(), t.elapsed());
        let t = std::time::Instant::now();
        let g = fit(&f, &CollarLimits::LEGACY);
        eprintln!("LEGACY fit of {} vertices: {:?} -> {}", f.total_vertices(), t.elapsed(), g.total_vertices());
        let t = std::time::Instant::now();
        eprintln!(
            "result {:?} total {} rings {} outer {}",
            check(&f, &CollarLimits::V0, 5.0, 1.0, 0.5),
            f.total_vertices(),
            f.coordinates.len(),
            f.coordinates[0].len()
        );
        eprintln!("check: {:?}", t.elapsed());
    }
}
