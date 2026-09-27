//! Slots: the boundaries a collar holds (protocol v1, §3.7). This is the
//! reference logic the firmware's `slots.c` and collar-sim follow, and
//! [`split_by_activation`] is the same rule for the server's view.
//!
//! - The boundary in effect at time t is the highest version whose activation
//!   time (`effective_at`, else the time it was received) is at or before t.
//!   A staged version is **dead** when a higher version activates at or
//!   before it.
//! - Only versions above every version held are accepted (anti-replay). A
//!   version equal to one held is re-acked with that version's current
//!   status; a lower one is `stale`.
//! - **Immediate** (no `effective_at`, or it has passed on the collar's GNSS
//!   clock): applied at once, every lower version dropped, acked `applied`.
//! - **Future**: stored as staged and acked `received`, after dead slots are
//!   pruned. No free slot, or not enough slot bytes, is `slots_full`.
//! - **Tick** on every fix with that fix's GNSS time, the only clock for
//!   activation: the highest due version applies, lower ones go (superseded
//!   staged slots get no ack), and it is acked `applied` at the fix's time.
//! - **Boot** keeps every slot (they are in flash) and enforces the active
//!   one at once, but forgets the clock: staged slots wait for the first fix.
//!
//! The order of checks on insert: ids (`bad_json`), `wrong_herd`,
//! `wrong_collar`, re-ack or `stale`, the shape rules, `slots_full`.

use chrono::{DateTime, Utc};
use op_geo::GeofenceConfig;

use crate::{Ack, AckStatus, BoundaryCommand, CollarLimits, RejectCode, SlotReport};

/// One held boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub cmd: BoundaryCommand,
    /// GNSS time it was received, if the collar had one.
    pub received_at: Option<DateTime<Utc>>,
    /// GNSS time it was applied (the active slot), if the collar had one.
    pub applied_at: Option<DateTime<Utc>>,
    /// Flash bytes of its record.
    pub bytes: usize,
}

impl Slot {
    /// `effective_at`, else the time received. `None` for an immediate
    /// boundary received without a clock (it is the active one).
    fn activation(&self) -> Option<DateTime<Utc>> {
        self.cmd.effective_at.or(self.received_at)
    }
}

/// What to acknowledge. `at` is the GNSS time, when the collar had one; the
/// caller fills a missing one (firmware: the modem's network time).
#[derive(Debug, Clone, PartialEq)]
pub struct SlotAck {
    pub command_id: String,
    pub version: u32,
    pub status: AckStatus,
    pub code: Option<RejectCode>,
    pub at: Option<DateTime<Utc>>,
}

impl SlotAck {
    fn of(cmd: &BoundaryCommand, status: AckStatus, code: Option<RejectCode>, at: Option<DateTime<Utc>>) -> Self {
        Self { command_id: cmd.command_id.clone(), version: cmd.version, status, code, at }
    }

    /// The wire ack, with `fallback` when there was no GNSS time.
    pub fn to_ack(&self, fallback: DateTime<Utc>) -> Ack {
        Ack {
            command_id: self.command_id.clone(),
            version: self.version,
            status: self.status,
            code: self.code,
            reason: self.code.map(|c| c.message().to_string()),
            at: self.at.unwrap_or(fallback),
            ..Default::default()
        }
    }
}

/// The boundaries one collar holds.
#[derive(Debug, Clone)]
pub struct SlotStore {
    limits: CollarLimits,
    herd_id: Option<String>,
    collar_id: Option<String>,
    defaults: GeofenceConfig,
    active: Option<Slot>,
    /// Ascending version.
    staged: Vec<Slot>,
    /// Last GNSS time seen since boot.
    clock: Option<DateTime<Utc>>,
}

impl SlotStore {
    /// An empty store for a collar with `limits`, in `herd_id`, with id
    /// `collar_id` (`None` skips that check). Missing margins in commands
    /// take the firmware defaults (5 m, 1 m).
    pub fn new(limits: CollarLimits, herd_id: Option<String>, collar_id: Option<String>) -> Self {
        Self { limits, herd_id, collar_id, defaults: GeofenceConfig::default(), active: None, staged: Vec::new(), clock: None }
    }

