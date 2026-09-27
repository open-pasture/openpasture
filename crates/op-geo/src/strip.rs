//! Strips across a paddock, for strip grazing: parallel bands a herd is moved
//! through one at a time.
//!
//! `orientation_deg` is the compass bearing the strips advance toward: 0 means
//! the strips run east-west and advance north (strip 1 is the southernmost),
//! 90 means they run north-south and advance east. Every strip lies across that
//! direction.
//!
//! How it works: the area is projected to local metres about its outer ring's
//! first vertex (as everywhere in this crate) and rotated so the advance
//! direction points up. The depth along that axis is cut into bands of equal
//! width ([`StripBy::Width`], the last band keeps what is left) or into equal
//! bands ([`StripBy::Count`]). A band thinner than [`min_strip_m`] (two warning
//! zones and the gap between them, so a strip is never all warning zone) joins
//! the band before it, or the one after it when it is the first. Each band is
//! intersected with the area (holes and concave edges included) by `geo`'s
//! boolean operations. A band can fall into several pieces on a concave
//! paddock; each piece is its own strip, ordered across the band. A piece
//! whose mean depth (area over its width across) is still under
//! [`min_strip_m`], such as a wedge where the paddock's edge nearly follows a
//! cut, joins the neighbouring piece it shares the longest cut with. A piece
//! with no neighbour stays a strip of its own, so the strips always cover the
//! whole area.

use geo::{Area, BooleanOps, Coord, LineString, MultiPolygon, Polygon as GeoPolygon};

use crate::LonLat;
use crate::polygon::Polygon;
use crate::projection::Projection;
use crate::ring::clean_ring;
use crate::shape::{SERVER_SLACK_M, min_gap_m};

/// How wide the strips are.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StripBy {
    /// Metres along the advance direction; the last strip keeps what is left.
    Width(f64),
    /// This many strips of equal width.
    Count(u32),
}

/// Most nominal bands one call cuts before merging thin ones; a width that
/// would make more is widened so it makes this many.
const MAX_BANDS: usize = 10_000;

/// The thinnest strip kept: two warning zones and the collar's gap between
/// them, plus the server's slack (12.5 m at the default 5 m warning distance).
pub fn min_strip_m(warn_m: f64) -> f64 {
    min_gap_m(warn_m) + SERVER_SLACK_M
}

/// Local metres rotated so the advance direction is +v and "across" is +u.
struct Frame {
    proj: Projection,
    sin: f64,
    cos: f64,
}

impl Frame {
    fn new(origin: LonLat, orientation_deg: f64) -> Self {
        let t = orientation_deg.rem_euclid(360.0).to_radians();
        Self { proj: Projection::new(origin), sin: t.sin(), cos: t.cos() }
    }

    fn to_uv(&self, p: LonLat) -> Coord {
        let [x, y] = self.proj.forward(p);
        Coord { x: x * self.cos - y * self.sin, y: x * self.sin + y * self.cos }
    }

    fn to_lonlat(&self, c: Coord) -> LonLat {
        let (u, v) = (c.x, c.y);
        self.proj.inverse([u * self.cos + v * self.sin, -u * self.sin + v * self.cos])
    }

    fn polygon(&self, p: &Polygon) -> Option<GeoPolygon> {
        let outer = p.outer_ring();
        if outer.len() < 3 {
            return None;
        }
        let ring = |r: &[LonLat]| LineString::new(r.iter().map(|q| self.to_uv(*q)).collect());
        let holes = p.holes().filter(|h| h.len() >= 3).map(|h| ring(&h)).collect();
        Some(GeoPolygon::new(ring(&outer), holes))
    }

    fn back(&self, g: &GeoPolygon) -> Option<Polygon> {
        let ring = |ls: &LineString| -> Option<Vec<LonLat>> {
            let pts: Vec<[f64; 2]> = simplify(&ls.0.iter().map(|c| [c.x, c.y]).collect::<Vec<_>>());
            let out = clean_ring(&pts.into_iter().map(|p| self.to_lonlat(Coord { x: p[0], y: p[1] })).collect::<Vec<_>>());
            (out.len() >= 3).then_some(out)
        };
        let outer = ring(g.exterior())?;
        Some(Polygon::from_rings(outer, g.interiors().iter().filter_map(ring)))
    }
}

