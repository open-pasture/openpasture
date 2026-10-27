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

impl Plan {
    /// The slot of short address `short` (1–65534).
    pub fn slot(&self, short: u16) -> Option<Slot> {
        (1..=65_534).contains(&short).then(|| Slot { k: (short - 1) % self.slots, g: (short - 1) / self.slots })
    }

    /// Whether `short` sends in GNSS minute `minute` (Unix time / 60) with a
    /// report cycle of `cycle_min` minutes. Normally once a cycle, in minute
    /// `(k + g) mod cycle`, which spreads a herd across the cycle and keeps
    /// collars of one slot index apart; in a fast window, group 0 sends every
    /// minute. A herd can hold `slots × cycle` collars, and `slots` of them
    /// can be fast at once.
    pub fn sends(&self, short: u16, minute: u32, cycle_min: u32, fast: bool) -> bool {
        let Some(s) = self.slot(short) else { return false };
        let cycle = cycle_min.max(1);
        if u32::from(s.g) >= cycle {
            return false;
        }
        if fast && s.g == 0 {
            return true;
        }
        minute % cycle == (u32::from(s.k) + u32::from(s.g)) % cycle
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

    #[test]
    fn slots_never_collide() {
        let p = &US915_500;
        for cycle in [1, 5, 15] {
            let herd = usize::from(p.slots) * cycle as usize;
            for minute in 0..cycle * 2 {
                let mut used = vec![false; usize::from(p.slots)];
                for short in 1..=herd as u16 {
                    if p.sends(short, minute, cycle, false) {
                        let k = usize::from(p.slot(short).unwrap().k);
                        assert!(!used[k], "cycle {cycle} minute {minute} slot {k}");
                        used[k] = true;
                    }
                }
            }
            // Everyone sends once per cycle.
            for short in 1..=herd as u16 {
                assert_eq!((0..cycle).filter(|m| p.sends(short, *m, cycle, false)).count(), 1);
            }
        }
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
