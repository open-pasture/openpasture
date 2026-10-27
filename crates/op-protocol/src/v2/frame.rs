//! LoRa frames (the LoRa binding, `opencollar/protocol/v2/lora.md`). The
//! first byte is `0x2k`: protocol 2, frame kind k. Integers are big-endian.
//!
//! ```text
//! beacon    20 | net u16 | anchor u16 | minute u32 | CBOR map
//! uplink    21 | net u16 | src u16 | fcnt u32 | link_len u8 | link (CBOR map) | AES-128-CCM(records) | tag 8
//! fragment  22 | net u16 | dst u16 | msg u32 | idx u16 | count u16 | data
//! hello     23 | net u16 | COSE_Sign1 by the device key (type 67)
//! ```
//!
//! An uplink frame is sealed with the collar's frame key: the nonce is
//! `device_id ‖ fcnt ‖ 0x01`, the associated data everything before the
//! ciphertext. Anchors relay frames as they are; only the server opens them.
//! The link section is readable by the anchor (which fragments a collar has
//! and which it needs) and authenticated with the rest.

use super::cbor::{self, IntMap, Value};
use super::crypto::{NONCE_BYTES, TAG_BYTES, ccm_open, ccm_seal};
use super::record::{Record, decode_records, encode_records};
use super::registry::Reject;

/// High nibble of every v2 frame's first byte.
pub const PROTOCOL: u8 = 0x20;
/// Short address meaning "every collar in the herd".
pub const BROADCAST: u16 = 0xffff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Beacon = 0,
    Uplink = 1,
    Fragment = 2,
    Hello = 3,
}

/// The kind and network of a frame, if it is a v2 frame at all.
pub fn peek(frame: &[u8]) -> Option<(Kind, u16)> {
    let (&first, rest) = frame.split_first()?;
    if first & 0xf0 != PROTOCOL || rest.len() < 2 {
        return None;
    }
    let kind = match first & 0x0f {
        0 => Kind::Beacon,
        1 => Kind::Uplink,
        2 => Kind::Fragment,
        3 => Kind::Hello,
        _ => return None,
    };
    Some((kind, u16::from_be_bytes([rest[0], rest[1]])))
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

// ---------------------------------------------------------------- uplink

/// The link section: which broadcast or unicast messages this collar holds
/// whole (`got`) and which fragments it still needs (`need`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Link {
    /// 1: message ids complete.
    pub got: Vec<u32>,
    /// 2: `[msg, from, bitmap]`: bit i (LSB first) of the bitmap set = fragment `from + i` missing.
    pub need: Vec<Need>,
    /// 3: a standby anchor's `[battery %, LTE quality 0–4]`, for the active
    /// anchor choosing who takes over.
    pub anchor: Option<[u8; 2]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    pub msg: u32,
    pub from: u16,
    pub missing: Vec<u8>,
}

impl Link {
    pub fn is_empty(&self) -> bool {
        self.got.is_empty() && self.need.is_empty() && self.anchor.is_none()
    }

    fn encode(&self) -> Vec<u8> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut m = IntMap::new();
        if !self.got.is_empty() {
            m.put(1, Value::Array(self.got.iter().map(|g| Value::uint(*g)).collect()));
        }
        if !self.need.is_empty() {
            m.put(
                2,
                Value::Array(self.need.iter().map(|n| Value::Array(vec![Value::uint(n.msg), Value::uint(n.from), Value::bytes(n.missing.clone())])).collect()),
            );
        }
        m.put_opt(3, self.anchor.map(|a| Value::Array(a.iter().map(|v| Value::uint(*v)).collect())));
        cbor::encode(&m.into_value())
    }

    fn decode(b: &[u8]) -> Option<Self> {
        if b.is_empty() {
            return Some(Self::default());
        }
        let m = IntMap::from_value(&cbor::decode(b).ok()?)?;
        let got = match m.get(1) {
            Some(v) => v.as_array()?.iter().map(Value::as_u32).collect::<Option<_>>()?,
            None => Vec::new(),
        };
        let need = match m.get(2) {
            Some(v) => v
                .as_array()?
                .iter()
                .map(|n| {
                    let [msg, from, bits, ..] = n.as_array()? else { return None };
                    Some(Need { msg: msg.as_u32()?, from: u16::try_from(from.as_u64()?).ok()?, missing: bits.as_bytes()?.to_vec() })
                })
                .collect::<Option<_>>()?,
            None => Vec::new(),
        };
        let anchor = m.get(3).and_then(Value::as_array).and_then(|a| {
            let [battery, lte, ..] = a else { return None };
            Some([u8::try_from(battery.as_u64()?).ok()?, u8::try_from(lte.as_u64()?).ok()?])
        });
        Some(Self { got, need, anchor })
    }
}