/// Parallel strips across `area` perpendicular to `orientation_deg` (0 =
/// strips run east-west, advancing north), in advance order. Strips thinner
/// than [`min_strip_m`]`(warn_m)` are merged into their neighbour; together the
/// strips cover `area`. An area with no outer ring gives no strips.
pub fn strips(area: &Polygon, orientation_deg: f64, by: StripBy, warn_m: f64) -> Vec<Polygon> {
    let outer = area.outer_ring();
    if outer.len() < 3 || !orientation_deg.is_finite() {
        return vec![];
    }
    let frame = Frame::new(outer[0], orientation_deg);
    let Some(shape) = frame.polygon(area) else { return vec![] };
    let (umin, umax, vmin, vmax) = extent(shape.exterior());
    let min_w = min_strip_m(if warn_m.is_finite() && warn_m >= 0.0 { warn_m } else { crate::shape::DEFAULT_WARN_M });
    let cuts = bands(vmax - vmin, by, min_w);

    // One intersection per band, a little wider than the area so nothing is lost at its edges.
    let pad = 1.0 + (umax - umin).max(vmax - vmin) * 1e-3;
    let mut pieces: Vec<Piece> = Vec::new();
    for (i, &(a, b)) in cuts.iter().enumerate() {
        let lo = if i == 0 { vmin - pad } else { vmin + a };
        let hi = if i + 1 == cuts.len() { vmax + pad } else { vmin + b };
        let band = rect(umin - pad, umax + pad, lo, hi);
        for g in shape.intersection(&band).0 {
            if g.unsigned_area() > 1e-6 {
                pieces.push(Piece::new(i, g));
            }
        }
    }
    merge_slivers(&mut pieces, min_w);
    pieces.sort_by(|a, b| a.band.cmp(&b.band).then(a.umid().total_cmp(&b.umid())));
    pieces.iter().filter_map(|p| frame.back(&p.poly)).collect()
}

/// How deep `area` is along the advance direction, in metres.
pub fn depth_m(area: &Polygon, orientation_deg: f64) -> f64 {
    let outer = area.outer_ring();
    if outer.len() < 3 || !orientation_deg.is_finite() {
        return 0.0;
    }
    let frame = Frame::new(outer[0], orientation_deg);
    let pts: Vec<Coord> = outer.iter().map(|p| frame.to_uv(*p)).collect();
    let (_, _, vmin, vmax) = extent(&LineString::new(pts));
    vmax - vmin
}

/// Hectares of `p` not covered by any of `cutouts` (e.g. a strip less its
/// exclusions). Equal to `p.area_ha()` when nothing overlaps.
pub fn area_outside_ha(p: &Polygon, cutouts: &[Polygon]) -> f64 {
    let full = p.area_ha();
    let outer = p.outer_ring();
    let Some(bb) = p.bbox() else { return full };
    let touching: Vec<&Polygon> =
        cutouts.iter().filter(|c| c.bbox().is_some_and(|cb| cb[0] <= bb[2] && cb[2] >= bb[0] && cb[1] <= bb[3] && cb[3] >= bb[1])).collect();
    if outer.len() < 3 || touching.is_empty() {
        return full;
    }
    let frame = Frame::new(outer[0], 0.0);
    let Some(shape) = frame.polygon(p) else { return full };
    let whole = shape.unsigned_area();
    if whole <= 0.0 {
        return 0.0;
    }
    let mut left = MultiPolygon(vec![shape]);
    for c in touching {
        if let Some(g) = frame.polygon(c) {
            left = left.difference(&g);
        }
    }
    (full * left.unsigned_area() / whole).clamp(0.0, full)
}

/// `[from, to)` offsets along the depth, thin bands merged.
fn bands(depth: f64, by: StripBy, min_w: f64) -> Vec<(f64, f64)> {
    if !(depth > 0.0) {
        return vec![(0.0, 0.0)];
    }
    let n = match by {
        StripBy::Count(n) => (n.max(1) as usize).min(MAX_BANDS),
        StripBy::Width(w) if w.is_finite() && w > 0.0 => ((depth / w - 1e-9).ceil().max(1.0) as usize).min(MAX_BANDS),
        StripBy::Width(_) => 1,
    };
    let w = match by {
        StripBy::Width(w) if w.is_finite() && w > 0.0 && n < MAX_BANDS => w,
        _ => depth / n as f64,
    };
    let nominal: Vec<(f64, f64)> = (0..n).map(|i| (i as f64 * w, ((i + 1) as f64 * w).min(depth))).collect();
    // Thin bands join the next one; a thin last band joins the one before it.
    let mut out: Vec<(f64, f64)> = Vec::new();
    let mut open: Option<(f64, f64)> = None;
    for (a, b) in nominal {
        let (start, _) = open.unwrap_or((a, b));
        if b - start >= min_w - 1e-9 {
            out.push((start, b));
            open = None;
        } else {
            open = Some((start, b));
        }
    }
    if let Some((a, b)) = open {
        match out.last_mut() {
            Some(last) => last.1 = b,
            None => out.push((a, b)),
        }
    }
    out
}

