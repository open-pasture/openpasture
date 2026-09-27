//! One emulated collar, without I/O: an animal grazing with its herd, a GNSS
//! receiver with noise, and the firmware (geofence with holes, cue policy with
//! kinds and episodes, slots and config; see `firmware.rs`) from op-geo and
//! op-protocol.
//!
//! The animal knows nothing about where a boundary is. It grazes, rests and
//! keeps near its herd; when its own collar cues, it turns away from the
//! nearest edge and walks briskly; when neighbours walk off, it tends to
//! follow. That is what makes a sweep work.

use chrono::{DateTime, Utc};
use op_geo::projection::Projection;
use op_geo::{CollarLimits, Cue, CueCommand, CueConfig, Geofence, GeofenceConfig, LonLat, Polygon};
use op_protocol::{Ack, VerifyingKey, WireCue, WireEpisode, WireFix};
use rand::Rng;

use crate::firmware::Firmware;

/// Width of the warning zone the collars use.
const WARN_M: f64 = 5.0;
/// Speed above which an animal counts as walking off (a cued animal).
const BRISK: f64 = 0.75;

/// A polygon the animal is subject to: the boundary its collar holds, or the
/// paddock's wire fence before any boundary arrives.
#[derive(Debug, Clone)]
pub struct Area {
    fence: Geofence,
    ring: Vec<LonLat>,
}

/// Room for any shape: an area takes a boundary as the collar holds it.
const ANY: CollarLimits = CollarLimits { outer: 1 << 20, holes: 1 << 12, hole_vertices: 1 << 20, total: 1 << 22, slots: 1, slot_bytes: 0 };

impl Area {
    /// One ring.
    #[cfg(test)]
    pub fn new(ring: &[LonLat]) -> Option<Area> {
        Self::from_polygon(&Polygon::from_ring(ring.to_vec()))
    }

    /// An outer ring and holes: inside the ring and outside every hole.
    pub fn from_polygon(p: &Polygon) -> Option<Area> {
        let fence = Geofence::from_polygon(GeofenceConfig::default(), p, 0, &ANY).ok()?;
        Some(Area { fence, ring: p.outer_ring() })
    }

    /// Signed distance to the edge in metres, + inside.
    pub fn margin(&self, p: LonLat) -> f64 {
        self.fence.margin_m(p)
    }

    /// Unit vector (east, north) away from the nearest edge: inward when
    /// inside, outward when outside.
    fn away_from_edge(&self, p: LonLat) -> [f64; 2] {
        let proj = Projection::new(p);
        let m = |e: f64, n: f64| self.margin(proj.offset(e, n));
        let g = unit([m(1.0, 0.0) - m(-1.0, 0.0), m(0.0, 1.0) - m(0.0, -1.0)]);
        if self.margin(p) >= 0.0 { g } else { [-g[0], -g[1]] }
    }

    /// The deepest point of a coarse grid, for placing animals at start.
    pub fn interior(&self) -> LonLat {
        let (lo, hi) =
            self.ring.iter().fold(([f64::MAX; 2], [f64::MIN; 2]), |(lo, hi), p| ([lo[0].min(p[0]), lo[1].min(p[1])], [hi[0].max(p[0]), hi[1].max(p[1])]));
        let grid = (0..=400).map(|k| [lo[0] + (hi[0] - lo[0]) * (k / 21) as f64 / 20.0, lo[1] + (hi[1] - lo[1]) * (k % 21) as f64 / 20.0]);
        grid.max_by(|a, b| self.margin(*a).total_cmp(&self.margin(*b))).unwrap_or(self.ring[0])
    }
}

/// What an animal notices of the rest of its herd.
#[derive(Debug, Clone, Copy)]
pub struct Herd {
    pub centre: LonLat,
    /// Mean velocity (east, north, m/s) of the neighbours walking off.
    pub off_vel: [f64; 2],
    /// Share of the neighbours walking off.
    pub off_share: f64,
}

