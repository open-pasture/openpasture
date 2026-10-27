//! Identity and ownership (`opencollar/protocol/v2/identity.md`): claim
//! codes and proofs, the factory record, the card and the hello.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde_json::{Value as Json, json};

use super::cbor::{self, IntMap, Value};
use super::cose::{self, Sign1};
use super::crypto::{hkdf, hmac_sha256, kid};
use super::json::hex;
use super::registry::{DeviceClass, MessageType, Reject};

/// A collar's device id: the kid of its device key.
pub fn device_id(device_key: &VerifyingKey) -> [u8; 8] {
    kid(device_key.as_bytes())
}

/// The v2 herd id of a herd a v1 server knew as `v1_herd_id` (`identity.md`
/// §10): the first 16 bytes of SHA-256 of `"opencollar/1 herd"` and the id.
/// v1 ids run to 64 characters; a v2 herd id is at most 16 bytes, so both
/// sides derive it the same way instead of carrying the old one.
pub fn herd_from_v1(v1_herd_id: &str) -> [u8; 16] {
    super::crypto::sha256(&[b"opencollar/1 herd", v1_herd_id.as_bytes()])[..16].try_into().unwrap()
}

/// Crockford base32, the claim code's alphabet.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters in a claim code (80 bits).
pub const CLAIM_CODE_LEN: usize = 16;

/// A claim code as typed or scanned, normalized: upper case, no `-` or
/// spaces, `I L` read as `1` and `O` as `0`. `None` unless 16 characters of
/// the alphabet remain.
pub fn normalize_claim_code(s: &str) -> Option<String> {
    let out: String = s
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            c => c,
        })
        .collect();
    (out.len() == CLAIM_CODE_LEN && out.bytes().all(|b| CROCKFORD.contains(&b))).then_some(out)
}

/// A new random claim code, for the fixture.
pub fn new_claim_code() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 10];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let bits = u128::from_be_bytes([[0u8; 6].as_slice(), &bytes].concat().try_into().unwrap());
    (0..CLAIM_CODE_LEN).rev().map(|i| CROCKFORD[((bits >> (5 * i)) & 31) as usize] as char).collect()
}

/// K_claim: HKDF of the normalized claim code with the device id.
pub fn claim_key(code: &str, device_id: &[u8; 8]) -> [u8; 32] {
    hkdf(code.as_bytes(), &[b"claim", device_id], 32).try_into().unwrap()
}

/// A bind's proof that its sender holds the claim code.
pub fn bind_proof(claim_key: &[u8; 32], device_id: &[u8; 8], nonce: &[u8; 8], server_key: &[u8; 32], herd: &[u8]) -> [u8; 16] {
    let len = [u8::try_from(herd.len()).expect("herd id under 256 bytes")];
    hmac_sha256(claim_key, &[b"bind", device_id, nonce, server_key, &len, herd])[..16].try_into().unwrap()
}

/// A collar's check of a bind's claim: its own current nonce, and the proof
/// for its claim code. Either wrong is `bad_claim`.
pub fn check_bind(bind: &super::command::Bind, herd: &[u8], device_id: &[u8; 8], nonce: &[u8; 8], claim_code: &str) -> Result<(), Reject> {
    let key = claim_key(claim_code, device_id);
    let want = bind_proof(&key, device_id, nonce, &bind.server_key, herd);
    // Constant time over the proof.
    let diff = want.iter().zip(bind.proof).fold(0u8, |acc, (a, b)| acc | (a ^ b));
    if bind.nonce == *nonce && diff == 0 { Ok(()) } else { Err(Reject::BadClaim) }
}

/// The factory record's `claim_check`: tells a right claim code from a typo.
pub fn claim_check(claim_key: &[u8; 32]) -> [u8; 16] {
    hmac_sha256(claim_key, &[b"check"])[..16].try_into().unwrap()
}

