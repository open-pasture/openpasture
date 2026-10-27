//! What the bridge knows about the collar on the port, built only from what
//! it printed: fixes, cues and episodes waiting for the next report, its
//! last `status`, the cadence of the config it accepted, and acks owed to
//! the server.
//!
//! Times are the collar's GNSS UTC. A fix line without one is dropped, never
//! stamped with this computer's clock. An episode ends at the fix that
//! printed it (the collar logs episodes after the fix that closed them).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use op_protocol::{
    Ack, AckStatus, CollarLimits, ConfigCommand, ConfigReject, DeviceInfo, Health, MAX_FIXES_PER_REPORT, PositionReport, RejectCode, SlotReport, WireCue,
    WireEpisode, WireFix, caps,
};

use crate::console::{AckLine, EpisodeLine, FixLine, Line, Logged, Status};

/// Report and download intervals before the collar takes a config.
pub const DEFAULT_INTERVAL_S: u32 = 60;
/// How long an ack waits for the collar's own log line (which has its time).
const ACK_WAIT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, PartialEq)]
pub struct OwedAck {
    pub ack: Ack,
    /// No GNSS time from the collar: stamped with this computer's clock.
    pub host_time: bool,
}

#[derive(Debug, Clone)]
struct PendingAck {
    version: u32,
    status: AckStatus,
    code: Option<RejectCode>,
    at: Option<DateTime<Utc>>,
    since: Instant,
}

#[derive(Default)]
pub struct Session {
    /// The last `status` reply; `None` before one or while unprovisioned.
    pub status: Option<Status>,
    /// Firmware version from the boot line or `status`.
    pub fw: Option<String>,
    fixes: Vec<WireFix>,
    cues: Vec<WireCue>,
    episodes: Vec<WireEpisode>,
    /// The newest fix's GNSS time and when it was read: the collar's clock.
    clock: Option<(DateTime<Utc>, Instant)>,
    last_version: Option<u32>,
    sats: Option<u32>,
    uptime_ms: Option<u64>,
    /// The config the collar accepted through this bridge.
    pub config: Option<ConfigCommand>,
    config_reject: Option<ConfigReject>,
    /// Command ids of the boundaries this bridge delivered, by version.
    delivered: HashMap<u32, String>,
    pending: Vec<PendingAck>,
    /// Fix lines dropped for having no time.
    pub untimed: u64,
    /// Fixes and cues dropped because the server was out of reach too long.
    pub dropped: u64,
}

