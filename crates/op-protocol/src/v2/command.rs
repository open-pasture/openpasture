//! Commands: what a collar obeys (message types 1–63). Each is a COSE_Sign1
//! whose payload is a deterministic CBOR map. Keys 0–15 are common to every
//! command; 16 and up belong to the type. Unknown keys are ignored.
//!
//! | Key | Name | |
//! |---|---|---|
//! | 0 | `type` | required |
//! | 1 | `herd` | required: 1–16 bytes |
//! | 2 | `device` | 8 bytes; present = for that collar only |
//! | 3 | `version` | required: 1 to 2³² − 1, increasing per collar and type |
//! | 4 | `issued_at` | required: Unix seconds |
//! | 5 | `expires_at` | Unix seconds; refused (`expired`) at or after it |
//! | 6 | `ref` | up to 16 bytes, the server's own id for it |
//! | 7 | `effective_at` | Unix seconds; when it takes effect (GNSS time) |

use ed25519_dalek::{SigningKey, VerifyingKey};
use op_geo::{CollarLimits, LonLat};
use serde_json::{Value as Json, json};

use super::cbor::{self, IntMap, Value};
use super::cose::{self, Sign1};
use super::crypto::{kid, sha256};
use super::json::{hex, int_map_json};
use super::registry::{CueMode, MessageType, Reject, Role};

/// Largest signed command, in bytes.
pub const MAX_COMMAND_BYTES: usize = 12_288;
/// Longest herd id, in bytes.
pub const MAX_HERD_BYTES: usize = 16;
/// Longest `ref`, in bytes.
pub const MAX_REF_BYTES: usize = 16;
/// Longest endpoint URL, in bytes.
pub const MAX_ENDPOINT_BYTES: usize = 256;

/// A ring: `[lon, lat]` in units of 10⁻⁷ degrees, unclosed. On the wire the
/// first vertex is absolute and each next one a difference from the one
/// before, all in one flat integer array.
pub type Ring = Vec<[i32; 2]>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub herd: Vec<u8>,
    pub device: Option<[u8; 8]>,
    pub version: u32,
    pub issued_at: u64,
    pub expires_at: Option<u64>,
    pub reference: Option<Vec<u8>>,
    pub effective_at: Option<u64>,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Boundary(Boundary),
    Config(Config),
    Membership(Membership),
    Roster(Roster),
    Bind(Bind),
    Handover(Handover),
    /// The new key proves itself; key 16 is the kid it replaces.
    Confirm {
        replaces: [u8; 8],
    },
    /// Key 16: the new owner key.
    Owner {
        owner_key: [u8; 32],
    },
    Firmware(FirmwareUpdate),
    /// Send the records from `from` to `to` (seq, inclusive) again.
    Resend {
        from: u32,
        to: u32,
    },
    Release,
    /// A type this build doesn't implement. A collar acks it `unsupported`.
    Other {
        message_type: u64,
        fields: IntMap,
    },
}

impl Body {
    pub fn message_type(&self) -> MessageType {
        match self {
            Self::Boundary(_) => MessageType::Boundary,
            Self::Config(_) => MessageType::Config,
            Self::Membership(_) => MessageType::Membership,
            Self::Roster(_) => MessageType::Roster,
            Self::Bind(_) => MessageType::Bind,
            Self::Handover(_) => MessageType::Handover,
            Self::Confirm { .. } => MessageType::Confirm,
            Self::Owner { .. } => MessageType::Owner,
            Self::Firmware(_) => MessageType::Firmware,
            Self::Resend { .. } => MessageType::Resend,
            Self::Release => MessageType::Release,
            Self::Other { message_type, .. } => MessageType::from_code(*message_type),
        }
    }
}

/// Type 1: the fence. Herd-wide, or one collar's own (`device`: escape
/// pens, per-collar copies).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Boundary {
    /// 16
    pub outer: Ring,
    /// 17, absent when empty.
    pub holes: Vec<Ring>,
    /// 18, absent for `audio`.
    pub cue_mode: Option<CueMode>,
    /// 19: warning zone width, centimetres (collar default 500).
    pub warn_cm: Option<u32>,
    /// 20: hysteresis, centimetres (collar default 100).
    pub hyst_cm: Option<u32>,
}

