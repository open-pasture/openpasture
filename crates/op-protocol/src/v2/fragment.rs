//! Splitting a signed message into LoRa fragments and putting it back
//! together. A message is named by its id: the first 4 bytes of SHA-256 of
//! the whole message, so a collar can tell a complete message from a
//! corrupted one before it checks the signature.

use super::crypto::sha256;
use super::frame::FragmentFrame;

/// The id of a message: the first 4 bytes of its SHA-256, big-endian.
pub fn message_id(message: &[u8]) -> u32 {
    u32::from_be_bytes(sha256(&[message])[..4].try_into().unwrap())
}

/// The fragments of `message`, each carrying at most `max_data` bytes.
pub fn split(message: &[u8], max_data: usize, net: u16, dst: u16) -> Vec<FragmentFrame> {
    assert!(max_data > 0 && !message.is_empty());
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assembled {
    /// Still missing fragments.
    Partial,
    /// Every fragment is in and the id matches.
    Complete,
    /// Every fragment is in but the id doesn't match: start again.
    Corrupt,
}

impl Reassembly {
    pub fn new(msg: u32, count: u16) -> Self {
        Self { msg, parts: vec![None; usize::from(count)] }
    }

    /// Add a fragment of this message. Fragments of other messages, out of
    /// range, or claiming another count are ignored.
    pub fn add(&mut self, f: &FragmentFrame) -> Assembled {
        let fits = f.msg == self.msg && usize::from(f.count) == self.parts.len() && usize::from(f.idx) < self.parts.len();
        if fits {
            self.parts[usize::from(f.idx)] = Some(f.data.clone());
        }
        if self.parts.iter().any(Option::is_none) {
            return Assembled::Partial;
        }
        if message_id(&self.concat()) == self.msg {
            Assembled::Complete
        } else {
            self.parts.iter_mut().for_each(|p| *p = None);
            Assembled::Corrupt
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_reassemble_with_losses() {
        let msg: Vec<u8> = (0..500u16).map(|i| (i * 7) as u8).collect();
        let frags = split(&msg, 200, 1, 0xffff);
        assert_eq!(frags.len(), 3);
        let mut r = Reassembly::new(frags[0].msg, 3);
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
        let mut r = Reassembly::new(frags[0].msg, 2);
        r.add(&frags[0]);
        assert_eq!(r.add(&frags[1]), Assembled::Corrupt);
        assert_eq!(r.need(8), Some((0, vec![0b11])), "starts again");
    }
}