impl Herd {
    /// From the other animals' (position, velocity).
    pub fn of(others: impl Iterator<Item = (LonLat, [f64; 2])>) -> Option<Herd> {
        let v: Vec<(LonLat, [f64; 2])> = others.collect();
        let n = v.len() as f64;
        let off: Vec<[f64; 2]> = v.iter().map(|x| x.1).filter(|u| u[0].hypot(u[1]) > BRISK).collect();
        let k = off.len().max(1) as f64;
        (!v.is_empty()).then(|| Herd {
            centre: [v.iter().map(|x| x.0[0]).sum::<f64>() / n, v.iter().map(|x| x.0[1]).sum::<f64>() / n],
            off_vel: [off.iter().map(|u| u[0]).sum::<f64>() / k, off.iter().map(|u| u[1]).sum::<f64>() / k],
            off_share: off.len() as f64 / n,
        })
    }
}

fn unit(v: [f64; 2]) -> [f64; 2] {
    let len = v[0].hypot(v[1]);
    if len < 1e-9 { [0.0, 0.0] } else { [v[0] / len, v[1] / len] }
}

fn blend(a: [f64; 2], b: [f64; 2], w: f64) -> [f64; 2] {
    [a[0] * (1.0 - w) + b[0] * w, a[1] * (1.0 - w) + b[1] * w]
}

/// Standard normal sample.
pub fn normal<R: Rng + ?Sized>(rng: &mut R) -> f64 {
    let u1: f64 = rng.gen_range(f64::EPSILON..1.0);
    let u2: f64 = rng.r#gen();
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Grazing,
    Resting,
    /// Walking after neighbours that moved off.
    Following,
    /// Walking briskly away from the edge after a cue.
    Fleeing,
}

#[derive(Debug, Clone)]
pub struct Animal {
    pub pos: LonLat,
    /// East, north, m/s over the last step.
    pub vel: [f64; 2],
    pub mode: Mode,
    /// Ignores cues more often than most.
    pub stubborn: bool,
    heading: f64,
    mode_left_s: f64,
    speed: f64,
}

impl Animal {
    pub fn new<R: Rng + ?Sized>(pos: LonLat, stubborn: bool, rng: &mut R) -> Animal {
        Animal {
            pos,
            vel: [0.0, 0.0],
            mode: Mode::Grazing,
            stubborn,
            heading: rng.gen_range(0.0..std::f64::consts::TAU),
            mode_left_s: rng.gen_range(120.0..900.0),
            speed: rng.gen_range(0.05..0.25),
        }
    }

    fn set(&mut self, mode: Mode, secs: f64) {
        self.mode = mode;
        self.mode_left_s = secs;
    }