/// A list of short texts (caps): every element text of at most 32 bytes.
fn text_list(v: &Value) -> Option<Vec<String>> {
    v.as_array()?.iter().map(|c| c.as_text().filter(|t| t.len() <= 32).map(str::to_owned)).collect()
}

// ---------------------------------------------------------------- card

/// What the card's QR code says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub serial: String,
    pub device_key: [u8; 32],
    pub claim_code: String,
}

impl Card {
    /// `OC2:<serial>:<device key, base64url>:<claim code>`
    pub fn text(&self) -> String {
        format!("OC2:{}:{}:{}", self.serial, URL_SAFE_NO_PAD.encode(self.device_key), self.claim_code)
    }

    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.trim().split(':');
        if parts.next()? != "OC2" {
            return None;
        }
        let serial = parts.next().filter(|s| serial_ok(s))?.to_owned();
        let device_key: [u8; 32] = URL_SAFE_NO_PAD.decode(parts.next()?).ok()?.try_into().ok()?;
        let claim_code = normalize_claim_code(parts.next()?)?;
        Some(Self { serial, device_key, claim_code })
    }
}

/// 1–32 characters of `A–Z 0–9 -`.
pub fn serial_ok(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}

// ---------------------------------------------------------------- factory record

/// The factory record (message type 64), signed by the maker key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactoryRecord {
    /// 1
    pub serial: String,
    /// 2
    pub class: DeviceClass,
    /// 3
    pub hw_rev: String,
    /// 4
    pub device_key: [u8; 32],
    /// 5
    pub fw: String,
    /// 6
    pub made_at: u64,
    /// 7
    pub maker: String,
    /// 8
    pub claim_check: Option<[u8; 16]>,
    /// 9: `{1 lora_region, 2 ble_addr, 3 imei, 4 iccid}`
    pub radio: Option<IntMap>,
    /// 10
    pub image_keys: Vec<[u8; 8]>,
    /// 11: `{1 fixture, 2 passed, 3 report, 4 at}`
    pub test: Option<IntMap>,
    /// 12
    pub caps: Vec<String>,
}

impl FactoryRecord {
    pub fn payload(&self) -> Vec<u8> {
        let mut m = IntMap::new();
        m.put(0, Value::uint(MessageType::Factory.code()))
            .put(1, Value::text(self.serial.clone()))
            .put(2, Value::uint(self.class.code()))
            .put(3, Value::text(self.hw_rev.clone()))
            .put(4, Value::bytes(self.device_key.to_vec()))
            .put(5, Value::text(self.fw.clone()))
            .put(6, Value::uint(self.made_at))
            .put(7, Value::text(self.maker.clone()))
            .put_opt(8, self.claim_check.map(|c| Value::bytes(c.to_vec())))
            .put_opt(9, self.radio.clone().map(IntMap::into_value))
            .put(10, Value::Array(self.image_keys.iter().map(|k| Value::bytes(k.to_vec())).collect()))
            .put_opt(11, self.test.clone().map(IntMap::into_value));
        if !self.caps.is_empty() {
            m.put(12, Value::Array(self.caps.iter().cloned().map(Value::Text).collect()));
        }
        cbor::encode(&m.into_value())
    }

    pub fn sign(&self, maker_key: &SigningKey) -> Vec<u8> {
        cose::sign(&self.payload(), maker_key)
    }