/// Type 2: one collar's settings (`device` required).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// 16: base report interval, 10–3600 s.
    pub report_s: u32,
    /// 17: GNSS fix interval, 1–3600 s.
    pub fix_s: Option<u32>,
    /// 18, 19: the fast window's intervals.
    pub fast_report_s: Option<u32>,
    pub fast_fix_s: Option<u32>,
    /// 20: fast until this GNSS time, then base again with no further command.
    pub fast_until: Option<u64>,
    /// 21: anchors, stations and direct collars: where to send batches.
    pub endpoint: Option<String>,
    /// 22: SHA-256 of the server's TLS SubjectPublicKeyInfo.
    pub endpoint_pin: Option<[u8; 32]>,
    /// 23: send fix history every this many seconds (0 or absent: never).
    pub track_s: Option<u32>,
    /// 24: anchors: how long one holds the herd's link before handing it on.
    pub anchor_hold_s: Option<u32>,
}

/// Type 3: which herd a collar is in, and its address there (`device`
/// required; `herd` is the herd it joins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    /// 16: 1–65534.
    pub short: u16,
    /// 17
    pub role: Role,
    /// 18: the herd's LoRa network id.
    pub net: u16,
    /// 19: channel plan (registry), default 1.
    pub plan: Option<u32>,
    /// 20: report cycle on LoRa, a multiple of 60 s, default 300.
    pub cycle_s: Option<u32>,
    /// 21: anchors: order of takeover, lower first.
    pub rank: Option<u32>,
}

/// Type 4: the herd's collars, for its anchors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roster {
    /// 16: `[short, device, role]` each.
    pub members: Vec<Member>,
    /// 17: device ids whose frames anchors drop.
    pub revoked: Vec<[u8; 8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub short: u16,
    pub device: [u8; 8],
    pub role: Role,
}

/// Type 5: claim an unclaimed collar (see `identity.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    /// 16; the envelope's kid must be its kid.
    pub server_key: [u8; 32],
    /// 17
    pub proof: [u8; 16],
    /// 18: the collar's claim nonce.
    pub nonce: [u8; 8],
    /// 19
    pub endpoint: Option<String>,
    /// 20
    pub owner_key: Option<[u8; 32]>,
    /// 21
    pub endpoint_pin: Option<[u8; 32]>,
}

/// Type 6: propose a new server key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handover {
    /// 16
    pub new_key: [u8; 32],
    /// 17: confirm by this GNSS time, else the collar keeps the old key.
    pub deadline: u64,
    /// 18
    pub endpoint: Option<String>,
    /// 19
    pub endpoint_pin: Option<[u8; 32]>,
    /// 20: 1 rotate, 2 transfer; for people only.
    pub reason: Option<u64>,
}

/// Type 9: install a release (see `firmware.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareUpdate {
    /// 16: the release manifest, a COSE_Sign1 by a release or image key, as is.
    pub manifest: Vec<u8>,
    /// 17: spread installs over this many seconds after `effective_at`.
    pub window_s: Option<u32>,
    /// 18: where anchors fetch the image; default `<endpoint>/fw/<digest hex>`.
    pub url: Option<String>,
}

// ---------------------------------------------------------------- encoding

fn ring_value(r: &Ring) -> Value {
    let mut out = Vec::with_capacity(r.len() * 2);
    let mut prev = [0i64; 2];
    for (i, p) in r.iter().enumerate() {
        let cur = [i64::from(p[0]), i64::from(p[1])];
        let d = if i == 0 { cur } else { [cur[0] - prev[0], cur[1] - prev[1]] };
        out.push(Value::int(d[0]));
        out.push(Value::int(d[1]));
        prev = cur;
    }
    Value::Array(out)
}

fn ring_from(v: &Value) -> Result<Ring, Reject> {
    let items = v.as_array().ok_or(Reject::BadCbor)?;
    if items.len() % 2 != 0 {
        return Err(Reject::BadCbor);
    }
    let mut acc = [0i64; 2];
    let mut ring = Vec::with_capacity(items.len() / 2);
    for (i, pair) in items.chunks(2).enumerate() {
        let d = [pair[0].as_i64().ok_or(Reject::BadCbor)?, pair[1].as_i64().ok_or(Reject::BadCbor)?];
        acc = if i == 0 { d } else { [acc[0].checked_add(d[0]).ok_or(Reject::OutOfRange)?, acc[1].checked_add(d[1]).ok_or(Reject::OutOfRange)?] };
        let p = [i32::try_from(acc[0]).map_err(|_| Reject::OutOfRange)?, i32::try_from(acc[1]).map_err(|_| Reject::OutOfRange)?];
        ring.push(p);
    }
    Ok(ring)
}

fn bytes_n<const N: usize>(v: Option<&Value>) -> Result<[u8; N], Reject> {
    v.and_then(Value::as_bytes).and_then(|b| b.try_into().ok()).ok_or(Reject::BadCbor)
}