    /// Move for `dt` seconds. `fence` is the boundary the collar holds, `wall`
    /// a physical fence, `cue` what the collar played at the last fix.
    pub fn step<R: Rng + ?Sized>(&mut self, dt: f64, fence: Option<&Area>, wall: Option<&Area>, herd: Option<Herd>, cue: Option<CueCommand>, rng: &mut R) {
        let proj = Projection::new(self.pos);
        let jitter = |v: [f64; 2], s: f64, rng: &mut R| unit([v[0] + s * normal(rng), v[1] + s * normal(rng)]);
        self.mode_left_s -= dt;

        if cue.is_some_and(|c| c.active) && fence.is_some() && !rng.gen_bool(if self.stubborn { 0.4 } else { 0.05 }) {
            self.set(Mode::Fleeing, 20.0);
        }
        if let Some(h) = herd
            && matches!(self.mode, Mode::Grazing | Mode::Resting)
            && h.off_share > 0.0
            && rng.gen_bool(1.0 - (-dt * h.off_share / 5.0).exp())
        {
            self.set(Mode::Following, rng.gen_range(20.0..45.0));
        }
        let to_herd = herd.map(|h| proj.forward(h.centre));
        let herd_d = to_herd.map_or(0.0, |t| t[0].hypot(t[1]));

        let (dir, speed) = match self.mode {
            Mode::Fleeing => match fence {
                Some(f) if self.mode_left_s > 0.0 && f.margin(self.pos).abs() < WARN_M + 2.0 => {
                    (jitter(f.away_from_edge(self.pos), 0.2, rng), rng.gen_range(0.85..1.15))
                }
                _ => {
                    self.set(Mode::Grazing, rng.gen_range(300.0..1200.0));
                    ([0.0, 0.0], 0.0)
                }
            },
            Mode::Following => {
                let h = herd.map_or([0.0, 0.0], |h| h.off_vel);
                if self.mode_left_s <= 0.0 {
                    self.set(Mode::Grazing, rng.gen_range(300.0..1200.0));
                }
                (jitter(h, 0.3, rng), (h[0].hypot(h[1]) * 0.6).clamp(0.3, 0.7))
            }
            Mode::Resting => {
                if self.mode_left_s <= 0.0 || herd_d > 50.0 {
                    self.set(Mode::Grazing, rng.gen_range(300.0..1200.0));
                }
                ([0.0, 0.0], 0.0)
            }
            Mode::Grazing => {
                if self.mode_left_s <= 0.0 {
                    self.set(Mode::Resting, rng.gen_range(60.0..600.0));
                }
                self.heading += 0.45 * normal(rng) * (dt / 5.0).sqrt();
                let mut v = [self.heading.cos(), self.heading.sin()];
                // Herd cohesion, stronger the further away.
                if let Some(t) = to_herd {
                    v = blend(v, unit(t), ((herd_d - 15.0) / 40.0).clamp(0.0, 0.9));
                }
                self.speed = (self.speed + 0.03 * normal(rng)).clamp(0.03, 0.3);
                // Far from the others: walk back to them.
                (unit(v), if herd_d > 50.0 { rng.gen_range(0.5..0.8) } else { self.speed })
            }
        };
        if dir != [0.0, 0.0] {
            self.heading = dir[1].atan2(dir[0]);
        }
        let next = proj.offset(dir[0] * speed * dt, dir[1] * speed * dt);
        // A wire fence stops it; it turns along or away.
        if wall.is_some_and(|w| w.margin(next) < 0.0 && w.margin(next) < w.margin(self.pos)) {
            let back = wall.map_or([0.0, 0.0], |w| w.away_from_edge(self.pos));
            self.heading = back[1].atan2(back[0]) + 0.8 * normal(rng);
            self.vel = [0.0, 0.0];
            return;
        }
        self.pos = next;
        self.vel = [dir[0] * speed, dir[1] * speed];
    }
}

/// A fix of the true position: accuracy 1.5-4 m, 7-12 satellites, ~3% dropouts.
pub fn gnss_fix<R: Rng + ?Sized>(truth: LonLat, at: DateTime<Utc>, rng: &mut R) -> Option<WireFix> {
    if rng.gen_bool(0.03) {
        return None;
    }
    let sats: u32 = rng.gen_range(7..=12);
    let accuracy_m = (4.2 - (sats as f64 - 7.0) * 0.4 + rng.gen_range(-0.5..0.5)).clamp(1.5, 4.0);
    let p = Projection::new(truth).offset(normal(rng) * accuracy_m * 0.6, normal(rng) * accuracy_m * 0.6);
    let r = |v: f64, k: f64| (v * k).round() / k;
    Some(WireFix {
        at,
        point: [r(p[0], 1e7), r(p[1], 1e7)],
        accuracy_m: r(accuracy_m, 10.0),
        sats: Some(sats),
        cn0: Some(r(34.0 + sats as f64 * 0.6 + rng.gen_range(-2.0..2.0), 10.0)),
        ttf_s: Some(r(rng.gen_range(0.8..2.5), 10.0)),
        ..Default::default()
    })
}

/// The fence the collar enforces for the boundary it holds.
struct Held {
    version: u32,
    fence: Geofence,
    area: Area,
}

