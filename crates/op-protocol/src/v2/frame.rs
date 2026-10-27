//! LoRa frames (the LoRa binding, `opencollar/protocol/v2/lora.md`). The
//! first byte is `0x2k`: protocol 2, frame kind k. Integers are big-endian.
//! No frame is longer than [`MAX_FRAME_BYTES`].
//!
//! ```text
//! beacon    20 | net u16 | anchor u16 | minute u32 | CBOR map | tag 8
//! uplink    21 | net u16 | src u16 | fcnt u32 | link_len u8 | link (CBOR map) | AES-128-CCM(records) | tag 8
//! fragment  22 | net u16 | dst u16 | msg u32 | idx u16 | count u16 | data
//! hello     23 | net u16 | COSE_Sign1 by the device key (type 67)
//! ```
//!
//! An uplink frame is sealed with the collar's frame key: the nonce is
//! `device_id ‖ fcnt ‖ 0x01`, the associated data everything before the
//! ciphertext. Anchors relay frames as they are; only the server opens them.
//! The link section is readable by the anchor (which fragments a collar has
//! and which it needs) and authenticated with the rest. A beacon is
//! authenticated with the herd's beacon key, which every claimed member gets
//! wrapped for it in its membership.

use super::cbor::{self, IntMap, Value};
use super::crypto::{BEACON_TAG_BYTES, NONCE_BYTES, TAG_BYTES, beacon_tag, ccm_open, ccm_seal, ct_eq};
use super::record::{Record, decode_records, encode_records};
use super::registry::Reject;

/// High nibble of every v2 frame's first byte.
pub const PROTOCOL: u8 = 0x20;
/// Short address meaning "every collar in the herd".
pub const BROADCAST: u16 = 0xffff;
/// Short address meaning "a collar with no short address yet" (an unclaimed
/// or joining collar): the message inside names its device.
pub const UNCLAIMED: u16 = 0;
/// Longest v2 frame of any kind, on any transport: a LoRa payload's limit.
pub const MAX_FRAME_BYTES: usize = 255;

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

/// Whether a receiver takes a frame heard on its channel (`lora.md` §4):
/// `own_net` is its herd's net (`None` while it has none), `anchor` whether
/// it is the active anchor. Frames of another net are dropped, except that
/// an anchor takes hellos with net 0 (collars without a herd), and a collar
/// without a net takes any beacon (to find a join window) and fragments
/// addressed to [`UNCLAIMED`] (its bind may be in them).
pub fn accepts(frame: &[u8], own_net: Option<u16>, anchor: bool) -> bool {
    let Some((kind, net)) = peek(frame) else { return false };
    if frame.len() > MAX_FRAME_BYTES {
        return false;
    }
    match (kind, own_net) {
        (Kind::Hello, Some(own)) => anchor && (net == own || net == 0),
        (Kind::Hello, None) => false,
        (Kind::Uplink, Some(own)) => anchor && net == own,
        (Kind::Beacon, Some(own)) => net == own,
        (Kind::Fragment, Some(own)) => net == own,
        (Kind::Beacon, None) => true,
        (Kind::Fragment, None) => u16_at(frame, 3) == Some(UNCLAIMED),
        (Kind::Uplink, None) => false,
    }
}