fn opt_bytes_n<const N: usize>(v: Option<&Value>) -> Result<Option<[u8; N]>, Reject> {
    v.map(|v| bytes_n(Some(v))).transpose()
}

fn uint(v: Option<&Value>) -> Result<u64, Reject> {
    v.and_then(Value::as_u64).ok_or(Reject::BadCbor)
}

fn opt_u64(v: Option<&Value>) -> Result<Option<u64>, Reject> {
    v.map(|v| v.as_u64().ok_or(Reject::BadCbor)).transpose()
}

fn opt_u32(v: Option<&Value>) -> Result<Option<u32>, Reject> {
    v.map(|v| v.as_u32().ok_or(Reject::BadCbor)).transpose()
}

fn opt_text(v: Option<&Value>, max: usize) -> Result<Option<String>, Reject> {
    v.map(|v| v.as_text().filter(|t| t.len() <= max).map(str::to_owned).ok_or(Reject::BadCbor)).transpose()
}

fn b(v: &[u8]) -> Value {
    Value::bytes(v.to_vec())
}

impl Command {
    /// The deterministic CBOR payload (what gets signed).
    pub fn payload(&self) -> Vec<u8> {
        let mut m = IntMap::new();
        m.put(0, Value::uint(self.body.message_type().code()))
            .put(1, b(&self.herd))
            .put_opt(2, self.device.map(|d| b(&d)))
            .put(3, Value::uint(self.version))
            .put(4, Value::uint(self.issued_at))
            .put_opt(5, self.expires_at.map(Value::uint))
            .put_opt(6, self.reference.as_deref().map(b))
            .put_opt(7, self.effective_at.map(Value::uint));
        match &self.body {
            Body::Boundary(x) => {
                m.put(16, ring_value(&x.outer))
                    .put_opt(17, (!x.holes.is_empty()).then(|| Value::Array(x.holes.iter().map(ring_value).collect())))
                    .put_opt(18, x.cue_mode.filter(|c| *c != CueMode::Audio).map(|c| Value::uint(c.code())))
                    .put_opt(19, x.warn_cm.map(Value::uint))
                    .put_opt(20, x.hyst_cm.map(Value::uint));
            }
            Body::Config(x) => {
                m.put(16, Value::uint(x.report_s))
                    .put_opt(17, x.fix_s.map(Value::uint))
                    .put_opt(18, x.fast_report_s.map(Value::uint))
                    .put_opt(19, x.fast_fix_s.map(Value::uint))
                    .put_opt(20, x.fast_until.map(Value::uint))
                    .put_opt(21, x.endpoint.clone().map(Value::Text))
                    .put_opt(22, x.endpoint_pin.map(|p| b(&p)))
                    .put_opt(23, x.track_s.map(Value::uint))
                    .put_opt(24, x.anchor_hold_s.map(Value::uint));
            }
            Body::Membership(x) => {
                m.put(16, Value::uint(x.short))
                    .put(17, Value::uint(x.role.code()))
                    .put(18, Value::uint(x.net))
                    .put_opt(19, x.plan.map(Value::uint))
                    .put_opt(20, x.cycle_s.map(Value::uint))
                    .put_opt(21, x.rank.map(Value::uint));
            }
            Body::Roster(x) => {
                let members = x.members.iter().map(|p| Value::Array(vec![Value::uint(p.short), b(&p.device), Value::uint(p.role.code())])).collect();
                m.put(16, Value::Array(members));
                if !x.revoked.is_empty() {
                    m.put(17, Value::Array(x.revoked.iter().map(|d| b(d)).collect()));
                }
            }
            Body::Bind(x) => {
                m.put(16, b(&x.server_key))
                    .put(17, b(&x.proof))
                    .put(18, b(&x.nonce))
                    .put_opt(19, x.endpoint.clone().map(Value::Text))
                    .put_opt(20, x.owner_key.map(|k| b(&k)))
                    .put_opt(21, x.endpoint_pin.map(|p| b(&p)));
            }
            Body::Handover(x) => {
                m.put(16, b(&x.new_key))
                    .put(17, Value::uint(x.deadline))
                    .put_opt(18, x.endpoint.clone().map(Value::Text))
                    .put_opt(19, x.endpoint_pin.map(|p| b(&p)))
                    .put_opt(20, x.reason.map(Value::uint));
            }
            Body::Confirm { replaces } => {
                m.put(16, b(replaces));
            }
            Body::Owner { owner_key } => {
                m.put(16, b(owner_key));
            }
            Body::Firmware(x) => {
                m.put(16, b(&x.manifest)).put_opt(17, x.window_s.map(Value::uint)).put_opt(18, x.url.clone().map(Value::Text));
            }
            Body::Resend { from, to } => {
                m.put(16, Value::uint(*from)).put(17, Value::uint(*to));
            }
            Body::Release => {}
            Body::Other { fields, .. } => {
                for (k, v) in fields.0.iter().filter(|(k, _)| *k >= 16) {
                    m.put(*k, v.clone());
                }
            }
        }
        cbor::encode(&m.into_value())
    }