    pub fn limits(&self) -> &CollarLimits {
        &self.limits
    }

    /// A signed config moved the collar to another herd.
    pub fn set_herd(&mut self, herd_id: Option<String>) {
        self.herd_id = herd_id;
    }

    /// Offer a command whose signature verified. `now` is the collar's GNSS
    /// time if it has a fix now; without one, the last fix's time since boot
    /// decides whether a staged boundary is already due.
    pub fn insert(&mut self, cmd: BoundaryCommand, now: Option<DateTime<Utc>>) -> SlotAck {
        if let Some(t) = now {
            self.clock = Some(t);
        }
        let reject = |cmd: &BoundaryCommand, code| SlotAck::of(cmd, AckStatus::Rejected, Some(code), now);
        if let Err(code) = cmd.check_ids() {
            return reject(&cmd, code);
        }
        if let (Some(theirs), Some(ours)) = (&cmd.herd_id, &self.herd_id)
            && theirs != ours
        {
            return reject(&cmd, RejectCode::WrongHerd);
        }
        if let (Some(theirs), Some(ours)) = (&cmd.collar_id, &self.collar_id)
            && theirs != ours
        {
            return reject(&cmd, RejectCode::WrongCollar);
        }
        if let Some(a) = self.active.as_ref().filter(|a| a.cmd.version == cmd.version) {
            return SlotAck::of(&a.cmd, AckStatus::Applied, None, a.applied_at);
        }
        if let Some(s) = self.staged.iter().find(|s| s.cmd.version == cmd.version) {
            return SlotAck::of(&s.cmd, AckStatus::Received, None, s.received_at);
        }
        if self.held() > 0 && cmd.version < self.have() {
            return reject(&cmd, RejectCode::Stale);
        }
        if let Err(code) = cmd.check_shape(&self.limits, &self.defaults) {
            return reject(&cmd, code);
        }
        let bytes = cmd.record_bytes();
        let future = cmd.effective_at.filter(|t| self.clock.is_none_or(|c| *t > c));
        let Some(at) = future else {
            if self.limits.slot_bytes > 0 && bytes > self.limits.slot_bytes {
                return reject(&cmd, RejectCode::SlotsFull);
            }
            // Every held version is lower.
            self.staged.clear();
            let ack = SlotAck::of(&cmd, AckStatus::Applied, None, now);
            self.active = Some(Slot { cmd, received_at: now, applied_at: now, bytes });
            return ack;
        };
        let slot = Slot { cmd, received_at: now, applied_at: None, bytes };
        // The new version is the highest, so any staged one activating at or after it is dead.
        let alive: Vec<Slot> = self.staged.iter().filter(|s| s.activation().is_some_and(|a| a < at)).cloned().collect();
        let count = usize::from(self.active.is_some()) + alive.len() + 1;
        let used = self.active.as_ref().map_or(0, |a| a.bytes) + alive.iter().map(|s| s.bytes).sum::<usize>() + bytes;
        if count > self.limits.slots || (self.limits.slot_bytes > 0 && used > self.limits.slot_bytes) {
            return reject(&slot.cmd, RejectCode::SlotsFull);
        }
        let ack = SlotAck::of(&slot.cmd, AckStatus::Received, None, now);
        self.staged = alive;
        self.staged.push(slot);
        ack
    }

    /// A fix at GNSS time `now`: apply the highest staged version that is due.
    pub fn tick(&mut self, now: DateTime<Utc>) -> Option<SlotAck> {
        self.clock = Some(now);
        let k = self.staged.iter().rposition(|s| s.activation().is_some_and(|a| a <= now))?;
        let mut slot = self.staged.remove(k);
        self.staged.drain(..k);
        slot.applied_at = Some(now);
        let ack = SlotAck::of(&slot.cmd, AckStatus::Applied, None, Some(now));
        self.active = Some(slot);
        Some(ack)
    }