/// What the terminal hears about a line, if anything.
pub type Said = Option<String>;

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// GNSS time now: the last fix's, carried forward on this computer's monotonic clock.
    pub fn collar_now(&self, now: Instant) -> Option<DateTime<Utc>> {
        let (at, read) = self.clock?;
        Some(at + chrono::Duration::from_std(now.saturating_duration_since(read)).ok()?)
    }

    /// Take in a log line.
    pub fn on_line(&mut self, l: &Logged, now: Instant) -> Said {
        if let Some(ms) = l.uptime_ms {
            self.uptime_ms = Some(ms);
        }
        match &l.line {
            Line::Boot { fw } => {
                self.fw = Some(fw.clone());
                Some(format!("collar started: firmware {fw}"))
            }
            Line::Fix(f) => self.on_fix(f, now),
            Line::Episode(e) => self.on_episode(e),
            Line::Ack(a) => self.on_ack_line(a, now),
            Line::Searching { .. } | Line::Reply(_) | Line::Log(_) | Line::Other(_) => None,
        }
    }

    fn on_fix(&mut self, f: &FixLine, now: Instant) -> Said {
        let Some(at) = f.at else {
            self.untimed += 1;
            return (self.untimed == 1)
                .then(|| "fix lines carry no GNSS time (firmware older than the `at=` field): fixes are not reported until it is reflashed".into());
        };
        self.clock = Some((at, now));
        self.sats = f.sats;
        let point = [f.lon, f.lat];
        let version = f.fence.as_ref().map(|fe| fe.version);
        self.fixes.push(WireFix { at, point, accuracy_m: f.accuracy_m, sats: f.sats, boundary_version: version, ..WireFix::default() });
        if let Some(fe) = &f.fence {
            self.last_version = Some(fe.version);
            if let Some((kind, level)) = fe.cue {
                self.cues.push(WireCue {
                    at,
                    kind: Some(kind.into()),
                    level,
                    dur_ms: None,
                    margin_m: fe.margin_m,
                    ring: fe.ring,
                    boundary_version: Some(fe.version),
                    point: Some(point),
                });
                return Some(format!("cue {} level {level} at {:.1} m (v{})", kind.as_str(), fe.margin_m, fe.version));
            }
        }
        None
    }

    fn on_episode(&mut self, e: &EpisodeLine) -> Said {
        let end = self.clock.map(|(at, _)| at)?;
        let start = end - chrono::Duration::milliseconds(e.dur_ms.max(0));
        self.episodes.push(WireEpisode {
            start,
            end,
            boundary_version: self.last_version,
            ring: e.ring,
            cues: e.cues,
            max_level: e.max_level,
            min_margin_m: e.min_margin_m,
            outcome: e.outcome.into(),
        });
        Some(format!("episode {} after {} cues", e.outcome.as_str(), e.cues))
    }

    /// The collar's own ack log line: its status and GNSS time win.
    fn on_ack_line(&mut self, a: &AckLine, now: Instant) -> Said {
        match self.pending.iter_mut().find(|p| p.version == a.version) {
            Some(p) => {
                p.status = a.status;
                p.code = a.code.or(p.code);
                p.at = a.at.or(p.at);
                None
            }
            None => {
                // An ack the collar made by itself: a staged boundary came into force.
                self.pending.push(PendingAck { version: a.version, status: a.status, code: a.code, at: a.at, since: now });
                Some(format!("boundary v{} {}", a.version, a.status.as_str()))
            }
        }
    }

    /// A boundary was written to the console.
    pub fn delivered(&mut self, version: u32, command_id: &str) {
        self.delivered.insert(version, command_id.to_owned());
    }

    /// The console's reply to a boundary: an ack is owed.
    pub fn boundary_reply(&mut self, version: u32, status: AckStatus, code: Option<RejectCode>, now: Instant) {
        let at = self.collar_now(now);
        match self.pending.iter_mut().find(|p| p.version == version) {
            Some(p) => {
                p.status = status;
                p.code = code.or(p.code);
            }
            None => self.pending.push(PendingAck { version, status, code, at, since: now }),
        }
    }

    /// Acks ready to post: those with the collar's time, or that waited long
    /// enough for it. Ones for boundaries this bridge didn't deliver can't be
    /// posted (no command id); they come back as `Err(version)`.
    pub fn take_acks(&mut self, now: Instant) -> Vec<Result<OwedAck, u32>> {
        let mut out = Vec::new();
        let mut keep = Vec::new();
        for p in std::mem::take(&mut self.pending) {
            if p.at.is_none() && now.saturating_duration_since(p.since) < ACK_WAIT {
                keep.push(p);
                continue;
            }
            let Some(command_id) = self.delivered.get(&p.version).cloned() else {
                out.push(Err(p.version));
                continue;
            };
            let (at, host_time) = match p.at.or_else(|| self.collar_now(now)) {
                Some(at) => (at, false),
                None => (Utc::now(), true),
            };
            let code = p.code.filter(|_| p.status == AckStatus::Rejected);
            out.push(Ok(OwedAck { ack: Ack { collar_id: None, command_id, version: p.version, status: p.status.into(), reason: None, code, at }, host_time }));
        }
        self.pending = keep;
        out
    }

    /// An ack the server didn't take for a reason worth retrying.
    pub fn retry_ack(&mut self, owed: OwedAck, now: Instant) {
        self.pending.push(PendingAck {
            version: owed.ack.version,
            status: owed.ack.status.known().unwrap_or_default(),
            code: owed.ack.code,
            at: Some(owed.ack.at),
            since: now,
        });
    }

    /// The collar took a config: its cadence is the bridge's from now on.
    pub fn config_accepted(&mut self, cmd: ConfigCommand) {
        self.config = Some(cmd);
        self.config_reject = None;
    }

    pub fn config_refused(&mut self, version: u32, code: Option<RejectCode>) {
        self.config_reject = Some(ConfigReject { version, code });
    }

    /// Report and download intervals, seconds: the config's fast ones until
    /// its `fast_until` (GNSS time), else its base ones, else 60 s.
    pub fn intervals(&self, now: Instant) -> (u32, u32) {
        let Some(c) = &self.config else { return (DEFAULT_INTERVAL_S, DEFAULT_INTERVAL_S) };
        let fast = match (c.fast_report_s, c.fast_poll_s, c.fast_until) {
            (Some(r), Some(p), Some(until)) if self.collar_now(now).unwrap_or_else(Utc::now) < until => Some((r, p)),
            (Some(r), Some(p), None) => Some((r, p)),
            _ => None,
        };
        fast.unwrap_or((c.report_s, c.poll_s))
    }

    /// What the collar is, as far as its console says. Firmware 0.2 prints no
    /// caps; it has every 0.2 cap and the V0 limits.
    pub fn device(&self) -> Option<DeviceInfo> {
        let fw = self.status.as_ref().map(|s| s.fw.clone()).or_else(|| self.fw.clone())?;
        let v02 = fw_at_least(&fw, 0, 2);
        Some(DeviceInfo {
            caps: if v02 { caps::ALL.iter().map(|c| (*c).to_owned()).collect() } else { Vec::new() },
            limits: v02.then_some(CollarLimits::V0),
            config_version: self.status.as_ref().and_then(|s| s.config_version),
            config_reject: self.config_reject.clone(),
            fw: Some(fw),
            // A bench collar on LTE (or USB): no herd-network class to report.
            device_class: None,
        })
    }

    pub fn waiting(&self) -> (usize, usize, usize) {
        (self.fixes.len(), self.cues.len(), self.episodes.len())
    }

    /// The next report: everything since the last one. Put it back with
    /// [`Session::restore`] if it doesn't get through.
    pub fn take_report(&mut self) -> PositionReport {
        let slots = self.status.as_ref().map(|s| {
            s.slots
                .iter()
                .map(|&v| SlotReport { version: v, status: if Some(v) == s.active { AckStatus::Applied } else { AckStatus::Received }, effective_at: None })
                .collect()
        });
        PositionReport {
            collar_id: self.status.as_ref().map(|s| s.collar_id.clone()),
            boundary_version: self.status.as_ref().and_then(|s| s.active).or(self.last_version),
            device: self.device(),
            slots,
            fixes: std::mem::take(&mut self.fixes),
            cues: std::mem::take(&mut self.cues),
            episodes: std::mem::take(&mut self.episodes),
            battery: None,
            health: Some(Health { sats: self.sats, uptime_s: self.uptime_ms.map(|ms| ms / 1000), ..Health::default() }),
        }
    }

    /// A report that didn't get through goes back in front of what came
    /// since; past [`MAX_FIXES_PER_REPORT`] the oldest go.
    pub fn restore(&mut self, mut rep: PositionReport) {
        rep.fixes.append(&mut self.fixes);
        rep.cues.append(&mut self.cues);
        rep.episodes.append(&mut self.episodes);
        for over in [rep.fixes.len(), rep.cues.len(), rep.episodes.len()] {
            self.dropped += over.saturating_sub(MAX_FIXES_PER_REPORT) as u64;
        }
        cap(&mut rep.fixes);
        cap(&mut rep.cues);
        cap(&mut rep.episodes);
        self.fixes = rep.fixes;
        self.cues = rep.cues;
        self.episodes = rep.episodes;
    }
}