struct Piece {
    band: usize,
    poly: GeoPolygon,
}

impl Piece {
    fn new(band: usize, poly: GeoPolygon) -> Self {
        Self { band, poly }
    }

    fn umid(&self) -> f64 {
        let (umin, umax, _, _) = extent(self.poly.exterior());
        (umin + umax) / 2.0
    }

    /// Mean depth along the advance direction: area over width across.
    fn thickness(&self) -> f64 {
        let (umin, umax, _, _) = extent(self.poly.exterior());
        let across = umax - umin;
        if across <= 0.0 { 0.0 } else { self.poly.unsigned_area() / across }
    }

    /// Edges along a cut: `(v, umin, umax)` for each edge of constant v.
    fn level_edges(&self) -> Vec<(f64, f64, f64)> {
        let mut out = Vec::new();
        for ls in std::iter::once(self.poly.exterior()).chain(self.poly.interiors()) {
            for w in ls.0.windows(2) {
                let (a, b) = (w[0], w[1]);
                if (a.y - b.y).abs() < EDGE_EPS && (a.x - b.x).abs() > EDGE_EPS {
                    out.push(((a.y + b.y) / 2.0, a.x.min(b.x), a.x.max(b.x)));
                }
            }
        }
        out
    }
}

/// Metres within which two points count as the same along a cut.
const EDGE_EPS: f64 = 1e-3;

/// Length of cut two pieces share.
fn shared(a: &[(f64, f64, f64)], b: &[(f64, f64, f64)]) -> f64 {
    let mut total = 0.0;
    for &(va, a0, a1) in a {
        for &(vb, b0, b1) in b {
            if (va - vb).abs() < EDGE_EPS {
                total += (a1.min(b1) - a0.max(b0)).max(0.0);
            }
        }
    }
    total
}

/// Join every piece thinner than `min_w` into the neighbour it shares the
/// most cut with, thinnest first, until none can join.
fn merge_slivers(pieces: &mut Vec<Piece>, min_w: f64) {
    loop {
        let mut order: Vec<usize> = (0..pieces.len()).filter(|&i| pieces[i].thickness() < min_w - 1e-9).collect();
        order.sort_by(|&a, &b| pieces[a].poly.unsigned_area().total_cmp(&pieces[b].poly.unsigned_area()));
        let mut merged = false;
        for i in order {
            let edges = pieces[i].level_edges();
            let best = (0..pieces.len())
                .filter(|&j| j != i)
                .map(|j| (j, shared(&edges, &pieces[j].level_edges())))
                .filter(|(_, s)| *s > EDGE_EPS)
                .max_by(|a, b| a.1.total_cmp(&b.1));
            let Some((j, _)) = best else { continue };
            let joined = pieces[i].poly.union(&pieces[j].poly);
            if joined.0.len() != 1 {
                continue;
            }
            let poly = joined.0.into_iter().next().unwrap_or_else(|| pieces[j].poly.clone());
            let band = pieces[j].band;
            pieces[j] = Piece::new(band, poly);
            pieces.remove(i);
            merged = true;
            break;
        }
        if !merged {
            return;
        }
    }
}

fn extent(ls: &LineString) -> (f64, f64, f64, f64) {
    ls.0.iter().fold((f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY), |(a, b, c, d), p| (a.min(p.x), b.max(p.x), c.min(p.y), d.max(p.y)))
}

fn rect(u0: f64, u1: f64, v0: f64, v1: f64) -> GeoPolygon {
    GeoPolygon::new(LineString::from(vec![(u0, v0), (u1, v0), (u1, v1), (u0, v1), (u0, v0)]), vec![])
}