    /// Reboot: slots stay, the clock is gone until the next fix.
    pub fn boot(&mut self) {
        self.clock = None;
    }

    /// Provisioning wipes every slot.
    pub fn wipe(&mut self) {
        self.active = None;
        self.staged.clear();
    }

    /// The boundary being enforced.
    pub fn active(&self) -> Option<&Slot> {
        self.active.as_ref()
    }

    /// Staged boundaries, ascending version.
    pub fn staged(&self) -> &[Slot] {
        &self.staged
    }

    /// Slots held, the active one included.
    pub fn held(&self) -> usize {
        usize::from(self.active.is_some()) + self.staged.len()
    }

    /// Highest version held, applied or staged; 0 with none (`have=` on the download).
    pub fn have(&self) -> u32 {
        self.staged.iter().map(|s| s.cmd.version).chain(self.active.as_ref().map(|a| a.cmd.version)).max().unwrap_or(0)
    }

    /// Slots left (`free=`).
    pub fn free(&self) -> usize {
        self.limits.slots.saturating_sub(self.held())
    }

    /// Slot bytes left (`free_bytes=`); `None` when the limits don't say.
    pub fn free_bytes(&self) -> Option<usize> {
        let used = self.active.as_ref().map_or(0, |a| a.bytes) + self.staged.iter().map(|s| s.bytes).sum::<usize>();
        (self.limits.slot_bytes > 0).then(|| self.limits.slot_bytes.saturating_sub(used))
    }

    /// The report's `slots`: everything held, ascending version.
    pub fn report(&self) -> Vec<SlotReport> {
        let active = self.active.iter().map(|a| SlotReport { version: a.cmd.version, status: AckStatus::Applied, effective_at: a.cmd.effective_at });
        let staged = self.staged.iter().map(|s| SlotReport { version: s.cmd.version, status: AckStatus::Received, effective_at: s.cmd.effective_at });
        let mut out: Vec<SlotReport> = active.chain(staged).collect();
        out.sort_by_key(|s| s.version);
        out
    }
}

/// A boundary set split at one moment.
#[derive(Debug, Clone, PartialEq)]
pub struct Split<T> {
    /// In effect now.
    pub active: Option<T>,
    /// Alive and not yet in effect, ascending version.
    pub staged: Vec<T>,
}