/// An uplink frame as heard: the header and link section in clear, the
/// records still sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uplink {
    pub net: u16,
    /// The sender's short address; 0 for a collar with no herd link (a
    /// direct collar's own frames in its batch).
    pub src: u16,
    /// Frame counter: never repeats under one frame key.
    pub fcnt: u32,
    pub link: Link,
    /// Everything before the ciphertext: the associated data.
    pub aad: Vec<u8>,
    /// Ciphertext and tag.
    pub sealed: Vec<u8>,
}

/// AES-CCM nonce of an uplink frame.
pub fn nonce(device_id: &[u8; 8], fcnt: u32) -> [u8; NONCE_BYTES] {
    let mut n = [0u8; NONCE_BYTES];
    n[..8].copy_from_slice(device_id);
    n[8..12].copy_from_slice(&fcnt.to_be_bytes());
    n[12] = 0x01;
    n
}

/// Build and seal an uplink frame.
pub fn seal_uplink(net: u16, src: u16, fcnt: u32, link: &Link, records: &[Record], key: &[u8; 16], device_id: &[u8; 8]) -> Vec<u8> {
    let link = link.encode();
    assert!(link.len() <= 0xff, "link section over 255 bytes");
    let mut out = vec![PROTOCOL | Kind::Uplink as u8];
    out.extend(net.to_be_bytes());
    out.extend(src.to_be_bytes());
    out.extend(fcnt.to_be_bytes());
    out.push(link.len() as u8);
    out.extend(&link);
    let sealed = ccm_seal(key, &nonce(device_id, fcnt), &out, &encode_records(records));
    out.extend(sealed);
    out
}

impl Uplink {
    /// Read the clear part. `bad_cbor` when it isn't an uplink frame.
    pub fn parse(frame: &[u8]) -> Result<Self, Reject> {
        if peek(frame).map(|p| p.0) != Some(Kind::Uplink) {
            return Err(Reject::BadCbor);
        }
        let src = u16_at(frame, 3).ok_or(Reject::BadCbor)?;
        let fcnt = u32_at(frame, 5).ok_or(Reject::BadCbor)?;
        let link_len = usize::from(*frame.get(9).ok_or(Reject::BadCbor)?);
        let body = 10 + link_len;
        if frame.len() < body + TAG_BYTES {
            return Err(Reject::BadCbor);
        }
        let link = Link::decode(&frame[10..body]).ok_or(Reject::BadCbor)?;
        Ok(Self { net: u16_at(frame, 1).unwrap(), src, fcnt, link, aad: frame[..body].to_vec(), sealed: frame[body..].to_vec() })
    }

    /// Open with the sender's frame key. `bad_sig` when it doesn't
    /// authenticate; otherwise each record, read on its own.
    pub fn open(&self, key: &[u8; 16], device_id: &[u8; 8]) -> Result<Vec<Result<Record, Reject>>, Reject> {
        let plain = ccm_open(key, &nonce(device_id, self.fcnt), &self.aad, &self.sealed).map_err(|_| Reject::BadSig)?;
        decode_records(&plain)
    }
}

// ---------------------------------------------------------------- beacon

/// The active anchor's beacon, at the start of every GNSS minute. Unsigned:
/// nothing in it changes a fence, a key or a clock, only when a collar
/// listens and talks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Beacon {
    pub net: u16,
    /// The anchor's short address.
    pub anchor: u16,
    /// Unix time / 60.
    pub minute: u32,
    /// 1: highest herd boundary version the anchor holds.
    pub bver: Option<u32>,
    /// 2: short addresses with a unicast message waiting.
    pub pending: Vec<u16>,
    /// 3: messages sent in this minute's downlink window.
    pub msgs: Vec<Announce>,
    /// 4: `[first byte index, bytes]`: bit k (LSB first, from byte 0) set =
    /// slot k's frame was heard in the previous minute.
    pub heard: Option<(u16, Vec<u8>)>,
    /// 5: `[battery %, LTE quality 0–4, rank]`.
    pub state: Option<[u32; 3]>,
    /// 6: the anchor taking over at the next hand-on.
    pub next: Option<u16>,
}

