use crate::LonLat;

pub const EARTH_RADIUS_M: f64 = 6_371_008.8;
pub const M_PER_DEG_LAT: f64 = EARTH_RADIUS_M * std::f64::consts::PI / 180.0;

/// Equirectangular projection to east/north metres around a reference point.
/// Same constants as the firmware. Fine at paddock scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projection {
    pub lon0: f64,
    pub lat0: f64,
    pub m_per_deg_lon: f64,
}

impl Projection {
    pub fn new(origin: LonLat) -> Self {
        Self { lon0: origin[0], lat0: origin[1], m_per_deg_lon: M_PER_DEG_LAT * origin[1].to_radians().cos() }
    }

    /// Lon/lat to local `[east_m, north_m]`.
    pub fn forward(&self, p: LonLat) -> [f64; 2] {
        [(p[0] - self.lon0) * self.m_per_deg_lon, (p[1] - self.lat0) * M_PER_DEG_LAT]
    }

    /// Local metres back to lon/lat, rounded to 7 decimals (about 1 cm).
    pub fn inverse(&self, p: [f64; 2]) -> LonLat {
        [round7(self.lon0 + p[0] / self.m_per_deg_lon), round7(self.lat0 + p[1] / M_PER_DEG_LAT)]
    }

    pub fn forward_ring(&self, ring: &[LonLat]) -> Vec<[f64; 2]> {
        ring.iter().map(|p| self.forward(*p)).collect()
    }

    pub fn inverse_ring(&self, ring: &[[f64; 2]]) -> Vec<LonLat> {
        ring.iter().map(|p| self.inverse(*p)).collect()
    }

    /// The point `east_m`, `north_m` metres from the origin.
    pub fn offset(&self, east_m: f64, north_m: f64) -> LonLat {
        [self.lon0 + east_m / self.m_per_deg_lon, self.lat0 + north_m / M_PER_DEG_LAT]
    }
}

pub fn round7(v: f64) -> f64 {
    (v * 1e7).round() / 1e7
}

/// Approximate distance in metres between two lon/lat points.
pub fn distance_m(a: LonLat, b: LonLat) -> f64 {
    let p = Projection::new(a).forward(b);
    p[0].hypot(p[1])
}