/// Drop repeated points and points on the straight line between their
/// neighbours (a cut leaves them where it crossed a straight edge).
fn simplify(ring: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut pts = clean_ring(ring);
    let mut changed = true;
    while changed && pts.len() > 3 {
        changed = false;
        let n = pts.len();
        for i in 0..n {
            let (a, b, c) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
            let cross = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
            let len = ((c[0] - a[0]).powi(2) + (c[1] - a[1]).powi(2)).sqrt();
            // Distance of b from the line a-c under a millimetre, and b between them.
            let between =
                (b[0] - a[0]) * (c[0] - a[0]) + (b[1] - a[1]) * (c[1] - a[1]) >= 0.0 && (b[0] - c[0]) * (a[0] - c[0]) + (b[1] - c[1]) * (a[1] - c[1]) >= 0.0;
            if len > 0.0 && (cross / len).abs() < 1e-3 && between {
                pts.remove(i);
                changed = true;
                break;
            }
        }
    }
    pts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::distance_m;

    // The live-check paddock near Ames: about 413 m east-west, 400 m north-south.
    fn ames() -> Polygon {
        Polygon::from_ring(vec![[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336]])
    }

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    fn total_ha(ss: &[Polygon]) -> f64 {
        ss.iter().map(Polygon::area_ha).sum()
    }

    /// Area of `area` the strips miss plus area they cover twice, in ha.
    fn gaps_and_overlaps(area: &Polygon, ss: &[Polygon]) -> f64 {
        let frame = Frame::new(area.outer_ring()[0], 0.0);
        let whole = MultiPolygon(vec![frame.polygon(area).unwrap()]);
        let mut union = MultiPolygon(vec![]);
        let mut sum = 0.0;
        for s in ss {
            let g = frame.polygon(s).unwrap();
            sum += g.unsigned_area();
            union = union.union(&g);
        }
        let missed = whole.difference(&union).unsigned_area();
        let extra = union.difference(&whole).unsigned_area();
        (missed + extra + (sum - union.unsigned_area()).abs()) / 10_000.0
    }

    #[test]
    fn count_cuts_equal_strips_advancing_north() {
        let ss = strips(&ames(), 0.0, StripBy::Count(12), 5.0);
        assert_eq!(ss.len(), 12);
        let a = ames().area_ha();
        for s in &ss {
            assert!(close(s.area_ha(), a / 12.0, a * 1e-3), "{} vs {}", s.area_ha(), a / 12.0);
            assert!(s.validated().is_ok());
            assert_eq!(s.outer_ring().len(), 4, "{:?}", s.outer_ring());
        }
        // Strip 1 is the southernmost; each next one lies north of it.
        let lat = |s: &Polygon| s.centroid().unwrap()[1];
        for w in ss.windows(2) {
            assert!(lat(&w[1]) > lat(&w[0]));
        }
        assert!(close(total_ha(&ss), a, 1e-4), "{} vs {a}", total_ha(&ss));
        assert!(gaps_and_overlaps(&ames(), &ss) < 1e-4);
    }

    #[test]
    fn width_cuts_from_the_start_and_the_rest_is_the_last_strip() {
        // 400 m deep advancing north: 30 m strips make 13 of 30 m and a 10 m rest,
        // too thin at 5 m warn (12.5 m), so it joins strip 13 (40 m).
        let ss = strips(&ames(), 0.0, StripBy::Width(30.0), 5.0);
        assert_eq!(ss.len(), 13);
        let depth = |s: &Polygon| {
            let b = s.bbox().unwrap();
            distance_m([b[0], b[1]], [b[0], b[3]])
        };
        for s in &ss[..12] {
            assert!(close(depth(s), 30.0, 0.05), "{}", depth(s));
        }
        assert!(close(depth(&ss[12]), 40.3, 0.3), "{}", depth(&ss[12]));
        assert!(gaps_and_overlaps(&ames(), &ss) < 1e-4);
        // A 20 m rest is thick enough to stay.
        let ss = strips(&ames(), 0.0, StripBy::Width(38.0), 5.0);
        assert_eq!(ss.len(), 11);
    }

    #[test]
    fn orientation_turns_the_strips() {
        // 90: strips run north-south and advance east.
        let ss = strips(&ames(), 90.0, StripBy::Count(4), 5.0);
        assert_eq!(ss.len(), 4);
        let c: Vec<LonLat> = ss.iter().map(|s| s.centroid().unwrap()).collect();
        for w in c.windows(2) {
            assert!(w[1][0] > w[0][0] && close(w[1][1], w[0][1], 1e-5));
        }
        // 180 advances south, 270 west: the same strips in reverse order.
        let south = strips(&ames(), 180.0, StripBy::Count(4), 5.0);
        assert!(south[0].centroid().unwrap()[1] > south[3].centroid().unwrap()[1]);
        let west = strips(&ames(), 270.0, StripBy::Count(4), 5.0);
        assert!(west[0].centroid().unwrap()[0] > west[3].centroid().unwrap()[0]);
        // 45: diagonal strips still cover the paddock exactly.
        let diag = strips(&ames(), 45.0, StripBy::Width(50.0), 5.0);
        assert!(diag.len() >= 10, "{}", diag.len());
        assert!(gaps_and_overlaps(&ames(), &diag) < 1e-3);
        assert!(close(total_ha(&diag), ames().area_ha(), 1e-3));
        for s in &diag {
            assert!(s.validated().is_ok(), "{s:?}");
        }
        // Depth follows the orientation.
        assert!(close(depth_m(&ames(), 0.0), 400.3, 0.5), "{}", depth_m(&ames(), 0.0));
        assert!(close(depth_m(&ames(), 90.0), 413.0, 0.5), "{}", depth_m(&ames(), 90.0));
        assert!(close(depth_m(&ames(), 360.0 + 90.0), depth_m(&ames(), 90.0), 1e-6));
    }

    #[test]
    fn thin_strips_merge_into_their_neighbours() {
        // 40 strips of 10 m are thinner than 12.5 m: pairs merge into 20 strips of 20 m.
        let ss = strips(&ames(), 0.0, StripBy::Count(40), 5.0);
        assert_eq!(ss.len(), 20);
        // A wider warning zone makes the least strip wider: warn 10 m needs 22.5 m, so
        // 10 m strips go in threes.
        let ss = strips(&ames(), 0.0, StripBy::Width(10.0), 10.0);
        assert_eq!(ss.len(), 13);
        assert!(gaps_and_overlaps(&ames(), &ss) < 1e-4);
        // A width under the least strip makes strips of whole multiples of it.
        let ss = strips(&ames(), 0.0, StripBy::Width(5.0), 5.0);
        assert_eq!(ss.len(), 26, "15 m strips from 5 m bands, the last 25 m");
        // One strip of everything when the paddock is thinner than the least strip.
        let sliver = Polygon::from_ring(vec![[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.03005], [-93.625, 42.03005]]);
        let ss = strips(&sliver, 0.0, StripBy::Count(3), 5.0);
        assert_eq!(ss.len(), 1);
    }

    #[test]
    fn a_wedge_left_by_a_cut_joins_its_neighbour() {
        // A paddock whose north edge climbs 20 m over its 414 m width: the band above
        // the last 100 m cut is 20.3 m deep at its east end but 0.3 m at its west end,
        // a wedge 10.3 m deep on average, so it joins the strip under it.
        let p = Polygon::from_ring(vec![[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336 + 20.0 / 111_195.0], [-93.625, 42.0336]]);
        let ss = strips(&p, 0.0, StripBy::Width(100.0), 5.0);
        assert_eq!(ss.len(), 4, "{:?}", ss.iter().map(Polygon::area_ha).collect::<Vec<_>>());
        // With a narrower warning zone (7 m least strip) the wedge is thick enough to stay.
        assert_eq!(strips(&p, 0.0, StripBy::Width(100.0), 2.25).len(), 5);
        assert!(gaps_and_overlaps(&p, &ss) < 1e-4);
        for s in &ss {
            assert!(s.validated().is_ok());
        }
    }

    #[test]
    fn concave_paddocks_split_a_band_into_pieces() {
        // A U open to the north: the upper bands cross both arms.
        let u = Polygon::from_ring(vec![
            [-93.625, 42.03],
            [-93.62, 42.03],
            [-93.62, 42.0336],
            [-93.621, 42.0336],
            [-93.621, 42.031],
            [-93.624, 42.031],
            [-93.624, 42.0336],
            [-93.625, 42.0336],
        ]);
        let ss = strips(&u, 0.0, StripBy::Width(100.0), 5.0);
        // South band whole (the U's base is 111 m deep: 100 m + 11 m), then two arms in
        // each band above; each arm's piece is its own strip, west before east.
        assert!(ss.len() >= 5, "{}", ss.len());
        assert!(gaps_and_overlaps(&u, &ss) < 1e-4);
        assert!(close(total_ha(&ss), u.area_ha(), 1e-3));
        for s in &ss {
            assert!(s.validated().is_ok());
        }
        let arms: Vec<&Polygon> = ss.iter().filter(|s| s.centroid().unwrap()[1] > 42.0322).collect();
        assert!(arms.len() >= 2);
        assert!(arms[0].centroid().unwrap()[0] < arms[1].centroid().unwrap()[0]);
    }

    #[test]
    fn holes_stay_holes_or_notch_the_strips() {
        // A pond in the middle of P1: strips around it carry it as a hole or a notch.
        let pond = vec![[-93.6235, 42.0315], [-93.6215, 42.0315], [-93.6215, 42.0325], [-93.6235, 42.0325]];
        let p = Polygon::from_rings(ames().outer_ring(), [pond.clone()]);
        let ss = strips(&p, 0.0, StripBy::Count(4), 5.0);
        assert!(ss.len() >= 4);
        assert!(close(total_ha(&ss), p.area_ha(), 1e-3), "{} vs {}", total_ha(&ss), p.area_ha());
        assert!(gaps_and_overlaps(&p, &ss) < 1e-4);
        // Nothing covers the pond's middle.
        assert!(!ss.iter().any(|s| s.contains([-93.6225, 42.032])));
        assert!(ss.iter().any(|s| s.contains([-93.6245, 42.0305])));
    }

    #[test]
    fn the_union_covers_the_area_for_any_orientation() {
        let p = Polygon::from_ring(vec![[-93.625, 42.03], [-93.62, 42.0305], [-93.6195, 42.034], [-93.6228, 42.0352], [-93.6252, 42.033]]);
        for deg in (0..360).step_by(15) {
            for by in [StripBy::Width(33.0), StripBy::Count(7)] {
                let ss = strips(&p, deg as f64, by, 5.0);
                assert!(!ss.is_empty());
                assert!(gaps_and_overlaps(&p, &ss) < 1e-3, "{deg} {by:?}: {}", gaps_and_overlaps(&p, &ss));
                for s in &ss {
                    assert!(s.validated().is_ok(), "{deg} {by:?}");
                }
            }
        }
    }

    #[test]
    fn area_outside_subtracts_what_overlaps() {
        let s = &strips(&ames(), 0.0, StripBy::Count(4), 5.0)[0];
        assert!(close(area_outside_ha(s, &[]), s.area_ha(), 1e-12));
        // An exclusion half in the strip, half out: only its inside half comes off.
        let b = s.bbox().unwrap();
        let mid = (b[1] + b[3]) / 2.0;
        let wet = Polygon::from_ring(vec![[-93.6240, mid], [-93.6230, mid], [-93.6230, b[1] - 0.0005], [-93.6240, b[1] - 0.0005]]);
        let inside = Polygon::from_ring(vec![[-93.6240, mid], [-93.6230, mid], [-93.6230, b[1]], [-93.6240, b[1]]]).area_ha();
        let got = area_outside_ha(s, &[wet]);
        assert!(close(got, s.area_ha() - inside, 1e-3), "{got} vs {}", s.area_ha() - inside);
        // An exclusion elsewhere takes nothing.
        let far = Polygon::from_ring(vec![[-93.60, 42.03], [-93.59, 42.03], [-93.59, 42.04]]);
        assert!(close(area_outside_ha(s, &[far]), s.area_ha(), 1e-9));
    }

    #[test]
    fn bad_input_gives_nothing_or_one_strip() {
        let empty = Polygon { kind: crate::PolygonType::Polygon, coordinates: vec![] };
        assert!(strips(&empty, 0.0, StripBy::Count(3), 5.0).is_empty());
        assert!(strips(&ames(), f64::NAN, StripBy::Count(3), 5.0).is_empty());
        assert_eq!(strips(&ames(), 0.0, StripBy::Width(f64::NAN), 5.0).len(), 1);
        assert_eq!(strips(&ames(), 0.0, StripBy::Width(-3.0), 5.0).len(), 1);
        assert_eq!(strips(&ames(), 0.0, StripBy::Count(0), 5.0).len(), 1);
    }
}
