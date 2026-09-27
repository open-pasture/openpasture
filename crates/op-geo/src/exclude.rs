//! Exclusions applied to a herd boundary before it is sent (protocol v1, §3.5
//! "server shaping"): each exclusion is cut out of the outer ring, becomes a
//! hole, is joined to the edge or to another hole, or is dropped.
//!
//! The rules keep every hole a collar could reject out of the result:
//! - An exclusion that crosses the boundary is cut out of it (`Cut`). If the
//!   cut splits the boundary, the largest piece is kept.
//! - An exclusion inside the boundary becomes a hole (`Hole`), grown about its
//!   centre to at least 110 m² when smaller (a collar refuses holes under
//!   100 m²; growing a hole is always the safe side).
//! - A hole closer to the edge than the gap (`2·warn_m + 2 m` plus the
//!   server's 0.5 m slack) is joined to the edge (`Join`): a notch as wide as
//!   the hole is cut from the hole to just outside the edge, so no corridor
//!   between them is all warning zone. Where the edge bends so that notch
//!   can't reach outside, a narrower one along the shortest way out is used.
//! - Holes closer to each other than the gap, or more holes than the collar
//!   holds, merge into their convex hull (`Join`), closest pair first.
//! - An exclusion that misses the boundary is `Drop`. So is one that covers
//!   the whole boundary (nothing would be left to graze), and every hole when
//!   the limits hold none (LEGACY: holes can't be enforced; the pre-send check
//!   reports `collars_no_holes`).
//!
//! Finally the shape is fitted to the limits ([`crate::shape::fit_gap`]).

use crate::LonLat;
use crate::limits::CollarLimits;
use crate::polygon::{Polygon, PolygonType};
use crate::projection::{Projection, round7};
use crate::ring::clean_ring;
use crate::shape::local::{self, P, Relation};
use crate::shape::{DEFAULT_WARN_M, MIN_HOLE_AREA_M2, SERVER_SLACK_M, fit_gap, min_gap_m, simple_after_rounding};

/// What happened to one exclusion.
///
/// `Drop` means the boundary doesn't change for it: it misses the boundary,
/// covers all of it, or the limits hold no holes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    /// Cut out of the outer ring (it crossed the edge, or its ground went with an earlier cut).
    Cut,
    /// Its own hole.
    Hole,
    /// Joined to the edge by a notch, or merged with other exclusions into one hole.
    Join,
    /// No effect on the boundary.
    Drop,
}

/// The boundary to send, and one placement per exclusion, in input order.
#[derive(Debug, Clone, PartialEq)]
pub struct Shaped {
    pub geometry: Polygon,
    pub placements: Vec<Placement>,
}

/// Holes are grown to this area so a collar's 100 m² floor never bites after rounding.
const GROWN_HOLE_M2: f64 = MIN_HOLE_AREA_M2 as f64 * 1.1;

struct Item {
    ring: Vec<P>,
    /// Exclusion indexes merged into this hole; empty for a hole the target already had.
    members: Vec<usize>,
    /// Joining it to the edge failed; leave it as a hole.
    stuck: bool,
}