    /// Sign with `key`: the COSE_Sign1 bytes that travel.
    pub fn sign(&self, key: &SigningKey) -> Vec<u8> {
        cose::sign(&self.payload(), key)
    }

    /// Read a payload map. Missing or mistyped fields are `bad_cbor`; an
    /// unknown type comes back as [`Body::Other`].
    pub fn from_payload(m: &IntMap) -> Result<Self, Reject> {
        let t = MessageType::from_code(uint(m.get(0))?);
        if !t.is_command() {
            return Err(Reject::BadCbor);
        }
        let herd = m.get(1).and_then(Value::as_bytes).filter(|h| (1..=MAX_HERD_BYTES).contains(&h.len())).ok_or(Reject::BadCbor)?.to_vec();
        let version = m.get(3).and_then(Value::as_u32).filter(|v| *v >= 1).ok_or(Reject::BadCbor)?;
        let reference = m.get(6).map(|v| v.as_bytes().filter(|r| r.len() <= MAX_REF_BYTES).map(<[u8]>::to_vec).ok_or(Reject::BadCbor)).transpose()?;
        let body = match t {
            MessageType::Boundary => Body::Boundary(Boundary {
                outer: ring_from(m.get(16).ok_or(Reject::BadCbor)?)?,
                holes: match m.get(17) {
                    Some(v) => v.as_array().ok_or(Reject::BadCbor)?.iter().map(ring_from).collect::<Result<_, _>>()?,
                    None => Vec::new(),
                },
                cue_mode: opt_u64(m.get(18))?.map(CueMode::from_code),
                warn_cm: opt_u32(m.get(19))?,
                hyst_cm: opt_u32(m.get(20))?,
            }),
            MessageType::Config => Body::Config(Config {
                report_s: m.get(16).and_then(Value::as_u32).ok_or(Reject::BadCbor)?,
                fix_s: opt_u32(m.get(17))?,
                fast_report_s: opt_u32(m.get(18))?,
                fast_fix_s: opt_u32(m.get(19))?,
                fast_until: opt_u64(m.get(20))?,
                endpoint: opt_text(m.get(21), MAX_ENDPOINT_BYTES)?,
                endpoint_pin: opt_bytes_n(m.get(22))?,
                track_s: opt_u32(m.get(23))?,
                anchor_hold_s: opt_u32(m.get(24))?,
            }),
            MessageType::Membership => Body::Membership(Membership {
                short: m.get(16).and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()).ok_or(Reject::BadCbor)?,
                role: Role::from_code(uint(m.get(17))?),
                net: m.get(18).and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()).ok_or(Reject::BadCbor)?,
                plan: opt_u32(m.get(19))?,
                cycle_s: opt_u32(m.get(20))?,
                rank: opt_u32(m.get(21))?,
            }),
            MessageType::Roster => {
                let members = m
                    .get(16)
                    .and_then(Value::as_array)
                    .ok_or(Reject::BadCbor)?
                    .iter()
                    .map(|p| {
                        let [short, device, role, ..] = p.as_array().ok_or(Reject::BadCbor)? else { return Err(Reject::BadCbor) };
                        Ok(Member {
                            short: short.as_u64().and_then(|s| u16::try_from(s).ok()).ok_or(Reject::BadCbor)?,
                            device: bytes_n(Some(device))?,
                            role: Role::from_code(uint(Some(role))?),
                        })
                    })
                    .collect::<Result<_, _>>()?;
                let revoked = match m.get(17) {
                    Some(v) => v.as_array().ok_or(Reject::BadCbor)?.iter().map(|d| bytes_n(Some(d))).collect::<Result<_, _>>()?,
                    None => Vec::new(),
                };
                Body::Roster(Roster { members, revoked })
            }
            MessageType::Bind => Body::Bind(Bind {
                server_key: bytes_n(m.get(16))?,
                proof: bytes_n(m.get(17))?,
                nonce: bytes_n(m.get(18))?,
                endpoint: opt_text(m.get(19), MAX_ENDPOINT_BYTES)?,
                owner_key: opt_bytes_n(m.get(20))?,
                endpoint_pin: opt_bytes_n(m.get(21))?,
            }),
            MessageType::Handover => Body::Handover(Handover {
                new_key: bytes_n(m.get(16))?,
                deadline: uint(m.get(17))?,
                endpoint: opt_text(m.get(18), MAX_ENDPOINT_BYTES)?,
                endpoint_pin: opt_bytes_n(m.get(19))?,
                reason: opt_u64(m.get(20))?,
            }),
            MessageType::Confirm => Body::Confirm { replaces: bytes_n(m.get(16))? },
            MessageType::Owner => Body::Owner { owner_key: bytes_n(m.get(16))? },
            MessageType::Firmware => Body::Firmware(FirmwareUpdate {
                manifest: m.get(16).and_then(Value::as_bytes).ok_or(Reject::BadCbor)?.to_vec(),
                window_s: opt_u32(m.get(17))?,
                url: opt_text(m.get(18), MAX_ENDPOINT_BYTES)?,
            }),
            MessageType::Resend => {
                Body::Resend { from: m.get(16).and_then(Value::as_u32).ok_or(Reject::BadCbor)?, to: m.get(17).and_then(Value::as_u32).ok_or(Reject::BadCbor)? }
            }
            MessageType::Release => Body::Release,
            other => Body::Other { message_type: other.code(), fields: IntMap(m.0.iter().filter(|(k, _)| *k >= 16).cloned().collect()) },
        };
        Ok(Self {
            herd,
            device: opt_bytes_n(m.get(2))?,
            version,
            issued_at: uint(m.get(4))?,
            expires_at: opt_u64(m.get(5))?,
            reference,
            effective_at: opt_u64(m.get(7))?,
            body,
        })
    }

    /// The JSON view.
    pub fn to_json(&self) -> Json {
        let mut o = serde_json::Map::new();
        o.insert("type".into(), json!(self.body.message_type().name()));
        if let Body::Other { message_type, .. } = &self.body {
            o.insert("type_code".into(), json!(message_type));
        }
        o.insert("herd".into(), json!(hex(&self.herd)));
        if let Some(d) = &self.device {
            o.insert("device".into(), json!(hex(d)));
        }
        o.insert("version".into(), json!(self.version));
        o.insert("issued_at".into(), json!(self.issued_at));
        if let Some(t) = self.expires_at {
            o.insert("expires_at".into(), json!(t));
        }
        if let Some(r) = &self.reference {
            o.insert("ref".into(), json!(hex(r)));
        }
        if let Some(t) = self.effective_at {
            o.insert("effective_at".into(), json!(t));
        }
        let mut put = |k: &str, v: Option<Json>| {
            if let Some(v) = v {
                o.insert(k.into(), v);
            }
        };
        let ring = |r: &Ring| json!(r);
        let hexed = |v: Option<&[u8]>| v.map(|v| json!(hex(v)));
        match &self.body {
            Body::Boundary(x) => {
                put("outer", Some(ring(&x.outer)));
                put("holes", (!x.holes.is_empty()).then(|| Json::Array(x.holes.iter().map(ring).collect())));
                put("cue_mode", x.cue_mode.filter(|c| *c != CueMode::Audio).map(|c| json!(c.name())));
                put("warn_cm", x.warn_cm.map(|v| json!(v)));
                put("hyst_cm", x.hyst_cm.map(|v| json!(v)));
            }
            Body::Config(x) => {
                put("report_s", Some(json!(x.report_s)));
                put("fix_s", x.fix_s.map(|v| json!(v)));
                put("fast_report_s", x.fast_report_s.map(|v| json!(v)));
                put("fast_fix_s", x.fast_fix_s.map(|v| json!(v)));
                put("fast_until", x.fast_until.map(|v| json!(v)));
                put("endpoint", x.endpoint.as_ref().map(|v| json!(v)));
                put("endpoint_pin", hexed(x.endpoint_pin.as_ref().map(|p| &p[..])));
                put("track_s", x.track_s.map(|v| json!(v)));
                put("anchor_hold_s", x.anchor_hold_s.map(|v| json!(v)));
            }
            Body::Membership(x) => {
                put("short", Some(json!(x.short)));
                put("role", Some(json!(x.role.name())));
                put("net", Some(json!(x.net)));
                put("plan", x.plan.map(|v| json!(v)));
                put("cycle_s", x.cycle_s.map(|v| json!(v)));
                put("rank", x.rank.map(|v| json!(v)));
            }
            Body::Roster(x) => {
                put(
                    "members",
                    Some(Json::Array(x.members.iter().map(|p| json!({"short": p.short, "device": hex(&p.device), "role": p.role.name()})).collect())),
                );
                put("revoked", (!x.revoked.is_empty()).then(|| Json::Array(x.revoked.iter().map(|d| json!(hex(d))).collect())));
            }
            Body::Bind(x) => {
                put("server_key", Some(json!(hex(&x.server_key))));
                put("proof", Some(json!(hex(&x.proof))));
                put("nonce", Some(json!(hex(&x.nonce))));
                put("endpoint", x.endpoint.as_ref().map(|v| json!(v)));
                put("owner_key", hexed(x.owner_key.as_ref().map(|k| &k[..])));
                put("endpoint_pin", hexed(x.endpoint_pin.as_ref().map(|p| &p[..])));
            }
            Body::Handover(x) => {
                put("new_key", Some(json!(hex(&x.new_key))));
                put("deadline", Some(json!(x.deadline)));
                put("endpoint", x.endpoint.as_ref().map(|v| json!(v)));
                put("endpoint_pin", hexed(x.endpoint_pin.as_ref().map(|p| &p[..])));
                put(
                    "reason",
                    x.reason.map(|v| {
                        json!(match v {
                            1 => "rotate",
                            2 => "transfer",
                            _ => "other",
                        })
                    }),
                );
            }
            Body::Confirm { replaces } => put("replaces", Some(json!(hex(replaces)))),
            Body::Owner { owner_key } => put("owner_key", Some(json!(hex(owner_key)))),
            Body::Firmware(x) => {
                put("manifest", Some(json!(hex(&x.manifest))));
                put("window_s", x.window_s.map(|v| json!(v)));
                put("url", x.url.as_ref().map(|v| json!(v)));
            }
            Body::Resend { from, to } => {
                put("from", Some(json!(from)));
                put("to", Some(json!(to)));
            }
            Body::Release => {}
            Body::Other { fields, .. } => put("fields", Some(int_map_json(fields))),
        }
        Json::Object(o)
    }

    /// What a collar checks once the signature verifies, in order:
    /// `wrong_collar` (another collar's), `wrong_herd` (another herd's; a
    /// membership or bind names the herd it moves the collar to, so it is
    /// exempt), then `expired` against `now` (GNSS time; skipped without one).
    pub fn check_scope(&self, device_id: &[u8; 8], herd: Option<&[u8]>, now: Option<u64>) -> Result<(), Reject> {
        if self.device.is_some_and(|d| d != *device_id) {
            return Err(Reject::WrongCollar);
        }
        let moves_herd = matches!(self.body, Body::Membership(_) | Body::Bind(_));
        if !moves_herd && herd.is_some_and(|h| h != self.herd.as_slice()) {
            return Err(Reject::WrongHerd);
        }
        if let (Some(exp), Some(now)) = (self.expires_at, now)
            && now >= exp
        {
            return Err(Reject::Expired);
        }
        Ok(())
    }
}

