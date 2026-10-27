//! Counters that never repeat (`messages.md` §7.1), and the server's replay
//! window.
//!
//! A collar keeps three lifetime counters: `seq` (records), `fcnt` (frames:
//! the AES-CCM nonce) and the batch counter (anchors, direct collars). Each
//! counts from 0 for the life of the device and never goes back: not on a
//! reboot or power cut, a release, a factory reset, a new claim, a handover
//! or a firmware update or rollback. That makes `(device_id, fcnt)` unique
//! for the life of the device key, so no nonce repeats under any frame key
//! (RFC 3610 §2.1), and `(device_id, seq)` a record's identity forever.
//!
//! **Write-ahead reservation.** A counter only hands out values below a
//! reservation that is already durable. When it runs out it first writes a
//! new reservation [`BLOCK`] higher, and only then continues. After a
//! reboot it resumes at the durable reservation, skipping whatever of the
//! last block went unused. A power cut at any instant can only skip values,
//! never repeat one. ([`Counter`]; [`Journal`] is the two-copy flash record
//! that makes each reservation write atomic.)

/// Values reserved at a time. Bigger means fewer flash writes and more
/// values skipped per reboot.
pub const BLOCK: u32 = 64;

/// A counter that never repeats a value, as long as `persist` is durable
/// before it returns `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counter {
    next: u32,
    /// Values below this are reserved durably.
    reserved: u32,
}

impl Counter {
    /// Resume after a boot from the durable reservation (`None`: a new
    /// device, from 0).
    pub fn boot(durable: Option<u32>) -> Self {
        let r = durable.unwrap_or(0);
        Self { next: r, reserved: r }
    }

    /// The next value. When the block is used up, `persist` must durably
    /// store the new reservation first; if it can't, nothing is handed out.
    /// `None` too when the counter is exhausted (it never wraps: the collar
    /// stops sealing frames until a new device key, `messages.md` §7.1).
    pub fn take(&mut self, mut persist: impl FnMut(u32) -> bool) -> Option<u32> {
        if self.next == self.reserved {
            let r = self.reserved.checked_add(BLOCK)?;
            if !persist(r) {
                return None;
            }
            self.reserved = r;
        }
        let v = self.next;
        self.next += 1;
        Some(v)
    }

    pub fn peek(&self) -> u32 {
        self.next
    }
}

/// The flash record of one counter's reservation: two copies, each `[gen
/// u32, value u32, crc32 u32]`. A write goes to the copy with the older
/// generation; a read takes the valid copy with the newer one. A write cut
/// short leaves the other copy, the previous reservation, intact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Journal {
    pub copies: [[u8; 12]; 2],
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for b in data {
        c ^= u32::from(*b);
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 };
        }
    }
    !c
}

impl Journal {
    fn copy(&self, i: usize) -> Option<(u32, u32)> {
        let c = &self.copies[i];
        let g = u32::from_le_bytes(c[0..4].try_into().unwrap());
        let v = u32::from_le_bytes(c[4..8].try_into().unwrap());
        let crc = u32::from_le_bytes(c[8..12].try_into().unwrap());
        (crc == crc32(&c[..8]) && g != 0).then_some((g, v))
    }

    /// The durable reservation, if any.
    pub fn read(&self) -> Option<u32> {
        [self.copy(0), self.copy(1)].into_iter().flatten().max_by_key(|(g, _)| *g).map(|(_, v)| v)
    }

    /// The bytes to write and where, for a new reservation `value`.
    pub fn next_write(&self, value: u32) -> (usize, [u8; 12]) {
        let (g0, g1) = (self.copy(0).map_or(0, |c| c.0), self.copy(1).map_or(0, |c| c.0));
        let (slot, gen_) = if g0 <= g1 { (0, g1 + 1) } else { (1, g0 + 1) };
        let mut c = [0u8; 12];
        c[0..4].copy_from_slice(&gen_.to_le_bytes());
        c[4..8].copy_from_slice(&value.to_le_bytes());
        let crc = crc32(&c[..8]);
        c[8..12].copy_from_slice(&crc.to_le_bytes());
        (slot, c)
    }
}

/// How many frame counters below the highest one a server still accepts
/// (frames come late by another anchor or from a backlog).
pub const REPLAY_WINDOW: u32 = 128;

/// A server's replay state for one device's frames (`server.md` §7): it
/// accepts a frame counter it hasn't seen, above the highest one or within
/// [`REPLAY_WINDOW`] below it. Kept with the device for as long as the
/// server keeps the device; a restore from backup loses at most what came
/// after the backup, and records are still deduplicated by `(device, seq)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayWindow {
    pub high: Option<u32>,
    /// Bit i set: `high − i` seen.
    pub seen: u128,
}

