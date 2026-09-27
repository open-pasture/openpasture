//! Inverse map projections for imported paddock files: Transverse Mercator
//! (UTM and state plane TM zones) and Lambert Conformal Conic (one or two
//! standard parallels, the state plane LCC zones). Formulas from Snyder,
//! "Map Projections: A Working Manual" (USGS Professional Paper 1395), on
//! the file's own ellipsoid. NAD83 and WGS 84 are treated as one datum
//! (they differ by less than 2 m).

use std::f64::consts::{FRAC_PI_2, FRAC_PI_4};

use op_geo::LonLat;

/// Semi-major axis in metres and inverse flattening (0 = a sphere).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ellipsoid {
    pub a: f64,
    pub inv_f: f64,
}

impl Ellipsoid {
    pub const GRS80: Self = Self { a: 6_378_137.0, inv_f: 298.257_222_101 };
    pub const WGS84: Self = Self { a: 6_378_137.0, inv_f: 298.257_223_563 };

    fn e2(&self) -> f64 {
        if self.inv_f == 0.0 {
            return 0.0;
        }
        let f = 1.0 / self.inv_f;
        2.0 * f - f * f
    }
}

/// A projection's defining parameters, angles in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Method {
    TransverseMercator {
        lat0: f64,
        lon0: f64,
        k0: f64,
    },
    /// Two standard parallels (`sp2: Some`), or one at `lat0` with scale `k0`.
    LambertConic {
        lat0: f64,
        lon0: f64,
        sp1: f64,
        sp2: Option<f64>,
        k0: f64,
    },
}

/// A projected coordinate system: x, y in `unit_m` metres per file unit,
/// false easting and northing in the same file units.
#[derive(Debug, Clone, PartialEq)]
pub struct Projected {
    pub ellipsoid: Ellipsoid,
    pub method: Method,
    pub false_easting: f64,
    pub false_northing: f64,
    pub unit_m: f64,
    prepared: Prepared,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Prepared {
    Tm { lon0: f64, k0: f64, m0: f64 },
    Lcc { lon0: f64, n: f64, af: f64, rho0: f64 },
}

impl Projected {
    pub fn new(ellipsoid: Ellipsoid, method: Method, false_easting: f64, false_northing: f64, unit_m: f64) -> Self {
        let e2 = ellipsoid.e2();
        let e = e2.sqrt();
        let a = ellipsoid.a;
        let prepared = match method {
            Method::TransverseMercator { lat0, lon0, k0 } => Prepared::Tm { lon0: lon0.to_radians(), k0, m0: meridian_arc(a, e2, lat0.to_radians()) },
            Method::LambertConic { lat0, lon0, sp1, sp2, k0 } => {
                let phi0 = lat0.to_radians();
                let (n, f) = match sp2 {
                    Some(sp2) if (sp1 - sp2).abs() > 1e-10 => {
                        let (p1, p2) = (sp1.to_radians(), sp2.to_radians());
                        let (m1, m2) = (lcc_m(e, p1), lcc_m(e, p2));
                        let (t1, t2) = (lcc_t(e, p1), lcc_t(e, p2));
                        let n = (m1.ln() - m2.ln()) / (t1.ln() - t2.ln());
                        (n, m1 / (n * t1.powf(n)))
                    }
                    Some(_) => {
                        let p1 = sp1.to_radians();
                        let n = p1.sin();
                        (n, lcc_m(e, p1) / (n * lcc_t(e, p1).powf(n)))
                    }
                    None => {
                        let n = phi0.sin();
                        (n, lcc_m(e, phi0) / (n * lcc_t(e, phi0).powf(n)))
                    }
                };
                // Two standard parallels define the scale; one takes the file's k0.
                let k = if sp2.is_some() { 1.0 } else { k0 };
                let af = a * f * k;
                Prepared::Lcc { lon0: lon0.to_radians(), n, af, rho0: af * lcc_t(e, phi0).powf(n) }
            }
        };
        Self { ellipsoid, method, false_easting, false_northing, unit_m, prepared }
    }

    /// `[lon, lat]` in degrees for a point in the file's units.
    pub fn inverse(&self, x: f64, y: f64) -> LonLat {
        let (x, y) = ((x - self.false_easting) * self.unit_m, (y - self.false_northing) * self.unit_m);
        let e2 = self.ellipsoid.e2();
        let a = self.ellipsoid.a;
        let (lon, lat) = match self.prepared {
            Prepared::Tm { lon0, k0, m0 } => tm_inverse(a, e2, lon0, k0, m0, x, y),
            Prepared::Lcc { lon0, n, af, rho0 } => {
                let e = e2.sqrt();
                let dy = rho0 - y;
                let rho = n.signum() * (x * x + dy * dy).sqrt();
                let theta = (n.signum() * x).atan2(n.signum() * dy);
                let lon = theta / n + lon0;
                let lat = if rho == 0.0 {
                    n.signum() * FRAC_PI_2
                } else {
                    let t = (rho / af).powf(1.0 / n);
                    lcc_phi(e, t)
                };
                (lon, lat)
            }
        };
        [normalize_lon(lon.to_degrees()), lat.to_degrees()]
    }