/// A command as received, after its envelope and signature check out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub command: Command,
    /// The kid it was signed with.
    pub kid: [u8; 8],
    /// First 8 bytes of SHA-256 of the bytes received: names it in acks.
    pub digest: [u8; 8],
}

/// The first 8 bytes of SHA-256 of a signed message's bytes.
pub fn digest(bytes: &[u8]) -> [u8; 8] {
    sha256(&[bytes])[..8].try_into().unwrap()
}

/// Open a signed command as a collar does, in order: `too_large`, the
/// envelope (`bad_cbor`), the key (`unknown_kid`: `key_for` knows no key with
/// that kid; a bind is checked against the key it carries), the signature
/// (`bad_sig`), then the payload (`bad_cbor`). Scope, versions and the
/// type's own rules come after.
pub fn open(bytes: &[u8], key_for: impl Fn(&[u8; 8]) -> Option<VerifyingKey>) -> Result<Received, Reject> {
    if bytes.len() > MAX_COMMAND_BYTES {
        return Err(Reject::TooLarge);
    }
    let env = Sign1::parse(bytes)?;
    let map = env.payload_map();
    // A bind carries its own key: check it against that one.
    let enclosed = map.as_ref().ok().and_then(|m| {
        (m.get(0).and_then(Value::as_u64) == Some(MessageType::Bind.code()))
            .then(|| m.get(16).and_then(Value::as_bytes).and_then(|k| <[u8; 32]>::try_from(k).ok()))
            .flatten()
            .and_then(|k| VerifyingKey::from_bytes(&k).ok())
    });
    let key = match enclosed {
        Some(k) => k,
        None => key_for(&env.kid).ok_or(Reject::UnknownKid)?,
    };
    env.verify(&key)?;
    let command = Command::from_payload(&map?)?;
    Ok(Received { command, kid: env.kid, digest: digest(bytes) })
}

