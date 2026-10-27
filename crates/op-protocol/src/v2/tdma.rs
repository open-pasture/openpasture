//! Airtime on a herd's LoRa channel: every GNSS minute is split into a
//! beacon, a downlink window, one uplink slot per collar and a join window
//! (`opencollar/protocol/v2/lora.md`). Everything is computed from the
//! collar's short address and GNSS time, so nothing about slots is ever sent.

/// A channel plan: radio settings and the shape of the minute. Values are
/// parameters, to be fixed against the radio module's FCC grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub id: u32,
    /// Channel centres, kHz. A herd uses `channels[net % len]`.
    pub channels_khz: &'static [u32],
    pub bw_khz: u32,
    pub sf: u8,
    /// Conducted power, dBm.
    pub tx_dbm: i8,
    /// Most time on one channel per 20 s, for plans that must hop (FCC
    /// 15.247 frequency hopping); `None` for digitally modulated 500 kHz
    /// channels, which don't.
    pub max_dwell_ms: Option<u32>,
    /// Downlink window, ms after the minute.
    pub downlink_ms: (u32, u32),
    /// First uplink slot, ms after the minute.
    pub uplink_ms: u32,
    pub slot_ms: u32,
    /// Uplink slots per minute.
    pub slots: u16,
    /// Join window (hellos, backlog), ms after the minute.
    pub join_ms: (u32, u32),
    /// A collar starts sending this long into its slot (clock error).
    pub guard_ms: u32,
    /// Largest uplink frame, bytes (fits a slot with guards).
    pub max_uplink: usize,
    /// Largest beacon or fragment frame, bytes.
    pub max_downlink: usize,
}

/// Plan 1: US 902–928 MHz, eight 500 kHz channels (903.0 + 1.6 n MHz) at SF8.
/// A 128-byte frame is on air about 90 ms.
pub const US915_500: Plan = Plan {
    id: 1,
    channels_khz: &[903_000, 904_600, 906_200, 907_800, 909_400, 911_000, 912_600, 914_200],
    bw_khz: 500,
    sf: 8,
    tx_dbm: 20,
    max_dwell_ms: None,
    downlink_ms: (250, 8_000),
    uplink_ms: 8_000,
    slot_ms: 160,
    slots: 300,
    join_ms: (56_000, 59_800),
    guard_ms: 20,
    max_uplink: 128,
    max_downlink: 222,
};

/// The plan with this id, if this build knows it.
pub fn plan(id: u32) -> Option<&'static Plan> {
    [&US915_500].into_iter().find(|p| p.id == id)
}

/// Where a short address talks: slot `k` within the minute, group `g`.
/// Collars sharing a slot index (one group apart) never share a minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub k: u16,
    pub g: u16,
}

/// How a net's minutes are shared out: every collar of a net MUST hold the
/// same schedule (membership keys 19, 20 and 22), so each slot of each
/// minute has exactly one owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Report cycle in minutes, 1–60.
    pub cycle: u32,
    /// Groups of `slots` short addresses the net uses, 1..=cycle. Short `s`
    /// is in group `(s − 1) div slots`; one in group ≥ `groups` never sends.
    pub groups: u32,
}

impl Schedule {
    /// From membership's `cycle_s` (default 300) and `groups` (default 1).
    pub fn new(cycle_s: Option<u32>, groups: Option<u32>) -> Option<Self> {
        let cycle = cycle_s.unwrap_or(300);
        let groups = groups.unwrap_or(1);
        (cycle % 60 == 0 && (60..=3600).contains(&cycle) && (1..=cycle / 60).contains(&groups)).then_some(Self { cycle: cycle / 60, groups })
    }

    /// Who owns slot `k` in `minute`: the group whose phase it is, or, in a
    /// spare phase (no group of that number), the group whose fast turn it
    /// is. `(group, spare)`.
    pub fn owner(&self, k: u16, minute: u32) -> (u32, bool) {
        let phase = (u64::from(minute) + u64::from(self.cycle) * 65_536 - u64::from(k)) % u64::from(self.cycle);
        let phase = phase as u32;
        if phase < self.groups { (phase, false) } else { ((phase - self.groups) % self.groups, true) }
    }
}

impl Plan {
    /// The slot of short address `short` (1–65534).
    pub fn slot(&self, short: u16) -> Option<Slot> {
        (1..=65_534).contains(&short).then(|| Slot { k: (short - 1) % self.slots, g: (short - 1) / self.slots })
    }

    /// Whether `short` sends in GNSS minute `minute` (Unix time / 60).
    ///
    /// Slot `k` of minute `m` is in phase `p = (m − k) mod cycle`. Phase `p <
    /// groups` belongs to group `p`: every collar sends once a cycle, in its
    /// own phase. The other `cycle − groups` phases are **spare**; spare
    /// phase `j` (counting from `groups`) belongs to group `j mod groups`,
    /// and a collar in a fast window also sends in its group's spare phases.
    /// So each slot of each minute has one owner, whoever is fast: a net of
    /// `slots × groups` collars never collides. With one group (≤ 300
    /// collars) a fast collar sends every minute; with `groups = cycle`
    /// there are no spare phases and fast is the normal rate ([`Plan::per_cycle`]).
    pub fn sends(&self, short: u16, minute: u32, sched: Schedule, fast: bool) -> bool {
        let Some(s) = self.slot(short) else { return false };
        if u32::from(s.g) >= sched.groups {
            return false;
        }
        let (owner, spare) = sched.owner(s.k, minute);
        owner == u32::from(s.g) && (!spare || fast)
    }

