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
//!
//! `device` is required for the types that only ever concern one collar
//! (config, membership, bind, resend, release): one without it is `bad_cbor`,
//! never read as "every collar in the herd".

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
    /// The new key proves itself; key 16 is the kid it replaces, key 17 the
    /// digest of the handover it confirms.
    Confirm {
        replaces: [u8; 8],
        handover: [u8; 8],
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
    Zones(super::policy::Zones),
    Lease(super::policy::Lease),
    /// Key 16: a trust update (type 68) signed by a firmware key, for the
    /// bootloader (`firmware.md`).
    Trust {
        update: Vec<u8>,
    },
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
            Self::Zones(_) => MessageType::Zones,
            Self::Lease(_) => MessageType::Lease,
            Self::Trust { .. } => MessageType::Trust,
            Self::Other { message_type, .. } => MessageType::from_code(*message_type),
        }
    }

    /// Types that only ever concern one collar: `device` is required.
    pub fn needs_device(t: MessageType) -> bool {
        matches!(t, MessageType::Config | MessageType::Membership | MessageType::Bind | MessageType::Resend | MessageType::Release)
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
    /// 21: the policy epoch the server checked it against (`policy`).
    pub policy: Option<u32>,
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
    /// 22: groups of short addresses the net uses (`tdma::Schedule`),
    /// 1..=cycle; default 1.
    pub groups: Option<u32>,
    /// 23: the herd's beacon key, wrapped for this collar
    /// (`frame::wrap_beacon_key`).
    pub beacon_key: Option<[u8; super::frame::WRAPPED_KEY_BYTES]>,
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
    /// 20: 1 rotate, 2 transfer. A transfer moves ownership too: on
    /// confirm the owner key becomes `new_owner` (none when absent), and
    /// while an owner key is enrolled only it may sign one.
    pub reason: Option<u64>,
    /// 21: a transfer's new owner key.
    pub new_owner: Option<[u8; 32]>,
}