/// An emulated collar on an animal: the animal, its GNSS receiver, and the
/// firmware (fence, cue policy, slots, config) from op-geo and op-protocol.
pub struct Collar {
    pub animal: Animal,
    pub battery: f64,
    pub firmware: Firmware,
    held: Option<Held>,
    cue: Cue,
    last_cue: Option<CueCommand>,
    /// The herd's paddock, a wire fence until the first boundary arrives.
    paddock: Option<Area>,
    /// Episodes that ended, for the next report.
    episodes: Vec<WireEpisode>,
    /// Fix attempts and fixes got since the last report.
    pub fix_attempts: u32,
    pub fix_ok: u32,
}

/// Battery drain per hour.
const DRAIN_PER_HOUR: f64 = 0.004;

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

impl Collar {
    pub fn new(animal: Animal, battery: f64, paddock: Option<Area>, firmware: Firmware) -> Collar {
        Collar {
            animal,
            battery,
            firmware,
            held: None,
            cue: Cue::new(CueConfig::default()),
            last_cue: None,
            paddock,
            episodes: Vec::new(),
            fix_attempts: 0,
            fix_ok: 0,
        }
    }

    /// The fence being enforced.
    pub fn held_version(&self) -> Option<u32> {
        self.firmware.active_version()
    }

    /// A downloaded command (raw bytes): checked and offered to the slots.
    pub fn offer(&mut self, wire: &[u8], server_key: &VerifyingKey, now: DateTime<Utc>) -> Option<Ack> {
        let a = self.firmware.receive(wire, server_key, now);
        self.follow(now);
        a
    }

    /// Swap the fence when the slots changed the boundary in effect. As the
    /// firmware: unarmed until the next inside fix; a running episode ends
    /// as `boundary_changed`.
    fn follow(&mut self, now: DateTime<Utc>) {
        let Some(cmd) = self.firmware.active().cloned() else { return };
        if self.held.as_ref().is_some_and(|h| h.version == cmd.version) {
            return;
        }
        let old = self.held.as_ref().map(|h| h.version);
        let fence = if self.firmware.profile.is_legacy() {
            cmd.geofence(GeofenceConfig::default())
        } else {
            cmd.fence(GeofenceConfig::default(), &self.firmware.profile.limits())
        };
        let (Ok(fence), Some(area)) = (fence, Area::from_polygon(&cmd.polygon())) else { return };
        self.cue.rearm_at(now.timestamp_millis());
        self.cue.set_mode(cmd.cue_mode);
        self.collect_episodes(old);
        self.held = Some(Held { version: cmd.version, fence, area });
    }

    fn collect_episodes(&mut self, version: Option<u32>) {
        let eps = self.cue.take_episodes();
        if !self.firmware.profile.is_legacy() {
            self.episodes.extend(eps.iter().filter_map(|e| WireEpisode::from_episode(e, version)));
        }
    }

    /// Episodes for the next report (none from firmware 0.1).
    pub fn take_episodes(&mut self) -> Vec<WireEpisode> {
        std::mem::take(&mut self.episodes)
    }

    /// The fence state at the last fix under the held boundary.
    #[cfg(test)]
    pub fn state(&self) -> Option<op_geo::GeofenceState> {
        self.held.as_ref().map(|h| h.fence.state())
    }