/// One message in a downlink window: id, fragment count, addressee
/// ([`BROADCAST`] for the herd) and message type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announce {
    pub msg: u32,
    pub count: u16,
    pub dst: u16,
    pub message_type: u64,
}

impl Beacon {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![PROTOCOL | Kind::Beacon as u8];
        out.extend(self.net.to_be_bytes());
        out.extend(self.anchor.to_be_bytes());
        out.extend(self.minute.to_be_bytes());
        let mut m = IntMap::new();
        m.put_opt(1, self.bver.map(Value::uint));
        if !self.pending.is_empty() {
            m.put(2, Value::Array(self.pending.iter().map(|s| Value::uint(*s)).collect()));
        }
        if !self.msgs.is_empty() {
            m.put(
                3,
                Value::Array(
                    self.msgs
                        .iter()
                        .map(|a| Value::Array(vec![Value::uint(a.msg), Value::uint(a.count), Value::uint(a.dst), Value::uint(a.message_type)]))
                        .collect(),
                ),
            );
        }
        m.put_opt(4, self.heard.as_ref().map(|(first, bits)| Value::Array(vec![Value::uint(*first), Value::bytes(bits.clone())])));
        m.put_opt(5, self.state.map(|s| Value::Array(s.iter().map(|v| Value::uint(*v)).collect())));
        m.put_opt(6, self.next.map(Value::uint));
        out.extend(cbor::encode(&m.into_value()));
        out
    }

    pub fn decode(frame: &[u8]) -> Result<Self, Reject> {
        if peek(frame).map(|p| p.0) != Some(Kind::Beacon) {
            return Err(Reject::BadCbor);
        }
        let bad = || Reject::BadCbor;
        let anchor = u16_at(frame, 3).ok_or_else(bad)?;
        let minute = u32_at(frame, 5).ok_or_else(bad)?;
        let m = IntMap::from_value(&cbor::decode(&frame[9..]).map_err(|_| bad())?).ok_or_else(bad)?;
        let u16s = |v: &Value| v.as_array().and_then(|a| a.iter().map(|x| x.as_u64().and_then(|n| u16::try_from(n).ok())).collect::<Option<Vec<_>>>());
        Ok(Self {
            net: u16_at(frame, 1).unwrap(),
            anchor,
            minute,
            bver: m.get(1).and_then(Value::as_u32),
            pending: m.get(2).and_then(u16s).unwrap_or_default(),
            msgs: m
                .get(3)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            let [msg, count, dst, t, ..] = x.as_array()? else { return None };
                            Some(Announce {
                                msg: msg.as_u32()?,
                                count: u16::try_from(count.as_u64()?).ok()?,
                                dst: u16::try_from(dst.as_u64()?).ok()?,
                                message_type: t.as_u64()?,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            heard: m.get(4).and_then(Value::as_array).and_then(|a| {
                let [first, bits, ..] = a else { return None };
                Some((u16::try_from(first.as_u64()?).ok()?, bits.as_bytes()?.to_vec()))
            }),
            state: m.get(5).and_then(Value::as_array).and_then(|a| {
                let v: Vec<u32> = a.iter().take(3).map(Value::as_u32).collect::<Option<_>>()?;
                v.try_into().ok()
            }),
            next: m.get(6).and_then(Value::as_u64).and_then(|n| u16::try_from(n).ok()),
        })
    }

    /// Whether the frame in slot `k` was heard.
    pub fn heard_slot(&self, k: u16) -> bool {
        let Some((first, bits)) = &self.heard else { return false };
        let byte = usize::from(k / 8);
        byte >= usize::from(*first) && bits.get(byte - usize::from(*first)).is_some_and(|b| b & (1 << (k % 8)) != 0)
    }

    /// The `heard` field for a set of slots.
    pub fn heard_from(slots: impl IntoIterator<Item = u16>) -> Option<(u16, Vec<u8>)> {
        let mut bits = Vec::<u8>::new();
        for k in slots {
            let byte = usize::from(k / 8);
            if bits.len() <= byte {
                bits.resize(byte + 1, 0);
            }
            bits[byte] |= 1 << (k % 8);
        }
        let first = bits.iter().position(|b| *b != 0)?;
        Some((first as u16, bits[first..].to_vec()))
    }
}

// ---------------------------------------------------------------- fragment

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FragmentFrame {
    pub net: u16,
    pub dst: u16,
    pub msg: u32,
    pub idx: u16,
    pub count: u16,
    pub data: Vec<u8>,
}

impl FragmentFrame {
    pub const HEADER_BYTES: usize = 13;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![PROTOCOL | Kind::Fragment as u8];
        out.extend(self.net.to_be_bytes());
        out.extend(self.dst.to_be_bytes());
        out.extend(self.msg.to_be_bytes());
        out.extend(self.idx.to_be_bytes());
        out.extend(self.count.to_be_bytes());
        out.extend(&self.data);
        out
    }

    pub fn decode(frame: &[u8]) -> Result<Self, Reject> {
        if peek(frame).map(|p| p.0) != Some(Kind::Fragment) || frame.len() < Self::HEADER_BYTES {
            return Err(Reject::BadCbor);
        }
        Ok(Self {
            net: u16_at(frame, 1).unwrap(),
            dst: u16_at(frame, 3).unwrap(),
            msg: u32_at(frame, 5).unwrap(),
            idx: u16_at(frame, 9).unwrap(),
            count: u16_at(frame, 11).unwrap(),
            data: frame[Self::HEADER_BYTES..].to_vec(),
        })
    }
}