/// A frame of `kind`, no longer than [`MAX_FRAME_BYTES`] (`too_large`).
fn expect(frame: &[u8], kind: Kind) -> Result<(), Reject> {
    if peek(frame).map(|p| p.0) != Some(kind) {
        return Err(Reject::BadCbor);
    }
    if frame.len() > MAX_FRAME_BYTES {
        return Err(Reject::TooLarge);
    }
    Ok(())
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// A profile map (deterministic, untagged, uint keys) from the bytes.
fn map_of(b: &[u8]) -> Option<IntMap> {
    IntMap::from_value(&cbor::decode_untagged(b).ok()?)
}

fn u16_of(v: &Value) -> Option<u16> {
    v.as_u64().and_then(|n| u16::try_from(n).ok())
}

/// An optional field: absent is `Ok(None)`, present but unreadable is an error.
fn opt<T>(v: Option<&Value>, read: impl Fn(&Value) -> Option<T>) -> Option<Option<T>> {
    match v {
        None => Some(None),
        Some(v) => read(v).map(Some),
    }
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

/// Longest `need` bitmap: 64 fragments.
pub const MAX_NEED_BYTES: usize = 8;

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

    /// A known key that is present must read; unknown keys are ignored.
    fn decode(b: &[u8]) -> Option<Self> {
        if b.is_empty() {
            return Some(Self::default());
        }
        let m = map_of(b)?;
        let got = opt(m.get(1), |v| v.as_array()?.iter().map(Value::as_u32).collect::<Option<_>>())?.unwrap_or_default();
        let need = opt(m.get(2), |v| {
            v.as_array()?
                .iter()
                .map(|n| {
                    let [msg, from, bits] = n.as_array()? else { return None };
                    let missing = bits.as_bytes().filter(|b| b.len() <= MAX_NEED_BYTES)?.to_vec();
                    Some(Need { msg: msg.as_u32()?, from: u16_of(from)?, missing })
                })
                .collect::<Option<_>>()
        })?
        .unwrap_or_default();
        let anchor = opt(m.get(3), |v| {
            let [battery, lte] = v.as_array()? else { return None };
            Some([u8::try_from(battery.as_u64()?).ok().filter(|b| *b <= 100)?, u8::try_from(lte.as_u64()?).ok().filter(|q| *q <= 4)?])
        })?;
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
    /// Frame counter: counts every frame the collar ever seals and never
    /// repeats (`messages.md` §7.1).
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

/// Bytes an uplink frame adds around its records array: header, `link_len`
/// and the tag.
pub const UPLINK_OVERHEAD: usize = 10 + TAG_BYTES;

/// Build and seal an uplink frame. Panics if the link section is over 255
/// bytes (a sender's bug).
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
    /// Read the clear part. `bad_cbor` when it isn't an uplink frame or its
    /// link section doesn't read; `too_large` past [`MAX_FRAME_BYTES`].
    pub fn parse(frame: &[u8]) -> Result<Self, Reject> {
        expect(frame, Kind::Uplink)?;
        let src = u16_at(frame, 3).ok_or(Reject::BadCbor)?;
        let fcnt = u32_at(frame, 5).ok_or(Reject::BadCbor)?;
        let link_len = usize::from(*frame.get(9).ok_or(Reject::BadCbor)?);
        let body = 10 + link_len;
        if frame.len() < body + TAG_BYTES {
            return Err(Reject::BadCbor);
        }
        let link = Link::decode(&frame[10..body]).ok_or(Reject::BadCbor)?;
        Ok(Self { net: u16_at(frame, 1).ok_or(Reject::BadCbor)?, src, fcnt, link, aad: frame[..body].to_vec(), sealed: frame[body..].to_vec() })
    }

    /// Open with the sender's frame key. `bad_sig` when it doesn't
    /// authenticate; otherwise each record, read on its own.
    pub fn open(&self, key: &[u8; 16], device_id: &[u8; 8]) -> Result<Vec<Result<Record, Reject>>, Reject> {
        let plain = ccm_open(key, &nonce(device_id, self.fcnt), &self.aad, &self.sealed).map_err(|_| Reject::BadSig)?;
        decode_records(&plain)
    }
}

// ---------------------------------------------------------------- beacon

/// The active anchor's beacon, at the start of every GNSS minute,
/// authenticated with the herd's beacon key. Nothing in it changes a fence,
/// a key or a clock: it says when to listen, what is coming, which anchor is
/// active and which frames it heard (a radio receipt, never a reason to
/// forget a record).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Beacon {
    pub net: u16,
    /// The anchor's short address.
    pub anchor: u16,
    /// Unix time / 60.
    pub minute: u32,
    /// 1: highest herd boundary version the anchor holds.
    pub bver: Option<u32>,
    /// 2: short addresses with a unicast message waiting; 0 for collars
    /// without one (joining).
    pub pending: Vec<u16>,
    /// 3: messages sent in this minute's downlink window.
    pub msgs: Vec<Announce>,
    /// 4: `[first byte index, bytes]`: bit k (LSB first, from byte 0) set =
    /// slot k's frame was heard in the previous minute.
    pub heard: Option<(u16, Vec<u8>)>,
    /// 5: `[battery %, LTE quality 0–4, rank]`.
    pub state: Option<[u32; 3]>,
    /// 6: `[short, minute]`: the anchor taking over, and the GNSS minute it
    /// beacons first.
    pub handon: Option<(u16, u32)>,
    /// 7: the anchor's election term; 0 is never sent.
    pub term: u32,
}

/// One message in a downlink window: id, fragment count, addressee
/// ([`BROADCAST`] for the herd, [`UNCLAIMED`] for joining collars) and
/// message type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announce {
    pub msg: u32,
    pub count: u16,
    pub dst: u16,
    pub message_type: u64,
}