    /// Read a signed record without checking the maker's signature (an
    /// unknown maker's record is still usable, shown as unverified); returns
    /// the maker kid with it. Check it with [`Sign1::verify`].
    pub fn read(bytes: &[u8]) -> Result<(Sign1, Self), Reject> {
        let env = Sign1::parse(bytes)?;
        let m = env.payload_map()?;
        let bad = || Reject::BadCbor;
        if m.get(0).and_then(Value::as_u64) != Some(MessageType::Factory.code()) {
            return Err(bad());
        }
        let text = |k, max: usize| m.get(k).and_then(Value::as_text).filter(|t| t.len() <= max).map(str::to_owned).ok_or_else(bad);
        let serial = text(1, 32)?;
        if !serial_ok(&serial) {
            return Err(bad());
        }
        let rec = Self {
            serial,
            class: DeviceClass::from_code(m.get(2).and_then(Value::as_u64).ok_or_else(bad)?),
            hw_rev: text(3, 16)?,
            device_key: m.get(4).and_then(Value::as_bytes).and_then(|k| k.try_into().ok()).ok_or_else(bad)?,
            fw: text(5, 32)?,
            made_at: m.get(6).and_then(Value::as_u64).ok_or_else(bad)?,
            maker: text(7, 64)?,
            // Present means it must read (Astra 01 F14: malformed image keys
            // and caps were dropped one by one).
            claim_check: m.get(8).map(|c| c.as_bytes().and_then(|c| c.try_into().ok()).ok_or_else(bad)).transpose()?,
            radio: m.get(9).map(|r| IntMap::from_value(r).ok_or_else(bad)).transpose()?,
            image_keys: m
                .get(10)
                .and_then(Value::as_array)
                .and_then(|a| a.iter().map(|k| k.as_bytes()?.try_into().ok()).collect::<Option<_>>())
                .ok_or_else(bad)?,
            test: m.get(11).map(|t| IntMap::from_value(t).ok_or_else(bad)).transpose()?,
            caps: m.get(12).map(|c| text_list(c).ok_or_else(bad)).transpose()?.unwrap_or_default(),
        };
        Ok((env, rec))
    }

    pub fn device_id(&self) -> [u8; 8] {
        kid(&self.device_key)
    }

    pub fn to_json(&self) -> Json {
        let names = |m: &IntMap, table: &[(u64, &str)]| {
            Json::Object(
                m.0.iter()
                    .map(|(k, v)| (table.iter().find(|(n, _)| n == k).map_or_else(|| k.to_string(), |(_, s)| s.to_string()), super::json::value_json(v)))
                    .collect(),
            )
        };
        let mut o = json!({
            "type": "factory",
            "serial": self.serial,
            "class": self.class.to_json(),
            "hw_rev": self.hw_rev,
            "device_key": hex(&self.device_key),
            "device_id": hex(&self.device_id()),
            "fw": self.fw,
            "made_at": self.made_at,
            "maker": self.maker,
            "image_keys": self.image_keys.iter().map(|k| hex(k)).collect::<Vec<_>>(),
        });
        if let Some(c) = self.claim_check {
            o["claim_check"] = json!(hex(&c));
        }
        if let Some(r) = &self.radio {
            o["radio"] = names(r, &[(1, "lora_region"), (2, "ble_addr"), (3, "imei"), (4, "iccid")]);
        }
        if let Some(t) = &self.test {
            o["test"] = names(t, &[(1, "fixture"), (2, "passed"), (3, "report"), (4, "at")]);
        }
        if !self.caps.is_empty() {
            o["caps"] = json!(self.caps);
        }
        o
    }
}

// ---------------------------------------------------------------- hello

/// A collar introducing itself (message type 67), signed by its device key,
/// whose kid is the collar's device id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// 16
    pub class: DeviceClass,
    /// 17
    pub fw: String,
    /// 18: present while unclaimed.
    pub nonce: Option<[u8; 8]>,
    /// 19
    pub caps: Vec<String>,
    /// 20
    pub device_key: [u8; 32],
}

impl Hello {
    pub fn payload(&self) -> Vec<u8> {
        let mut m = IntMap::new();
        m.put(0, Value::uint(MessageType::Hello.code()))
            .put(16, Value::uint(self.class.code()))
            .put(17, Value::text(self.fw.clone()))
            .put_opt(18, self.nonce.map(|n| Value::bytes(n.to_vec())))
            .put(19, Value::Array(self.caps.iter().cloned().map(Value::Text).collect()))
            .put(20, Value::bytes(self.device_key.to_vec()));
        cbor::encode(&m.into_value())
    }

