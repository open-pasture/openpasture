//! Splitting a signed message into LoRa fragments and putting it back
//! together. A message is named by its id: the first 4 bytes of SHA-256 of
//! the whole message, so a collar can tell a complete message from a
//! corrupted one before it checks the signature (the id is not
//! authentication: the signature is).
//!
//! Everything a receiver holds is bounded before anything is allocated
//! (`lora.md` §7.1): a command is at most [`MAX_COMMAND_FRAGMENTS`] fragments
//! and [`MAX_COMMAND_BYTES`] bytes, a collar assembles at most
//! [`MAX_LIVE`] messages at once, and an assembly with no new fragment for
//! [`EXPIRE_S`] is dropped. Firmware images (type 128) are not assembled in
//! memory: they stream to flash, against the size their signed manifest gave.

use std::collections::BTreeMap;

use super::command::MAX_COMMAND_BYTES;
use super::crypto::sha256;
use super::frame::FragmentFrame;

/// Most fragments of one command or document.
pub const MAX_COMMAND_FRAGMENTS: u16 = 64;
/// Most messages a collar assembles at once.
pub const MAX_LIVE: usize = 4;
/// An assembly that gets no new fragment for this long is dropped, seconds.
pub const EXPIRE_S: u64 = 1800;

/// The id of a message: the first 4 bytes of its SHA-256, big-endian.
pub fn message_id(message: &[u8]) -> u32 {
    u32::from_be_bytes(sha256(&[message])[..4].try_into().unwrap())
}

/// The fragments of `message`, each carrying at most `max_data` bytes.
pub fn split(message: &[u8], max_data: usize, net: u16, dst: u16) -> Vec<FragmentFrame> {
    assert!(max_data > 0 && max_data <= FragmentFrame::MAX_DATA && !message.is_empty());
    let count = message.len().div_ceil(max_data);
    assert!(count <= usize::from(u16::MAX), "message too large to fragment");
    let msg = message_id(message);
    message.chunks(max_data).enumerate().map(|(i, data)| FragmentFrame { net, dst, msg, idx: i as u16, count: count as u16, data: data.to_vec() }).collect()
}

/// A message being received.
#[derive(Debug, Clone)]
pub struct Reassembly {
    pub msg: u32,
    parts: Vec<Option<Vec<u8>>>,
    held: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assembled {
    /// Still missing fragments.
    Partial,
    /// Every fragment is in and the id matches.
    Complete,
    /// Every fragment is in but the id doesn't match, or the fragments add
    /// up to more than a command can be: start again.
    Corrupt,
}

impl Reassembly {
    /// Start assembling message `msg` of `count` fragments. `None` (and
    /// nothing allocated) when `count` is 0 or more than a command can be.
    pub fn new(msg: u32, count: u16) -> Option<Self> {
        (1..=MAX_COMMAND_FRAGMENTS).contains(&count).then(|| Self { msg, parts: vec![None; usize::from(count)], held: 0 })
    }

    /// Add a fragment of this message. Fragments of other messages, out of
    /// range, or claiming another count are ignored.
    pub fn add(&mut self, f: &FragmentFrame) -> Assembled {
        let fits = f.msg == self.msg && usize::from(f.count) == self.parts.len() && usize::from(f.idx) < self.parts.len() && !f.data.is_empty();
        if fits {
            let slot = &mut self.parts[usize::from(f.idx)];
            self.held = self.held - slot.as_ref().map_or(0, Vec::len) + f.data.len();
            *slot = Some(f.data.clone());
            if self.held > MAX_COMMAND_BYTES {
                self.clear();
                return Assembled::Corrupt;
            }
        }
        if self.parts.iter().any(Option::is_none) {
            return Assembled::Partial;
        }
        if message_id(&self.concat()) == self.msg {
            Assembled::Complete
        } else {
            self.clear();
            Assembled::Corrupt
        }
    }

    fn clear(&mut self) {
        self.parts.iter_mut().for_each(|p| *p = None);
        self.held = 0;
    }

    fn concat(&self) -> Vec<u8> {
        self.parts.iter().flatten().flatten().copied().collect()
    }

    /// The message, once complete.
    pub fn message(&self) -> Option<Vec<u8>> {
        (self.parts.iter().all(Option::is_some) && message_id(&self.concat()) == self.msg).then(|| self.concat())
    }