    /// How many times a cycle a collar of `group` sends: `(normal, fast)`.
    /// Spare phases go round the groups, so lower groups may get one more.
    pub fn per_cycle(sched: Schedule, group: u32) -> (u32, u32) {
        let spare = sched.cycle - sched.groups;
        let extra = if group < spare { (spare - 1 - group) / sched.groups + 1 } else { 0 };
        (1, 1 + extra)
    }

    /// The most collars a net can hold with this schedule.
    pub fn capacity(&self, sched: Schedule) -> u32 {
        u32::from(self.slots) * sched.groups
    }

    /// When slot `k` starts, ms after the minute.
    pub fn slot_start_ms(&self, k: u16) -> u32 {
        self.uplink_ms + u32::from(k) * self.slot_ms
    }

    /// The herd's channel, kHz.
    pub fn channel_khz(&self, net: u16) -> u32 {
        self.channels_khz[usize::from(net) % self.channels_khz.len()]
    }

    /// Time on air of a LoRa packet of `bytes` payload (explicit header,
    /// CRC on, coding rate 4/5, 8-symbol preamble, low data rate
    /// optimisation off), microseconds.
    pub fn airtime_us(&self, bytes: usize) -> u64 {
        let sf = i64::from(self.sf);
        let t_sym_us = (1u64 << self.sf) * 1000 / u64::from(self.bw_khz);
        let payload_bits = 8 * bytes as i64 - 4 * sf + 28 + 16;
        let n = 8 + (payload_bits.max(0) + 4 * sf - 1) / (4 * sf) * 5;
        (t_sym_us * 49 / 4) + t_sym_us * n as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every collar of a full net fast at once, or none, or any mix: no two
    /// ever share a slot in a minute. (Astra 01 F01: under the old rule,
    /// short 1 fast and short 301 shared slot 0 in minute 1, and a fast group
    /// 0 overlapped 1 200 slot-minutes in five minutes of a 1 500-collar net.)
    #[test]
    fn slots_never_collide_fast_or_not() {
        let p = &US915_500;
        for cycle in [1u32, 2, 5, 15] {
            for groups in 1..=cycle.min(6) {
                let sched = Schedule::new(Some(cycle * 60), Some(groups)).unwrap();
                let herd = p.capacity(sched) as u16;
                // Pseudo-random mixes of fast and normal collars.
                for mix in 0u32..4 {
                    let fast = |short: u16| match mix {
                        0 => false,
                        1 => true,
                        2 => short % 3 == 0,
                        _ => u32::from(short).wrapping_mul(2_654_435_761) >> 31 == 1,
                    };
                    for minute in 29_841_840..29_841_840 + cycle * 2 + 3 {
                        let mut used = vec![0u16; usize::from(p.slots)];
                        for short in 1..=herd {
                            if p.sends(short, minute, sched, fast(short)) {
                                let k = usize::from(p.slot(short).unwrap().k);
                                assert_eq!(used[k], 0, "cycle {cycle} groups {groups} mix {mix} minute {minute}: {short} and {} in slot {k}", used[k]);
                                used[k] = short;
                            }
                        }
                    }
                }
                for short in (1..=herd).step_by(37) {
                    let (normal, fast) = Plan::per_cycle(sched, u32::from(p.slot(short).unwrap().g));
                    let count = |f| (0..cycle).filter(|m| p.sends(short, 29_841_840 + m, sched, f)).count() as u32;
                    assert_eq!((count(false), count(true)), (normal, fast), "cycle {cycle} groups {groups} short {short}");
                }
                assert!(!p.sends(herd + 1, 29_841_840, sched, true), "a short past the net's groups never sends");
            }
        }
        // The review's case: short 1 fast and short 301 normal, minute 1.
        let two = Schedule::new(Some(300), Some(2)).unwrap();
        assert!(!(p.sends(1, 1, two, true) && p.sends(301, 1, two, false)));
        assert_eq!(Plan::per_cycle(Schedule::new(Some(300), None).unwrap(), 0), (1, 5), "≤ 300 collars: fast is every minute");
        assert_eq!(Plan::per_cycle(Schedule::new(Some(300), Some(5)).unwrap(), 4), (1, 1), "a full net has no spare minutes");
        assert_eq!(Plan::per_cycle(Schedule::new(Some(300), Some(2)).unwrap(), 1), (1, 2), "600 collars: group 1 sends twice a cycle when fast");
    }

    #[test]
    fn a_full_frame_fits_its_slot() {
        let p = &US915_500;
        assert!(p.airtime_us(p.max_uplink) / 1000 + 2 * u64::from(p.guard_ms) <= u64::from(p.slot_ms));
        // 20 bytes at SF7/125 kHz is about 57 ms (the LoRa plan's figure).
        let sf7 = Plan { sf: 7, bw_khz: 125, ..*p };
        assert_eq!(sf7.airtime_us(20) / 1000, 56);
        assert!(p.slot_start_ms(p.slots - 1) + p.slot_ms <= p.join_ms.0);
    }
}