    pub fn sign(&self, device_key: &SigningKey) -> Vec<u8> {
        cose::sign(&self.payload(), device_key)
    }

    /// Check a signed hello against the key it carries (the kid must be that
    /// key's, so the device id can't be borrowed).
    pub fn open(bytes: &[u8]) -> Result<([u8; 8], Self), Reject> {
        let env = Sign1::parse(bytes)?;
        let m = env.payload_map()?;
        if m.get(0).and_then(Value::as_u64) != Some(MessageType::Hello.code()) {
            return Err(Reject::BadCbor);
        }
        let device_key: [u8; 32] = m.get(20).and_then(Value::as_bytes).and_then(|k| k.try_into().ok()).ok_or(Reject::BadCbor)?;
        env.verify(&VerifyingKey::from_bytes(&device_key).map_err(|_| Reject::BadSig)?)?;
        let hello = Self {
            class: DeviceClass::from_code(m.get(16).and_then(Value::as_u64).ok_or(Reject::BadCbor)?),
            fw: m.get(17).and_then(Value::as_text).ok_or(Reject::BadCbor)?.to_owned(),
            nonce: m.get(18).map(|n| n.as_bytes().and_then(|n| n.try_into().ok()).ok_or(Reject::BadCbor)).transpose()?,
            caps: m.get(19).map(|c| text_list(c).ok_or(Reject::BadCbor)).transpose()?.unwrap_or_default(),
            device_key,
        };
        Ok((env.kid, hello))
    }

    pub fn to_json(&self) -> Json {
        let mut o = json!({
            "type": "hello",
            "class": self.class.to_json(),
            "fw": self.fw,
            "caps": self.caps,
            "device_key": hex(&self.device_key),
            "device_id": hex(&kid(&self.device_key)),
        });
        if let Some(n) = self.nonce {
            o["nonce"] = json!(hex(&n));
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_codes() {
        assert_eq!(normalize_claim_code("7q4k-9m2x h8d3-tv5w").as_deref(), Some("7Q4K9M2XH8D3TV5W"));
        assert_eq!(normalize_claim_code("IO00-0000-0000-0000").as_deref(), Some("1000000000000000"));
        assert_eq!(normalize_claim_code("U000000000000000"), None, "U isn't Crockford");
        assert_eq!(normalize_claim_code("123"), None);
        let c = new_claim_code();
        assert_eq!(normalize_claim_code(&c), Some(c));
    }

    #[test]
    fn v1_herd_ids_map_to_16_bytes() {
        let a = herd_from_v1("herd_01JB3K9M2XH8D3TV5W7Q4K9M2X");
        assert_eq!(a, herd_from_v1("herd_01JB3K9M2XH8D3TV5W7Q4K9M2X"));
        assert_ne!(a, herd_from_v1("herd_01JB3K9M2XH8D3TV5W7Q4K9M2Y"));
    }

    #[test]
    fn card_round_trip() {
        let card = Card { serial: "OCN-2611-00123".into(), device_key: [7; 32], claim_code: "7Q4K9M2XH8D3TV5W".into() };
        assert_eq!(Card::parse(&card.text()), Some(card.clone()));
        assert_eq!(Card::parse(&format!("{}:future", card.text())), Some(card), "later fields are ignored");
        assert_eq!(Card::parse("OC1:x:y:z"), None);
    }

    #[test]
    fn hello_binds_its_key() {
        let k = SigningKey::from_bytes(&[4; 32]);
        let h =
            Hello { class: DeviceClass::Node, fw: "2.0.0".into(), nonce: Some([1; 8]), caps: vec!["holes".into()], device_key: k.verifying_key().to_bytes() };
        let (id, back) = Hello::open(&h.sign(&k)).unwrap();
        assert_eq!((id, back), (device_id(&k.verifying_key()), h.clone()));
        let thief = SigningKey::from_bytes(&[5; 32]);
        assert!(Hello::open(&h.sign(&thief)).is_err(), "another key can't speak for this device");
    }
}