    /// The `need` of a link section: the first missing fragment and a
    /// bitmap from it (bit i, LSB first, set = fragment `from + i` missing),
    /// at most `max_bytes` long. `None` when nothing is missing.
    pub fn need(&self, max_bytes: usize) -> Option<(u16, Vec<u8>)> {
        let from = self.parts.iter().position(Option::is_none)?;
        let mut bits = vec![0u8; max_bytes.max(1)];
        for (i, p) in self.parts.iter().enumerate().skip(from).take(bits.len() * 8) {
            if p.is_none() {
                let k = i - from;
                bits[k / 8] |= 1 << (k % 8);
            }
        }
        while bits.last() == Some(&0) {
            bits.pop();
        }
        Some((from as u16, bits))
    }
}

/// The fragment indexes a `need` asks for.
pub fn wanted(from: u16, bits: &[u8]) -> Vec<u16> {
    (0..bits.len() * 8).filter(|k| bits[k / 8] & (1 << (k % 8)) != 0).filter_map(|k| from.checked_add(k as u16)).collect()
}

/// A collar's messages in assembly: at most [`MAX_LIVE`], each dropped
/// [`EXPIRE_S`] after its last new fragment. A new message when all are in
/// use takes the place of the one that has waited longest.
#[derive(Debug, Clone, Default)]
pub struct Assemblies {
    live: BTreeMap<u32, (Reassembly, u64)>,
}

impl Assemblies {
    /// Take a fragment heard at `now` (seconds); the whole message when it
    /// completes. Fragments with an unacceptable count are dropped unseen.
    pub fn add(&mut self, f: &FragmentFrame, now: u64) -> Option<Vec<u8>> {
        self.live.retain(|_, (_, at)| now.saturating_sub(*at) < EXPIRE_S);
        if !self.live.contains_key(&f.msg) {
            let r = Reassembly::new(f.msg, f.count)?;
            if self.live.len() >= MAX_LIVE {
                let oldest = self.live.iter().min_by_key(|(_, (_, at))| *at).map(|(m, _)| *m)?;
                self.live.remove(&oldest);
            }
            self.live.insert(f.msg, (r, now));
        }
        let (r, at) = self.live.get_mut(&f.msg)?;
        *at = now;
        match r.add(f) {
            Assembled::Complete => {
                let m = r.message();
                self.live.remove(&f.msg);
                m
            }
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_reassemble_with_losses() {
        let msg: Vec<u8> = (0..500u16).map(|i| (i * 7) as u8).collect();
        let frags = split(&msg, 200, 1, 0xffff);
        assert_eq!(frags.len(), 3);
        let mut r = Reassembly::new(frags[0].msg, 3).unwrap();
        assert_eq!(r.add(&frags[2]), Assembled::Partial);
        assert_eq!(r.need(8), Some((0, vec![0b011])));
        assert_eq!(wanted(0, &[0b011]), vec![0, 1]);
        assert_eq!(r.add(&frags[0]), Assembled::Partial);
        assert_eq!(r.need(8), Some((1, vec![0b1])));
        assert_eq!(r.add(&frags[1]), Assembled::Complete);
        assert_eq!(r.message().unwrap(), msg);
        assert_eq!(r.need(8), None);
    }

    #[test]
    fn a_bad_fragment_is_caught_by_the_id() {
        let msg = vec![1u8; 300];
        let mut frags = split(&msg, 200, 1, 2);
        frags[1].data[0] ^= 0xff;
        let mut r = Reassembly::new(frags[0].msg, 2).unwrap();
        r.add(&frags[0]);
        assert_eq!(r.add(&frags[1]), Assembled::Corrupt);
        assert_eq!(r.need(8), Some((0, vec![0b11])), "starts again");
    }

    /// Astra 01 F11: an unauthenticated count of 65 535 allocated a 1.5 MB
    /// table before anything was checked.
    #[test]
    fn counts_and_sizes_are_bounded_before_allocation() {
        assert!(Reassembly::new(1, 65_535).is_none());
        assert!(Reassembly::new(1, 0).is_none());
        assert!(Reassembly::new(1, MAX_COMMAND_FRAGMENTS).is_some());
        // 64 full fragments would be 15 488 bytes: over a command's 12 288.
        let mut r = Reassembly::new(7, 64).unwrap();
        let mut last = Assembled::Partial;
        for idx in 0..64 {
            last = r.add(&FragmentFrame { net: 1, dst: 2, msg: 7, idx, count: 64, data: vec![0; FragmentFrame::MAX_DATA] });
            if last == Assembled::Corrupt {
                break;
            }
        }
        assert_eq!(last, Assembled::Corrupt);
    }

    #[test]
    fn at_most_four_live_and_they_expire() {
        let mut a = Assemblies::default();
        let frag = |msg: u32| FragmentFrame { net: 1, dst: 0xffff, msg, idx: 0, count: 2, data: vec![1] };
        for (i, m) in [1u32, 2, 3, 4, 5].iter().enumerate() {
            assert_eq!(a.add(&frag(*m), i as u64), None);
        }
        assert_eq!(a.len(), MAX_LIVE, "the oldest made room");
        assert_eq!(a.add(&FragmentFrame { count: 65_535, ..frag(9) }, 10), None);
        assert_eq!(a.len(), MAX_LIVE, "an impossible count takes nothing");
        a.add(&frag(6), 10 + EXPIRE_S);
        assert_eq!(a.len(), 1, "the rest expired");
        let msg = vec![5u8; 300];
        let f = split(&msg, 200, 1, 0xffff);
        assert_eq!(a.add(&f[1], 2_000), None);
        assert_eq!(a.add(&f[0], 2_001), Some(msg));
    }
}
