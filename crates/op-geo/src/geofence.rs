//! Geofence engine. Port of `opencollar/firmware/src/geofence.c`, with holes
//! (protocol v1, §3.6).
//!
//! The boundary is projected once into a local metric plane (east/north metres
//! around the outer ring's first vertex). Every fix is projected the same way
//! and compared against the rings in metres: inside means inside the outer
//! ring and outside every hole (even-odd per ring), and the margin is the
//! distance to the nearest edge of any ring, positive inside.

use serde::{Deserialize, Serialize};

use crate::limits::CollarLimits;
use crate::polygon::Polygon;
use crate::projection::{M_PER_DEG_LAT, Projection};
use crate::ring::{clean_ring, distance_to_segment};
use crate::{GeoError, LonLat, MAX_COLLAR_VERTICES};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GeofenceState {
    /// No usable fix yet.
    #[default]
    Unknown,
    /// Inside, clear of the warning zone.
    Inside,
    /// Inside, within `warn_m` of the edge.
    Warning,
    /// Outside the polygon.
    Outside,
}

impl GeofenceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Inside => "inside",
            Self::Warning => "warning",
            Self::Outside => "outside",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "inside" => Self::Inside,
            "warning" => Self::Warning,
            "outside" => Self::Outside,
            _ => Self::Unknown,
        }
    }
}

impl std::fmt::Display for GeofenceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeofenceConfig {
    /// Width of the warning zone inside the edge.
    pub warn_m: f64,
    /// Extra margin needed to step back to a calmer state.
    pub hysteresis_m: f64,
    /// Fixes worse than this don't change state.
    pub max_accuracy_m: f64,
}

