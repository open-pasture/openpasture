//! Single rings: `[x, y]` points, unclosed unless noted.

use crate::projection::round7;
use crate::{GeoError, LonLat};

type P = [f64; 2];

fn same(a: P, b: P) -> bool {
    (a[0] - b[0]).abs() < 1e-12 && (a[1] - b[1]).abs() < 1e-12
}

/// Drop the closing vertex and consecutive duplicates.
pub fn clean_ring(ring: &[P]) -> Vec<P> {
    let mut points: Vec<P> = Vec::with_capacity(ring.len());
    for &p in ring {
        if points.last().is_some_and(|last| same(*last, p)) {
            continue;
        }
        points.push(p);
    }
    while points.len() > 1 && same(points[0], points[points.len() - 1]) {
        points.pop();
    }
    points
}

/// Shoelace area. Positive when counter-clockwise.
pub fn signed_area(ring: &[P]) -> f64 {
    let n = ring.len();
    let mut total = 0.0;
    for i in 0..n {
        let [x1, y1] = ring[i];
        let [x2, y2] = ring[(i + 1) % n];
        total += x1 * y2 - x2 * y1;
    }
    total / 2.0
}

/// Even-odd ray cast toward +x.
pub fn point_in_ring(point: P, ring: &[P]) -> bool {
    let [x, y] = point;
    let mut inside = false;
    let n = ring.len();
    if n == 0 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let [xi, yi] = ring[i];
        let [xj, yj] = ring[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn orient(a: P, b: P, c: P) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

/// True when two segments cross properly (touching ends don't count).
pub fn segments_cross(p1: P, p2: P, q1: P, q2: P) -> bool {
    let (d1, d2) = (orient(q1, q2, p1), orient(q1, q2, p2));
    let (d3, d4) = (orient(p1, p2, q1), orient(p1, p2, q2));
    ((d1 > 0.0) != (d2 > 0.0)) && ((d3 > 0.0) != (d4 > 0.0))
}

/// True when no two non-adjacent edges cross.
pub fn ring_is_simple(ring: &[P]) -> bool {
    let points = clean_ring(ring);
    let n = points.len();
    for i in 0..n {
        let (a1, a2) = (points[i], points[(i + 1) % n]);
        for j in (i + 1)..n {
            if (j + 1) % n == i || j == (i + 1) % n {
                continue;
            }
            if segments_cross(a1, a2, points[j], points[(j + 1) % n]) {
                return false;
            }
        }
    }
    true
}

/// Distance from a point to a segment, clamped to the segment ends.
pub fn distance_to_segment(p: P, a: P, b: P) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let mut t = 0.0;
    if len2 > 0.0 {
        t = (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0);
    }
    let cx = a[0] + t * dx - p[0];
    let cy = a[1] + t * dy - p[1];
    cx.hypot(cy)
}

/// Check a lon/lat ring and return it unclosed, cleaned and rounded to 7
/// decimals. `max_vertices` is the collar budget when the ring goes to a collar.
pub fn validate_ring(ring: &[LonLat], max_vertices: Option<usize>) -> Result<Vec<LonLat>, GeoError> {
    let rounded: Vec<LonLat> = ring.iter().map(|p| [round7(p[0]), round7(p[1])]).collect();
    let points = clean_ring(&rounded);
    for p in &points {
        if !p[0].is_finite() || !p[1].is_finite() || !(-180.0..=180.0).contains(&p[0]) || !(-90.0..=90.0).contains(&p[1]) {
            return Err(GeoError::OutOfRange);
        }
    }
    if points.len() < 3 {
        return Err(GeoError::TooFewVertices);
    }
    if let Some(max) = max_vertices
        && points.len() > max
    {
        return Err(GeoError::TooManyVertices(points.len(), max));
    }
    if !ring_is_simple(&points) {
        return Err(GeoError::SelfIntersecting);
    }
    let local = crate::Projection::new(points[0]).forward_ring(&points);
    if signed_area(&local).abs() < 1e-3 {
        return Err(GeoError::ZeroArea);
    }
    Ok(points)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_drops_closing_and_duplicates() {
        let r = clean_ring(&[[0.0, 0.0], [1.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]);
        assert_eq!(r, vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]);
    }

    #[test]
    fn bowtie_is_not_simple() {
        assert!(!ring_is_simple(&[[0.0, 0.0], [1.0, 1.0], [1.0, 0.0], [0.0, 1.0]]));
        assert!(ring_is_simple(&[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]));
    }

    #[test]
    fn validate_ring_checks() {
        let sq = [[-92.41, 38.12], [-92.40, 38.12], [-92.40, 38.13], [-92.41, 38.13], [-92.41, 38.12]];
        assert_eq!(validate_ring(&sq, Some(64)).unwrap().len(), 4);
        assert_eq!(validate_ring(&sq[..2], None), Err(GeoError::TooFewVertices));
        assert_eq!(validate_ring(&[[0.0, 0.0], [200.0, 0.0], [0.0, 1.0]], None), Err(GeoError::OutOfRange));
        let bowtie = [[-92.41, 38.12], [-92.40, 38.13], [-92.40, 38.12], [-92.41, 38.13]];
        assert_eq!(validate_ring(&bowtie, None), Err(GeoError::SelfIntersecting));
        let line = [[-92.41, 38.12], [-92.40, 38.12], [-92.39, 38.12]];
        assert_eq!(validate_ring(&line, None), Err(GeoError::ZeroArea));
        let circle: Vec<LonLat> = (0..70)
            .map(|i| {
                let a = 2.0 * std::f64::consts::PI * i as f64 / 70.0;
                [-92.395 + 0.004 * a.cos(), 38.125 + 0.004 * a.sin()]
            })
            .collect();
        assert_eq!(validate_ring(&circle, Some(64)), Err(GeoError::TooManyVertices(70, 64)));
    }
}