    /// Move `dt` seconds, take a fix, apply any staged boundary that is due
    /// on the fix's time, run the fence and the cue policy.
    pub fn tick<R: Rng + ?Sized>(&mut self, now: DateTime<Utc>, dt: f64, herd: Option<Herd>, rng: &mut R) -> (Option<WireFix>, Option<WireCue>) {
        let fence = self.held.as_ref().map(|h| &h.area);
        let wall = if self.held.is_none() { self.paddock.as_ref() } else { None };
        self.animal.step(dt, fence, wall, herd, self.last_cue.take(), rng);
        self.battery = (self.battery - DRAIN_PER_HOUR * dt / 3600.0).max(0.0);
        self.fix_attempts += 1;
        let Some(mut fix) = gnss_fix(self.animal.pos, now, rng) else { return (None, None) };
        self.fix_ok += 1;
        if self.firmware.tick(fix.at).is_some() {
            self.follow(fix.at);
        }
        let legacy = self.firmware.profile.is_legacy();
        if !legacy {
            fix.hdop = Some(round1(fix.accuracy_m / 2.0));
            fix.boundary_version = self.held.as_ref().map(|h| h.version);
        }
        let mut cue = None;
        if let Some(h) = self.held.as_mut() {
            let r = h.fence.update(fix.point, fix.accuracy_m);
            let c = self.cue.update(&r, h.fence.config().warn_m, now.timestamp_millis());
            if c.active {
                let margin_m = round1(r.margin_m);
                cue = Some(if legacy {
                    WireCue { at: now, level: c.volume, margin_m, point: Some(fix.point), ..Default::default() }
                } else {
                    WireCue {
                        at: now,
                        kind: c.kind,
                        level: c.volume,
                        dur_ms: Some(u32::from(c.duration_ms)),
                        margin_m,
                        ring: u16::try_from(r.nearest_ring).ok(),
                        boundary_version: Some(h.version),
                        point: Some(fix.point),
                    }
                });
                self.last_cue = Some(c);
            }
            let version = Some(h.version);
            self.collect_episodes(version);
        }
        (Some(fix), cue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware::Profile;
    use op_geo::Polygon;
    use op_geo::projection::distance_m;
    use op_protocol::{AckStatus, BoundaryCommand};
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    const O: LonLat = [-92.405, 38.125];

    /// `w` x `h` metres, south-west corner `e`, `n` metres from O.
    fn rect(e: f64, n: f64, w: f64, h: f64) -> Vec<LonLat> {
        let p = Projection::new(O);
        vec![p.offset(e, n), p.offset(e + w, n), p.offset(e + w, n + h), p.offset(e, n + h)]
    }

    fn server() -> op_protocol::SigningKey {
        op_protocol::SigningKey::from_bytes(&[9; 32])
    }

    /// Signed wire bytes for the herd, as the server sends them.
    fn wire(version: u32, outer: Vec<LonLat>, holes: Vec<Vec<LonLat>>, effective_at: Option<DateTime<Utc>>) -> Vec<u8> {
        let mut c = BoundaryCommand::from_shape(format!("bnd_{version}"), version, &Polygon::from_rings(outer, holes), effective_at).unwrap();
        c.herd_id = Some("herd_a".into());
        op_protocol::sign_command(&mut c, &server());
        serde_json::to_vec(&c).unwrap()
    }

    fn firmware(p: Profile, i: usize) -> Firmware {
        Firmware::new(p, "herd_a", &format!("col_{i}"), "https://farm.example.com/collar/v1")
    }

    fn herd(ring: &[LonLat], n: usize, rng: &mut StdRng) -> Vec<Collar> {
        let area = Area::new(ring).unwrap();
        let p = Projection::new(area.interior());
        (0..n)
            .map(|i| {
                Collar::new(Animal::new(p.offset(10.0 * normal(rng), 10.0 * normal(rng)), i % 4 == 3, rng), 0.9, Some(area.clone()), firmware(Profile::V0, i))
            })
            .collect()
    }

    fn view(h: &[Collar], skip: usize) -> Option<Herd> {
        Herd::of(h.iter().enumerate().filter(|(j, _)| *j != skip).map(|(_, c)| (c.animal.pos, c.animal.vel)))
    }

    /// One fix of `dt` seconds for every collar. Returns cues.
    fn tick_all(h: &mut [Collar], now: DateTime<Utc>, dt: f64, rng: &mut StdRng) -> usize {
        let mut cues = 0;
        for i in 0..h.len() {
            let v = view(h, i);
            cues += h[i].tick(now, dt, v, rng).1.is_some() as usize;
        }
        cues
    }

    /// `steps` fixes of 5 s. Returns (share of true positions inside `ring`, cues).
    fn run(h: &mut [Collar], ring: &[LonLat], t0: DateTime<Utc>, steps: usize, rng: &mut StdRng) -> (f64, usize) {
        let area = Area::new(ring).unwrap();
        let (mut inside, mut cues) = (0, 0);
        for s in 0..steps {
            cues += tick_all(h, t0 + chrono::Duration::seconds(5 * s as i64), 5.0, rng);
            inside += h.iter().filter(|c| area.margin(c.animal.pos) >= 0.0).count();
        }
        (inside as f64 / (steps * h.len()) as f64, cues)
    }

    fn offer_all(h: &mut [Collar], w: &[u8], now: DateTime<Utc>) {
        let k = server().verifying_key();
        h.iter_mut().for_each(|x| drop(x.offer(w, &k, now)));
    }

    #[test]
    fn herd_stays_mostly_inside_and_meets_the_fence() {
        let mut rng = StdRng::seed_from_u64(7);
        let ring = rect(0.0, 0.0, 120.0, 90.0);
        let mut h = herd(&ring, 8, &mut rng);
        let t0 = Utc::now();
        for c in &mut h {
            assert_eq!(c.offer(&wire(1, ring.clone(), vec![], None), &server().verifying_key(), t0).unwrap().status, AckStatus::Applied);
        }
        let (inside, cues) = run(&mut h, &ring, t0, 1440, &mut rng);
        assert!(inside > 0.9, "inside {inside}");
        assert!(cues > 0);
        let c = Herd::of(h.iter().map(|a| (a.animal.pos, a.animal.vel))).unwrap().centre;
        assert!(h.iter().all(|a| distance_m(a.animal.pos, c) < 80.0), "a loose herd, not scattered");
        assert!(h.iter().all(|c| (0.85..0.9).contains(&c.battery)));
        // Firmware 0.2 reports episodes of the cues it played, under the boundary held.
        let eps: Vec<WireEpisode> = h.iter_mut().flat_map(|c| c.take_episodes()).collect();
        assert!(!eps.is_empty());
        assert!(eps.iter().all(|e| e.boundary_version == Some(1) && e.cues >= 1));
    }

    #[test]
    fn keeps_to_the_paddock_without_a_boundary() {
        let mut rng = StdRng::seed_from_u64(11);
        let ring = rect(0.0, 0.0, 100.0, 100.0);
        let mut h = herd(&ring, 6, &mut rng);
        let (inside, cues) = run(&mut h, &ring, Utc::now(), 720, &mut rng);
        assert!(inside > 0.99, "inside {inside}");
        assert_eq!(cues, 0);
    }

    #[test]
    fn warning_cue_moves_an_animal_away_from_the_edge() {
        let mut rng = StdRng::seed_from_u64(2);
        let area = Area::new(&rect(0.0, 0.0, 100.0, 100.0)).unwrap();
        let p = Projection::new(O);
        let mut a = Animal::new(p.offset(50.0, 3.0), false, &mut rng);
        let beep = CueCommand { active: true, freq_hz: 2730, volume: 3, duration_ms: 300, ..Default::default() };
        a.step(1.0, Some(&area), None, None, Some(beep), &mut rng);
        a.step(1.0, Some(&area), None, None, Some(beep), &mut rng);
        assert_eq!(a.mode, Mode::Fleeing);
        for _ in 0..10 {
            a.step(1.0, Some(&area), None, None, None, &mut rng);
        }
        let at = p.forward(a.pos);
        assert!(at[1] > WARN_M + 1.0, "walked north, off the south edge: {at:?}");
        assert!((at[0] - 50.0).abs() < 5.0, "straight away from the edge: {at:?}");
        assert_ne!(a.mode, Mode::Fleeing, "calms once out of the warning zone");
    }

    #[test]
    fn an_animal_a_new_boundary_leaves_outside_is_not_cued_or_moved() {
        let mut rng = StdRng::seed_from_u64(3);
        let (old, new) = (rect(0.0, 0.0, 100.0, 100.0), rect(130.0, 0.0, 100.0, 100.0));
        let mut h = herd(&old, 6, &mut rng);
        let t0 = Utc::now();
        offer_all(&mut h, &wire(1, old.clone(), vec![], None), t0);
        run(&mut h, &old, t0, 60, &mut rng);
        let t1 = t0 + chrono::Duration::minutes(5);
        offer_all(&mut h, &wire(2, new.clone(), vec![], None), t1);
        let area = Area::new(&new).unwrap();
        for s in 0..360 {
            let before: Vec<LonLat> = h.iter().map(|c| c.animal.pos).collect();
            let cues = tick_all(&mut h, t1 + chrono::Duration::seconds(5 * s), 5.0, &mut rng);
            for (c, b) in h.iter().zip(before) {
                assert!(distance_m(c.animal.pos, b) < 5.0, "walks, never jumps");
                if area.margin(c.animal.pos) < -5.0 {
                    // Unknown only until its first fix under the new boundary.
                    assert!(matches!(c.state(), Some(op_geo::GeofenceState::Outside | op_geo::GeofenceState::Unknown)));
                }
            }
            assert_eq!(cues, 0, "nobody crossed, so nobody is cued");
        }
        let near = h.iter().filter(|c| area.margin(c.animal.pos) > -20.0).count();
        assert!(near <= 1, "no pull toward the new boundary: {near} near it");
    }

    #[test]
    fn a_slow_back_line_pushes_a_small_herd_into_a_corner() {
        let mut rng = StdRng::seed_from_u64(9);
        let field = rect(0.0, 0.0, 200.0, 100.0);
        let (target, done) = (rect(160.0, 60.0, 40.0, 40.0), 40);
        // Step k of `done`: back and south edges advance 4 m and 1.5 m, inside the warning zone.
        let step = |k: usize| {
            let s = k as f64 / done as f64;
            rect(160.0 * s, 60.0 * s, 200.0 - 160.0 * s, 100.0 - 60.0 * s)
        };
        let p = Projection::new(O);
        let mut h: Vec<Collar> = (0..5)
            .map(|i| {
                let a = Animal::new(p.offset(40.0 + 6.0 * normal(&mut rng), 50.0 + 6.0 * normal(&mut rng)), i == 4, &mut rng);
                Collar::new(a, 0.9, None, firmware(Profile::V0, i))
            })
            .collect();
        let t0 = Utc::now();
        offer_all(&mut h, &wire(1, field, vec![], None), t0);
        let (mut k, mut last_step, mut waiting_since) = (0, 0, 0);
        let mut stragglers = vec![false; h.len()];
        for s in 1..=(90 * 12) {
            let secs = 5 * s as i64;
            tick_all(&mut h, t0 + chrono::Duration::seconds(secs), 5.0, &mut rng);
            if k == done {
                break;
            }
            // As the server: every animal in the sweep ahead of the next line, at most every 30 s.
            let next = Area::new(&step(k + 1)).unwrap();
            let behind: Vec<usize> = (0..h.len()).filter(|&i| !stragglers[i] && next.margin(h[i].animal.pos) < 1.0).collect();
            if behind.is_empty() && secs - last_step >= 30 {
                k += 1;
                (last_step, waiting_since) = (secs, secs);
                offer_all(&mut h, &wire(k as u32 + 1, step(k), vec![], None), t0 + chrono::Duration::seconds(secs));
            } else if !behind.is_empty() && secs - waiting_since >= 300 {
                behind.iter().for_each(|&i| stragglers[i] = true);
            }
            if behind.is_empty() {
                waiting_since = secs;
            }
        }
        let area = Area::new(&target).unwrap();
        let in_target = h.iter().filter(|c| area.margin(c.animal.pos) > 0.0).count();
        assert_eq!(k, done, "the sweep reached the target in 90 min");
        assert!(in_target >= 4, "{in_target} of 5 in the corner, stragglers {stragglers:?}");
    }

    #[test]
    fn a_hole_is_a_crossing_and_the_fence_knows_the_ring() {
        let mut rng = StdRng::seed_from_u64(4);
        let p = Projection::new(O);
        let pond = rect(60.0, 40.0, 30.0, 30.0);
        let mut c = Collar::new(Animal::new(p.offset(50.0, 55.0), false, &mut rng), 0.9, None, firmware(Profile::V0, 0));
        let t0 = Utc::now();
        c.offer(&wire(1, rect(0.0, 0.0, 150.0, 120.0), vec![pond.clone()], None), &server().verifying_key(), t0).unwrap();
        // Walked into the pond: outside, cued with the outside tone about ring 1.
        let steps = [[50.0, 55.0], [56.0, 55.0], [75.0, 55.0]];
        let mut last = None;
        for (i, at) in steps.iter().enumerate() {
            c.animal.pos = p.offset(at[0], at[1]);
            let h = c.held.as_mut().unwrap();
            let r = h.fence.update(c.animal.pos, 1.0);
            last = Some((r, c.cue.update(&r, 5.0, (t0 + chrono::Duration::seconds(5 * i as i64)).timestamp_millis())));
        }
        let (r, cmd) = last.unwrap();
        assert_eq!((r.state, r.nearest_ring), (op_geo::GeofenceState::Outside, 1));
        assert_eq!((cmd.active, cmd.kind), (true, Some(op_geo::CueKind::Outside)));
        // The same boundary without the hole on a legacy collar: refused (no holes there).
        let mut l = Collar::new(Animal::new(p.offset(50.0, 55.0), false, &mut rng), 0.9, None, firmware(Profile::Legacy, 1));
        let a = l.offer(&wire(1, rect(0.0, 0.0, 150.0, 120.0), vec![pond], None), &server().verifying_key(), t0).unwrap();
        assert_eq!(a.status, AckStatus::Rejected);
        assert!(l.held_version().is_none());
    }

    #[test]
    fn staged_boundaries_apply_on_the_collars_own_clock() {
        let mut rng = StdRng::seed_from_u64(5);
        let ring = rect(0.0, 0.0, 120.0, 90.0);
        let mut h = herd(&ring, 1, &mut rng);
        let t0 = op_protocol::wire_time::trunc_secs(Utc::now());
        let k = server().verifying_key();
        h[0].offer(&wire(1, ring.clone(), vec![], None), &k, t0);
        h[0].offer(&wire(2, rect(0.0, 0.0, 110.0, 90.0), vec![], Some(t0 + chrono::Duration::seconds(60))), &k, t0);
        assert_eq!((h[0].held_version(), h[0].firmware.staged()), (Some(1), vec![2]));
        // No server in sight: fixes every 5 s apply it at its time.
        for s in 1..=14 {
            tick_all(&mut h, t0 + chrono::Duration::seconds(5 * s), 5.0, &mut rng);
        }
        assert_eq!(h[0].held_version(), Some(2));
        let applied = h[0].firmware.pending_acks().iter().find(|a| a.version == 2 && a.status == AckStatus::Applied).unwrap().clone();
        assert!(applied.at >= t0 + chrono::Duration::seconds(60) && applied.at <= t0 + chrono::Duration::seconds(66), "{}", applied.at);
    }

    #[test]
    fn gnss_noise() {
        let mut rng = StdRng::seed_from_u64(5);
        let fixes: Vec<WireFix> = (0..2000).filter_map(|_| gnss_fix(O, Utc::now(), &mut rng)).collect();
        assert!((20..=110).contains(&(2000 - fixes.len())), "dropouts");
        assert!(fixes.iter().all(|f| (1.5..=4.0).contains(&f.accuracy_m) && (7..=12).contains(&f.sats.unwrap()) && distance_m(O, f.point) < 20.0));
    }
}