/// Apply `exclusions` (their outer rings) to `target`, for collars with
/// `limits` and a warning distance of `warn_m`.
pub fn shape_target(target: &Polygon, exclusions: &[Polygon], limits: &CollarLimits, warn_m: f64) -> Shaped {
    let mut placements = vec![Placement::Drop; exclusions.len()];
    let warn_m = if warn_m.is_finite() && warn_m >= 0.0 { warn_m } else { DEFAULT_WARN_M };
    let gap = min_gap_m(warn_m) + SERVER_SLACK_M;
    let outer_ll = target.outer_ring();
    if outer_ll.len() < 3 {
        return Shaped { geometry: target.clone(), placements };
    }
    let proj = Projection::new(outer_ll[0]);
    let mut outer = local::ccw(proj.forward_ring(&outer_ll));
    let original = outer.clone();
    let mut items: Vec<Item> =
        target.holes().filter(|h| h.len() >= 3).map(|h| Item { ring: local::cw(proj.forward_ring(&h)), members: Vec::new(), stuck: false }).collect();

    for (i, e) in exclusions.iter().enumerate() {
        let ring = e.outer_ring();
        if ring.len() < 3 {
            continue;
        }
        let er = local::ccw(proj.forward_ring(&ring));
        if local::area(&er) < 1e-6 {
            continue;
        }
        match local::relate(&outer, &er) {
            Relation::Crosses => {
                if let Some(o) = cut_across(&proj, &outer, &er) {
                    outer = o;
                    placements[i] = Placement::Cut;
                }
            }
            Relation::Inside => items.push(Item { ring: local::cw(local::grow_to_area(er, GROWN_HOLE_M2)), members: vec![i], stuck: false }),
            // Its ground was already cut away with an earlier exclusion.
            Relation::Apart if matches!(local::relate(&original, &er), Relation::Crosses | Relation::Inside) => placements[i] = Placement::Cut,
            Relation::Covers | Relation::Apart => {}
        }
    }

    let set = |placements: &mut Vec<Placement>, item: &Item, p: Placement| {
        for m in &item.members {
            placements[*m] = p;
        }
    };
    'settle: loop {
        // Against the edge.
        for k in 0..items.len() {
            match local::relate(&outer, &items[k].ring) {
                Relation::Crosses => {
                    let item = items.remove(k);
                    if let Some(o) = cut_across(&proj, &outer, &item.ring) {
                        outer = o;
                    }
                    set(&mut placements, &item, if item.members.len() == 1 { Placement::Cut } else { Placement::Join });
                    continue 'settle;
                }
                Relation::Covers | Relation::Apart => {
                    // A cut took the edge past it: its ground is outside already.
                    let item = items.remove(k);
                    set(&mut placements, &item, if item.members.len() == 1 { Placement::Cut } else { Placement::Join });
                    continue 'settle;
                }
                Relation::Inside if !items[k].stuck && local::ring_distance(&outer, &items[k].ring) < gap => {
                    match local::notches(&outer, &items[k].ring).iter().find_map(|n| local::cut(&outer, n).filter(|o| sound(&proj, o))) {
                        Some(o) => {
                            outer = o;
                            let item = items.remove(k);
                            set(&mut placements, &item, Placement::Join);
                        }
                        None => items[k].stuck = true,
                    }
                    continue 'settle;
                }
                Relation::Inside => {}
            }
        }
        // Against each other.
        for a in 0..items.len() {
            for b in a + 1..items.len() {
                if local::relate(&items[a].ring, &items[b].ring) != Relation::Apart || local::ring_distance(&items[a].ring, &items[b].ring) < gap {
                    merge(&mut items, a, b);
                    continue 'settle;
                }
            }
        }
        if limits.holes > 0 && items.len() > limits.holes {
            let mut best = (f64::INFINITY, 0, 1);
            for a in 0..items.len() {
                for b in a + 1..items.len() {
                    let d = local::ring_distance(&items[a].ring, &items[b].ring);
                    if d < best.0 {
                        best = (d, a, b);
                    }
                }
            }
            merge(&mut items, best.1, best.2);
            continue 'settle;
        }
        break;
    }

    let mut rings = vec![outer];
    for item in items {
        if limits.holes == 0 {
            set(&mut placements, &item, Placement::Drop);
            continue;
        }
        set(&mut placements, &item, if item.members.len() == 1 { Placement::Hole } else { Placement::Join });
        rings.push(item.ring);
    }
    let coordinates: Vec<Vec<LonLat>> = rings
        .iter()
        .map(|r| {
            let mut ll = clean_ring(&proj.inverse_ring(r).into_iter().map(|p| [round7(p[0]), round7(p[1])]).collect::<Vec<_>>());
            if let Some(first) = ll.first().copied() {
                ll.push(first);
            }
            ll
        })
        .collect();
    let geometry = fit_gap(&Polygon { kind: PolygonType::Polygon, coordinates }, limits, gap);
    Shaped { geometry, placements }
}

/// The ring stays simple once rounded to 7 decimals.
fn sound(proj: &Projection, r: &[P]) -> bool {
    simple_after_rounding(&proj.inverse_ring(r))
}

