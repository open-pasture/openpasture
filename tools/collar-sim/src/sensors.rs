//! What `--imu`, `--drop` and `--cell` add to a collar: motion from an IMU
//! (seconds without moving, tilt from level), a collar that fell off its
//! animal and lies where it fell, and the LTE-M cell signal it measures from a
//! tower near the farm. None of it changes how the animals move.

use chrono::{DateTime, Utc};
use op_geo::LonLat;
use op_geo::projection::{Projection, distance_m};
use op_protocol::CellInfo;
use rand::Rng;

use crate::herd::{Mode, normal};

/// Faster than this (m/s) and the IMU feels the animal move.
const MOVING: f64 = 0.02;

/// A collar's IMU: when the animal last moved, and the collar's tilt.
#[derive(Debug, Clone)]
pub struct Motion {
    last_moved: DateTime<Utc>,
    tilt_deg: f64,
}

impl Motion {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self { last_moved: now, tilt_deg: 20.0 }
    }

    /// After a step: the animal's speed and what it is doing. On a neck the
    /// collar tilts with the head: down to graze, up to walk or rest.
    pub fn update<R: Rng + ?Sized>(&mut self, now: DateTime<Utc>, speed: f64, mode: Mode, rng: &mut R) {
        if speed > MOVING {
            self.last_moved = now;
        }
        let (mean, sd) = match mode {
            Mode::Grazing => (35.0, 8.0),
            Mode::Resting => (12.0, 4.0),
            Mode::Following | Mode::Fleeing => (18.0, 5.0),
        };
        self.tilt_deg = (mean + sd * normal(rng)).clamp(0.0, 55.0);
    }

    /// `(still_s, tilt_deg)` now.
    pub fn read(&self, now: DateTime<Utc>) -> (u32, f64) {
        (still(self.last_moved, now), round1(self.tilt_deg))
    }
}

/// A collar that comes off its animal at `at` and lies on its side where it fell.
#[derive(Debug, Clone)]
pub struct Fall {
    pub at: DateTime<Utc>,
    pub fell: Option<Fallen>,
}

#[derive(Debug, Clone, Copy)]
pub struct Fallen {
    pub point: LonLat,
    pub since: DateTime<Utc>,
    pub tilt_deg: f64,
}

impl Fall {
    pub fn at(at: DateTime<Utc>) -> Self {
        Self { at, fell: None }
    }

    /// Falls once `now` reaches its time; true on the tick it happens.
    pub fn check<R: Rng + ?Sized>(&mut self, now: DateTime<Utc>, point: LonLat, rng: &mut R) -> bool {
        if self.fell.is_some() || now < self.at {
            return false;
        }
        self.fell = Some(Fallen { point, since: now, tilt_deg: rng.gen_range(72.0..90.0) });
        true
    }
}

fn still(since: DateTime<Utc>, now: DateTime<Utc>) -> u32 {
    u32::try_from((now - since).num_seconds().max(0)).unwrap_or(u32::MAX)
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// The fallen collar's IMU: still since it fell, tilted on its side.
pub fn fallen_read(f: &Fallen, now: DateTime<Utc>) -> (u32, f64) {
    (still(f.since, now), round1(f.tilt_deg))
}

/// One LTE-M tower near the farm and what a collar measures from it: RSRP
/// falls off with distance, with a gentle rise and dip across the ground
/// (terrain) and a little noise per reading.
#[derive(Debug, Clone, Copy)]
pub struct Radio {
    tower: LonLat,
    origin: LonLat,
}

impl Radio {
    /// A tower about 1.1 km north-east of `centre`.
    pub fn near(centre: LonLat) -> Self {
        Self { tower: Projection::new(centre).offset(700.0, 850.0), origin: centre }
    }

    /// The signal at `p` with no noise, dBm.
    pub fn rsrp_at(&self, p: LonLat) -> f64 {
        let d = distance_m(self.tower, p).max(50.0);
        let [x, y] = Projection::new(self.origin).forward(p);
        -70.0 - 32.0 * (d / 100.0).log10() + 5.0 * (x / 37.0).sin() * (y / 53.0).cos()
    }

    pub fn measure<R: Rng + ?Sized>(&self, p: LonLat, now: DateTime<Utc>, rng: &mut R) -> CellInfo {
        let rsrp = (self.rsrp_at(p) + 2.0 * normal(rng)).clamp(-140.0, -44.0).round();
        CellInfo {
            rsrp_dbm: Some(rsrp),
            rsrq_db: Some(round1((-10.0 + (rsrp + 105.0) * 0.1 + normal(rng)).clamp(-20.0, -3.0))),
            snr_db: Some(round1(((rsrp + 115.0) / 2.0 + normal(rng)).clamp(-5.0, 25.0))),
            mode: Some("ltem".into()),
            band: Some(12),
            cell_id: Some("1A2B3C".into()),
            tac: Some(1234),
            at: Some(now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    const C: LonLat = [-93.62, 42.03];

    #[test]
    fn the_imu_counts_still_seconds_and_a_fallen_collar_lies_tilted() {
        let mut rng = StdRng::seed_from_u64(1);
        let t0 = chrono::Utc::now();
        let mut m = Motion::new(t0);
        m.update(t0 + chrono::Duration::seconds(5), 0.2, Mode::Grazing, &mut rng);
        m.update(t0 + chrono::Duration::seconds(10), 0.0, Mode::Resting, &mut rng);
        let (s, tilt) = m.read(t0 + chrono::Duration::seconds(65));
        assert_eq!(s, 60);
        assert!(tilt < 55.0);
        let mut f = Fall::at(t0 + chrono::Duration::seconds(30));
        assert!(!f.check(t0, C, &mut rng));
        assert!(f.check(t0 + chrono::Duration::seconds(30), C, &mut rng));
        assert!(!f.check(t0 + chrono::Duration::seconds(35), C, &mut rng), "falls once");
        let (s, tilt) = fallen_read(&f.fell.unwrap(), t0 + chrono::Duration::minutes(50));
        assert_eq!(s, 49 * 60 + 30);
        assert!(tilt > 60.0);
    }

    #[test]
    fn the_cell_signal_weakens_away_from_the_tower() {
        let r = Radio::near(C);
        let p = Projection::new(C);
        let near = r.rsrp_at(p.offset(600.0, 750.0));
        let far = r.rsrp_at(p.offset(-600.0, -750.0));
        assert!(near > far + 10.0, "{near} {far}");
        assert!((-125.0..-60.0).contains(&r.rsrp_at(C)), "{}", r.rsrp_at(C));
        let mut rng = StdRng::seed_from_u64(2);
        let c = r.measure(C, chrono::Utc::now(), &mut rng);
        assert_eq!(c.mode.as_deref(), Some("ltem"));
        assert!(c.rsrp_dbm.unwrap().fract() == 0.0);
    }
}