// ---------------------------------------------------------------- hello

/// An unclaimed (or lost) collar introducing itself in a join window. The
/// signature, by its device key, lets the server tie the radio to the card.
pub fn hello_frame(net: u16, signed: &[u8]) -> Vec<u8> {
    let mut out = vec![PROTOCOL | Kind::Hello as u8];
    out.extend(net.to_be_bytes());
    out.extend(signed);
    out
}

/// The signed hello inside a hello frame.
pub fn hello_signed(frame: &[u8]) -> Result<&[u8], Reject> {
    if peek(frame).map(|p| p.0) != Some(Kind::Hello) {
        return Err(Reject::BadCbor);
    }
    Ok(&frame[3..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::record::{RecordBody, Status};

    #[test]
    fn uplink_seals_and_opens() {
        let key = [3u8; 16];
        let dev = [9u8; 8];
        let rec = Record { seq: 1, t: 2, body: RecordBody::Status(Status { lon: 1, lat: 2, acc_dm: 3, bver: 4, ..Default::default() }) };
        let link = Link { got: vec![0xdead_beef], need: vec![], anchor: Some([76, 3]) };
        let f = seal_uplink(0x1234, 7, 42, &link, std::slice::from_ref(&rec), &key, &dev);
        let up = Uplink::parse(&f).unwrap();
        assert_eq!((up.net, up.src, up.fcnt, &up.link), (0x1234, 7, 42, &link));
        assert_eq!(up.open(&key, &dev).unwrap(), vec![Ok(rec)]);
        assert_eq!(up.open(&key, &[8; 8]), Err(Reject::BadSig), "another collar's nonce");
        let mut moved = f.clone();
        moved[4] ^= 1; // src
        assert_eq!(Uplink::parse(&moved).unwrap().open(&key, &dev), Err(Reject::BadSig), "the header is authenticated");
    }

    #[test]
    fn beacon_round_trip() {
        let b = Beacon {
            net: 1,
            anchor: 3,
            minute: 29_841_840,
            bver: Some(57),
            pending: vec![12],
            msgs: vec![Announce { msg: 1, count: 2, dst: BROADCAST, message_type: 1 }],
            heard: Beacon::heard_from([0, 5, 17]),
            state: Some([80, 3, 1]),
            next: Some(4),
        };
        let d = Beacon::decode(&b.encode()).unwrap();
        assert_eq!(d, b);
        assert!(d.heard_slot(17) && d.heard_slot(0) && !d.heard_slot(6));
    }
}