/// The collars' rule for a set of boundaries at `now`. `key` gives each
/// item's version and activation time (`effective_at`, else when it was
/// created). `active` is the highest version activating at or before `now`;
/// `staged` the later ones that are still alive (no higher version activates
/// at or before them), ascending. Everything else is dead or superseded.
pub fn split_by_activation<T>(items: impl IntoIterator<Item = T>, now: DateTime<Utc>, key: impl Fn(&T) -> (u32, DateTime<Utc>)) -> Split<T> {
    let mut items: Vec<T> = items.into_iter().collect();
    items.sort_by_key(|t| key(t).0);
    let active_at = items.iter().rposition(|t| key(t).1 <= now);
    let mut rest = match active_at {
        Some(k) => items.split_off(k + 1),
        None => std::mem::take(&mut items),
    };
    let active = active_at.and_then(|_| items.pop());
    // Walk down from the highest version, keeping the earliest activation above.
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut alive = Vec::with_capacity(rest.len());
    while let Some(t) = rest.pop() {
        let at = key(&t).1;
        if earliest.is_none_or(|e| at < e) {
            earliest = Some(at);
            alive.push(t);
        }
    }
    alive.reverse();
    Split { active, staged: alive }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use op_geo::Polygon;

    fn t0() -> DateTime<Utc> {
        crate::wire_time::parse("2026-09-27T12:00:00Z").unwrap()
    }

    fn cmd(version: u32, at: Option<i64>) -> BoundaryCommand {
        let poly = Polygon::from_ring(vec![[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336]]);
        let mut c = BoundaryCommand::from_shape(format!("bnd_{version}"), version, &poly, at.map(|s| t0() + Duration::seconds(s))).unwrap();
        c.herd_id = Some("herd_1".into());
        c
    }

    fn store() -> SlotStore {
        SlotStore::new(CollarLimits::V0, Some("herd_1".into()), Some("col_a".into()))
    }

    #[test]
    fn immediate_then_staged() {
        let mut s = store();
        let a = s.insert(cmd(1, None), Some(t0()));
        assert_eq!((a.status, a.at), (AckStatus::Applied, Some(t0())));
        let a = s.insert(cmd(2, Some(60)), Some(t0()));
        assert_eq!(a.status, AckStatus::Received);
        assert_eq!((s.have(), s.held(), s.free()), (2, 2, 14));
        assert_eq!(s.free_bytes(), Some(24_576 - 2 * (192 + 32)));
        assert!(s.tick(t0() + Duration::seconds(59)).is_none());
        let a = s.tick(t0() + Duration::seconds(61)).unwrap();
        assert_eq!((a.version, a.at), (2, Some(t0() + Duration::seconds(61))));
        assert_eq!((s.active().unwrap().cmd.version, s.held()), (2, 1));
    }

    #[test]
    fn reject_order() {
        let mut s = store();
        s.insert(cmd(5, None), Some(t0()));
        let mut other_herd = cmd(3, None);
        other_herd.herd_id = Some("herd_2".into());
        assert_eq!(s.insert(other_herd, None).code, Some(RejectCode::WrongHerd), "herd before stale");
        let mut other_collar = cmd(6, None);
        other_collar.collar_id = Some("col_b".into());
        assert_eq!(s.insert(other_collar, None).code, Some(RejectCode::WrongCollar));
        let mut bad = cmd(4, None);
        bad.boundary.truncate(2);
        assert_eq!(s.insert(bad, None).code, Some(RejectCode::Stale), "stale before shape");
        let mut bad = cmd(6, None);
        bad.boundary.truncate(2);
        assert_eq!(s.insert(bad, None).code, Some(RejectCode::TooFewVertices));
        let mut long = cmd(6, None);
        long.command_id = "x".repeat(65);
        assert_eq!(s.insert(long, None).code, Some(RejectCode::BadJson));
        assert_eq!(s.active().unwrap().cmd.version, 5, "rejects change nothing");
    }

    #[test]
    fn to_ack_fills_the_time_and_reason() {
        let mut s = store();
        s.insert(cmd(5, None), Some(t0()));
        let a = s.insert(cmd(4, None), None);
        let wire = a.to_ack(t0());
        assert_eq!((wire.status, wire.code, wire.at), (AckStatus::Rejected, Some(RejectCode::Stale), t0()));
        assert!(wire.reason.is_some());
    }

    #[test]
    fn split_matches_the_collar_rule() {
        let at = |s: i64| t0() + Duration::seconds(s);
        // (version, activation)
        let set = vec![(1, at(-100)), (2, at(-50)), (3, at(200)), (4, at(100)), (5, at(300)), (6, at(300))];
        let s = split_by_activation(set.clone(), t0(), |x| (x.0, x.1));
        assert_eq!(s.active, Some((2, at(-50))));
        // 3 is dead (4 activates before it); 5 is dead (6 activates with it).
        assert_eq!(s.staged, vec![(4, at(100)), (6, at(300))]);
        let s = split_by_activation(set, at(150), |x| (x.0, x.1));
        assert_eq!((s.active, s.staged), (Some((4, at(100))), vec![(6, at(300))]));
        let s = split_by_activation(vec![(7, at(10))], t0(), |x: &(u32, DateTime<Utc>)| (x.0, x.1));
        assert_eq!((s.active, s.staged), (None, vec![(7, at(10))]));
        let empty: Split<(u32, DateTime<Utc>)> = split_by_activation(vec![], t0(), |x: &(u32, DateTime<Utc>)| (x.0, x.1));
        assert_eq!((empty.active, empty.staged.len()), (None, 0));
    }
}
