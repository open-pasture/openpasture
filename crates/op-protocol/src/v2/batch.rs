//! Anchor ↔ server (the HTTP binding, `opencollar/protocol/v2/http.md`).
//!
//! An anchor, station or direct collar posts a **batch**: the frames it heard
//! (and its own), signed with its device key (message type 66). The reply
//! carries the herd's waiting downlinks, so nothing ever polls.

use ed25519_dalek::{SigningKey, VerifyingKey};

use super::cbor::{self, IntMap, Value};
use super::cose::{self, Sign1};
use super::registry::{MessageType, Reject};

/// Most frames in one batch.
pub const MAX_FRAMES: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batch {
    /// 16: the sender's batch counter. A retry resends the same batch.
    pub seq: u64,
    /// 17: GNSS time sent, Unix seconds.
    pub sent_at: u64,
    /// 18: the herd the sender serves; absent for a direct collar.
    pub herd: Option<Vec<u8>>,
    /// 19: the highest downlink id it holds.
    pub have: u64,
    /// 20: `[frame]` for its own, `[frame, at, rssi_dbm, snr_db]` for a heard one.
    pub frames: Vec<Relayed>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relayed {
    pub frame: Vec<u8>,
    pub rx: Option<Rx>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rx {
    /// GNSS time heard, Unix seconds.
    pub at: u64,
    pub rssi_dbm: i32,
    pub snr_db: i32,
}

impl Batch {
    pub fn payload(&self) -> Vec<u8> {
        let mut m = IntMap::new();
        m.put(0, Value::uint(MessageType::Batch.code()))
            .put(16, Value::uint(self.seq))
            .put(17, Value::uint(self.sent_at))
            .put_opt(18, self.herd.clone().map(Value::Bytes))
            .put(19, Value::uint(self.have))
            .put(
                20,
                Value::Array(
                    self.frames
                        .iter()
                        .map(|f| {
                            let mut v = vec![Value::bytes(f.frame.clone())];
                            if let Some(rx) = f.rx {
                                v.extend([Value::uint(rx.at), Value::int(rx.rssi_dbm), Value::int(rx.snr_db)]);
                            }
                            Value::Array(v)
                        })
                        .collect(),
                ),
            );
        cbor::encode(&m.into_value())
    }

    /// The signed batch, as posted.
    pub fn sign(&self, device_key: &SigningKey) -> Vec<u8> {
        cose::sign(&self.payload(), device_key)
    }

    /// Check a posted batch: the envelope, the sender's device key
    /// (`key_for` its kid, which is its device id), the payload.
    pub fn open(bytes: &[u8], key_for: impl Fn(&[u8; 8]) -> Option<VerifyingKey>) -> Result<([u8; 8], Self), Reject> {
        let env = Sign1::parse(bytes)?;
        env.verify(&key_for(&env.kid).ok_or(Reject::UnknownKid)?)?;
        let m = env.payload_map()?;
        if m.get(0).and_then(Value::as_u64) != Some(MessageType::Batch.code()) {
            return Err(Reject::BadCbor);
        }
        let frames = m.get(20).and_then(Value::as_array).ok_or(Reject::BadCbor)?;
        if frames.len() > MAX_FRAMES {
            return Err(Reject::TooLarge);
        }
        let frames = frames
            .iter()
            .map(|f| {
                let a = f.as_array().ok_or(Reject::BadCbor)?;
                let frame = a.first().and_then(Value::as_bytes).ok_or(Reject::BadCbor)?.to_vec();
                let rx = match a {
                    [_, at, rssi, snr, ..] => Some(Rx {
                        at: at.as_u64().ok_or(Reject::BadCbor)?,
                        rssi_dbm: rssi.as_i32().ok_or(Reject::BadCbor)?,
                        snr_db: snr.as_i32().ok_or(Reject::BadCbor)?,
                    }),
                    _ => None,
                };
                Ok(Relayed { frame, rx })
            })
            .collect::<Result<_, Reject>>()?;
        let uint = |k| m.get(k).and_then(Value::as_u64).ok_or(Reject::BadCbor);
        Ok((
            env.kid,
            Self {
                seq: uint(16)?,
                sent_at: uint(17)?,
                herd: m.get(18).map(|h| h.as_bytes().filter(|h| (1..=16).contains(&h.len())).map(<[u8]>::to_vec).ok_or(Reject::BadCbor)).transpose()?,
                have: uint(19)?,
                frames,
            },
        ))
    }
}

/// The server's answer to a batch. Unsigned: every command inside is signed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    /// 1: the batch seq stored. The sender can drop that batch.
    pub ack: u64,
    /// 2: `[id, to, message]`: downlinks above the batch's `have`.
    pub down: Vec<Downlink>,
    /// 3: post again within this many seconds (a fast window, a queue).
    pub next_s: Option<u32>,
    /// 4: the server's Unix time; for logs, never for a fence.
    pub time: Option<u64>,
    /// 5: downlink ids to stop delivering (superseded or done).
    pub drop: Vec<u64>,
    /// 6: the short address of the anchor to hand the herd's link to next.
    pub next_anchor: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downlink {
    /// Increasing per herd.
    pub id: u64,
    /// One collar's device id; `None` for the whole herd.
    pub to: Option<[u8; 8]>,
    /// A signed command, as is.
    pub message: Vec<u8>,
}

impl Reply {
    pub fn encode(&self) -> Vec<u8> {
        let mut m = IntMap::new();
        m.put(1, Value::uint(self.ack));
        if !self.down.is_empty() {
            m.put(
                2,
                Value::Array(
                    self.down
                        .iter()
                        .map(|d| Value::Array(vec![Value::uint(d.id), d.to.map_or(Value::Null, |t| Value::bytes(t.to_vec())), Value::bytes(d.message.clone())]))
                        .collect(),
                ),
            );
        }
        m.put_opt(3, self.next_s.map(Value::uint)).put_opt(4, self.time.map(Value::uint));
        if !self.drop.is_empty() {
            m.put(5, Value::Array(self.drop.iter().map(|d| Value::uint(*d)).collect()));
        }
        m.put_opt(6, self.next_anchor.map(Value::uint));
        cbor::encode(&m.into_value())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Reject> {
        let m = IntMap::from_value(&cbor::decode_untagged(bytes).map_err(|_| Reject::BadCbor)?).ok_or(Reject::BadCbor)?;
        let present = |k: u64, read: &dyn Fn(&Value) -> Option<u64>| m.get(k).map(|v| read(v).ok_or(Reject::BadCbor)).transpose();
        let down = match m.get(2) {
            Some(v) => v
                .as_array()
                .ok_or(Reject::BadCbor)?
                .iter()
                .map(|d| {
                    let [id, to, msg, ..] = d.as_array().ok_or(Reject::BadCbor)? else { return Err(Reject::BadCbor) };
                    Ok(Downlink {
                        id: id.as_u64().ok_or(Reject::BadCbor)?,
                        to: if to.is_null() { None } else { Some(to.as_bytes().and_then(|t| t.try_into().ok()).ok_or(Reject::BadCbor)?) },
                        message: msg.as_bytes().ok_or(Reject::BadCbor)?.to_vec(),
                    })
                })
                .collect::<Result<_, Reject>>()?,
            None => Vec::new(),
        };
        // A known field present with the wrong type is `bad_cbor`, never
        // read as absent.
        Ok(Self {
            ack: m.get(1).and_then(Value::as_u64).ok_or(Reject::BadCbor)?,
            down,
            next_s: present(3, &|v| v.as_u32().map(u64::from))?.map(|n| n as u32),
            time: present(4, &Value::as_u64)?,
            drop: match m.get(5) {
                None => Vec::new(),
                Some(v) => v.as_array().and_then(|a| a.iter().map(Value::as_u64).collect::<Option<_>>()).ok_or(Reject::BadCbor)?,
            },
            next_anchor: present(6, &|v| v.as_u64().filter(|n| (1..=65_534).contains(n)))?.map(|n| n as u16),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::crypto::kid;

    #[test]
    fn batch_and_reply_round_trip() {
        let anchor = SigningKey::from_bytes(&[5; 32]);
        let b = Batch {
            seq: 9,
            sent_at: 1_790_510_460,
            herd: Some(vec![1; 8]),
            have: 3,
            frames: vec![
                Relayed { frame: vec![0x21, 0, 1], rx: Some(Rx { at: 1_790_510_458, rssi_dbm: -97, snr_db: 6 }) },
                Relayed { frame: vec![0x21], rx: None },
            ],
        };
        let bytes = b.sign(&anchor);
        let (who, back) = Batch::open(&bytes, |k| (*k == kid(anchor.verifying_key().as_bytes())).then(|| anchor.verifying_key())).unwrap();
        assert_eq!((who, back), (kid(anchor.verifying_key().as_bytes()), b));
        let r =
            Reply { ack: 9, down: vec![Downlink { id: 4, to: None, message: vec![0xd2] }], next_s: Some(60), time: None, drop: vec![2], next_anchor: Some(2) };
        assert_eq!(Reply::decode(&r.encode()).unwrap(), r);
    }
}