impl Default for GeofenceConfig {
    /// The V0 firmware's values.
    fn default() -> Self {
        Self { warn_m: 5.0, hysteresis_m: 1.0, max_accuracy_m: 10.0 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct GeofenceResult {
    pub state: GeofenceState,
    /// Signed distance to the nearest edge of any ring: + inside, - outside.
    pub margin_m: f64,
    /// Fix accuracy too poor; state held from the last good fix.
    pub degraded: bool,
    /// State differs from the previous result.
    pub changed: bool,
    /// The ring that edge belongs to: 0 the outer ring, 1.. the holes.
    pub nearest_ring: usize,
}

/// One ring within [`Geofence::pts`]: `len` points from `start`, and its
/// bounding box `[min_x, min_y, max_x, max_y]` in metres.
#[derive(Debug, Clone, Copy)]
struct Span {
    start: usize,
    len: usize,
    bbox: [f64; 4],
}

#[derive(Debug, Clone)]
pub struct Geofence {
    cfg: GeofenceConfig,
    version: u32,
    projection: Projection,
    /// Every ring's points, outer ring first.
    pts: Vec<[f64; 2]>,
    rings: Vec<Span>,
    state: GeofenceState,
}

impl Geofence {
    /// One ring, as the V0 firmware holds it. Fails if the ring has fewer
    /// than 3 or more than 64 vertices. A closing vertex is dropped first.
    pub fn new(cfg: GeofenceConfig, vertices: &[LonLat], version: u32) -> Result<Self, GeoError> {
        let ring = clean_ring(vertices);
        if ring.len() < 3 {
            return Err(GeoError::TooFewVertices);
        }
        if ring.len() > MAX_COLLAR_VERTICES {
            return Err(GeoError::TooManyVertices(ring.len(), MAX_COLLAR_VERTICES));
        }
        Ok(Self::build(cfg, &[ring], version))
    }

    /// The outer ring and holes of `polygon`, within `limits` by count. The
    /// shape rules themselves are [`crate::shape::check`]'s job.
    pub fn from_polygon(cfg: GeofenceConfig, polygon: &Polygon, version: u32, limits: &CollarLimits) -> Result<Self, GeoError> {
        let rings: Vec<Vec<LonLat>> = polygon.coordinates.iter().map(|r| clean_ring(r)).collect();
        let Some(outer) = rings.first() else {
            return Err(GeoError::Empty);
        };
        if rings.iter().any(|r| r.len() < 3) {
            return Err(GeoError::TooFewVertices);
        }
        let holes = rings.len() - 1;
        if holes > limits.holes {
            return Err(GeoError::TooManyHoles(holes, limits.holes));
        }
        if outer.len() > limits.outer {
            return Err(GeoError::TooManyVertices(outer.len(), limits.outer));
        }
        if let Some(h) = rings[1..].iter().find(|h| h.len() > limits.hole_vertices) {
            return Err(GeoError::TooManyVertices(h.len(), limits.hole_vertices));
        }
        let total: usize = rings.iter().map(Vec::len).sum();
        if total > limits.total {
            return Err(GeoError::TooManyVertices(total, limits.total));
        }
        Ok(Self::build(cfg, &rings, version))
    }

    fn build(cfg: GeofenceConfig, rings: &[Vec<LonLat>], version: u32) -> Self {
        let projection = Projection::new(rings[0][0]);
        let mut pts = Vec::with_capacity(rings.iter().map(Vec::len).sum());
        let mut spans = Vec::with_capacity(rings.len());
        for ring in rings {
            let start = pts.len();
            pts.extend(projection.forward_ring(ring));
            let bbox = pts[start..].iter().fold([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY], |b, p| {
                [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])]
            });
            spans.push(Span { start, len: ring.len(), bbox });
        }
        Self { cfg, version, projection, pts, rings: spans, state: GeofenceState::Unknown }
    }

    pub fn version(&self) -> u32 {
        self.version
    }
    pub fn config(&self) -> &GeofenceConfig {
        &self.cfg
    }
    pub fn state(&self) -> GeofenceState {
        self.state
    }
    /// Carry a state over, e.g. when a simulated collar swaps boundaries.
    pub fn set_state(&mut self, state: GeofenceState) {
        self.state = state;
    }
    /// Number of rings: 1 plus the holes.
    pub fn rings(&self) -> usize {
        self.rings.len()
    }

    /// Signed distance in metres from a point to the boundary: + inside, - outside.
    pub fn margin_m(&self, p: LonLat) -> f64 {
        self.measure(p).0
    }

    /// Signed distance to the nearest edge of any ring, and that ring
    /// (0 outer, 1.. holes; the lowest index on a tie). A hole whose bounding
    /// box is farther than the nearest edge so far is skipped: it can't be
    /// nearer, and a point outside its box isn't in it.
    pub fn measure(&self, p: LonLat) -> (f64, usize) {
        let q = self.projection.forward(p);
        let (mut inside, mut min_d) = self.ring_test(q, &self.rings[0]);
        let mut nearest = 0;
        for (k, span) in self.rings.iter().enumerate().skip(1) {
            let b = span.bbox;
            let dx = (b[0] - q[0]).max(q[0] - b[2]).max(0.0);
            let dy = (b[1] - q[1]).max(q[1] - b[3]).max(0.0);
            if dx.hypot(dy) > min_d {
                continue;
            }
            let (in_hole, d) = self.ring_test(q, span);
            if in_hole {
                inside = false;
            }
            if d < min_d {
                min_d = d;
                nearest = k;
            }
        }
        (if inside { min_d } else { -min_d }, nearest)
    }

    /// Even-odd ray cast toward +x and the least distance to one ring's edges.
    fn ring_test(&self, q: [f64; 2], span: &Span) -> (bool, f64) {
        let [px, py] = q;
        let pts = &self.pts[span.start..span.start + span.len];
        let n = pts.len();
        let mut inside = false;
        let mut min_d = f64::INFINITY;
        let mut j = n - 1;
        for i in 0..n {
            let [xi, yi] = pts[i];
            let [xj, yj] = pts[j];
            if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
                inside = !inside;
            }
            min_d = min_d.min(distance_to_segment([px, py], [xi, yi], [xj, yj]));
            j = i;
        }
        (inside, min_d)
    }

