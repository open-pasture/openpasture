//! GeoJSON Polygon: an outer ring and optional holes, each ring closed.

use serde::{Deserialize, Serialize};

use crate::projection::Projection;
use crate::ring::{clean_ring, point_in_ring, signed_area, validate_ring};
use crate::{GeoError, LonLat};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolygonType {
    #[default]
    Polygon,
}

/// `{ "type": "Polygon", "coordinates": [[[lon, lat], ...], ...] }`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Polygon {
    #[serde(rename = "type")]
    pub kind: PolygonType,
    pub coordinates: Vec<Vec<LonLat>>,
}

impl Polygon {
    /// A polygon from one ring, closed if it isn't already.
    pub fn from_ring(ring: impl Into<Vec<LonLat>>) -> Self {
        Self { kind: PolygonType::Polygon, coordinates: vec![close(ring.into())] }
    }

    /// An outer ring and holes, each closed if it isn't already.
    pub fn from_rings(outer: impl Into<Vec<LonLat>>, holes: impl IntoIterator<Item = Vec<LonLat>>) -> Self {
        let mut coordinates = vec![close(outer.into())];
        coordinates.extend(holes.into_iter().map(close));
        Self { kind: PolygonType::Polygon, coordinates }
    }

    /// Vertices over every ring, closing vertices and consecutive duplicates
    /// not counted: what a collar stores.
    pub fn total_vertices(&self) -> usize {
        self.coordinates.iter().map(|r| clean_ring(r).len()).sum()
    }

    /// The outer ring, unclosed, without consecutive duplicates.
    pub fn outer_ring(&self) -> Vec<LonLat> {
        self.coordinates.first().map(|r| clean_ring(r)).unwrap_or_default()
    }

    pub fn holes(&self) -> impl Iterator<Item = Vec<LonLat>> + '_ {
        self.coordinates.iter().skip(1).map(|r| clean_ring(r))
    }

    /// Check every ring and return the polygon normalised: rings closed,
    /// coordinates rounded to 7 decimals.
    pub fn validated(&self) -> Result<Polygon, GeoError> {
        if self.coordinates.is_empty() {
            return Err(GeoError::Empty);
        }
        let mut rings = Vec::with_capacity(self.coordinates.len());
        for ring in &self.coordinates {
            rings.push(close(validate_ring(ring, None)?));
        }
        Ok(Polygon { kind: PolygonType::Polygon, coordinates: rings })
    }

    pub fn is_valid(&self) -> bool {
        self.validated().is_ok()
    }

    /// Geodesic area in hectares, holes removed, whichever way each ring
    /// winds: the outer ring's area less each hole's, never below zero.
    pub fn area_ha(&self) -> f64 {
        let Some(outer) = self.coordinates.first() else {
            return 0.0;
        };
        let holes: f64 = self.coordinates.iter().skip(1).map(|r| ring_area_m2(r)).sum();
        (ring_area_m2(outer) - holes).max(0.0) / 10_000.0
    }

    /// Inside the outer ring and outside every hole.
    pub fn contains(&self, p: LonLat) -> bool {
        let outer = self.outer_ring();
        if outer.len() < 3 || !point_in_ring(p, &outer) {
            return false;
        }
        !self.holes().any(|h| point_in_ring(p, &h))
    }

    /// Area-weighted centroid of the outer ring.
    pub fn centroid(&self) -> Option<LonLat> {
        let outer = self.outer_ring();
        if outer.len() < 3 {
            return None;
        }
        let proj = Projection::new(outer[0]);
        let pts = proj.forward_ring(&outer);
        let a = signed_area(&pts);
        if a.abs() < 1e-9 {
            return None;
        }
        let (mut cx, mut cy) = (0.0, 0.0);
        for i in 0..pts.len() {
            let [x1, y1] = pts[i];
            let [x2, y2] = pts[(i + 1) % pts.len()];
            let f = x1 * y2 - x2 * y1;
            cx += (x1 + x2) * f;
            cy += (y1 + y2) * f;
        }
        Some(proj.inverse([cx / (6.0 * a), cy / (6.0 * a)]))
    }

    /// `[min_lon, min_lat, max_lon, max_lat]` of the outer ring.
    pub fn bbox(&self) -> Option<[f64; 4]> {
        let outer = self.outer_ring();
        let first = outer.first()?;
        Some(outer.iter().fold([first[0], first[1], first[0], first[1]], |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])]))
    }
}