// ---------------------------------------------------------------- the type's own checks

/// Collar defaults when a boundary leaves the margins out.
pub const DEFAULT_WARN_CM: u32 = 500;
pub const DEFAULT_HYST_CM: u32 = 100;

impl Boundary {
    /// From `[lon, lat]` rings in degrees, rounded to 7 decimals.
    pub fn from_lonlat(outer: &[LonLat], holes: &[Vec<LonLat>]) -> Self {
        let e7 = |r: &[LonLat]| r.iter().map(|p| [(p[0] * 1e7).round() as i32, (p[1] * 1e7).round() as i32]).collect::<Ring>();
        Self { outer: e7(outer), holes: holes.iter().map(|h| e7(h)).collect(), ..Default::default() }
    }

    /// The rings in degrees.
    pub fn lonlat(r: &Ring) -> Vec<LonLat> {
        r.iter().map(|p| [f64::from(p[0]) / 1e7, f64::from(p[1]) / 1e7]).collect()
    }

    /// `warn_m` and `hysteresis_m`.
    pub fn margins_m(&self) -> (f64, f64) {
        (f64::from(self.warn_cm.unwrap_or(DEFAULT_WARN_CM)) / 100.0, f64::from(self.hyst_cm.unwrap_or(DEFAULT_HYST_CM)) / 100.0)
    }