    /// The point in the file's units for `[lon, lat]` in degrees.
    #[cfg(test)]
    pub fn forward(&self, p: LonLat) -> [f64; 2] {
        let (lon, lat) = (p[0].to_radians(), p[1].to_radians());
        let e2 = self.ellipsoid.e2();
        let a = self.ellipsoid.a;
        let [x, y] = match self.prepared {
            Prepared::Tm { lon0, k0, m0 } => {
                let ep2 = e2 / (1.0 - e2);
                let (s, c) = lat.sin_cos();
                let n = a / (1.0 - e2 * s * s).sqrt();
                let t = lat.tan().powi(2);
                let cc = ep2 * c * c;
                let aa = (lon - lon0) * c;
                let m = meridian_arc(a, e2, lat);
                let x = k0 * n * (aa + (1.0 - t + cc) * aa.powi(3) / 6.0 + (5.0 - 18.0 * t + t * t + 72.0 * cc - 58.0 * ep2) * aa.powi(5) / 120.0);
                let y = k0
                    * (m - m0
                        + n * lat.tan()
                            * (aa * aa / 2.0
                                + (5.0 - t + 9.0 * cc + 4.0 * cc * cc) * aa.powi(4) / 24.0
                                + (61.0 - 58.0 * t + t * t + 600.0 * cc - 330.0 * ep2) * aa.powi(6) / 720.0));
                [x, y]
            }
            Prepared::Lcc { lon0, n, af, rho0 } => {
                let rho = af * lcc_t(e2.sqrt(), lat).powf(n);
                let theta = n * (lon - lon0);
                [rho * theta.sin(), rho0 - rho * theta.cos()]
            }
        };
        [x / self.unit_m + self.false_easting, y / self.unit_m + self.false_northing]
    }
}

fn normalize_lon(mut lon: f64) -> f64 {
    while lon > 180.0 {
        lon -= 360.0;
    }
    while lon < -180.0 {
        lon += 360.0;
    }
    lon
}

/// Distance along the meridian from the equator to `phi` (Snyder 3-21).
fn meridian_arc(a: f64, e2: f64, phi: f64) -> f64 {
    let (e4, e6) = (e2 * e2, e2 * e2 * e2);
    a * ((1.0 - e2 / 4.0 - 3.0 * e4 / 64.0 - 5.0 * e6 / 256.0) * phi - (3.0 * e2 / 8.0 + 3.0 * e4 / 32.0 + 45.0 * e6 / 1024.0) * (2.0 * phi).sin()
        + (15.0 * e4 / 256.0 + 45.0 * e6 / 1024.0) * (4.0 * phi).sin()
        - (35.0 * e6 / 3072.0) * (6.0 * phi).sin())
}

/// Snyder 8-7 to 8-25: the footpoint latitude, then the series back to
/// latitude and longitude. Accurate to millimetres within a UTM zone.
fn tm_inverse(a: f64, e2: f64, lon0: f64, k0: f64, m0: f64, x: f64, y: f64) -> (f64, f64) {
    let ep2 = e2 / (1.0 - e2);
    let m = m0 + y / k0;
    let (e4, e6) = (e2 * e2, e2 * e2 * e2);
    let mu = m / (a * (1.0 - e2 / 4.0 - 3.0 * e4 / 64.0 - 5.0 * e6 / 256.0));
    let r = (1.0 - e2).sqrt();
    let e1 = (1.0 - r) / (1.0 + r);
    let phi1 = mu
        + (3.0 * e1 / 2.0 - 27.0 * e1.powi(3) / 32.0) * (2.0 * mu).sin()
        + (21.0 * e1 * e1 / 16.0 - 55.0 * e1.powi(4) / 32.0) * (4.0 * mu).sin()
        + (151.0 * e1.powi(3) / 96.0) * (6.0 * mu).sin()
        + (1097.0 * e1.powi(4) / 512.0) * (8.0 * mu).sin();
    let (s1, c1) = phi1.sin_cos();
    let cc1 = ep2 * c1 * c1;
    let t1 = phi1.tan().powi(2);
    let w = 1.0 - e2 * s1 * s1;
    let n1 = a / w.sqrt();
    let r1 = a * (1.0 - e2) / w.powf(1.5);
    let d = x / (n1 * k0);
    let lat = phi1
        - (n1 * phi1.tan() / r1)
            * (d * d / 2.0 - (5.0 + 3.0 * t1 + 10.0 * cc1 - 4.0 * cc1 * cc1 - 9.0 * ep2) * d.powi(4) / 24.0
                + (61.0 + 90.0 * t1 + 298.0 * cc1 + 45.0 * t1 * t1 - 252.0 * ep2 - 3.0 * cc1 * cc1) * d.powi(6) / 720.0);
    let lon = lon0
        + (d - (1.0 + 2.0 * t1 + cc1) * d.powi(3) / 6.0 + (5.0 - 2.0 * cc1 + 28.0 * t1 - 3.0 * cc1 * cc1 + 8.0 * ep2 + 24.0 * t1 * t1) * d.powi(5) / 120.0)
            / c1;
    (lon, lat)
}

/// Snyder 14-15.
fn lcc_m(e: f64, phi: f64) -> f64 {
    let s = phi.sin();
    phi.cos() / (1.0 - e * e * s * s).sqrt()
}

/// Snyder 15-9.
fn lcc_t(e: f64, phi: f64) -> f64 {
    let s = phi.sin();
    (FRAC_PI_4 - phi / 2.0).tan() / ((1.0 - e * s) / (1.0 + e * s)).powf(e / 2.0)
}

/// Snyder 7-9, iterated until it settles.
fn lcc_phi(e: f64, t: f64) -> f64 {
    let mut phi = FRAC_PI_2 - 2.0 * t.atan();
    for _ in 0..30 {
        let s = phi.sin();
        let next = FRAC_PI_2 - 2.0 * (t * ((1.0 - e * s) / (1.0 + e * s)).powf(e / 2.0)).atan();
        if (next - phi).abs() < 1e-14 {
            return next;
        }
        phi = next;
    }
    phi
}

#[cfg(test)]
mod tests {
    use super::*;

