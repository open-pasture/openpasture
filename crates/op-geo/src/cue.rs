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
//!
//! Protocol v1 (§3.6) adds cue kinds (`warn`, `outside`), a track mode that
//! evaluates the fence without sound, and episodes: a run of armed warning
//! cues, ending `turned_back` (the animal reached inside), `crossed`, `rest`
//! (the 20 s cap forced a rest) or `boundary_changed` (a new boundary).

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

/// Which cue played. These are the only two kinds there are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CueKind {
    /// The warning tone, louder toward the edge.
    #[default]
    Warn,
    /// The tone after a crossing, for up to 10 s.
    Outside,
}

impl CueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Outside => "outside",
        }
    }
}

/// What a collar does with a boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CueMode {
    /// Cue the animal.
    #[default]
    Audio,
    /// Evaluate the fence and report state only: no sound, no cues, no episodes.
    Track,
}

impl CueMode {
    pub fn is_audio(&self) -> bool {
        *self == Self::Audio
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CueCommand {
    pub active: bool,
    pub freq_hz: u16,
    /// 0-4, matches the Qwiic Buzzer.
    pub volume: u8,
    pub duration_ms: u16,
    /// Set whenever `active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<CueKind>,
}

/// How an episode ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    /// The animal reached inside, clear of the warning zone.
    #[default]
    TurnedBack,
    /// The animal went outside.
    Crossed,
    /// The 20 s cap on continuous cueing forced a rest.
    Rest,
    /// A new boundary was applied.
    BoundaryChanged,
}

impl EpisodeOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TurnedBack => "turned_back",
            Self::Crossed => "crossed",
            Self::Rest => "rest",
            Self::BoundaryChanged => "boundary_changed",
        }
    }
}

/// A run of armed warning cues. Times are milliseconds on the caller's
/// clock, as [`Cue::update`]'s `now_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    /// The first warning cue.
    pub start: i64,
    /// The fix (or boundary change) that ended it.
    pub end: i64,
    /// Nearest ring at the first cue: 0 outer, 1.. holes.
    pub ring: usize,
    /// Warning cues played.
    pub cues: u32,
    /// Loudest warning cue.
    pub max_level: u8,
    /// Least margin over its fixes, the ending one included (negative when it crossed).
    pub min_margin_m: f64,
    pub outcome: EpisodeOutcome,
}

/// Builds [`Episode`]s from what the fence and the cue policy did at each fix.
#[derive(Debug, Clone, Default)]
pub struct EpisodeTracker {
    open: Option<Episode>,
}

impl EpisodeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether an episode is running.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// After each fix: the fence result, what the cue policy played, and
    /// whether the policy began a forced rest at this fix. Returns an episode
    /// that ended here.
    pub fn observe(&mut self, r: &GeofenceResult, cmd: &CueCommand, rest_began: bool, now_ms: i64) -> Option<Episode> {
        let warn = cmd.active && cmd.kind == Some(CueKind::Warn);
        if let Some(ep) = self.open.as_mut() {
            ep.min_margin_m = ep.min_margin_m.min(r.margin_m);
            if warn {
                ep.cues += 1;
                ep.max_level = ep.max_level.max(cmd.volume);
            }
            let outcome = match r.state {
                GeofenceState::Outside => Some(EpisodeOutcome::Crossed),
                GeofenceState::Inside => Some(EpisodeOutcome::TurnedBack),
                _ if rest_began => Some(EpisodeOutcome::Rest),
                _ => None,
            };
            return outcome.and_then(|o| self.close(o, now_ms));
        }
        if warn {
            self.open = Some(Episode {
                start: now_ms,
                end: now_ms,
                ring: r.nearest_ring,
                cues: 1,
                max_level: cmd.volume,
                min_margin_m: r.margin_m,
                outcome: EpisodeOutcome::TurnedBack,
            });
        }
        None
    }

    /// A new boundary was applied: ends a running episode.
    pub fn boundary_changed(&mut self, now_ms: i64) -> Option<Episode> {
        self.close(EpisodeOutcome::BoundaryChanged, now_ms)
    }

    fn close(&mut self, outcome: EpisodeOutcome, now_ms: i64) -> Option<Episode> {
        let mut ep = self.open.take()?;
        ep.end = now_ms;
        ep.outcome = outcome;
        Some(ep)
    }
}