    /// The v1 shape rules, in their order (`bad_margins` first). An unknown
    /// cue mode is `bad_cbor`: a collar must not guess how to cue.
    pub fn check(&self, limits: &CollarLimits) -> Result<(), Reject> {
        if self.cue_mode.is_some_and(|c| !c.is_known()) {
            return Err(Reject::BadCbor);
        }
        let (warn_m, hyst_m) = self.margins_m();
        let holes: Vec<Vec<LonLat>> = self.holes.iter().map(Self::lonlat).collect();
        op_geo::shape::check_rings(&Self::lonlat(&self.outer), &holes, limits, warn_m, hyst_m, 0.0).map_err(Reject::from)
    }
}

/// Shortest and longest report interval, seconds.
pub const REPORT_S: std::ops::RangeInclusive<u32> = 10..=3600;
/// Shortest and longest fix interval, seconds.
pub const FIX_S: std::ops::RangeInclusive<u32> = 1..=3600;

impl Config {
    /// `bad_config`: an interval out of range, a fast window without its
    /// report interval, an endpoint that isn't `https://` (bench firmware may
    /// also take `http://`), a track interval under a minute, an anchor hold
    /// outside 10 min–24 h.
    pub fn check(&self, bench: bool) -> Result<(), Reject> {
        let ok = REPORT_S.contains(&self.report_s)
            && self.fast_report_s.is_none_or(|s| REPORT_S.contains(&s))
            && [self.fix_s, self.fast_fix_s].into_iter().flatten().all(|s| FIX_S.contains(&s))
            && (self.fast_until.is_none() || self.fast_report_s.is_some())
            && self.endpoint.as_deref().is_none_or(|e| endpoint_ok(e, bench))
            && self.track_s.is_none_or(|s| s == 0 || (60..=86_400).contains(&s))
            && self.anchor_hold_s.is_none_or(|s| (600..=86_400).contains(&s));
        if ok { Ok(()) } else { Err(Reject::BadConfig) }
    }
}

/// `https://` and a host, at most 256 bytes; `http://` too on bench builds.
pub fn endpoint_ok(e: &str, bench: bool) -> bool {
    let rest = e.strip_prefix("https://").or_else(|| e.strip_prefix("http://").filter(|_| bench));
    e.len() <= MAX_ENDPOINT_BYTES && rest.is_some_and(|h| !h.is_empty())
}

impl Membership {
    /// `bad_config`: a short address of 0 or 65535, an unknown role, a cycle
    /// that isn't a whole number of minutes from 1 to 60.
    pub fn check(&self) -> Result<(), Reject> {
        let ok = (1..=65_534).contains(&self.short) && self.role.is_known() && self.cycle_s.is_none_or(|c| c % 60 == 0 && (60..=3600).contains(&c));
        if ok { Ok(()) } else { Err(Reject::BadConfig) }
    }
}

