//! Union and difference of simple rings, for turning boundary commands into
//! single collar rings. Port of the agent kit's `collars/rings.py`.
//!
//! Transitions merge the old paddock, a corridor and the new paddock;
//! exclusions that cross a boundary are cut out of it
//! ([`crate::exclude::Placement::Cut`]), and exclusions inside it become holes
//! ([`crate::exclude::Placement::Hole`]) instead. Greiner-Hormann clipping over
//! rings projected to local metres.
//!
//! Paddocks often share fence lines, so edges overlap exactly. That is a
//! degenerate case for the algorithm, so on a degenerate input the clip ring is
//! grown outward by a small margin and the operation retried. The growth is
//! always toward the safe side: an exclusion gets slightly bigger, and a
//! transition area gets slightly bigger for its short open phase.

use crate::LonLat;
use crate::projection::Projection;
use crate::ring::{clean_ring, distance_to_segment, point_in_ring, signed_area};

type P = [f64; 2];

const PARAM_TOLERANCE: f64 = 1e-9;
const DISTANCE_TOLERANCE_M: f64 = 1e-6;
/// Outward growth (metres) tried when the exact operation hits a degenerate case.
pub const RETRY_MARGINS_M: [f64; 2] = [0.5, 1.5];

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RingError {
    /// The clip ring sits wholly inside the subject: it is a hole, not a cut
    /// (see [`crate::exclude::shape_target`]).
    #[error("The exclusion sits entirely inside the boundary, so it is a hole, not a cut.")]
    Hole,
    #[error("{0}")]
    Degenerate(&'static str),
    #[error("{0}")]
    Failed(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Union,
    Difference,
}

fn on_boundary(p: P, ring: &[P]) -> bool {
    (0..ring.len()).any(|i| distance_to_segment(p, ring[i], ring[(i + 1) % ring.len()]) < DISTANCE_TOLERANCE_M)
}

/// Grow a ring outward by `distance` metres using mitered vertex offsets.
pub fn offset_ring(ring: &[P], distance: f64) -> Vec<P> {
    let ccw = signed_area(ring) > 0.0;
    let n = ring.len();
    let mut grown = Vec::with_capacity(n);
    for i in 0..n {
        let prev = ring[(i + n - 1) % n];
        let point = ring[i];
        let next = ring[(i + 1) % n];
        let mut normals = [[0.0; 2]; 2];
        for (k, (a, b)) in [(prev, point), (point, next)].into_iter().enumerate() {
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let mut len = dx.hypot(dy);
            if len == 0.0 {
                len = 1.0;
            }
            normals[k] = if ccw { [dy / len, -dx / len] } else { [-dy / len, dx / len] };
        }
        let (mut bx, mut by) = (normals[0][0] + normals[1][0], normals[0][1] + normals[1][1]);
        let mut norm = bx.hypot(by);
        if norm < 1e-9 {
            bx = normals[0][0];
            by = normals[0][1];
            norm = 1.0;
        }
        bx /= norm;
        by /= norm;
        let cos_half = bx * normals[0][0] + by * normals[0][1];
        let scale = distance / cos_half.max(0.3);
        grown.push([point[0] + bx * scale, point[1] + by * scale]);
    }
    grown
}

#[derive(Clone)]
struct Node {
    p: P,
    next: usize,
    prev: usize,
    intersect: bool,
    entry: bool,
    neighbor: Option<usize>,
    alpha: f64,
    visited: bool,
}

struct Graph {
    nodes: Vec<Node>,
}

impl Graph {
    fn node(p: P, intersect: bool, alpha: f64) -> Node {
        Node { p, next: 0, prev: 0, intersect, entry: false, neighbor: None, alpha, visited: false }
    }

    fn build(&mut self, ring: &[P]) -> Vec<usize> {
        let start = self.nodes.len();
        let n = ring.len();
        for &p in ring {
            self.nodes.push(Self::node(p, false, 0.0));
        }
        let ids: Vec<usize> = (start..start + n).collect();
        for k in 0..n {
            self.nodes[ids[k]].next = ids[(k + 1) % n];
            self.nodes[ids[k]].prev = ids[(k + n - 1) % n];
        }
        ids
    }

    fn insert(&mut self, node: usize, start: usize, end: usize) {
        let mut current = self.nodes[start].next;
        while current != end && self.nodes[current].intersect && self.nodes[current].alpha < self.nodes[node].alpha {
            current = self.nodes[current].next;
        }
        let prev = self.nodes[current].prev;
        self.nodes[node].next = current;
        self.nodes[node].prev = prev;
        self.nodes[prev].next = node;
        self.nodes[current].prev = node;
    }
}

fn intersection(p1: P, p2: P, q1: P, q2: P) -> Result<Option<(f64, f64, P)>, RingError> {
    let (rx, ry) = (p2[0] - p1[0], p2[1] - p1[1]);
    let (sx, sy) = (q2[0] - q1[0], q2[1] - q1[1]);
    let denom = rx * sy - ry * sx;
    let (qpx, qpy) = (q1[0] - p1[0], q1[1] - p1[1]);
    if denom.abs() < 1e-12 {
        // Parallel. Overlapping collinear edges are degenerate for the algorithm.
        if (qpx * ry - qpy * rx).abs() < 1e-9
            && (distance_to_segment(q1, p1, p2) < DISTANCE_TOLERANCE_M
                || distance_to_segment(q2, p1, p2) < DISTANCE_TOLERANCE_M
                || distance_to_segment(p1, q1, q2) < DISTANCE_TOLERANCE_M)
        {
            return Err(RingError::Degenerate("Edges overlap."));
        }
        return Ok(None);
    }
    let a = (qpx * sy - qpy * sx) / denom;
    let b = (qpx * ry - qpy * rx) / denom;
    if a < -PARAM_TOLERANCE || a > 1.0 + PARAM_TOLERANCE || b < -PARAM_TOLERANCE || b > 1.0 + PARAM_TOLERANCE {
        return Ok(None);
    }
    if a.abs().min((1.0 - a).abs()).min(b.abs()).min((1.0 - b).abs()) <= PARAM_TOLERANCE * 10.0 {
        return Err(RingError::Degenerate("Rings touch at a vertex."));
    }
    Ok(Some((a, b, [p1[0] + a * rx, p1[1] + a * ry])))
}

/// Greiner-Hormann for simple rings.
fn boolean(subject: &[P], clip: &[P], op: Op) -> Result<Vec<Vec<P>>, RingError> {
    let mut g = Graph { nodes: Vec::new() };
    let subject_ids = g.build(subject);
    let clip_ids = g.build(clip);
    let mut found = false;
    for i in 0..subject_ids.len() {
        let s1 = subject_ids[i];
        let s2 = subject_ids[(i + 1) % subject_ids.len()];
        for j in 0..clip_ids.len() {
            let c1 = clip_ids[j];
            let c2 = clip_ids[(j + 1) % clip_ids.len()];
            let Some((a, b, p)) = intersection(g.nodes[s1].p, g.nodes[s2].p, g.nodes[c1].p, g.nodes[c2].p)? else {
                continue;
            };
            let s_node = g.nodes.len();
            g.nodes.push(Graph::node(p, true, a));
            let c_node = g.nodes.len();
            g.nodes.push(Graph::node(p, true, b));
            g.nodes[s_node].neighbor = Some(c_node);
            g.nodes[c_node].neighbor = Some(s_node);
            g.insert(s_node, s1, s2);
            g.insert(c_node, c1, c2);
            found = true;
        }
    }

    if !found {
        if subject.iter().any(|p| on_boundary(*p, clip)) || clip.iter().any(|p| on_boundary(*p, subject)) {
            return Err(RingError::Degenerate("Rings touch without crossing."));
        }
        let subject_inside = point_in_ring(subject[0], clip);
        let clip_inside = point_in_ring(clip[0], subject);
        return match op {
            Op::Union if subject_inside => Ok(vec![clip.to_vec()]),
            Op::Union if clip_inside => Ok(vec![subject.to_vec()]),
            Op::Union => Ok(vec![subject.to_vec(), clip.to_vec()]),
            Op::Difference if subject_inside => Ok(vec![]),
            Op::Difference if clip_inside => Err(RingError::Hole),
            Op::Difference => Ok(vec![subject.to_vec()]),
        };
    }

    // Entry/exit marking. Union flips both lists; difference flips the subject.
    for (start, other, flip) in [(subject_ids[0], clip, true), (clip_ids[0], subject, op == Op::Union)] {
        let start_p = g.nodes[start].p;
        if on_boundary(start_p, other) {
            return Err(RingError::Degenerate("Vertex lies on the other ring."));
        }
        let mut inside = point_in_ring(start_p, other);
        let mut node = start;
        loop {
            if g.nodes[node].intersect {
                g.nodes[node].entry = (!inside) != flip;
                inside = !inside;
            }
            node = g.nodes[node].next;
            if node == start {
                break;
            }
        }
    }

    let mut intersections = Vec::new();
    let mut node = subject_ids[0];
    loop {
        if g.nodes[node].intersect {
            intersections.push(node);
        }
        node = g.nodes[node].next;
        if node == subject_ids[0] {
            break;
        }
    }

    let mut results = Vec::new();
    for start in intersections {
        if g.nodes[start].visited {
            continue;
        }
        let mut polygon = vec![g.nodes[start].p];
        let mut current = start;
        g.nodes[current].visited = true;
        if let Some(n) = g.nodes[current].neighbor {
            g.nodes[n].visited = true;
        }
        let mut closed = false;
        for _ in 0..10_000 {
            let forward = g.nodes[current].entry;
            loop {
                current = if forward { g.nodes[current].next } else { g.nodes[current].prev };
                polygon.push(g.nodes[current].p);
                if g.nodes[current].intersect {
                    break;
                }
            }
            g.nodes[current].visited = true;
            let Some(neighbor) = g.nodes[current].neighbor else {
                return Err(RingError::Failed("Broken intersection graph.".into()));
            };
            g.nodes[neighbor].visited = true;
            current = neighbor;
            if current == start || g.nodes[current].neighbor == Some(start) {
                closed = true;
                break;
            }
        }
        if !closed {
            return Err(RingError::Failed("Clipping did not close.".into()));
        }
        let cleaned = clean_ring(&polygon);
        if cleaned.len() >= 3 && signed_area(&cleaned).abs() > 1e-6 {
            results.push(cleaned);
        }
    }
    Ok(results)
}

fn with_retries(subject: &[P], clip: &[P], op: Op) -> Result<Vec<Vec<P>>, RingError> {
    let mut last_error = match boolean(subject, clip, op) {
        Ok(r) => return Ok(r),
        Err(RingError::Hole) => return Err(RingError::Hole),
        Err(e) => e,
    };
    for margin in RETRY_MARGINS_M {
        match boolean(subject, &offset_ring(clip, margin), op) {
            Ok(r) => return Ok(r),
            Err(RingError::Hole) => return Err(RingError::Hole),
            Err(e) => last_error = e,
        }
    }
    Err(RingError::Failed(format!("Could not combine the shapes cleanly ({last_error}).")))
}

/// `subject` minus `clip`, both simple rings in local metres: every piece
/// left (none when `clip` covers `subject`). Degenerate contact (shared
/// edges, touching vertices) grows `clip` slightly, as [`subtract_ring`] does.
pub(crate) fn subtract_pieces(subject: &[P], clip: &[P]) -> Result<Vec<Vec<P>>, RingError> {
    with_retries(&clean_ring(subject), &clean_ring(clip), Op::Difference)
}

/// Merge lon/lat rings into one ring. Fails when they do not form one area.
pub fn union_rings(rings: &[Vec<LonLat>]) -> Result<Vec<LonLat>, RingError> {
    let cleaned: Vec<Vec<P>> = rings.iter().filter(|r| !r.is_empty()).map(|r| clean_ring(r)).collect();
    let Some(first) = cleaned.first() else {
        return Err(RingError::Failed("Nothing to merge.".into()));
    };
    let projection = Projection::new(first[0]);
    let mut merged = projection.forward_ring(first);
    for ring in &cleaned[1..] {
        let mut pieces = with_retries(&merged, &projection.forward_ring(ring), Op::Union)?;
        if pieces.len() != 1 {
            return Err(RingError::Failed("The shapes do not join into one connected area without gaps.".into()));
        }
        merged = pieces.remove(0);
    }
    Ok(projection.inverse_ring(&merged))
}

/// Cut an exclusion out of a boundary ring.
///
/// Returns the new ring, or `None` when the exclusion does not touch the
/// boundary. Fails when the exclusion sits wholly inside (a hole), splits the
/// boundary, or covers it.
pub fn subtract_ring(boundary: &[LonLat], exclusion: &[LonLat]) -> Result<Option<Vec<LonLat>>, RingError> {
    let boundary_points = clean_ring(boundary);
    if boundary_points.is_empty() {
        return Err(RingError::Failed("Nothing to cut from.".into()));
    }
    let projection = Projection::new(boundary_points[0]);
    let subject = projection.forward_ring(&boundary_points);
    let clip = projection.forward_ring(&clean_ring(exclusion));
    let pieces = with_retries(&subject, &clip, Op::Difference)?;
    if pieces.is_empty() {
        return Err(RingError::Failed("The exclusion covers the whole boundary.".into()));
    }
    if pieces.len() > 1 {
        return Err(RingError::Failed("The exclusion splits the boundary into separate pieces.".into()));
    }
    if pieces[0].len() == subject.len() && (signed_area(&pieces[0]).abs() - signed_area(&subject).abs()).abs() < 1e-6 {
        return Ok(None);
    }
    Ok(Some(projection.inverse_ring(&pieces[0])))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Same paddocks as the kit's collar tests.
    fn home() -> Vec<LonLat> {
        vec![[-92.41, 38.12], [-92.40, 38.12], [-92.40, 38.13], [-92.41, 38.13], [-92.41, 38.12]]
    }
    fn north() -> Vec<LonLat> {
        vec![[-92.40, 38.12], [-92.39, 38.12], [-92.39, 38.13], [-92.40, 38.13], [-92.40, 38.12]]
    }

    #[test]
    fn union_of_neighbouring_paddocks_covers_both() {
        let merged = union_rings(&[home(), north()]).unwrap();
        assert!(point_in_ring([-92.405, 38.125], &merged));
        assert!(point_in_ring([-92.395, 38.125], &merged));
        assert!(!point_in_ring([-92.385, 38.125], &merged));
    }

    #[test]
    fn union_of_overlapping_rings() {
        let a = vec![[0.0, 0.0], [0.001, 0.0], [0.001, 0.001], [0.0, 0.001]];
        let b = vec![[0.0005, 0.0005], [0.0015, 0.0005], [0.0015, 0.0015], [0.0005, 0.0015]];
        let merged = union_rings(&[a, b]).unwrap();
        assert_eq!(merged.len(), 8);
        assert!(point_in_ring([0.0012, 0.0012], &merged));
        assert!(point_in_ring([0.0002, 0.0002], &merged));
    }

    #[test]
    fn union_without_a_path_is_refused() {
        let far = vec![[-92.30, 38.12], [-92.29, 38.12], [-92.29, 38.13], [-92.30, 38.13]];
        assert!(union_rings(&[home(), far]).is_err());
    }

    #[test]
    fn exclusion_at_edge_is_cut() {
        let pond_edge = vec![[-92.392, 38.124], [-92.389, 38.124], [-92.389, 38.126], [-92.392, 38.126]];
        let cut = subtract_ring(&north(), &pond_edge).unwrap().unwrap();
        assert!(!point_in_ring([-92.3905, 38.125], &cut));
        assert!(point_in_ring([-92.395, 38.125], &cut));
    }

    #[test]
    fn exclusion_inside_is_a_hole() {
        let hole = vec![[-92.396, 38.124], [-92.394, 38.124], [-92.394, 38.126]];
        assert_eq!(subtract_ring(&north(), &hole), Err(RingError::Hole));
    }

    #[test]
    fn exclusion_elsewhere_changes_nothing() {
        let away = vec![[-92.30, 38.12], [-92.29, 38.12], [-92.29, 38.13]];
        assert_eq!(subtract_ring(&north(), &away), Ok(None));
    }

    #[test]
    fn cuts_and_unions_do_not_depend_on_winding() {
        let rev = |r: &[LonLat]| r.iter().rev().copied().collect::<Vec<_>>();
        let area = |r: &[LonLat]| crate::Polygon::from_ring(r.to_vec()).area_ha();
        let pond_edge = vec![[-92.392, 38.124], [-92.389, 38.124], [-92.389, 38.126], [-92.392, 38.126]];
        let pond = vec![[-92.396, 38.124], [-92.394, 38.124], [-92.394, 38.126]];
        let cut = area(&subtract_ring(&north(), &pond_edge).unwrap().unwrap());
        let merged = area(&union_rings(&[home(), north()]).unwrap());
        // The shared fence line makes the union grow the clip ring by 0.5 m.
        let both = area(&home()) + area(&north());
        assert!(merged >= both && merged < both + 0.5, "{merged} vs {both}");
        for b in [north(), rev(&north())] {
            for e in [pond_edge.clone(), rev(&pond_edge)] {
                let c = subtract_ring(&b, &e).unwrap().unwrap();
                assert!((area(&c) - cut).abs() < 1e-6, "{} vs {cut} ha", area(&c));
                assert!(!point_in_ring([-92.3905, 38.125], &c));
                assert!(point_in_ring([-92.395, 38.125], &c));
            }
            for h in [pond.clone(), rev(&pond)] {
                assert_eq!(subtract_ring(&b, &h), Err(RingError::Hole));
            }
            for a in [home(), rev(&home())] {
                let m = union_rings(&[a, b.clone()]).unwrap();
                assert!((area(&m) - merged).abs() < 1e-6, "{} vs {merged} ha", area(&m));
            }
        }
    }

    #[test]
    fn exclusion_covering_everything_fails() {
        let big = vec![[-93.0, 38.0], [-92.0, 38.0], [-92.0, 39.0], [-93.0, 39.0]];
        assert!(subtract_ring(&north(), &big).is_err());
    }
}