    fn classify(&self, margin: f64) -> GeofenceState {
        let (warn, hyst) = (self.cfg.warn_m, self.cfg.hysteresis_m);
        match self.state {
            GeofenceState::Outside => {
                // Must come back past the edge by the hysteresis margin.
                if margin < hyst {
                    GeofenceState::Outside
                } else if margin < warn + hyst {
                    GeofenceState::Warning
                } else {
                    GeofenceState::Inside
                }
            }
            GeofenceState::Warning => {
                if margin < 0.0 {
                    GeofenceState::Outside
                } else if margin < warn + hyst {
                    GeofenceState::Warning
                } else {
                    GeofenceState::Inside
                }
            }
            _ => {
                if margin < 0.0 {
                    GeofenceState::Outside
                } else if margin < warn {
                    GeofenceState::Warning
                } else {
                    GeofenceState::Inside
                }
            }
        }
    }

    /// Feed a fix. `accuracy_m` is the receiver's horizontal accuracy estimate.
    pub fn update(&mut self, p: LonLat, accuracy_m: f64) -> GeofenceResult {
        let prev = self.state;
        let (margin_m, nearest_ring) = self.measure(p);
        let degraded = accuracy_m > self.cfg.max_accuracy_m;
        if !degraded {
            self.state = self.classify(margin_m);
        }
        GeofenceResult { state: self.state, margin_m, degraded, changed: self.state != prev, nearest_ring }
    }
}

/// Metres per degree of longitude at a latitude, as the firmware computes it.
pub fn m_per_deg_lon(lat: f64) -> f64 {
    M_PER_DEG_LAT * lat.to_radians().cos()
}