/// The keys a collar trusts (see `identity.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trust {
    /// The current server key; `None` while unclaimed.
    pub server: Option<VerifyingKey>,
    /// A handover's key, waiting for its confirm.
    pub pending: Option<VerifyingKey>,
    pub owner: Option<VerifyingKey>,
}

impl Trust {
    /// The trusted key with this kid, for [`open`].
    pub fn key_for(&self, kid: &[u8; 8]) -> Option<VerifyingKey> {
        [self.server, self.pending, self.owner].into_iter().flatten().find(|k| key_id(k) == *kid)
    }

    /// Who may sign what, once the signature verifies: a bind only while
    /// unclaimed (`claimed`); a confirm only by the pending key, naming the
    /// current one (`bad_sig`); a handover or release by the server or owner
    /// key; an owner command by the owner key, or the server key while no
    /// owner key is enrolled; everything else by the server key alone
    /// (`unknown_kid`).
    pub fn authorize(&self, r: &Received) -> Result<(), Reject> {
        let is = |k: &Option<VerifyingKey>| k.is_some_and(|k| key_id(&k) == r.kid);
        match &r.command.body {
            Body::Bind(_) if self.server.is_some() => Err(Reject::Claimed),
            Body::Bind(_) => Ok(()),
            Body::Confirm { replaces } => {
                let names_current = self.server.is_some_and(|k| key_id(&k) == *replaces);
                if is(&self.pending) && names_current { Ok(()) } else { Err(Reject::BadSig) }
            }
            Body::Handover(_) | Body::Release if is(&self.server) || is(&self.owner) => Ok(()),
            Body::Owner { .. } if is(&self.owner) || (self.owner.is_none() && is(&self.server)) => Ok(()),
            Body::Owner { .. } | Body::Handover(_) | Body::Release => Err(Reject::UnknownKid),
            _ if is(&self.server) => Ok(()),
            _ => Err(Reject::UnknownKid),
        }
    }
}

/// The kid a signed command claims, without checking anything else.
pub fn signer(bytes: &[u8]) -> Option<[u8; 8]> {
    Sign1::parse(bytes).ok().map(|s| s.kid)
}

/// kid of a raw 32-byte public key.
pub fn key_id(key: &VerifyingKey) -> [u8; 8] {
    kid(key.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&core::array::from_fn(|i| i as u8))
    }

    fn boundary() -> Command {
        Command {
            herd: vec![0x11; 8],
            device: None,
            version: 57,
            issued_at: 1_790_510_400,
            expires_at: None,
            reference: None,
            effective_at: None,
            body: Body::Boundary(Boundary {
                outer: vec![[-936_250_000, 420_300_000], [-936_200_000, 420_300_000], [-936_200_000, 420_336_000], [-936_250_000, 420_336_000]],
                ..Default::default()
            }),
        }
    }

    #[test]
    fn round_trip_and_open() {
        let c = boundary();
        let bytes = c.sign(&key());
        let r = open(&bytes, |k| (*k == key_id(&key().verifying_key())).then(|| key().verifying_key())).unwrap();
        assert_eq!(r.command, c);
        assert_eq!(r.command.payload(), Sign1::parse(&bytes).unwrap().payload, "deterministic");
        assert_eq!(open(&bytes, |_| None).unwrap_err(), Reject::UnknownKid);
    }

    #[test]
    fn ring_deltas() {
        let r: Ring = vec![[10, 20], [15, 18], [-5, 30]];
        let v = ring_value(&r);
        assert_eq!(v, Value::Array([10, 20, 5, -2, -20, 12].map(Value::int).to_vec()));
        assert_eq!(ring_from(&v).unwrap(), r);
        assert_eq!(ring_from(&Value::Array(vec![Value::int(1)])), Err(Reject::BadCbor));
    }

    #[test]
    fn scope() {
        let mut c = boundary();
        let me = [7; 8];
        c.check_scope(&me, Some(&[0x11; 8]), None).unwrap();
        assert_eq!(c.check_scope(&me, Some(&[0x22; 8]), None), Err(Reject::WrongHerd));
        c.device = Some([8; 8]);
        assert_eq!(c.check_scope(&me, Some(&[0x11; 8]), None), Err(Reject::WrongCollar));
        c.device = Some(me);
        c.expires_at = Some(100);
        assert_eq!(c.check_scope(&me, None, Some(100)), Err(Reject::Expired));
        c.check_scope(&me, None, None).unwrap();
    }
}