fn cap<T>(v: &mut Vec<T>) {
    let over = v.len().saturating_sub(MAX_FIXES_PER_REPORT);
    v.drain(..over);
}

/// `0.2.0` ≥ 0.2; anything unreadable is not.
pub fn fw_at_least(fw: &str, major: u32, minor: u32) -> bool {
    let mut parts = fw.split(['.', '-', '+']).map(|p| p.parse::<u32>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(a), Some(b)) => (a, b) >= (major, minor),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::{Reply, parse};

    fn feed(s: &mut Session, text: &str, now: Instant) {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let l = parse(line);
            if let Line::Reply(Reply::Status(st)) = &l.line {
                s.status = Some(st.clone());
            }
            s.on_line(&l, now);
        }
    }

    #[test]
    fn a_bench_transcript_becomes_a_report() {
        let mut s = Session::new();
        let now = Instant::now();
        feed(&mut s, include_str!("../tests/fixtures/bench-session.log"), now);
        assert_eq!(s.untimed, 0);
        let rep = s.take_report();
        let d = rep.device.as_ref().unwrap();
        assert_eq!((d.fw.as_deref(), d.caps.len(), d.limits, d.config_version), (Some("0.2.0"), 6, Some(CollarLimits::V0), None));
        assert_eq!(rep.collar_id.as_deref(), Some("col_bench"));
        assert_eq!(rep.boundary_version, Some(3));
        assert_eq!(rep.slots.as_ref().unwrap().iter().map(|s| (s.version, s.status)).collect::<Vec<_>>(), [(3, AckStatus::Applied), (4, AckStatus::Received)]);
        // Every fix keeps the collar's own time.
        assert_eq!(rep.fixes.len(), 6);
        assert_eq!(rep.fixes[0].at.to_rfc3339(), "2026-10-26T15:04:00+00:00");
        assert_eq!(rep.fixes[0].point, [-92.41, 38.12]);
        assert_eq!(rep.fixes[5].boundary_version, Some(3));
        // Two warning cues and the outside tone, then the episode that ended at the crossing.
        assert_eq!(rep.cues.iter().map(|c| (c.kind.as_ref().unwrap().as_str(), c.level)).collect::<Vec<_>>(), [("warn", 2), ("warn", 4), ("outside", 4)]);
        assert_eq!(rep.episodes.len(), 1);
        let e = &rep.episodes[0];
        assert_eq!((e.end.to_rfc3339().as_str(), e.start.to_rfc3339().as_str()), ("2026-10-26T15:04:05+00:00", "2026-10-26T15:04:03+00:00"));
        assert_eq!(rep.health.as_ref().unwrap().uptime_s, Some(125));
        rep.validate().unwrap();
        // Nothing waits after a report is taken.
        assert_eq!(s.waiting(), (0, 0, 0));
        // The staged v4 came into force on its own: the ack is owed, but this
        // bridge never delivered v4, so it has no command id for it.
        assert_eq!(s.take_acks(now), vec![Err(4)]);
    }

    #[test]
    fn fixes_without_time_are_never_stamped() {
        let mut s = Session::new();
        let now = Instant::now();
        let said = s.on_line(&parse("[00:00:05.000,000] <inf> opencollar: fix 38.1,-92.4 acc=4.0m sats=6 (no boundary)"), now);
        assert!(said.unwrap().contains("no GNSS time"));
        assert!(s.on_line(&parse("[00:00:06.000,000] <inf> opencollar: fix 38.1,-92.4 acc=4.0m sats=6 (no boundary)"), now).is_none());
        assert_eq!((s.untimed, s.take_report().fixes.len()), (2, 0));
    }

    #[test]
    fn acks_take_the_collar_time() {
        let mut s = Session::new();
        let t0 = Instant::now();
        s.on_line(&parse("[00:00:05.000,000] <inf> opencollar: fix 38.1,-92.4 acc=4.0m sats=6 (no boundary) at=2026-10-26T15:00:00Z"), t0);
        s.delivered(7, "bnd_7");
        s.boundary_reply(7, AckStatus::Applied, None, t0 + Duration::from_secs(2));
        // The log line hasn't come yet, but the carried clock is known: ready at once.
        let acks = s.take_acks(t0 + Duration::from_secs(2));
        let Ok(owed) = &acks[0] else { panic!() };
        assert_eq!((owed.ack.command_id.as_str(), owed.ack.at.to_rfc3339().as_str(), owed.host_time), ("bnd_7", "2026-10-26T15:00:02+00:00", false));

        // A rejection with the log line's own time.
        s.delivered(8, "bnd_8");
        s.boundary_reply(8, AckStatus::Rejected, Some(RejectCode::BadSig), t0);
        s.on_line(&parse("[00:00:07.000,000] <inf> opencollar: ack v8 rejected bad_sig at 2026-10-26T15:00:01Z (2 pending)"), t0);
        let Ok(owed) = s.take_acks(t0).remove(0) else { panic!() };
        assert!(owed.ack.status == AckStatus::Rejected);
        assert_eq!((owed.ack.code, owed.ack.at.to_rfc3339()), (Some(RejectCode::BadSig), "2026-10-26T15:00:01+00:00".into()));
    }

    #[test]
    fn no_gnss_time_yet_means_host_time_said_out_loud() {
        let mut s = Session::new();
        let t0 = Instant::now();
        s.delivered(2, "bnd_2");
        s.boundary_reply(2, AckStatus::Received, None, t0);
        assert!(s.take_acks(t0).is_empty(), "waits for the collar's log line");
        let acks = s.take_acks(t0 + ACK_WAIT);
        let Ok(owed) = &acks[0] else { panic!() };
        assert!(owed.host_time);
    }

    #[test]
    fn cadence_follows_the_accepted_config() {
        let mut s = Session::new();
        let now = Instant::now();
        assert_eq!(s.intervals(now), (60, 60));
        s.on_line(&parse("[00:00:05.000,000] <inf> opencollar: fix 38.1,-92.4 acc=4.0m sats=6 (no boundary) at=2026-10-26T15:00:00Z"), now);
        let until = "2026-10-26T15:10:00Z".parse().unwrap();
        s.config_accepted(ConfigCommand {
            report_s: 120,
            poll_s: 300,
            fast_report_s: Some(10),
            fast_poll_s: Some(15),
            fast_until: Some(until),
            ..ConfigCommand::default()
        });
        assert_eq!(s.intervals(now), (10, 15));
        s.on_line(&parse("[00:20:05.000,000] <inf> opencollar: fix 38.1,-92.4 acc=4.0m sats=6 (no boundary) at=2026-10-26T15:20:00Z"), now);
        assert_eq!(s.intervals(now), (120, 300));
    }

    #[test]
    fn a_report_that_failed_goes_back_first() {
        let mut s = Session::new();
        let now = Instant::now();
        s.on_line(&parse("[00:00:05.000,000] <inf> opencollar: fix 38.1,-92.4 acc=4.0m sats=6 (no boundary) at=2026-10-26T15:00:00Z"), now);
        let rep = s.take_report();
        s.on_line(&parse("[00:00:06.000,000] <inf> opencollar: fix 38.1,-92.4 acc=4.0m sats=6 (no boundary) at=2026-10-26T15:00:01Z"), now);
        s.restore(rep);
        let again = s.take_report();
        assert_eq!(again.fixes.iter().map(|f| f.at.timestamp() % 60).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn firmware_versions() {
        assert!(fw_at_least("0.2.0", 0, 2) && fw_at_least("0.3.1-rc1", 0, 2) && fw_at_least("1.0.0", 0, 2));
        assert!(!fw_at_least("0.1.9", 0, 2) && !fw_at_least("dev", 0, 2));
    }
}