/// Bytes before a beacon's CBOR map.
const BEACON_HEADER: usize = 9;

impl Beacon {
    fn body(&self) -> Vec<u8> {
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
        m.put_opt(6, self.handon.map(|(short, minute)| Value::Array(vec![Value::uint(short), Value::uint(minute)])));
        if self.term > 0 {
            m.put(7, Value::uint(self.term));
        }
        out.extend(cbor::encode(&m.into_value()));
        out
    }

    /// The frame, tagged with the herd's beacon key.
    pub fn encode(&self, beacon_key: &[u8; 16]) -> Vec<u8> {
        let mut out = self.body();
        let tag = beacon_tag(beacon_key, &out);
        out.extend(tag);
        out
    }

    /// Read a beacon that authenticates under one of `keys` (a collar holds
    /// the herd's current beacon key and, during a change, the next one).
    /// `bad_sig` when none verifies: the collar treats it as not heard.
    pub fn decode(frame: &[u8], keys: &[[u8; 16]]) -> Result<Self, Reject> {
        let b = Self::decode_unverified(frame)?;
        let (body, tag) = frame.split_at(frame.len() - BEACON_TAG_BYTES);
        if keys.iter().any(|k| ct_eq(&beacon_tag(k, body), tag)) { Ok(b) } else { Err(Reject::BadSig) }
    }