/// Handover reason 2: a sale, or any change of who owns the collar.
pub const TRANSFER: u64 = 2;
/// Longest a handover may stay pending: 30 days.
pub const MAX_HANDOVER_S: u64 = 30 * 86_400;

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
                    .put_opt(20, x.hyst_cm.map(Value::uint))
                    .put_opt(21, x.policy.map(Value::uint));
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
                    .put_opt(21, x.rank.map(Value::uint))
                    .put_opt(22, x.groups.map(Value::uint))
                    .put_opt(23, x.beacon_key.map(|k| b(&k)));
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
                    .put_opt(20, x.reason.map(Value::uint))
                    .put_opt(21, x.new_owner.map(|k| b(&k)));
            }
            Body::Confirm { replaces, handover } => {
                m.put(16, b(replaces)).put(17, b(handover));
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
            Body::Zones(z) => {
                m.put(16, Value::Array(z.zones.iter().map(ring_value).collect())).put(17, Value::uint(z.epoch));
            }
            Body::Lease(l) => {
                m.put(16, Value::uint(l.epoch)).put(17, Value::uint(l.until)).put(18, b(&l.digest));
            }
            Body::Trust { update } => {
                m.put(16, b(update));
            }
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
        let device = opt_bytes_n(m.get(2))?;
        if device.is_none() && Body::needs_device(t) {
            return Err(Reject::BadCbor);
        }
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
                policy: opt_u32(m.get(21))?,
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
                groups: opt_u32(m.get(22))?,
                beacon_key: opt_bytes_n(m.get(23))?,
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
                new_owner: opt_bytes_n(m.get(21))?,
            }),
            MessageType::Confirm => Body::Confirm { replaces: bytes_n(m.get(16))?, handover: bytes_n(m.get(17))? },
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
            MessageType::Zones => Body::Zones(super::policy::Zones {
                zones: m.get(16).and_then(Value::as_array).ok_or(Reject::BadCbor)?.iter().map(ring_from).collect::<Result<_, _>>()?,
                epoch: m.get(17).and_then(Value::as_u32).filter(|e| *e >= 1).ok_or(Reject::BadCbor)?,
            }),
            MessageType::Lease => Body::Lease(super::policy::Lease {
                epoch: m.get(16).and_then(Value::as_u32).filter(|e| *e >= 1).ok_or(Reject::BadCbor)?,
                until: uint(m.get(17))?,
                digest: bytes_n(m.get(18))?,
            }),
            MessageType::Trust => Body::Trust { update: m.get(16).and_then(Value::as_bytes).ok_or(Reject::BadCbor)?.to_vec() },
            other => Body::Other { message_type: other.code(), fields: IntMap(m.0.iter().filter(|(k, _)| *k >= 16).cloned().collect()) },
        };
        Ok(Self { herd, device, version, issued_at: uint(m.get(4))?, expires_at: opt_u64(m.get(5))?, reference, effective_at: opt_u64(m.get(7))?, body })
    }

    /// The JSON view.
    pub fn to_json(&self) -> Json {
        let mut o = serde_json::Map::new();
        o.insert("type".into(), self.body.message_type().to_json());
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
                put("policy", x.policy.map(|v| json!(v)));
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
                put("role", Some(x.role.to_json()));
                put("net", Some(json!(x.net)));
                put("plan", x.plan.map(|v| json!(v)));
                put("cycle_s", x.cycle_s.map(|v| json!(v)));
                put("rank", x.rank.map(|v| json!(v)));
                put("groups", x.groups.map(|v| json!(v)));
                put("beacon_key", hexed(x.beacon_key.as_ref().map(|k| &k[..])));
            }
            Body::Roster(x) => {
                put(
                    "members",
                    Some(Json::Array(x.members.iter().map(|p| json!({"short": p.short, "device": hex(&p.device), "role": p.role.to_json()})).collect())),
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
                    x.reason.map(|v| match v {
                        1 => json!("rotate"),
                        2 => json!("transfer"),
                        n => json!(n),
                    }),
                );
                put("new_owner", hexed(x.new_owner.as_ref().map(|k| &k[..])));
            }
            Body::Confirm { replaces, handover } => {
                put("replaces", Some(json!(hex(replaces))));
                put("handover", Some(json!(hex(handover))));
            }
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
            Body::Zones(z) => {
                put("zones", Some(Json::Array(z.zones.iter().map(ring).collect())));
                put("epoch", Some(json!(z.epoch)));
            }
            Body::Lease(l) => {
                put("epoch", Some(json!(l.epoch)));
                put("until", Some(json!(l.until)));
                put("digest", Some(json!(hex(&l.digest))));
            }
            Body::Trust { update } => put("update", Some(json!(hex(update)))),
            Body::Other { fields, .. } => put("fields", Some(int_map_json(fields))),
        }
        Json::Object(o)
    }

    /// What a collar checks once the signature verifies, in order:
    /// `wrong_collar` (another collar's), `wrong_herd` (another herd's; a
    /// membership or bind names the herd it moves the collar to, so it is
    /// exempt), then `expired` against `now` (GNSS time). A command with an
    /// `expires_at` and no trustworthy time yet is `not_now`: the collar
    /// holds it unapplied until it has GNSS time, then checks again (Astra
    /// 01 F10: it used to pass).
    pub fn check_scope(&self, device_id: &[u8; 8], herd: Option<&[u8]>, now: Option<u64>) -> Result<(), Reject> {
        if self.device.is_some_and(|d| d != *device_id) {
            return Err(Reject::WrongCollar);
        }
        let moves_herd = matches!(self.body, Body::Membership(_) | Body::Bind(_));
        if !moves_herd && herd.is_some_and(|h| h != self.herd.as_slice()) {
            return Err(Reject::WrongHerd);
        }
        match (self.expires_at, now) {
            (Some(exp), Some(now)) if now >= exp => Err(Reject::Expired),
            (Some(_), None) => Err(Reject::NotNow),
            _ => Ok(()),
        }
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
    /// `unsupported` for a channel plan this collar doesn't have (it lists
    /// its plans in its device record); `bad_config` for a short address of
    /// 0 or 65535, an unknown role, a cycle that isn't a whole number of
    /// minutes from 1 to 60, `groups` outside 1..=cycle, or a short address
    /// in a group past `groups` (it would never send).
    pub fn check(&self) -> Result<(), Reject> {
        let plan = super::tdma::plan(self.plan.unwrap_or(1)).ok_or(Reject::Unsupported)?;
        let sched = super::tdma::Schedule::new(self.cycle_s, self.groups).ok_or(Reject::BadConfig)?;
        let ok = (1..=65_534).contains(&self.short) && self.role.is_known() && plan.slot(self.short).is_some_and(|s| u32::from(s.g) < sched.groups);
        if ok { Ok(()) } else { Err(Reject::BadConfig) }
    }

    /// The net's schedule (cycle and groups).
    pub fn schedule(&self) -> Option<super::tdma::Schedule> {
        super::tdma::Schedule::new(self.cycle_s, self.groups)
    }
}