/// Cut `clip`, which crosses `outer`, out of it. If the exact cut leaves
/// slivers that rounding would fold, cut `clip` grown by 0.5 m, then 1.5 m
/// (always the safe side), then take the exact cut anyway.
fn cut_across(proj: &Projection, outer: &[P], clip: &[P]) -> Option<Vec<P>> {
    let exact = local::cut(outer, clip)?;
    if sound(proj, &exact) {
        return Some(exact);
    }
    for grow in crate::clip::RETRY_MARGINS_M {
        if let Some(o) = local::cut(outer, &crate::clip::offset_ring(clip, grow)).filter(|o| sound(proj, o)) {
            return Some(o);
        }
    }
    Some(exact)
}

/// Merge items `a` and `b` (a < b) into their convex hull.
fn merge(items: &mut Vec<Item>, a: usize, b: usize) {
    let second = items.remove(b);
    let first = items.remove(a);
    let ring = local::cw(local::convex_hull(first.ring.into_iter().chain(second.ring)));
    let mut members = first.members;
    members.extend(second.members);
    items.push(Item { ring, members, stuck: false });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shape::{ShapeCode, check};

    const O: LonLat = [-93.62, 42.03];

    fn ring(pts: &[[f64; 2]]) -> Vec<LonLat> {
        let p = Projection::new(O);
        pts.iter().map(|q| p.offset(q[0], q[1])).map(|q| [round7(q[0]), round7(q[1])]).collect()
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Polygon {
        Polygon::from_ring(ring(&[[x, y], [x + w, y], [x + w, y + h], [x, y + h]]))
    }

    fn at(x: f64, y: f64) -> LonLat {
        Projection::new(O).offset(x, y)
    }

    fn field() -> Polygon {
        rect(0.0, 0.0, 200.0, 200.0)
    }

    fn valid(s: &Shaped, limits: &CollarLimits) {
        check(&s.geometry, limits, 5.0, 1.0, SERVER_SLACK_M).unwrap_or_else(|c| panic!("{c}: {:?}", s.geometry));
    }

    #[test]
    fn an_exclusion_inside_becomes_a_hole() {
        let s = shape_target(&field(), &[rect(80.0, 80.0, 30.0, 30.0)], &CollarLimits::V0, 5.0);
        assert_eq!(s.placements, vec![Placement::Hole]);
        assert_eq!(s.geometry.coordinates.len(), 2);
        valid(&s, &CollarLimits::V0);
        assert!(!s.geometry.contains(at(95.0, 95.0)));
        assert!(s.geometry.contains(at(50.0, 50.0)));
    }

    #[test]
    fn an_exclusion_across_the_edge_is_cut() {
        let s = shape_target(&field(), &[rect(180.0, 80.0, 40.0, 40.0)], &CollarLimits::V0, 5.0);
        assert_eq!(s.placements, vec![Placement::Cut]);
        assert_eq!(s.geometry.coordinates.len(), 1);
        valid(&s, &CollarLimits::V0);
        assert!(!s.geometry.contains(at(190.0, 100.0)));
        assert!(s.geometry.contains(at(170.0, 100.0)));
    }

    #[test]
    fn an_exclusion_near_the_edge_is_joined_to_it() {
        // 8 m from the east edge: closer than the 12.5 m gap.
        let s = shape_target(&field(), &[rect(172.0, 80.0, 20.0, 20.0)], &CollarLimits::V0, 5.0);
        assert_eq!(s.placements, vec![Placement::Join]);
        assert_eq!(s.geometry.coordinates.len(), 1, "a notch, not a hole");
        valid(&s, &CollarLimits::V0);
        assert!(!s.geometry.contains(at(182.0, 90.0)), "the exclusion");
        assert!(!s.geometry.contains(at(196.0, 90.0)), "the corridor to the edge");
        assert!(s.geometry.contains(at(196.0, 60.0)), "the rest of the edge");
    }

    #[test]
    fn close_holes_merge() {
        let s = shape_target(&field(), &[rect(60.0, 80.0, 20.0, 20.0), rect(86.0, 80.0, 20.0, 20.0)], &CollarLimits::V0, 5.0);
        assert_eq!(s.placements, vec![Placement::Join, Placement::Join]);
        assert_eq!(s.geometry.coordinates.len(), 2);
        valid(&s, &CollarLimits::V0);
        assert!(!s.geometry.contains(at(83.0, 90.0)), "the gap between them is closed");
    }

    #[test]
    fn far_apart_holes_stay_apart() {
        let s = shape_target(&field(), &[rect(40.0, 80.0, 20.0, 20.0), rect(120.0, 80.0, 20.0, 20.0)], &CollarLimits::V0, 5.0);
        assert_eq!(s.placements, vec![Placement::Hole, Placement::Hole]);
        assert_eq!(s.geometry.coordinates.len(), 3);
        valid(&s, &CollarLimits::V0);
    }

    #[test]
    fn a_wider_warning_zone_needs_a_wider_gap() {
        // 20 m apart: fine at warn 5 (gap 12.5), merged at warn 10 (gap 22.5).
        let ex = [rect(40.0, 80.0, 20.0, 20.0), rect(80.0, 80.0, 20.0, 20.0)];
        assert_eq!(shape_target(&field(), &ex, &CollarLimits::V0, 5.0).placements, vec![Placement::Hole, Placement::Hole]);
        let s = shape_target(&field(), &ex, &CollarLimits::V0, 10.0);
        assert_eq!(s.placements, vec![Placement::Join, Placement::Join]);
        check(&s.geometry, &CollarLimits::V0, 10.0, 1.0, SERVER_SLACK_M).unwrap();
    }

    #[test]
    fn placements_and_area_do_not_depend_on_winding() {
        // Reversed about the first vertex, so the projection origin stays put.
        let rev = |r: &[LonLat]| -> Vec<LonLat> { std::iter::once(r[0]).chain(r[1..].iter().rev().copied()).collect() };
        let outer = ring(&[[0.0, 0.0], [200.0, 0.0], [200.0, 200.0], [0.0, 200.0]]);
        let own_hole = ring(&[[20.0, 20.0], [50.0, 20.0], [50.0, 50.0], [20.0, 50.0]]);
        // Inside (a hole), across the east edge (a cut), 8 m from the north edge (a join).
        let ex = [rect(100.0, 100.0, 30.0, 30.0), rect(180.0, 20.0, 40.0, 40.0), rect(60.0, 172.0, 20.0, 20.0)];
        let want = vec![Placement::Hole, Placement::Cut, Placement::Join];
        let base = shape_target(&Polygon::from_rings(outer.clone(), [own_hole.clone()]), &ex, &CollarLimits::V0, 5.0);
        assert_eq!(base.placements, want);
        for flip_outer in [false, true] {
            for flip_hole in [false, true] {
                for flip_ex in [false, true] {
                    let target =
                        Polygon::from_rings(if flip_outer { rev(&outer) } else { outer.clone() }, [if flip_hole { rev(&own_hole) } else { own_hole.clone() }]);
                    let ex: Vec<Polygon> = ex.iter().map(|e| if flip_ex { Polygon::from_ring(rev(&e.outer_ring())) } else { e.clone() }).collect();
                    let s = shape_target(&target, &ex, &CollarLimits::V0, 5.0);
                    let label = format!("outer flipped {flip_outer}, hole {flip_hole}, exclusions {flip_ex}");
                    assert_eq!(s.placements, want, "{label}");
                    valid(&s, &CollarLimits::V0);
                    assert_eq!(s.geometry.coordinates.len(), 3, "{label}: own hole and the new one");
                    assert!(
                        (s.geometry.area_ha() - base.geometry.area_ha()).abs() < 1e-4,
                        "{label}: {} vs {} ha",
                        s.geometry.area_ha(),
                        base.geometry.area_ha()
                    );
                    assert!(s.geometry.area_ha() < target.area_ha() && s.geometry.area_ha() > 3.0, "{label}: {} ha", s.geometry.area_ha());
                    for p in [at(35.0, 35.0), at(115.0, 115.0), at(190.0, 40.0), at(70.0, 196.0)] {
                        assert!(!s.geometry.contains(p), "{label}: {p:?} excluded");
                    }
                    assert!(s.geometry.contains(at(150.0, 150.0)), "{label}");
                }
            }
        }
    }

    #[test]
    fn misses_and_covers_are_dropped() {
        let s = shape_target(&field(), &[rect(400.0, 400.0, 20.0, 20.0), rect(-50.0, -50.0, 400.0, 400.0)], &CollarLimits::V0, 5.0);
        assert_eq!(s.placements, vec![Placement::Drop, Placement::Drop]);
        assert_eq!(s.geometry, field().validated().unwrap());
    }

    #[test]
    fn legacy_limits_drop_holes_but_keep_cuts() {
        let s = shape_target(&field(), &[rect(80.0, 80.0, 30.0, 30.0), rect(180.0, 80.0, 40.0, 40.0)], &CollarLimits::LEGACY, 5.0);
        assert_eq!(s.placements, vec![Placement::Drop, Placement::Cut]);
        assert_eq!(s.geometry.coordinates.len(), 1);
        check(&s.geometry, &CollarLimits::LEGACY, 5.0, 1.0, SERVER_SLACK_M).unwrap();
    }

    #[test]
    fn small_exclusions_grow_to_a_valid_hole() {
        let s = shape_target(&field(), &[rect(95.0, 95.0, 4.0, 4.0)], &CollarLimits::V0, 5.0);
        assert_eq!(s.placements, vec![Placement::Hole]);
        valid(&s, &CollarLimits::V0);
        assert!(!s.geometry.contains(at(97.0, 97.0)));
        assert!(Polygon::from_ring(s.geometry.coordinates[1].clone()).area_ha() * 10_000.0 >= 100.0);
    }

    #[test]
    fn more_exclusions_than_holes_merge_down_to_the_limit() {
        let limits = CollarLimits { holes: 3, ..CollarLimits::V0 };
        let ex: Vec<Polygon> = (0..6).map(|k| rect(20.0 + 30.0 * k as f64, 90.0, 12.0, 12.0)).collect();
        let s = shape_target(&field(), &ex, &limits, 5.0);
        assert!(s.geometry.coordinates.len() - 1 <= 3);
        assert!(s.placements.iter().all(|p| matches!(p, Placement::Hole | Placement::Join)));
        check(&s.geometry, &limits, 5.0, 1.0, SERVER_SLACK_M).unwrap();
        for k in 0..6 {
            assert!(!s.geometry.contains(at(26.0 + 30.0 * k as f64, 96.0)), "exclusion {k} still excluded");
        }
    }

    #[test]
    fn a_cut_that_exposes_a_hole_to_the_edge_joins_it() {
        // The first exclusion cuts the east side back to x = 150; the second,
        // a hole at 132-142, is then within the gap of the new edge.
        let s = shape_target(&field(), &[rect(150.0, -10.0, 60.0, 220.0), rect(130.0, 90.0, 12.0, 12.0)], &CollarLimits::V0, 5.0);
        assert_eq!(s.placements[0], Placement::Cut);
        assert_eq!(s.placements[1], Placement::Join);
        valid(&s, &CollarLimits::V0);
        assert!(!s.geometry.contains(at(136.0, 96.0)));
    }

    #[test]
    fn shared_fence_lines_cut_cleanly() {
        // An exclusion sharing the target's south edge exactly.
        let s = shape_target(&field(), &[rect(50.0, 0.0, 30.0, 30.0)], &CollarLimits::V0, 5.0);
        assert_eq!(s.placements, vec![Placement::Cut]);
        valid(&s, &CollarLimits::V0);
        assert!(!s.geometry.contains(at(65.0, 10.0)));
    }

    #[test]
    fn result_never_fails_on_hole_codes() {
        // A crowd of exclusions of every kind still gives a valid shape.
        let mut ex = Vec::new();
        for k in 0..30 {
            let (x, y) = ((k * 37 % 190) as f64, (k * 53 % 190) as f64);
            ex.push(rect(x, y, 6.0 + (k % 4) as f64 * 5.0, 6.0 + (k % 3) as f64 * 5.0));
        }
        let s = shape_target(&field(), &ex, &CollarLimits::V0, 5.0);
        let r = check(&s.geometry, &CollarLimits::V0, 5.0, 1.0, SERVER_SLACK_M);
        assert!(
            !matches!(r, Err(ShapeCode::HoleTooClose | ShapeCode::HoleTooSmall | ShapeCode::HolesOverlap | ShapeCode::RingsCross | ShapeCode::TooManyHoles)),
            "{r:?}"
        );
        for (k, e) in ex.iter().enumerate() {
            let c = e.centroid().unwrap();
            assert!(s.placements[k] == Placement::Drop || !s.geometry.contains(c), "exclusion {k} ({:?}) is excluded", s.placements[k]);
        }
    }

    /// Tiny deterministic generator for the stress test.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    #[test]
    fn random_exclusions_always_give_a_safe_valid_shape() {
        let mut rng = Lcg(42);
        let probe: Vec<LonLat> = (0..900).map(|k| at(-20.0 + 8.0 * (k % 30) as f64, -20.0 + 8.0 * (k / 30) as f64)).collect();
        for round in 0..300 {
            // A random convex-ish or concave target and 1-12 random exclusions.
            let n = 5 + (rng.next() * 20.0) as usize;
            let target_pts: Vec<[f64; 2]> = (0..n)
                .map(|i| {
                    let a = std::f64::consts::TAU * i as f64 / n as f64;
                    let r = 70.0 + rng.next() * 40.0;
                    [100.0 + r * a.cos(), 100.0 + r * a.sin()]
                })
                .collect();
            let target = Polygon::from_ring(ring(&target_pts));
            let count = 1 + (rng.next() * 12.0) as usize;
            let ex: Vec<Polygon> = (0..count)
                .map(|_| {
                    let (x, y) = (rng.next() * 220.0 - 10.0, rng.next() * 220.0 - 10.0);
                    let (w, h) = (3.0 + rng.next() * 35.0, 3.0 + rng.next() * 35.0);
                    rect(x, y, w, h)
                })
                .collect();
            let warn = [3.0, 5.0, 8.0][round % 3];
            let limits = if round % 5 == 0 { CollarLimits { holes: 2, ..CollarLimits::V0 } } else { CollarLimits::V0 };
            let s = shape_target(&target, &ex, &limits, warn);
            let r = check(&s.geometry, &limits, warn, 1.0, SERVER_SLACK_M);
            assert!(r.is_ok(), "round {round}: {r:?} {:?}", s.placements);
            // Points within 5 cm of an edge can flip either way with 7-decimal rounding.
            let clear = |p: &LonLat, poly: &Polygon| poly.coordinates.iter().all(|r| edge_distance(*p, r) > 0.05);
            for p in probe.iter().filter(|p| clear(p, &target)) {
                assert!(!s.geometry.contains(*p) || target.contains(*p), "round {round}: grew at {p:?}");
            }
            for (k, e) in ex.iter().enumerate() {
                let overlap = probe.iter().filter(|p| clear(p, e) && clear(p, &target)).any(|p| e.contains(*p) && target.contains(*p));
                if s.placements[k] == Placement::Drop {
                    let covers = target.outer_ring().iter().all(|v| e.contains(*v));
                    assert!(!overlap || covers, "round {round}: exclusion {k} dropped but overlaps the target");
                    continue;
                }
                for p in probe.iter().filter(|p| clear(p, e)) {
                    assert!(!(e.contains(*p) && s.geometry.contains(*p)), "round {round}: exclusion {k} ({:?}) not excluded at {p:?}", s.placements[k]);
                }
            }
        }
    }

    fn edge_distance(p: LonLat, ring: &[LonLat]) -> f64 {
        let proj = Projection::new(p);
        let r = proj.forward_ring(&clean_ring(ring));
        (0..r.len()).map(|i| crate::ring::distance_to_segment([0.0, 0.0], r[i], r[(i + 1) % r.len()])).fold(f64::INFINITY, f64::min)
    }
}
