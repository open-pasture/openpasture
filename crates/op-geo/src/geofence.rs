//! Geofence engine. Port of `opencollar/firmware/src/geofence.c`.
//!
//! The boundary is projected once into a local metric plane (east/north metres
//! around the polygon's first vertex). Every fix is projected the same way and
//! compared against the polygon in metres.

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeofenceResult {
    pub state: GeofenceState,
    /// Signed distance to the edge: + inside, - outside.
    pub margin_m: f64,
    /// Fix accuracy too poor; state held from the last good fix.
    pub degraded: bool,
    /// State differs from the previous result.
    pub changed: bool,
}

#[derive(Debug, Clone)]
pub struct Geofence {
    cfg: GeofenceConfig,
    version: u32,
    projection: Projection,
    pts: Vec<[f64; 2]>,
    state: GeofenceState,
}

impl Geofence {
    /// Fails if the ring has fewer than 3 or more than 64 vertices. A closing
    /// vertex is dropped first.
    pub fn new(cfg: GeofenceConfig, vertices: &[LonLat], version: u32) -> Result<Self, GeoError> {
        let ring = clean_ring(vertices);
        if ring.len() < 3 {
            return Err(GeoError::TooFewVertices);
        }
        if ring.len() > MAX_COLLAR_VERTICES {
            return Err(GeoError::TooManyVertices(ring.len(), MAX_COLLAR_VERTICES));
        }
        let projection = Projection::new(ring[0]);
        let pts = projection.forward_ring(&ring);
        Ok(Self { cfg, version, projection, pts, state: GeofenceState::Unknown })
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

    /// Signed distance in metres from a point to the boundary: + inside, - outside.
    pub fn margin_m(&self, p: LonLat) -> f64 {
        let [px, py] = self.projection.forward(p);
        let n = self.pts.len();
        let mut inside = false;
        let mut min_d = f64::INFINITY;
        let mut j = n - 1;
        for i in 0..n {
            let [xi, yi] = self.pts[i];
            let [xj, yj] = self.pts[j];
            if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
                inside = !inside;
            }
            min_d = min_d.min(distance_to_segment([px, py], [xi, yi], [xj, yj]));
            j = i;
        }
        if inside { min_d } else { -min_d }
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
        let margin_m = self.margin_m(p);
        let degraded = accuracy_m > self.cfg.max_accuracy_m;
        if !degraded {
            self.state = self.classify(margin_m);
        }
        GeofenceResult { state: self.state, margin_m, degraded, changed: self.state != prev }
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

    #[test]
    fn state_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&GeofenceState::Warning).unwrap(), "\"warning\"");
    }
}