impl Handover {
    /// `bad_config` for a deadline not after `issued_at`, or more than 30
    /// days after it (Astra 01 F10: nothing checked it).
    pub fn check(&self, issued_at: u64) -> Result<(), Reject> {
        let ok = self.deadline > issued_at && self.deadline - issued_at <= MAX_HANDOVER_S && self.reason.is_none_or(|r| r == 1 || r == TRANSFER);
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
    /// The digest of the handover that staged `pending`: a confirm must
    /// name it.
    pub pending_handover: Option<[u8; 8]>,
    pub owner: Option<VerifyingKey>,
}

impl Trust {
    /// The trusted key with this kid, for [`open`].
    pub fn key_for(&self, kid: &[u8; 8]) -> Option<VerifyingKey> {
        [self.server, self.pending, self.owner].into_iter().flatten().find(|k| key_id(k) == *kid)
    }

    /// Who may sign what, once the signature verifies (`identity.md` §4.4):
    ///
    /// - a bind only while unclaimed (`claimed`);
    /// - a confirm only by the pending key, naming the current key and the
    ///   digest of the handover that staged it (`bad_sig`; a confirm for
    ///   another proposal can't commit this one);
    /// - a release: the owner key when one is enrolled, else the server key
    ///   (a stolen server key can't erase the owner's recovery);
    /// - a transfer handover: the owner key when one is enrolled, else the
    ///   server key; any other handover: the server or owner key;
    /// - an owner command by the owner key, or the server key while no owner
    ///   key is enrolled;
    /// - everything else by the server key alone (`unknown_kid`).
    pub fn authorize(&self, r: &Received) -> Result<(), Reject> {
        let is = |k: &Option<VerifyingKey>| k.is_some_and(|k| key_id(&k) == r.kid);
        let owner_or_server = is(&self.owner) || (self.owner.is_none() && is(&self.server));
        match &r.command.body {
            Body::Bind(_) if self.server.is_some() => Err(Reject::Claimed),
            Body::Bind(_) => Ok(()),
            Body::Confirm { replaces, handover } => {
                let names_current = self.server.is_some_and(|k| key_id(&k) == *replaces);
                let names_proposal = self.pending_handover == Some(*handover);
                if is(&self.pending) && names_current && names_proposal { Ok(()) } else { Err(Reject::BadSig) }
            }
            Body::Release | Body::Owner { .. } if owner_or_server => Ok(()),
            Body::Handover(h) if h.reason == Some(TRANSFER) && owner_or_server => Ok(()),
            Body::Handover(h) if h.reason != Some(TRANSFER) && (is(&self.server) || is(&self.owner)) => Ok(()),
            Body::Owner { .. } | Body::Handover(_) | Body::Release => Err(Reject::UnknownKid),
            _ if is(&self.server) => Ok(()),
            _ => Err(Reject::UnknownKid),
        }
    }
}

/// What a collar remembers of the commands it took, besides boundaries
/// (which follow the slot rules): per type, the highest version and the
/// digest of the command that carried it; and when it was claimed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Versions {
    pub held: std::collections::BTreeMap<u64, (u32, [u8; 8])>,
    /// The `issued_at` of the bind that claimed it. Commands issued before
    /// it are `stale`: a claim starts versions over, and this keeps the
    /// commands of an earlier claim (even by the same server key) from
    /// being replayed.
    pub claimed_at: u64,
}