    const FT_US: f64 = 1200.0 / 3937.0;
    const CLARKE_1866: Ellipsoid = Ellipsoid { a: 6_378_206.4, inv_f: 294.978_698_2 };
    const AIRY_1830: Ellipsoid = Ellipsoid { a: 6_377_563.396, inv_f: 299.324_964_6 };

    fn dms(d: f64, m: f64, s: f64) -> f64 {
        d.signum() * (d.abs() + m / 60.0 + s / 3600.0)
    }

    fn close(got: LonLat, want: LonLat, tol_deg: f64) {
        assert!((got[0] - want[0]).abs() < tol_deg && (got[1] - want[1]).abs() < tol_deg, "got {got:?}, want {want:?}");
    }

    /// EPSG Guidance Note 7-2 worked example: NAD27 / Texas South Central.
    #[test]
    fn lambert_two_parallels_matches_the_epsg_example() {
        let p = Projected::new(
            CLARKE_1866,
            Method::LambertConic { lat0: dms(27.0, 50.0, 0.0), lon0: -99.0, sp1: dms(28.0, 23.0, 0.0), sp2: Some(dms(30.0, 17.0, 0.0)), k0: 1.0 },
            2_000_000.0,
            0.0,
            FT_US,
        );
        let [x, y] = p.forward([-96.0, 28.5]);
        assert!((x - 2_963_503.91).abs() < 0.02 && (y - 254_759.80).abs() < 0.02, "{x} {y}");
        // The published coordinates are rounded to the centimetre.
        close(p.inverse(2_963_503.91, 254_759.80), [-96.0, 28.5], 2e-7);
    }

    /// EPSG Guidance Note 7-2 worked example: OSGB 1936 / British National Grid.
    #[test]
    fn transverse_mercator_matches_the_epsg_example() {
        let p = Projected::new(AIRY_1830, Method::TransverseMercator { lat0: 49.0, lon0: -2.0, k0: 0.999_601_271_7 }, 400_000.0, -100_000.0, 1.0);
        let [x, y] = p.forward([0.5, 50.5]);
        assert!((x - 577_274.99).abs() < 0.02 && (y - 69_740.50).abs() < 0.02, "{x} {y}");
        close(p.inverse(577_274.99, 69_740.50), [0.5, 50.5], 2e-7);
    }