#[derive(Debug, Clone)]
pub struct Cue {
    cfg: CueConfig,
    mode: CueMode,
    cueing: bool,
    active_since_ms: i64,
    rest_until_ms: i64,
    outside_since_ms: i64,
    armed: bool,
    /// Outside since the last rearm, so warning doesn't arm.
    seen_outside: bool,
    /// Crossed out; the outside tone window is running.
    escaping: bool,
    episodes: EpisodeTracker,
    done: Vec<Episode>,
    last_ms: i64,
}

/// 1 at the inner edge of the zone, 4 at the boundary.
fn warning_volume(margin_m: f64, warn_m: f64) -> u8 {
    let depth = warn_m - margin_m;
    let v = 1 + (3.0 * depth / warn_m) as i32;
    v.clamp(1, 4) as u8
}

impl Cue {
    pub fn new(cfg: CueConfig) -> Self {
        Self {
            cfg,
            mode: CueMode::Audio,
            cueing: false,
            active_since_ms: 0,
            rest_until_ms: 0,
            outside_since_ms: 0,
            armed: false,
            seen_outside: false,
            escaping: false,
            episodes: EpisodeTracker::new(),
            done: Vec::new(),
            last_ms: 0,
        }
    }

    pub fn mode(&self) -> CueMode {
        self.mode
    }

    /// Switch between cueing and tracking only. A running episode ends as
    /// `boundary_changed`: the mode comes with a boundary.
    pub fn set_mode(&mut self, mode: CueMode) {
        if mode != self.mode {
            self.done.extend(self.episodes.boundary_changed(self.last_ms));
            self.cueing = false;
        }
        self.mode = mode;
    }

    /// Call whenever a new boundary is applied: silent until the next inside
    /// fix. Keeps any forced rest that is running. A running episode ends as
    /// `boundary_changed` at the last fix's time; see [`Cue::rearm_at`].
    pub fn rearm(&mut self) {
        self.rearm_at(self.last_ms);
    }

    /// [`Cue::rearm`] at `now_ms`.
    pub fn rearm_at(&mut self, now_ms: i64) {
        self.done.extend(self.episodes.boundary_changed(now_ms));
        self.armed = false;
        self.seen_outside = false;
        self.escaping = false;
        self.cueing = false;
    }

    /// Whether a crossing would be cued now.
    pub fn armed(&self) -> bool {
        self.armed
    }

    /// Episodes that ended since the last call.
    pub fn take_episodes(&mut self) -> Vec<Episode> {
        std::mem::take(&mut self.done)
    }

    pub fn update(&mut self, r: &GeofenceResult, warn_m: f64, now_ms: i64) -> CueCommand {
        self.last_ms = now_ms;
        let (cmd, rest_began) = self.decide(r, warn_m, now_ms);
        if self.mode == CueMode::Track {
            return CueCommand::default();
        }
        if let Some(ep) = self.episodes.observe(r, &cmd, rest_began, now_ms) {
            self.done.push(ep);
        }
        cmd
    }