/// Whether a command is new or one the collar already took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fresh {
    New,
    /// The very command it holds (same digest): acked again with its
    /// current status, nothing done twice.
    Repeat,
}

impl Versions {
    /// The version rule (`messages.md` §4.6) for anything but a boundary: a
    /// repeat of the command held is [`Fresh::Repeat`]; anything else at or
    /// below the held version, or issued before the claim, is `stale`.
    pub fn check(&self, r: &Received) -> Result<Fresh, Reject> {
        let c = &r.command;
        if !matches!(c.body, Body::Bind(_)) && c.issued_at < self.claimed_at {
            return Err(Reject::Stale);
        }
        match self.held.get(&c.body.message_type().code()) {
            Some((_, d)) if *d == r.digest => Ok(Fresh::Repeat),
            Some((v, _)) if c.version <= *v => Err(Reject::Stale),
            _ => Ok(Fresh::New),
        }
    }

    /// Record a command taken. A bind starts every version over.
    pub fn take(&mut self, r: &Received) {
        if matches!(r.command.body, Body::Bind(_)) {
            self.held.clear();
            self.claimed_at = r.command.issued_at;
        }
        self.held.insert(r.command.body.message_type().code(), (r.command.version, r.digest));
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
        assert_eq!(c.check_scope(&me, None, None), Err(Reject::NotNow), "no clock: wait, never pass");
        c.expires_at = None;
        c.check_scope(&me, None, None).unwrap();
    }

    fn signed_body(body: Body, device: Option<[u8; 8]>) -> Vec<u8> {
        Command { device, body, ..boundary() }.sign(&key())
    }

    fn opened(bytes: &[u8]) -> Result<Received, Reject> {
        open(bytes, |_| Some(key().verifying_key()))
    }

    /// Astra 01 F10: config, membership and the like without `device` were
    /// accepted as herd-wide.
    #[test]
    fn one_collar_types_need_their_device() {
        let membership =
            Body::Membership(Membership { short: 12, role: Role::Node, net: 1, plan: None, cycle_s: None, rank: None, groups: None, beacon_key: None });
        assert_eq!(opened(&signed_body(membership.clone(), None)).unwrap_err(), Reject::BadCbor);
        assert!(opened(&signed_body(membership, Some([7; 8]))).is_ok());
        assert_eq!(opened(&signed_body(Body::Config(Config { report_s: 300, ..Default::default() }), None)).unwrap_err(), Reject::BadCbor);
        assert_eq!(opened(&signed_body(Body::Release, None)).unwrap_err(), Reject::BadCbor);
        assert_eq!(opened(&signed_body(Body::Resend { from: 1, to: 2 }, None)).unwrap_err(), Reject::BadCbor);
    }

    #[test]
    fn membership_and_handover_ranges() {
        let m = Membership { short: 301, role: Role::Node, net: 1, plan: None, cycle_s: None, rank: None, groups: None, beacon_key: None };
        assert_eq!(m.check(), Err(Reject::BadConfig), "group 1 of a one-group net");
        Membership { groups: Some(2), ..m.clone() }.check().unwrap();
        assert_eq!(Membership { groups: Some(6), ..m.clone() }.check(), Err(Reject::BadConfig), "more groups than minutes in the cycle");
        assert_eq!(Membership { plan: Some(999), ..m.clone() }.check(), Err(Reject::Unsupported), "a plan this collar doesn't have");
        let h = Handover { new_key: [1; 32], deadline: 1000 + MAX_HANDOVER_S, endpoint: None, endpoint_pin: None, reason: None, new_owner: None };
        h.check(1000).unwrap();
        assert_eq!(Handover { deadline: 1001 + MAX_HANDOVER_S, ..h.clone() }.check(1000), Err(Reject::BadConfig));
        assert_eq!(Handover { deadline: 1000, ..h }.check(1000), Err(Reject::BadConfig));
    }

