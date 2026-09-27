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

    /// Geodesic area in hectares, holes removed.
    pub fn area_ha(&self) -> f64 {
        use geo::GeodesicArea;
        let to_ls = |r: &Vec<LonLat>| geo::LineString::from(close(clean_ring(r)).into_iter().map(|p| (p[0], p[1])).collect::<Vec<_>>());
        let Some(outer) = self.coordinates.first() else {
            return 0.0;
        };
        let poly = geo::Polygon::new(to_ls(outer), self.coordinates.iter().skip(1).map(to_ls).collect());
        poly.geodesic_area_unsigned() / 10_000.0
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
}