impl ReplayWindow {
    /// Whether to accept a frame with this counter (and remember it). Call
    /// only once the frame has authenticated.
    pub fn accept(&mut self, fcnt: u32) -> bool {
        let Some(high) = self.high else {
            *self = Self { high: Some(fcnt), seen: 1 };
            return true;
        };
        if fcnt > high {
            let shift = fcnt - high;
            self.seen = if shift >= REPLAY_WINDOW { 0 } else { self.seen << shift };
            self.seen |= 1;
            self.high = Some(fcnt);
            return true;
        }
        let back = high - fcnt;
        if back >= REPLAY_WINDOW || self.seen & (1 << back) != 0 {
            return false;
        }
        self.seen |= 1 << back;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device whose flash is a [`Journal`], power-cut at a given write.
    struct Device {
        flash: Journal,
        writes: usize,
        cut_at: Option<(usize, usize)>,
    }

    impl Device {
        /// Persist a reservation; a cut tears the write after `bytes` bytes
        /// and the device dies (returns false).
        fn persist(&mut self, value: u32) -> bool {
            let (slot, data) = self.flash.next_write(value);
            self.writes += 1;
            if let Some((w, bytes)) = self.cut_at
                && w == self.writes
            {
                self.flash.copies[slot][..bytes].copy_from_slice(&data[..bytes]);
                return false;
            }
            self.flash.copies[slot] = data;
            true
        }
    }

    /// Astra 01 F05: under the old rule (persist a high-water mark at
    /// least every 64 frames, resume 64 above it after a reboot) a collar
    /// that persisted 128 resumes at 192, sends it, loses power before its
    /// next write, and sends 192 again. Kept as the counter-example.
    #[test]
    fn the_old_rule_repeats_a_nonce() {
        let mut persisted = 128u32;
        let mut sent = Vec::new();
        for _boot in 0..2 {
            let mut next = persisted + 64;
            let v = next;
            sent.push(v);
            next += 1;
            // It would write `v` now (a multiple of 64), but the power goes first.
            let _ = next;
            persisted = persisted.max(128);
        }
        assert_eq!(sent, vec![192, 192], "the literal rule reuses 192");
        // The reservation rule at the same place: nothing at or above 192 is
        // sent until 256 is durable, so the cut costs a frame, not a nonce.
        let mut flash = 192u32;
        let mut c = Counter::boot(Some(flash));
        assert_eq!(c.take(|_| false), None, "the power goes during the write: nothing sent");
        let mut c = Counter::boot(Some(flash));
        assert_eq!(
            c.take(|v| {
                flash = v;
                true
            }),
            Some(192),
            "192 goes out once, after 256 is durable"
        );
        assert_eq!(flash, 256);
    }

    /// Every power cut, at every byte of every reservation write, over a
    /// run of reboots: no value is ever handed out twice.
    #[test]
    fn no_power_cut_repeats_a_value() {
        for cut_write in 1..=4 {
            for cut_bytes in 0..=12 {
                let mut dev = Device { flash: Journal::default(), writes: 0, cut_at: Some((cut_write, cut_bytes)) };
                let mut used = std::collections::BTreeSet::new();
                for boot in 0..6 {
                    let mut c = Counter::boot(dev.flash.read());
                    // Use a different number of values each boot.
                    for _ in 0..(boot * 37 + 5) {
                        match c.take(|v| dev.persist(v)) {
                            Some(v) => assert!(used.insert(v), "cut at write {cut_write} byte {cut_bytes}: {v} used twice"),
                            None => break, // the device died mid-write: reboot
                        }
                    }
                }
                assert!(!used.is_empty());
            }
        }
    }

    #[test]
    fn a_counter_never_wraps() {
        let mut c = Counter::boot(Some(u32::MAX - 1));
        assert_eq!(c.take(|_| true), None, "no room for another block");
        let mut c = Counter::boot(Some(u32::MAX - BLOCK));
        assert_eq!(c.take(|_| true), Some(u32::MAX - BLOCK));
    }

    /// Astra 01 F05: the codec opened the same frame twice. A server keeps
    /// this window per device and drops the second.
    #[test]
    fn replays_and_late_frames() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(100));
        assert!(!w.accept(100), "the same frame again");
        assert!(w.accept(90), "late, within the window");
        assert!(!w.accept(90));
        assert!(w.accept(300));
        assert!(!w.accept(100), "too far behind now");
        assert!(w.accept(299) && w.accept(173));
        assert!(!w.accept(172), "128 below the highest");
    }
}