    /// The cue for this fix, and whether a forced rest began at it.
    fn decide(&mut self, r: &GeofenceResult, warn_m: f64, now_ms: i64) -> (CueCommand, bool) {
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

        if self.mode == CueMode::Track {
            return (cmd, false);
        }
        if r.state == GeofenceState::Warning && self.armed {
            want = true;
            cmd.freq_hz = self.cfg.warn_freq_hz;
            cmd.volume = warning_volume(r.margin_m, warn_m);
            cmd.kind = Some(CueKind::Warn);
        } else if r.state == GeofenceState::Outside && self.escaping && now_ms - self.outside_since_ms < self.cfg.outside_max_ms as i64 {
            want = true;
            cmd.freq_hz = self.cfg.outside_freq_hz;
            cmd.volume = 4;
            cmd.kind = Some(CueKind::Outside);
        }

        if !want {
            self.cueing = false;
            return (CueCommand::default(), false);
        }
        if now_ms < self.rest_until_ms {
            return (CueCommand::default(), false);
        }
        if !self.cueing {
            self.cueing = true;
            self.active_since_ms = now_ms;
        } else if now_ms - self.active_since_ms >= self.cfg.max_active_ms as i64 {
            self.cueing = false;
            self.rest_until_ms = now_ms + self.cfg.rest_ms as i64;
            return (CueCommand::default(), true);
        }

        cmd.active = true;
        cmd.duration_ms = self.cfg.beep_ms;
        (cmd, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(state: GeofenceState, margin_m: f64) -> GeofenceResult {
        GeofenceResult { state, margin_m, ..Default::default() }
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

    #[test]
    fn cues_carry_their_kind() {
        let (mut gf, mut c) = (square(0.0, 1), Cue::new(CueConfig::default()));
        step(&mut gf, &mut c, 50.0, 50.0, 0);
        let (cmd, _) = step(&mut gf, &mut c, 50.0, 3.0, 1_000);
        assert_eq!(cmd.kind, Some(CueKind::Warn));
        let (cmd, _) = step(&mut gf, &mut c, 50.0, -2.0, 2_000);
        assert_eq!(cmd.kind, Some(CueKind::Outside));
        let (cmd, _) = step(&mut gf, &mut c, 50.0, -2.0, 30_000);
        assert!(!cmd.active && cmd.kind.is_none());
        assert_eq!(serde_json::to_string(&CueKind::Outside).unwrap(), "\"outside\"");
        assert_eq!(serde_json::to_string(&EpisodeOutcome::BoundaryChanged).unwrap(), "\"boundary_changed\"");
        assert_eq!(serde_json::to_string(&CueMode::Track).unwrap(), "\"track\"");
    }

    #[test]
    fn episode_turned_back() {
        let (mut gf, mut c) = (square(0.0, 1), Cue::new(CueConfig::default()));
        step(&mut gf, &mut c, 50.0, 50.0, 0);
        step(&mut gf, &mut c, 50.0, 4.0, 1_000);
        step(&mut gf, &mut c, 50.0, 1.5, 2_000);
        step(&mut gf, &mut c, 50.0, 3.0, 3_000);
        assert!(c.take_episodes().is_empty(), "still running");
        step(&mut gf, &mut c, 50.0, 20.0, 4_000);
        let eps = c.take_episodes();
        assert_eq!(eps.len(), 1);
        let e = eps[0];
        assert_eq!((e.start, e.end, e.cues, e.ring, e.outcome), (1_000, 4_000, 3, 0, EpisodeOutcome::TurnedBack));
        assert_eq!(e.max_level, 3);
        assert!(near_m(e.min_margin_m, 1.5), "{}", e.min_margin_m);
        assert!(c.take_episodes().is_empty(), "taken once");
    }

    #[test]
    fn episode_crossed() {
        let (mut gf, mut c) = (square(0.0, 1), Cue::new(CueConfig::default()));
        step(&mut gf, &mut c, 50.0, 50.0, 0);
        step(&mut gf, &mut c, 50.0, 2.0, 1_000);
        let (cmd, _) = step(&mut gf, &mut c, 50.0, -1.5, 2_000);
        assert_eq!(cmd.kind, Some(CueKind::Outside), "the crossing is cued");
        let e = c.take_episodes()[0];
        assert_eq!((e.cues, e.outcome, e.end), (1, EpisodeOutcome::Crossed, 2_000));
        assert!(near_m(e.min_margin_m, -1.5));
        // The outside tone that follows is not a new episode.
        step(&mut gf, &mut c, 50.0, -3.0, 3_000);
        assert!(c.take_episodes().is_empty());
    }

    #[test]
    fn episode_rest() {
        let (mut gf, mut c) = (square(0.0, 1), Cue::new(CueConfig::default()));
        step(&mut gf, &mut c, 50.0, 50.0, 0);
        let mut t = 1_000;
        while t <= 21_000 {
            step(&mut gf, &mut c, 50.0, 2.5, t);
            t += 1_000;
        }
        let e = c.take_episodes()[0];
        assert_eq!((e.start, e.end, e.cues, e.outcome), (1_000, 21_000, 20, EpisodeOutcome::Rest));
        // Silent while resting (until 51 s); cueing again after it starts a new episode.
        assert!(!step(&mut gf, &mut c, 50.0, 2.5, 30_000).0.active);
        assert!(!step(&mut gf, &mut c, 50.0, 2.5, 50_000).0.active);
        assert!(c.take_episodes().is_empty());
        assert!(step(&mut gf, &mut c, 50.0, 2.5, 52_000).0.active);
        step(&mut gf, &mut c, 50.0, 30.0, 53_000);
        assert_eq!(c.take_episodes()[0].outcome, EpisodeOutcome::TurnedBack);
    }

    #[test]
    fn episode_boundary_changed() {
        let (mut gf, mut c) = (square(0.0, 1), Cue::new(CueConfig::default()));
        step(&mut gf, &mut c, 50.0, 50.0, 0);
        step(&mut gf, &mut c, 50.0, 3.0, 1_000);
        gf = square(30.0, 2);
        c.rearm_at(1_500);
        let e = c.take_episodes()[0];
        assert_eq!((e.end, e.outcome), (1_500, EpisodeOutcome::BoundaryChanged));
        // rearm() without a time ends one at the last fix.
        step(&mut gf, &mut c, 50.0, 33.0, 2_000);
        c.rearm();
        let e = c.take_episodes()[0];
        assert_eq!((e.start, e.end, e.outcome), (2_000, 2_000, EpisodeOutcome::BoundaryChanged));
    }

    #[test]
    fn track_mode_is_silent() {
        let (mut gf, mut c) = (square(0.0, 1), Cue::new(CueConfig::default()));
        c.set_mode(CueMode::Track);
        assert_eq!(c.mode(), CueMode::Track);
        let mut states = Vec::new();
        for (k, n) in [50.0, 3.0, 1.0, -2.0, -5.0, 3.0, 50.0].into_iter().enumerate() {
            let (cmd, st) = step(&mut gf, &mut c, 50.0, n, 1_000 * k as i64);
            assert_eq!(cmd, CueCommand::default(), "no sound");
            states.push(st);
        }
        assert!(c.take_episodes().is_empty(), "no episodes");
        assert_eq!(states[3], GeofenceState::Outside, "the fence still runs");
        // Back to audio: armed by the last inside fix, so it cues again.
        c.set_mode(CueMode::Audio);
        assert!(step(&mut gf, &mut c, 50.0, 3.0, 10_000).0.active);
    }

    #[test]
    fn walking_into_a_hole_is_a_crossing() {
        use crate::limits::CollarLimits;
        use crate::polygon::Polygon;
        let p = Projection::new(O);
        let mut poly = Polygon::from_ring(vec![p.offset(0.0, 0.0), p.offset(100.0, 0.0), p.offset(100.0, 100.0), p.offset(0.0, 100.0)]);
        poly.coordinates.push(vec![p.offset(40.0, 40.0), p.offset(60.0, 40.0), p.offset(60.0, 60.0), p.offset(40.0, 60.0)]);
        let mut gf = Geofence::from_polygon(GeofenceConfig::default(), &poly, 1, &CollarLimits::V0).unwrap();
        let mut c = Cue::new(CueConfig::default());
        step(&mut gf, &mut c, 50.0, 20.0, 0);
        let (cmd, _) = step(&mut gf, &mut c, 50.0, 37.0, 1_000);
        assert_eq!(cmd.kind, Some(CueKind::Warn), "the warning zone runs along the hole");
        let (cmd, st) = step(&mut gf, &mut c, 50.0, 42.0, 2_000);
        assert_eq!((cmd.kind, st), (Some(CueKind::Outside), GeofenceState::Outside));
        let e = c.take_episodes()[0];
        assert_eq!((e.ring, e.outcome), (1, EpisodeOutcome::Crossed));
    }

    #[test]
    fn a_hole_drawn_on_an_animal_leaves_it_silent() {
        use crate::limits::CollarLimits;
        use crate::polygon::Polygon;
        let p = Projection::new(O);
        let outer = vec![p.offset(0.0, 0.0), p.offset(100.0, 0.0), p.offset(100.0, 100.0), p.offset(0.0, 100.0)];
        let mut gf = Geofence::from_polygon(GeofenceConfig::default(), &Polygon::from_ring(outer.clone()), 1, &CollarLimits::V0).unwrap();
        let mut c = Cue::new(CueConfig::default());
        step(&mut gf, &mut c, 50.0, 50.0, 0);
        // A new boundary with a hole on top of the animal.
        let mut holed = Polygon::from_ring(outer);
        holed.coordinates.push(vec![p.offset(40.0, 40.0), p.offset(60.0, 40.0), p.offset(60.0, 60.0), p.offset(40.0, 60.0)]);
        gf = Geofence::from_polygon(GeofenceConfig::default(), &holed, 2, &CollarLimits::V0).unwrap();
        c.rearm_at(500);
        let mut t = 1_000;
        for n in [50.0, 45.0, 41.0, 38.0, 36.0] {
            let (cmd, _) = step(&mut gf, &mut c, 50.0, n, t);
            assert!(!cmd.active, "silent while it walks out (n = {n})");
            t += 1_000;
        }
        // Clear of the warning zone: armed again.
        step(&mut gf, &mut c, 50.0, 25.0, t);
        assert!(c.armed());
    }

    fn near_m(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.05
    }
}