/// Host tests from `opencollar/firmware/tests/host/test_main.c`.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // Origin near Nashville, TN.
    pub const LAT0: f64 = 36.1627;
    pub const LON0: f64 = -86.7816;

    /// Point offset from the origin by east/north metres.
    pub fn at(east_m: f64, north_m: f64) -> LonLat {
        [LON0 + east_m / m_per_deg_lon(LAT0), LAT0 + north_m / M_PER_DEG_LAT]
    }

    pub fn near(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    const CFG: GeofenceConfig = GeofenceConfig { warn_m: 5.0, hysteresis_m: 1.0, max_accuracy_m: 10.0 };

    /// 100 m square.
    fn square() -> Geofence {
        Geofence::new(CFG, &[at(0.0, 0.0), at(100.0, 0.0), at(100.0, 100.0), at(0.0, 100.0)], 1).unwrap()
    }

    #[test]
    fn margin() {
        let gf = square();
        assert!(near(gf.margin_m(at(50.0, 50.0)), 50.0, 0.05));
        assert!(near(gf.margin_m(at(50.0, 3.0)), 3.0, 0.05));
        assert!(near(gf.margin_m(at(50.0, -10.0)), -10.0, 0.05));
        // Off a corner: distance is to the vertex, not the extended edge.
        assert!(near(gf.margin_m(at(110.0, 110.0)), -(200.0f64.sqrt()), 0.05));
    }

    #[test]
    fn concave() {
        // L shape: the notch at (75,75) is outside.
        let l = [at(0.0, 0.0), at(100.0, 0.0), at(100.0, 50.0), at(50.0, 50.0), at(50.0, 100.0), at(0.0, 100.0)];
        let gf = Geofence::new(CFG, &l, 1).unwrap();
        assert!(gf.margin_m(at(75.0, 75.0)) < 0.0);
        assert!(near(gf.margin_m(at(75.0, 75.0)), -25.0, 0.05));
        assert!(gf.margin_m(at(25.0, 75.0)) > 0.0);
    }

    #[test]
    fn invalid() {
        assert!(Geofence::new(CFG, &[at(0.0, 0.0), at(1.0, 1.0)], 1).is_err());
        let many: Vec<LonLat> = (0..65)
            .map(|i| {
                let a = 2.0 * std::f64::consts::PI * i as f64 / 65.0;
                at(50.0 * a.cos(), 50.0 * a.sin())
            })
            .collect();
        assert!(Geofence::new(CFG, &many, 1).is_err());
    }

    #[test]
    fn states_and_hysteresis() {
        let mut gf = square();
        assert_eq!(gf.state(), GeofenceState::Unknown);

        let r = gf.update(at(50.0, 50.0), 3.0);
        assert!(r.state == GeofenceState::Inside && r.changed);

        let r = gf.update(at(50.0, 4.0), 3.0);
        assert!(r.state == GeofenceState::Warning && r.changed);

        // Just past warn_m but inside the hysteresis band: stay in warning.
        let r = gf.update(at(50.0, 5.5), 3.0);
        assert!(r.state == GeofenceState::Warning && !r.changed);

        let r = gf.update(at(50.0, 6.5), 3.0);
        assert_eq!(r.state, GeofenceState::Inside);

        let r = gf.update(at(50.0, -1.0), 3.0);
        assert_eq!(r.state, GeofenceState::Outside);

        // Barely back inside: still outside until past the hysteresis margin.
        let r = gf.update(at(50.0, 0.5), 3.0);
        assert_eq!(r.state, GeofenceState::Outside);

        let r = gf.update(at(50.0, 2.0), 3.0);
        assert_eq!(r.state, GeofenceState::Warning);
    }

    #[test]
    fn degraded_fix_holds_state() {
        let mut gf = square();
        gf.update(at(50.0, 50.0), 3.0);
        let r = gf.update(at(50.0, -20.0), 25.0);
        assert!(r.degraded);
        assert_eq!(r.state, GeofenceState::Inside);
        assert!(r.margin_m < 0.0);
    }

    fn poly(rings: &[&[[f64; 2]]]) -> Polygon {
        let mut p = Polygon::from_ring(rings[0].iter().map(|q| at(q[0], q[1])).collect::<Vec<_>>());
        for h in &rings[1..] {
            p.coordinates.push(h.iter().map(|q| at(q[0], q[1])).collect());
        }
        p
    }

    const SQUARE: [[f64; 2]; 4] = [[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]];
    const MIDDLE: [[f64; 2]; 4] = [[40.0, 40.0], [60.0, 40.0], [60.0, 60.0], [40.0, 60.0]];

    fn holed() -> Geofence {
        Geofence::from_polygon(CFG, &poly(&[&SQUARE, &MIDDLE]), 1, &CollarLimits::V0).unwrap()
    }

    #[test]
    fn margins_around_and_inside_a_hole() {
        let gf = holed();
        assert_eq!(gf.rings(), 2);
        // In the hole: outside, 10 m from its edge.
        let (m, ring) = gf.measure(at(50.0, 50.0));
        assert!(near(m, -10.0, 0.01) && ring == 1, "{m} {ring}");
        // Between the hole and the south edge, nearer the hole.
        let (m, ring) = gf.measure(at(50.0, 30.0));
        assert!(near(m, 10.0, 0.01) && ring == 1, "{m} {ring}");
        // Nearer the outer edge.
        let (m, ring) = gf.measure(at(50.0, 5.0));
        assert!(near(m, 5.0, 0.01) && ring == 0, "{m} {ring}");
        // Outside everything.
        let (m, ring) = gf.measure(at(50.0, -3.0));
        assert!(near(m, -3.0, 0.01) && ring == 0, "{m} {ring}");
        // Off the hole's corner: distance to the corner.
        let (m, _) = gf.measure(at(35.0, 35.0));
        assert!(near(m, 50.0f64.sqrt(), 0.01), "{m}");
    }

    #[test]
    fn concave_hole() {
        // An L-shaped hole; its notch at (70,70) is grazeable.
        let l = [[40.0, 40.0], [80.0, 40.0], [80.0, 60.0], [60.0, 60.0], [60.0, 80.0], [40.0, 80.0]];
        let gf = Geofence::from_polygon(CFG, &poly(&[&SQUARE, &l]), 1, &CollarLimits::V0).unwrap();
        let (m, ring) = gf.measure(at(70.0, 70.0));
        assert!(near(m, 10.0, 0.01) && ring == 1, "notch: inside, {m}");
        let (m, ring) = gf.measure(at(50.0, 70.0));
        assert!(near(m, -10.0, 0.01) && ring == 1, "in the L: outside, {m}");
    }

    #[test]
    fn nearest_ring_among_several_holes() {
        let east = [[70.0, 40.0], [90.0, 40.0], [90.0, 60.0], [70.0, 60.0]];
        let west = [[10.0, 40.0], [30.0, 40.0], [30.0, 60.0], [10.0, 60.0]];
        let gf = Geofence::from_polygon(CFG, &poly(&[&SQUARE, &west, &east]), 1, &CollarLimits::V0).unwrap();
        assert_eq!(gf.measure(at(66.0, 50.0)).1, 2);
        assert_eq!(gf.measure(at(34.0, 50.0)).1, 1);
        assert_eq!(gf.measure(at(50.0, 97.0)).1, 0);
        assert!(gf.measure(at(80.0, 50.0)).0 < 0.0);
    }

    #[test]
    fn skipping_far_holes_changes_nothing() {
        let holes: Vec<[[f64; 2]; 4]> = (0..9)
            .map(|k| {
                let (x, y) = (8.0 + 30.0 * (k % 3) as f64, 8.0 + 30.0 * (k / 3) as f64);
                [[x, y], [x + 14.0, y], [x + 14.0, y + 14.0], [x, y + 14.0]]
            })
            .collect();
        let mut rings: Vec<&[[f64; 2]]> = vec![&SQUARE];
        rings.extend(holes.iter().map(|h| &h[..]));
        let p = poly(&rings);
        let gf = Geofence::from_polygon(CFG, &p, 1, &CollarLimits::V0).unwrap();
        let proj = gf.projection;
        let all: Vec<Vec<[f64; 2]>> = p.coordinates.iter().map(|r| proj.forward_ring(&clean_ring(r))).collect();
        for i in 0..40 {
            for j in 0..40 {
                let q = at(-10.0 + 3.0 * i as f64, -10.0 + 3.0 * j as f64);
                let xy = proj.forward(q);
                let mut brute = (f64::INFINITY, 0);
                let mut inside = crate::ring::point_in_ring(xy, &all[0]);
                for (k, r) in all.iter().enumerate() {
                    if k > 0 && crate::ring::point_in_ring(xy, r) {
                        inside = false;
                    }
                    let d = (0..r.len()).map(|e| distance_to_segment(xy, r[e], r[(e + r.len() - 1) % r.len()])).fold(f64::INFINITY, f64::min);
                    if d < brute.0 {
                        brute = (d, k);
                    }
                }
                let expect = if inside { brute.0 } else { -brute.0 };
                assert_eq!(gf.measure(q), (expect, brute.1));
            }
        }
    }

    #[test]
    fn from_polygon_enforces_limits_by_count() {
        let p = poly(&[&SQUARE, &MIDDLE]);
        assert_eq!(Geofence::from_polygon(CFG, &p, 1, &CollarLimits::LEGACY).unwrap_err(), GeoError::TooManyHoles(1, 0));
        let circle: Vec<[f64; 2]> = (0..129)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / 129.0;
                [50.0 + 50.0 * a.cos(), 50.0 + 50.0 * a.sin()]
            })
            .collect();
        assert!(matches!(Geofence::from_polygon(CFG, &poly(&[&circle]), 1, &CollarLimits::V0), Err(GeoError::TooManyVertices(129, 128))));
        assert_eq!(
            Geofence::from_polygon(CFG, &Polygon { kind: crate::PolygonType::Polygon, coordinates: vec![] }, 1, &CollarLimits::V0).unwrap_err(),
            GeoError::Empty
        );
        // One ring through from_polygon matches Geofence::new exactly.
        let one = Geofence::from_polygon(CFG, &poly(&[&SQUARE]), 1, &CollarLimits::LEGACY).unwrap();
        let old = square();
        for q in [at(50.0, 3.0), at(-4.0, 20.0), at(99.0, 99.0)] {
            assert_eq!(one.margin_m(q), old.margin_m(q));
        }
    }

    #[test]
    fn walking_into_a_hole_is_leaving_the_fence() {
        let mut gf = holed();
        assert_eq!(gf.update(at(50.0, 20.0), 3.0).state, GeofenceState::Inside);
        let r = gf.update(at(50.0, 37.0), 3.0);
        assert_eq!((r.state, r.nearest_ring), (GeofenceState::Warning, 1));
        let r = gf.update(at(50.0, 41.0), 3.0);
        assert_eq!((r.state, r.nearest_ring), (GeofenceState::Outside, 1));
    }

    #[test]
    fn state_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&GeofenceState::Warning).unwrap(), "\"warning\"");
    }
}