    /// Read a beacon without checking its tag. Only a collar with no beacon
    /// key (unclaimed) does this, and only to find the join window.
    pub fn decode_unverified(frame: &[u8]) -> Result<Self, Reject> {
        expect(frame, Kind::Beacon)?;
        let bad = || Reject::BadCbor;
        if frame.len() < BEACON_HEADER + 1 + BEACON_TAG_BYTES {
            return Err(bad());
        }
        let anchor = u16_at(frame, 3).ok_or_else(bad)?;
        let minute = u32_at(frame, 5).ok_or_else(bad)?;
        let m = map_of(&frame[BEACON_HEADER..frame.len() - BEACON_TAG_BYTES]).ok_or_else(bad)?;
        let u16s = |v: &Value| v.as_array()?.iter().map(u16_of).collect::<Option<Vec<_>>>();
        Ok(Self {
            net: u16_at(frame, 1).ok_or_else(bad)?,
            anchor,
            minute,
            bver: opt(m.get(1), Value::as_u32).ok_or_else(bad)?,
            pending: opt(m.get(2), u16s).ok_or_else(bad)?.unwrap_or_default(),
            msgs: opt(m.get(3), |v| {
                v.as_array()?
                    .iter()
                    .map(|x| {
                        let [msg, count, dst, t] = x.as_array()? else { return None };
                        Some(Announce { msg: msg.as_u32()?, count: u16_of(count).filter(|c| *c >= 1)?, dst: u16_of(dst)?, message_type: t.as_u64()? })
                    })
                    .collect::<Option<_>>()
            })
            .ok_or_else(bad)?
            .unwrap_or_default(),
            heard: opt(m.get(4), |v| {
                let [first, bits] = v.as_array()? else { return None };
                Some((u16_of(first)?, bits.as_bytes().filter(|b| b.len() <= 64)?.to_vec()))
            })
            .ok_or_else(bad)?,
            state: opt(m.get(5), |v| {
                let [battery, lte, rank] = v.as_array()? else { return None };
                Some([battery.as_u32().filter(|b| *b <= 100)?, lte.as_u32().filter(|q| *q <= 4)?, rank.as_u32()?])
            })
            .ok_or_else(bad)?,
            handon: opt(m.get(6), |v| {
                let [short, minute] = v.as_array()? else { return None };
                Some((u16_of(short).filter(|s| (1..BROADCAST).contains(s))?, minute.as_u32()?))
            })
            .ok_or_else(bad)?,
            term: opt(m.get(7), |v| v.as_u32().filter(|t| *t >= 1)).ok_or_else(bad)?.unwrap_or(0),
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

    /// Which of two active anchors' beacons wins (`lora.md` §5): the higher
    /// term, then the lower rank, then the lower short address. Both anchors
    /// and every node use this order, so a herd converges on one anchor.
    pub fn outranks(&self, other: &Beacon) -> bool {
        let key = |b: &Beacon| (std::cmp::Reverse(b.term), b.state.map_or(u32::MAX, |s| s[2]), b.anchor);
        key(self) < key(other)
    }
}

/// Whether an active anchor stays silent in `minute` to listen for another
/// active anchor on its net (`lora.md` §5): one minute in 8, chosen from its
/// short address so two anchors rarely listen in the same minute.
pub fn listen_minute(net: u16, anchor: u16, minute: u32) -> bool {
    let h = super::crypto::sha256(&[b"listen", &net.to_be_bytes(), &anchor.to_be_bytes(), &minute.to_be_bytes()]);
    h[0] & 7 == 0
}

// ---------------------------------------------------------------- beacon key

/// Bytes of a beacon key wrapped for one collar: the key and a CCM tag.
pub const WRAPPED_KEY_BYTES: usize = 16 + TAG_BYTES;

fn wrap_nonce(device_id: &[u8; 8], version: u32) -> [u8; NONCE_BYTES] {
    let mut n = [0u8; NONCE_BYTES];
    n[..8].copy_from_slice(device_id);
    n[8..12].copy_from_slice(&version.to_be_bytes());
    n[12] = 0x02;
    n
}

fn wrap_aad(herd: &[u8], net: u16) -> Vec<u8> {
    let mut a = b"beacon".to_vec();
    a.push(herd.len() as u8);
    a.extend(herd);
    a.extend(net.to_be_bytes());
    a
}

/// A herd's beacon key wrapped for one collar (membership key 23): AES-CCM
/// under its wrap key (`crypto::wrap_key`), nonce `device_id ‖ version ‖
/// 0x02` (the membership's version, so never repeated), bound to the herd and net.
pub fn wrap_beacon_key(wrap_key: &[u8; 16], device_id: &[u8; 8], version: u32, herd: &[u8], net: u16, beacon_key: &[u8; 16]) -> [u8; WRAPPED_KEY_BYTES] {
    ccm_seal(wrap_key, &wrap_nonce(device_id, version), &wrap_aad(herd, net), beacon_key).try_into().unwrap()
}

/// The beacon key in a membership, if it unwraps for this collar.
pub fn unwrap_beacon_key(wrap_key: &[u8; 16], device_id: &[u8; 8], version: u32, herd: &[u8], net: u16, wrapped: &[u8]) -> Option<[u8; 16]> {
    ccm_open(wrap_key, &wrap_nonce(device_id, version), &wrap_aad(herd, net), wrapped).ok()?.try_into().ok()
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
    /// Most data one fragment frame can carry.
    pub const MAX_DATA: usize = MAX_FRAME_BYTES - Self::HEADER_BYTES;

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

    /// `bad_cbor` unless `1 ≤ count`, `idx < count` and it carries 1 to
    /// [`Self::MAX_DATA`] bytes. Whether `count` is acceptable for the
    /// announced message is the reassembler's question.
    pub fn decode(frame: &[u8]) -> Result<Self, Reject> {
        expect(frame, Kind::Fragment)?;
        let bad = || Reject::BadCbor;
        if frame.len() <= Self::HEADER_BYTES {
            return Err(bad());
        }
        let f = Self {
            net: u16_at(frame, 1).ok_or_else(bad)?,
            dst: u16_at(frame, 3).ok_or_else(bad)?,
            msg: u32_at(frame, 5).ok_or_else(bad)?,
            idx: u16_at(frame, 9).ok_or_else(bad)?,
            count: u16_at(frame, 11).ok_or_else(bad)?,
            data: frame[Self::HEADER_BYTES..].to_vec(),
        };
        if f.count == 0 || f.idx >= f.count {
            return Err(bad());
        }
        Ok(f)
    }
}

// ---------------------------------------------------------------- hello

/// An unclaimed (or lost) collar introducing itself in a join window, with
/// net 0 while it has none. The signature, by its device key, lets the
/// server tie the radio to the card. Panics past [`MAX_FRAME_BYTES`].
pub fn hello_frame(net: u16, signed: &[u8]) -> Vec<u8> {
    let mut out = vec![PROTOCOL | Kind::Hello as u8];
    out.extend(net.to_be_bytes());
    out.extend(signed);
    assert!(out.len() <= MAX_FRAME_BYTES, "a hello frame over {MAX_FRAME_BYTES} bytes");
    out
}

/// The signed hello inside a hello frame.
pub fn hello_signed(frame: &[u8]) -> Result<&[u8], Reject> {
    expect(frame, Kind::Hello)?;
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

    /// Astra 01 F12: a 65 544-byte uplink panicked in CCM before its tag was
    /// checked. Now anything past a LoRa payload is refused as it is read.
    #[test]
    fn hostile_uplink_lengths() {
        let mut huge = vec![0x21, 0, 1, 0, 2, 0, 0, 0, 3, 0];
        huge.resize(10 + 65_536 + 8, 0);
        assert_eq!(Uplink::parse(&huge), Err(Reject::TooLarge));
        let up = Uplink { net: 1, src: 2, fcnt: 3, link: Link::default(), aad: vec![0x21; 10], sealed: vec![0; 65_536 + 8] };
        assert_eq!(up.open(&[0; 16], &[0; 8]), Err(Reject::BadSig), "open never panics either");
    }

    #[test]
    fn link_fields_that_are_present_must_read() {
        let mut m = IntMap::new();
        m.put(3, Value::Array(vec![Value::uint(255u8), Value::uint(3u8)]));
        assert_eq!(Link::decode(&cbor::encode(&m.clone().into_value())), None, "battery 255");
        m.put(3, Value::Array(vec![Value::uint(76u8), Value::uint(3u8)])).put(9, Value::text("later"));
        assert_eq!(Link::decode(&cbor::encode(&m.into_value())).unwrap().anchor, Some([76, 3]), "unknown keys are ignored");
    }

    fn beacon() -> Beacon {
        Beacon {
            net: 1,
            anchor: 3,
            minute: 29_841_840,
            bver: Some(57),
            pending: vec![12],
            msgs: vec![Announce { msg: 1, count: 2, dst: BROADCAST, message_type: 1 }],
            heard: Beacon::heard_from([0, 5, 17]),
            state: Some([80, 3, 1]),
            handon: Some((4, 29_841_845)),
            term: 7,
        }
    }

    #[test]
    fn beacon_round_trip() {
        let key = [5u8; 16];
        let b = beacon();
        let d = Beacon::decode(&b.encode(&key), &[[1; 16], key]).unwrap();
        assert_eq!(d, b);
        assert!(d.heard_slot(17) && d.heard_slot(0) && !d.heard_slot(6));
    }

    /// Astra 01 F03: an unsigned beacon with any `heard` bitmap, anchor,
    /// rank or successor was accepted.
    #[test]
    fn a_forged_beacon_is_not_heard() {
        let key = [5u8; 16];
        let mut forged = beacon().encode(&[6; 16]);
        assert_eq!(Beacon::decode(&forged, &[key]), Err(Reject::BadSig), "another key");
        let mut good = beacon().encode(&key);
        good[4] ^= 1; // anchor
        assert_eq!(Beacon::decode(&good, &[key]), Err(Reject::BadSig), "the header is authenticated");
        forged.truncate(9);
        assert_eq!(Beacon::decode(&forged, &[key]), Err(Reject::BadCbor));
    }

    #[test]
    fn election_order() {
        let a = Beacon { term: 8, state: Some([50, 2, 3]), anchor: 9, ..Default::default() };
        let b = Beacon { term: 7, state: Some([90, 4, 0]), anchor: 1, ..Default::default() };
        assert!(a.outranks(&b) && !b.outranks(&a), "the higher term wins");
        let c = Beacon { term: 8, state: Some([50, 2, 2]), anchor: 12, ..Default::default() };
        assert!(c.outranks(&a), "then the lower rank");
        let d = Beacon { term: 8, state: Some([50, 2, 2]), anchor: 11, ..Default::default() };
        assert!(d.outranks(&c), "then the lower short");
        // Every anchor listens about one minute in eight.
        let listens = (0..8_000).filter(|m| listen_minute(1, 3, *m)).count();
        assert!((800..1200).contains(&listens), "{listens}");
    }

    #[test]
    fn beacon_key_wraps_for_one_collar() {
        let wrap = [7u8; 16];
        let w = wrap_beacon_key(&wrap, &[1; 8], 4, b"HERD", 10, &[9; 16]);
        assert_eq!(unwrap_beacon_key(&wrap, &[1; 8], 4, b"HERD", 10, &w), Some([9; 16]));
        assert_eq!(unwrap_beacon_key(&wrap, &[2; 8], 4, b"HERD", 10, &w), None, "another collar");
        assert_eq!(unwrap_beacon_key(&wrap, &[1; 8], 4, b"HERD", 11, &w), None, "another net");
    }

    /// Astra 01 F16: "drop another net" contradicted the net-0 hello and
    /// the join reply to a collar with no short address.
    #[test]
    fn bootstrap_net_rules() {
        let hello0 = hello_frame(0, &[0xd2, 0x84]);
        assert!(accepts(&hello0, Some(10), true), "an anchor takes a net-0 hello");
        assert!(!accepts(&hello0, Some(10), false), "a node doesn't");
        assert!(!accepts(&hello_frame(11, &[0xd2]), Some(10), true), "another herd's hello");
        let to_joiner = FragmentFrame { net: 10, dst: UNCLAIMED, msg: 1, idx: 0, count: 1, data: vec![1] }.encode();
        assert!(accepts(&to_joiner, None, false), "a collar with no net takes fragments for joiners");
        assert!(!accepts(&FragmentFrame { dst: 5, ..FragmentFrame::decode(&to_joiner).unwrap() }.encode(), None, false));
        assert!(accepts(&Beacon { net: 10, ..Default::default() }.encode(&[0; 16]), None, false), "any beacon, to find the join window");
        assert!(!accepts(&Beacon { net: 11, ..Default::default() }.encode(&[0; 16]), Some(10), false));
    }

    #[test]
    fn fragment_headers_are_bounded() {
        let f = FragmentFrame { net: 1, dst: 2, msg: 3, idx: 0, count: 1, data: vec![1] };
        assert_eq!(FragmentFrame::decode(&f.encode()).unwrap(), f);
        for bad in [FragmentFrame { count: 0, idx: 0, ..f.clone() }, FragmentFrame { idx: 1, ..f.clone() }, FragmentFrame { data: vec![], ..f.clone() }] {
            assert_eq!(FragmentFrame::decode(&bad.encode()), Err(Reject::BadCbor));
        }
        assert_eq!(FragmentFrame::decode(&FragmentFrame { data: vec![0; 243], ..f }.encode()), Err(Reject::TooLarge));
    }
}
