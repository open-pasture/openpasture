//! Cue policy: turns geofence results into audio cues. Port of
//! `opencollar/firmware/src/cue.c`.
//!
//! V0 is audio only. Warning-zone cues get louder the closer the animal is to
//! the edge. Once outside, a distinct tone plays for a limited time and then
//! stops, so an animal that has escaped isn't cued indefinitely. Continuous
//! cueing is capped and followed by a rest period, which also stops GPS drift
//! near the edge from causing endless beeping.
//!
//! Only a crossing is cued. The fence is armed by an inside fix and disarmed
//! by a new boundary ([`Cue::rearm`]) or a crossing. While unarmed the collar
//! is silent: an animal a new boundary leaves outside isn't cued, and neither
//! is one walking back in through the warning zone. After a new boundary the
//! first fix inside the polygon (inside or warning) arms it; once the animal
//! has been outside, only a fix clear of the warning zone does.

use serde::{Deserialize, Serialize};

use crate::geofence::{GeofenceResult, GeofenceState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CueConfig {
    pub warn_freq_hz: u16,
    pub outside_freq_hz: u16,
    /// Length of each beep; one beep per fix.
    pub beep_ms: u16,
    /// Longest continuous cue before a forced rest.
    pub max_active_ms: u32,
    /// Silence after hitting `max_active_ms`.
    pub rest_ms: u32,
    /// How long to cue after leaving the polygon.
    pub outside_max_ms: u32,
}

impl Default for CueConfig {
    /// The V0 firmware's values.
    fn default() -> Self {
        Self { warn_freq_hz: 2730, outside_freq_hz: 1000, beep_ms: 300, max_active_ms: 20_000, rest_ms: 30_000, outside_max_ms: 10_000 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CueCommand {
    pub active: bool,
    pub freq_hz: u16,
    /// 0-4, matches the Qwiic Buzzer.
    pub volume: u8,
    pub duration_ms: u16,
}

#[derive(Debug, Clone)]
pub struct Cue {
    cfg: CueConfig,
    cueing: bool,
    active_since_ms: i64,
    rest_until_ms: i64,
    outside_since_ms: i64,
    armed: bool,
    /// Outside since the last rearm, so warning doesn't arm.
    seen_outside: bool,
    /// Crossed out; the outside tone window is running.
    escaping: bool,
}

/// 1 at the inner edge of the zone, 4 at the boundary.
fn warning_volume(margin_m: f64, warn_m: f64) -> u8 {
    let depth = warn_m - margin_m;
    let v = 1 + (3.0 * depth / warn_m) as i32;
    v.clamp(1, 4) as u8
}

impl Cue {
    pub fn new(cfg: CueConfig) -> Self {
        Self { cfg, cueing: false, active_since_ms: 0, rest_until_ms: 0, outside_since_ms: 0, armed: false, seen_outside: false, escaping: false }
    }

    /// Call whenever a new boundary is applied: silent until the next inside
    /// fix. Keeps any forced rest that is running.
    pub fn rearm(&mut self) {
        self.armed = false;
        self.seen_outside = false;
        self.escaping = false;
        self.cueing = false;
    }

    /// Whether a crossing would be cued now.
    pub fn armed(&self) -> bool {
        self.armed
    }

    pub fn update(&mut self, r: &GeofenceResult, warn_m: f64, now_ms: i64) -> CueCommand {
        let mut cmd = CueCommand::default();
        let mut want = false;

        match r.state {
            GeofenceState::Inside => {
                self.armed = true;
                self.escaping = false;
            }
            GeofenceState::Warning => {
                if !self.seen_outside {
                    self.armed = true;
                }
                self.escaping = false;
            }
            GeofenceState::Outside => {
                if self.armed {
                    // A real crossing: cue it, then stay quiet until back inside.
                    self.armed = false;
                    self.escaping = true;
                    self.outside_since_ms = now_ms;
                }
                self.seen_outside = true;
            }
            GeofenceState::Unknown => {}
        }

        if r.state == GeofenceState::Warning && self.armed {
            want = true;
            cmd.freq_hz = self.cfg.warn_freq_hz;
            cmd.volume = warning_volume(r.margin_m, warn_m);
        } else if r.state == GeofenceState::Outside && self.escaping && now_ms - self.outside_since_ms < self.cfg.outside_max_ms as i64 {
            want = true;
            cmd.freq_hz = self.cfg.outside_freq_hz;
            cmd.volume = 4;
        }

        if !want {
            self.cueing = false;
            return CueCommand::default();
        }
        if now_ms < self.rest_until_ms {
            return CueCommand::default();
        }
        if !self.cueing {
            self.cueing = true;
            self.active_since_ms = now_ms;
        } else if now_ms - self.active_since_ms >= self.cfg.max_active_ms as i64 {
            self.cueing = false;
            self.rest_until_ms = now_ms + self.cfg.rest_ms as i64;
            return CueCommand::default();
        }

        cmd.active = true;
        cmd.duration_ms = self.cfg.beep_ms;
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(state: GeofenceState, margin_m: f64) -> GeofenceResult {
        GeofenceResult { state, margin_m, degraded: false, changed: false }
    }

    #[test]
    fn volume_ramps() {
        let mut c = Cue::new(CueConfig::default());
        let cmd = c.update(&result(GeofenceState::Warning, 4.9), 5.0, 0);
        assert!(cmd.active && cmd.volume == 1 && cmd.freq_hz == 2730);
        let cmd = c.update(&result(GeofenceState::Warning, 0.1), 5.0, 1000);
        assert!(cmd.active && cmd.volume == 3);
        let cmd = c.update(&result(GeofenceState::Warning, 0.0), 5.0, 2000);
        assert_eq!(cmd.volume, 4);
        let cmd = c.update(&result(GeofenceState::Inside, 0.0), 5.0, 3000);
        assert!(!cmd.active);
    }

    #[test]
    fn rest_after_max_active() {
        let mut c = Cue::new(CueConfig::default());
        let r = result(GeofenceState::Warning, 2.0);
        assert!(c.update(&r, 5.0, 0).active);
        assert!(c.update(&r, 5.0, 19_000).active);
        assert!(!c.update(&r, 5.0, 20_000).active);
        assert!(!c.update(&r, 5.0, 49_000).active);
        assert!(c.update(&r, 5.0, 50_000).active);
    }

    #[test]
    fn outside_times_out() {
        let mut c = Cue::new(CueConfig::default());
        assert!(!c.update(&result(GeofenceState::Inside, 20.0), 5.0, 0).active);
        let r = result(GeofenceState::Outside, -3.0);
        let cmd = c.update(&r, 5.0, 1_000);
        assert!(cmd.active && cmd.freq_hz == 1000 && cmd.volume == 4);
        assert!(c.update(&r, 5.0, 10_000).active);
        assert!(!c.update(&r, 5.0, 11_000).active);
        assert!(!c.update(&r, 5.0, 60_000).active);
    }

    use crate::LonLat;
    use crate::geofence::{Geofence, GeofenceConfig};
    use crate::projection::Projection;

    const O: LonLat = [-86.7816, 36.1627];

    /// 100 m square with its south-west corner `north` metres north of O.
    fn square(north: f64, version: u32) -> Geofence {
        let p = Projection::new(O);
        let ring = [p.offset(0.0, north), p.offset(100.0, north), p.offset(100.0, north + 100.0), p.offset(0.0, north + 100.0)];
        Geofence::new(GeofenceConfig::default(), &ring, version).unwrap()
    }

    /// Feed a fix `e`, `n` metres from O through the fence and the cue.
    fn step(gf: &mut Geofence, c: &mut Cue, e: f64, n: f64, now_ms: i64) -> (CueCommand, GeofenceState) {
        let r = gf.update(Projection::new(O).offset(e, n), 3.0);
        (c.update(&r, gf.config().warn_m, now_ms), r.state)
    }

    #[test]
    fn only_a_crossing_is_cued() {
        let (mut gf, mut c) = (square(0.0, 1), Cue::new(CueConfig::default()));
        let mut t = 0;
        // A new boundary that leaves the animal outside: silent, state outside.
        while t < 60_000 {
            let (cmd, st) = step(&mut gf, &mut c, 50.0, -20.0, t);
            assert!(!cmd.active && st == GeofenceState::Outside);
            t += 1_000;
        }
        // Walks in through the warning zone: still silent, then arms inside.
        for n in [-2.0, 2.0, 4.0] {
            t += 1_000;
            assert!(!step(&mut gf, &mut c, 50.0, n, t).0.active);
        }
        assert!(!c.armed());
        t += 1_000;
        let (cmd, st) = step(&mut gf, &mut c, 50.0, 20.0, t);
        assert!(!cmd.active && st == GeofenceState::Inside && c.armed());

        // Armed: the warning zone cues, and crossing out cues for 10 s.
        t += 1_000;
        let (cmd, _) = step(&mut gf, &mut c, 50.0, 3.0, t);
        assert!(cmd.active && cmd.freq_hz == 2730);
        t += 1_000;
        let (cmd, st) = step(&mut gf, &mut c, 50.0, -1.0, t);
        assert!(cmd.active && cmd.freq_hz == 1000 && st == GeofenceState::Outside);
        assert!(step(&mut gf, &mut c, 50.0, -3.0, t + 9_000).0.active);
        assert!(!step(&mut gf, &mut c, 50.0, -3.0, t + 10_000).0.active);
        t += 10_000;

        // Coming back in after the crossing: no warning cues on the way in.
        for n in [2.0, 4.0, -2.0] {
            t += 1_000;
            assert!(!step(&mut gf, &mut c, 50.0, n, t).0.active);
        }
    }

    #[test]
    fn rearm_on_new_boundary() {
        let (mut gf, mut c) = (square(0.0, 1), Cue::new(CueConfig::default()));
        assert!(!step(&mut gf, &mut c, 50.0, 50.0, 0).0.active && c.armed());

        // A new boundary puts the animal in its warning zone: cued at once.
        gf = square(30.0, 2);
        c.rearm();
        assert!(!c.armed());
        let (cmd, st) = step(&mut gf, &mut c, 50.0, 33.0, 1_000);
        assert!(cmd.active && st == GeofenceState::Warning && cmd.freq_hz == 2730);

        // A new boundary that leaves it outside: no crossing, no cue.
        gf = square(60.0, 3);
        c.rearm();
        let (cmd, st) = step(&mut gf, &mut c, 50.0, 33.0, 2_000);
        assert!(!cmd.active && st == GeofenceState::Outside);
        assert!(!step(&mut gf, &mut c, 50.0, 40.0, 3_000).0.active);
    }
}