    /// Points projected by PROJ 9.3 (pyproj 3.6.1) from NAD83 lon/lat. The
    /// inverse lands within 1e-7 degrees (about a centimetre).
    #[test]
    fn inverse_matches_proj_for_utm_and_iowa_state_plane() {
        let utm15 = Projected::new(Ellipsoid::GRS80, Method::TransverseMercator { lat0: 0.0, lon0: -93.0, k0: 0.9996 }, 500_000.0, 0.0, 1.0);
        let iowa_n = |fe: f64, fnn: f64, unit: f64| {
            Projected::new(
                Ellipsoid::GRS80,
                Method::LambertConic { lat0: 41.5, lon0: -93.5, sp1: 43.266_666_666_666_7, sp2: Some(42.066_666_666_666_7), k0: 1.0 },
                fe,
                fnn,
                unit,
            )
        };
        let iowa_s = |fe: f64, unit: f64| {
            Projected::new(
                Ellipsoid::GRS80,
                Method::LambertConic { lat0: 40.0, lon0: -93.5, sp1: 41.783_333_333_333_3, sp2: Some(40.616_666_666_666_7), k0: 1.0 },
                fe,
                0.0,
                unit,
            )
        };
        let cases: Vec<(&str, Projected, Vec<(LonLat, [f64; 2])>)> = vec![
            (
                "26915",
                utm15,
                vec![
                    ([-93.62, 42.03], [448_677.093_953_558_9, 4_653_293.017_557_551]),
                    ([-93.9, 43.1], [426_761.354_691_814_1, 4_772_312.769_379_847]),
                    ([-91.2, 41.0], [651_386.272_685_377_4, 4_540_317.426_995_666]),
                    ([-96.4, 42.6], [221_066.795_345_248_13, 4_722_002.387_274_413]),
                ],
            ),
            (
                "3417",
                iowa_n(4_921_250.0, 3_280_833.3333, 0.304_800_609_601_219),
                vec![
                    ([-93.62, 42.03], [4_888_646.767_925_305, 3_474_001.287_300_229_5]),
                    ([-93.9, 43.1], [4_814_417.967_701_749, 3_864_176.245_984_219]),
                    ([-91.2, 41.0], [5_556_279.701_422_298, 3_107_243.927_107_402_6]),
                    ([-96.4, 42.6], [4_140_616.995_559_667_2, 3_695_089.005_562_45]),
                ],
            ),
            (
                "26975",
                iowa_n(1_500_000.0, 1_000_000.0, 1.0),
                vec![
                    ([-93.62, 42.03], [1_490_062.514_988_663, 1_058_877.710_134_690_2]),
                    ([-93.9, 43.1], [1_467_437.531_430_555_7, 1_177_803.275_392_700_7]),
                    ([-91.2, 41.0], [1_693_557.440_108_396_6, 947_089.843_172_182_6]),
                ],
            ),
            (
                "3418",
                iowa_s(1_640_416.6667, 0.304_800_609_601_219),
                vec![([-93.62, 42.03], [1_607_811.925_407_404_8, 739_657.857_527_972_8]), ([-91.2, 41.0], [2_275_190.890_429_983, 372_723.341_547_485_3])],
            ),
            ("26976", iowa_s(500_000.0, 1.0), vec![([-93.62, 42.03], [490_062.054_978_126_9, 225_448.165_870_857_83])]),
            (
                "2236 Florida East ftUS",
                Projected::new(
                    Ellipsoid::GRS80,
                    Method::TransverseMercator { lat0: 24.333_333_333_333_3, lon0: -81.0, k0: 0.999_941_177 },
                    656_166.667,
                    0.0,
                    0.304_800_609_601_219,
                ),
                vec![([-80.9, 26.5], [688_871.411_203_238_1, 787_437.368_581_421_8]), ([-81.4, 28.2], [527_329.218_805_194_5, 1_405_629.315_300_210_6])],
            ),
        ];
        for (name, p, points) in cases {
            for (ll, xy) in points {
                let got = p.inverse(xy[0], xy[1]);
                assert!((got[0] - ll[0]).abs() < 1e-7 && (got[1] - ll[1]).abs() < 1e-7, "{name}: {xy:?} → {got:?}, want {ll:?}");
            }
        }
    }

    #[test]
    fn lambert_one_parallel_round_trips() {
        let p = Projected::new(Ellipsoid::WGS84, Method::LambertConic { lat0: 42.0, lon0: -93.0, sp1: 42.0, sp2: None, k0: 0.9999 }, 200_000.0, 100_000.0, 1.0);
        for ll in [[-93.62, 42.03], [-92.0, 43.5], [-95.5, 40.2]] {
            close(p.inverse(p.forward(ll)[0], p.forward(ll)[1]), ll, 1e-10);
        }
        // At the origin the one-parallel form is exactly the false origin.
        let [x, y] = p.forward([-93.0, 42.0]);
        assert!((x - 200_000.0).abs() < 1e-6 && (y - 100_000.0).abs() < 1e-6);
    }
}