/// Geodesic area of one ring in m², either winding. `geo`'s unsigned area
/// reads a ring wound against its expectation as the rest of the Earth, so
/// take the signed area (negative when clockwise) and drop the sign.
/// Paddock rings are far smaller than a hemisphere, where that would break.
fn ring_area_m2(ring: &[LonLat]) -> f64 {
    use geo::GeodesicArea;
    let points = clean_ring(ring);
    if points.len() < 3 {
        return 0.0;
    }
    let ls = geo::LineString::from(close(points).into_iter().map(|p| (p[0], p[1])).collect::<Vec<_>>());
    geo::Polygon::new(ls, Vec::new()).geodesic_area_signed().abs()
}

fn close(mut ring: Vec<LonLat>) -> Vec<LonLat> {
    if let (Some(first), Some(last)) = (ring.first().copied(), ring.last().copied())
        && first != last
    {
        ring.push(first);
    }
    ring
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Polygon {
        serde_json::from_str(r#"{"type":"Polygon","coordinates":[[[-92.41,38.12],[-92.40,38.12],[-92.40,38.13],[-92.41,38.13],[-92.41,38.12]]]}"#).unwrap()
    }

    #[test]
    fn serde_round_trip() {
        let p = home();
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["type"], "Polygon");
        assert_eq!(json["coordinates"][0].as_array().unwrap().len(), 5);
        assert!(serde_json::from_str::<Polygon>(r#"{"type":"Point","coordinates":[1,2]}"#).is_err());
    }

    #[test]
    fn area_in_hectares() {
        // 0.01 deg lon x 0.01 deg lat at 38.125 N: about 874 m x 1110 m = ~97 ha.
        let a = home().area_ha();
        assert!((a - 97.0).abs() < 1.5, "{a}");
    }

    #[test]
    fn contains_and_holes() {
        let mut p = home();
        assert!(p.contains([-92.405, 38.125]));
        assert!(!p.contains([-92.395, 38.125]));
        p.coordinates.push(vec![[-92.406, 38.124], [-92.404, 38.124], [-92.404, 38.126], [-92.406, 38.126]]);
        assert!(!p.contains([-92.405, 38.125]));
        assert!(p.contains([-92.409, 38.121]));
        assert!(p.area_ha() < home().area_ha());
    }

    #[test]
    fn validated_closes_and_checks() {
        let open = Polygon { kind: PolygonType::Polygon, coordinates: vec![vec![[0.0, 0.0], [0.001, 0.0], [0.001, 0.001]]] };
        let v = open.validated().unwrap();
        assert_eq!(v.coordinates[0].len(), 4);
        assert_eq!(v.coordinates[0][0], v.coordinates[0][3]);
        let empty = Polygon { kind: PolygonType::Polygon, coordinates: vec![] };
        assert_eq!(empty.validated(), Err(GeoError::Empty));
    }

    #[test]
    fn from_rings_closes_and_counts() {
        let p = Polygon::from_rings(vec![[0.0, 0.0], [0.01, 0.0], [0.01, 0.01], [0.0, 0.01]], [vec![[0.004, 0.004], [0.006, 0.004], [0.006, 0.006]]]);
        assert_eq!(p.coordinates.len(), 2);
        assert!(p.coordinates.iter().all(|r| r.first() == r.last()));
        assert_eq!(p.total_vertices(), 7);
        assert_eq!(p.holes().count(), 1);
    }

    #[test]
    fn centroid_of_square() {
        let c = home().centroid().unwrap();
        assert!((c[0] + 92.405).abs() < 1e-6 && (c[1] - 38.125).abs() < 1e-6);
    }

    // The live-check paddock near Ames (about 16.5 ha), counter-clockwise.
    fn ames() -> Vec<LonLat> {
        vec![[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]]
    }

    // Counter-clockwise holes well inside it.
    fn pond() -> Vec<LonLat> {
        vec![[-93.6235, 42.0315], [-93.6215, 42.0315], [-93.6215, 42.0325], [-93.6235, 42.0325], [-93.6235, 42.0315]]
    }

    fn barn() -> Vec<LonLat> {
        vec![[-93.6245, 42.0305], [-93.624, 42.0305], [-93.6242, 42.031], [-93.6245, 42.0305]]
    }

    fn cw(ring: &[LonLat]) -> Vec<LonLat> {
        ring.iter().rev().copied().collect()
    }

    fn close_to(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9 * b.abs().max(1.0)
    }

    #[test]
    fn ring_area_ignores_winding() {
        let ccw = Polygon::from_ring(ames()).area_ha();
        assert!((ccw - 16.5).abs() < 0.3, "{ccw}");
        assert!(close_to(Polygon::from_ring(cw(&ames())).area_ha(), ccw));
        let pond = Polygon::from_ring(pond()).area_ha();
        assert!((pond - 1.83).abs() < 0.05, "{pond}");
        assert!(close_to(Polygon::from_ring(cw(&self::pond())).area_ha(), pond));
    }

    #[test]
    fn a_hole_is_subtracted_whichever_way_each_ring_winds() {
        let expected = Polygon::from_ring(ames()).area_ha() - Polygon::from_ring(pond()).area_ha();
        for (outer, hole, label) in [
            (ames(), pond(), "outer ccw, hole ccw"),
            (ames(), cw(&pond()), "outer ccw, hole cw"),
            (cw(&ames()), pond(), "outer cw, hole ccw"),
            (cw(&ames()), cw(&pond()), "outer cw, hole cw"),
        ] {
            let a = Polygon::from_rings(outer, [hole]).area_ha();
            assert!(close_to(a, expected), "{label}: {a} ha, want {expected}");
            assert!((a - 14.67).abs() < 0.3, "{label}: {a}");
        }
    }

    #[test]
    fn several_holes_of_mixed_winding_are_each_subtracted() {
        let expected = Polygon::from_ring(ames()).area_ha() - Polygon::from_ring(pond()).area_ha() - Polygon::from_ring(barn()).area_ha();
        for outer in [ames(), cw(&ames())] {
            for holes in [vec![pond(), barn()], vec![cw(&pond()), barn()], vec![pond(), cw(&barn())], vec![cw(&pond()), cw(&barn())]] {
                let a = Polygon::from_rings(outer.clone(), holes).area_ha();
                assert!(close_to(a, expected), "{a} ha, want {expected}");
            }
        }
    }

    #[test]
    fn area_is_never_negative() {
        // A hole bigger than its outer ring is invalid; the area floors at zero.
        assert_eq!(Polygon::from_rings(pond(), [ames()]).area_ha(), 0.0);
        let empty = Polygon { kind: PolygonType::Polygon, coordinates: vec![] };
        assert_eq!(empty.area_ha(), 0.0);
    }

    #[test]
    fn contains_and_centroid_ignore_winding() {
        let inside = [-93.6245, 42.033];
        let in_pond = [-93.6225, 42.032];
        let outside = [-93.61, 42.032];
        let centre = Polygon::from_ring(ames()).centroid().unwrap();
        for outer in [ames(), cw(&ames())] {
            for hole in [pond(), cw(&pond())] {
                let p = Polygon::from_rings(outer.clone(), [hole]);
                assert!(p.contains(inside));
                assert!(!p.contains(in_pond));
                assert!(!p.contains(outside));
                assert_eq!(p.centroid(), Some(centre));
                assert_eq!(p.validated().unwrap().area_ha(), p.area_ha());
            }
        }
    }
}