    /// Astra 01 F07: a server-signed release erased an enrolled owner, and
    /// a server key could sell the collar away from its owner.
    #[test]
    fn the_server_key_cant_erase_or_sell_past_the_owner() {
        let server = key();
        let owner = SigningKey::from_bytes(&[42; 32]);
        let trust = Trust { server: Some(server.verifying_key()), owner: Some(owner.verifying_key()), ..Default::default() };
        let by = |k: &SigningKey, body: Body| {
            let bytes = Command { device: Some([7; 8]), body, ..boundary() }.sign(k);
            open(&bytes, |kid| trust.key_for(kid)).unwrap()
        };
        assert_eq!(trust.authorize(&by(&server, Body::Release)), Err(Reject::UnknownKid));
        trust.authorize(&by(&owner, Body::Release)).unwrap();
        let sale = Handover { new_key: [1; 32], deadline: 2_000_000_000, endpoint: None, endpoint_pin: None, reason: Some(TRANSFER), new_owner: Some([2; 32]) };
        assert_eq!(trust.authorize(&by(&server, Body::Handover(sale.clone()))), Err(Reject::UnknownKid));
        trust.authorize(&by(&owner, Body::Handover(sale.clone()))).unwrap();
        trust.authorize(&by(&server, Body::Handover(Handover { reason: Some(1), new_owner: None, ..sale }))).unwrap();
        let no_owner = Trust { owner: None, ..trust.clone() };
        no_owner.authorize(&by(&server, Body::Release)).unwrap();
    }

    /// Astra 01 F06: a confirm named only the replaced kid, so a late one
    /// from an earlier proposal could commit a later one.
    #[test]
    fn a_confirm_commits_only_its_own_handover() {
        let next = SigningKey::from_bytes(&[43; 32]);
        let trust = Trust { server: Some(key().verifying_key()), pending: Some(next.verifying_key()), pending_handover: Some([5; 8]), owner: None };
        let confirm = |h: [u8; 8]| {
            let bytes = Command { body: Body::Confirm { replaces: key_id(&key().verifying_key()), handover: h }, ..boundary() }.sign(&next);
            open(&bytes, |kid| trust.key_for(kid)).unwrap()
        };
        trust.authorize(&confirm([5; 8])).unwrap();
        assert_eq!(trust.authorize(&confirm([6; 8])), Err(Reject::BadSig), "another proposal's confirm");
    }

    #[test]
    fn versions_repeat_stale_and_claims() {
        let mut v = Versions::default();
        let cfg = |version, issued_at| {
            let bytes = Command { device: Some([7; 8]), version, issued_at, body: Body::Config(Config { report_s: 300, ..Default::default() }), ..boundary() }
                .sign(&key());
            opened(&bytes).unwrap()
        };
        let first = cfg(4, 2_000);
        assert_eq!(v.check(&first), Ok(Fresh::New));
        v.take(&first);
        assert_eq!(v.check(&first), Ok(Fresh::Repeat), "the same command: ack it again");
        assert_eq!(v.check(&cfg(4, 2_001)), Err(Reject::Stale), "another one at the same version");
        assert_eq!(v.check(&cfg(5, 2_001)), Ok(Fresh::New));
        v.claimed_at = 3_000;
        assert_eq!(v.check(&cfg(9, 2_500)), Err(Reject::Stale), "issued before the claim: a replay");
    }
}
