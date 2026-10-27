//! Uplink records: what a collar reports. Each is a CBOR array
//! `[type, seq, t, …]`: its record type, the collar's sequence number and a
//! GNSS time in Unix seconds, then fields by position. Optional fields at the
//! end are left off; one in the middle is `null`. Elements past the ones
//! this build knows are ignored, and a record type it doesn't know is kept as
//! [`RecordBody::Other`], so a newer collar's report is never refused.
//!
//! `seq` counts every record a collar makes, across all types, from 0, and
//! never goes back (it survives reboots). A server dedupes on
//! `(device, seq)` and asks for gaps again with a resend command.

use op_geo::CollarLimits;
use serde_json::{Value as Json, json};

use super::cbor::{self, IntMap, Value};
use super::json::{Field, hex, table_json};
use super::registry::{AckStatus, CueKind, DeviceClass, EpisodeOutcome, FenceState, FirmwareState, MessageType, RecordType, Reject};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub seq: u32,
    /// GNSS time, Unix seconds: of the fix, cue, start of the episode, or
    /// when the record was made.
    pub t: u64,
    pub body: RecordBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordBody {
    Status(Status),
    /// Fix history; the first fix is at the record's `t`.
    Fixes(Vec<Fix>),
    Cue(Cue),
    Episode(Episode),
    Ack(Ack),
    /// Health counters by key (see [`HEALTH`]).
    Health(IntMap),
    Device(DeviceInfo),
    /// Every boundary the collar holds.
    Slots(Vec<SlotEntry>),
    /// An anchor's own state, by key (see [`ANCHOR`]).
    Anchor(IntMap),
    Firmware(FirmwareStatus),
    Other {
        record_type: u64,
        fields: Vec<Value>,
    },
}

/// Status flags.
pub mod flags {
    /// The fence is armed: a crossing would be cued.
    pub const ARMED: u32 = 1;
    /// Crossed out; the outside tone window is running.
    pub const ESCAPING: u32 = 2;
    pub const CHARGING: u32 = 4;
    /// In a fast window.
    pub const FAST: u32 = 8;
    /// No fix this period: the position is the last known one.
    pub const STALE: u32 = 16;
    /// The boundary is in track mode (no cues).
    pub const TRACK: u32 = 32;
}

/// `[1, seq, t, lon, lat, acc_dm, state, bver, battery, flags?]`: the latest
/// fix and what the fence made of it. Sent every report period.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// 10⁻⁷ degrees.
    pub lon: i32,
    pub lat: i32,
    /// Horizontal accuracy, decimetres.
    pub acc_dm: u32,
    pub state: Option<FenceState>,
    /// The boundary version enforced; 0 with none.
    pub bver: u32,
    /// 0–100; `null` when unknown.
    pub battery: Option<u8>,
    pub flags: Option<u32>,
}

/// One fix in a [`RecordBody::Fixes`] block. On the wire: the first fix's
/// `lon, lat, acc_dm`, then a flat array of `dt, dlon, dlat, acc_dm` for each
/// next fix, differences from the one before (accuracy is not a difference).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fix {
    pub t: u64,
    pub lon: i32,
    pub lat: i32,
    pub acc_dm: u32,
}

/// `[3, seq, t, kind, level, margin_cm, ring, bver, dur_ms?]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub kind: CueKind,
    /// 1–4.
    pub level: u8,
    /// Signed distance to the nearest edge, positive inside.
    pub margin_cm: i32,
    /// 0 the outer ring, 1.. holes.
    pub ring: u16,
    pub bver: u32,
    pub dur_ms: Option<u32>,
}

/// `[4, seq, start, dur_s, bver, ring, cues, max_level, min_margin_cm, outcome]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    pub dur_s: u32,
    pub bver: u32,
    pub ring: u16,
    pub cues: u32,
    pub max_level: u8,
    pub min_margin_cm: i32,
    pub outcome: EpisodeOutcome,
}

/// `[5, seq, t, type, version, status, code?, digest?]`: what became of a
/// command. `t` is when it happened (GNSS time; for `applied`, when it took
/// effect, possibly while offline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ack {
    pub message_type: MessageType,
    pub version: u32,
    pub status: AckStatus,
    pub code: Option<Reject>,
    /// First 8 bytes of SHA-256 of the signed bytes acked.
    pub digest: Option<[u8; 8]>,
}

/// `[version, status, effective_at?]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotEntry {
    pub version: u32,
    /// `applied` (in effect) or `received` (staged).
    pub status: AckStatus,
    pub effective_at: Option<u64>,
}

/// `[10, seq, t, state, version, digest?, code?]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareStatus {
    pub state: FirmwareState,
    /// The release this is about (semver).
    pub version: String,
    /// First 8 bytes of the image digest.
    pub digest: Option<[u8; 8]>,
    pub code: Option<Reject>,
}

/// `[7, seq, t, {…}]`: what the collar is and holds. Sent at boot, when any
/// of it changes, and once a day.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// 1
    pub class: Option<DeviceClass>,
    /// 2
    pub fw: Option<String>,
    /// 3
    pub caps: Vec<String>,
    /// 4: `{1 outer, 2 holes, 3 hole_vertices, 4 total, 5 slots, 6 slot_bytes}`
    pub limits: Option<CollarLimits>,
    /// 5
    pub hw_rev: Option<String>,
    /// 6: the server key it obeys.
    pub server_kid: Option<[u8; 8]>,
    /// 7: a handover's key, waiting for its confirm.
    pub pending_kid: Option<[u8; 8]>,
    /// 8
    pub owner_kid: Option<[u8; 8]>,
    /// 9: highest version held of each command type, `{type: version}`.
    pub versions: Vec<(MessageType, u32)>,
    /// 10
    pub herd: Option<Vec<u8>>,
    /// 11
    pub short: Option<u16>,
    /// 12: only while unclaimed.
    pub claim_nonce: Option<[u8; 8]>,
    /// 13: bootloader version.
    pub boot: Option<String>,
    /// 14: firmware anti-rollback counter.
    pub security_counter: Option<u32>,
    /// 15: kids of the firmware keys the bootloader trusts.
    pub image_keys: Vec<[u8; 8]>,
}

/// Health keys. Units are in the names; `_x10` is tenths.
pub const HEALTH: &[Field] = &[
    Field { key: 1, name: "fix_attempts", nested: None },
    Field { key: 2, name: "fix_ok", nested: None },
    Field { key: 3, name: "sats", nested: None },
    Field { key: 4, name: "cn0_dbhz", nested: None },
    Field { key: 5, name: "ttf_ds", nested: None },
    Field { key: 6, name: "hdop_x10", nested: None },
    Field { key: 7, name: "battery_mv", nested: None },
    Field { key: 8, name: "charging", nested: None },
    Field { key: 9, name: "temp_dc", nested: None },
    Field { key: 10, name: "uptime_s", nested: None },
    Field { key: 11, name: "reset", nested: None },
    Field { key: 12, name: "still_s", nested: None },
    Field { key: 13, name: "tilt_deg", nested: None },
    Field { key: 14, name: "cell", nested: Some(CELL) },
    Field { key: 15, name: "lora", nested: Some(LORA) },
    Field { key: 16, name: "solar_mv", nested: None },
];

pub const CELL: &[Field] = &[
    Field { key: 1, name: "rsrp_dbm", nested: None },
    Field { key: 2, name: "rsrq_db", nested: None },
    Field { key: 3, name: "snr_db", nested: None },
    Field { key: 4, name: "mode", nested: None },
    Field { key: 5, name: "band", nested: None },
    Field { key: 6, name: "cell_id", nested: None },
    Field { key: 7, name: "tac", nested: None },
];

pub const LORA: &[Field] = &[
    Field { key: 1, name: "rssi_dbm", nested: None },
    Field { key: 2, name: "snr_db", nested: None },
    Field { key: 3, name: "beacons", nested: None },
    Field { key: 4, name: "missed", nested: None },
    Field { key: 5, name: "frames", nested: None },
];

/// Anchor keys.
pub const ANCHOR: &[Field] = &[
    Field { key: 1, name: "active", nested: None },
    Field { key: 2, name: "battery", nested: None },
    Field { key: 3, name: "rsrp_dbm", nested: None },
    Field { key: 4, name: "heard", nested: None },
    Field { key: 5, name: "rank", nested: None },
    Field { key: 6, name: "queue", nested: None },
];

// ---------------------------------------------------------------- encoding

fn u(v: impl Into<u64>) -> Value {
    Value::uint(v)
}

fn i(v: impl Into<i64>) -> Value {
    Value::int(v)
}

fn opt(v: Option<Value>) -> Value {
    v.unwrap_or(Value::Null)
}

/// Drop trailing nulls: optional fields at the end are left off.
fn trim(mut v: Vec<Value>) -> Vec<Value> {
    while v.last().is_some_and(Value::is_null) {
        v.pop();
    }
    v
}

fn limits_value(l: &CollarLimits) -> Value {
    let mut m = IntMap::new();
    m.put(1, u(l.outer as u64))
        .put(2, u(l.holes as u64))
        .put(3, u(l.hole_vertices as u64))
        .put(4, u(l.total as u64))
        .put(5, u(l.slots as u64))
        .put(6, u(l.slot_bytes as u64));
    m.into_value()
}

fn limits_from(v: &Value) -> Option<CollarLimits> {
    let m = IntMap::from_value(v)?;
    let g = |k| m.get(k).and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok());
    Some(CollarLimits { outer: g(1)?, holes: g(2)?, hole_vertices: g(3)?, total: g(4)?, slots: g(5)?, slot_bytes: g(6)? })
}

impl DeviceInfo {
    fn value(&self) -> Value {
        let mut m = IntMap::new();
        m.put_opt(1, self.class.map(|c| u(c.code())))
            .put_opt(2, self.fw.clone().map(Value::Text))
            .put_opt(3, (!self.caps.is_empty()).then(|| Value::Array(self.caps.iter().cloned().map(Value::Text).collect())))
            .put_opt(4, self.limits.as_ref().map(limits_value))
            .put_opt(5, self.hw_rev.clone().map(Value::Text))
            .put_opt(6, self.server_kid.map(|k| Value::bytes(k.to_vec())))
            .put_opt(7, self.pending_kid.map(|k| Value::bytes(k.to_vec())))
            .put_opt(8, self.owner_kid.map(|k| Value::bytes(k.to_vec())))
            .put_opt(9, (!self.versions.is_empty()).then(|| Value::Map(self.versions.iter().map(|(t, v)| (u(t.code()), u(*v))).collect())))
            .put_opt(10, self.herd.clone().map(Value::Bytes))
            .put_opt(11, self.short.map(u))
            .put_opt(12, self.claim_nonce.map(|n| Value::bytes(n.to_vec())))
            .put_opt(13, self.boot.clone().map(Value::Text))
            .put_opt(14, self.security_counter.map(u))
            .put_opt(15, (!self.image_keys.is_empty()).then(|| Value::Array(self.image_keys.iter().map(|k| Value::bytes(k.to_vec())).collect())));
        m.into_value()
    }

    fn from_value(v: &Value) -> Option<Self> {
        let m = IntMap::from_value(v)?;
        let kid = |k| m.get(k).and_then(Value::as_bytes).and_then(|b| <[u8; 8]>::try_from(b).ok());
        let text = |k| m.get(k).and_then(Value::as_text).map(str::to_owned);
        Some(Self {
            class: m.get(1).and_then(Value::as_u64).map(DeviceClass::from_code),
            fw: text(2),
            caps: m.get(3).and_then(Value::as_array).map(|a| a.iter().filter_map(|c| c.as_text().map(str::to_owned)).collect()).unwrap_or_default(),
            limits: m.get(4).and_then(limits_from),
            hw_rev: text(5),
            server_kid: kid(6),
            pending_kid: kid(7),
            owner_kid: kid(8),
            versions: m
                .get(9)
                .and_then(Value::as_map)
                .map(|e| e.iter().filter_map(|(t, v)| Some((MessageType::from_code(t.as_u64()?), v.as_u32()?))).collect())
                .unwrap_or_default(),
            herd: m.get(10).and_then(Value::as_bytes).map(<[u8]>::to_vec),
            short: m.get(11).and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()),
            claim_nonce: kid(12),
            boot: text(13),
            security_counter: m.get(14).and_then(Value::as_u32),
            image_keys: m
                .get(15)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|k| <[u8; 8]>::try_from(k.as_bytes()?).ok()).collect())
                .unwrap_or_default(),
        })
    }

    fn to_json(&self) -> Json {
        let mut o = serde_json::Map::new();
        let mut put = |k: &str, v: Option<Json>| {
            if let Some(v) = v {
                o.insert(k.into(), v);
            }
        };
        put("class", self.class.map(|c| json!(c.name())));
        put("fw", self.fw.as_ref().map(|v| json!(v)));
        put("caps", (!self.caps.is_empty()).then(|| json!(self.caps)));
        put("limits", self.limits.as_ref().map(|l| json!(l)));
        put("hw_rev", self.hw_rev.as_ref().map(|v| json!(v)));
        put("server_kid", self.server_kid.map(|k| json!(hex(&k))));
        put("pending_kid", self.pending_kid.map(|k| json!(hex(&k))));
        put("owner_kid", self.owner_kid.map(|k| json!(hex(&k))));
        put("versions", (!self.versions.is_empty()).then(|| Json::Object(self.versions.iter().map(|(t, v)| (t.name().to_string(), json!(v))).collect())));
        put("herd", self.herd.as_ref().map(|h| json!(hex(h))));
        put("short", self.short.map(|v| json!(v)));
        put("claim_nonce", self.claim_nonce.map(|n| json!(hex(&n))));
        put("boot", self.boot.as_ref().map(|v| json!(v)));
        put("security_counter", self.security_counter.map(|v| json!(v)));
        put("image_keys", (!self.image_keys.is_empty()).then(|| Json::Array(self.image_keys.iter().map(|k| json!(hex(k))).collect())));
        Json::Object(o)
    }
}

impl Record {
    pub fn record_type(&self) -> RecordType {
        match &self.body {
            RecordBody::Status(_) => RecordType::Status,
            RecordBody::Fixes(_) => RecordType::Fixes,
            RecordBody::Cue(_) => RecordType::Cue,
            RecordBody::Episode(_) => RecordType::Episode,
            RecordBody::Ack(_) => RecordType::Ack,
            RecordBody::Health(_) => RecordType::Health,
            RecordBody::Device(_) => RecordType::Device,
            RecordBody::Slots(_) => RecordType::Slots,
            RecordBody::Anchor(_) => RecordType::Anchor,
            RecordBody::Firmware(_) => RecordType::Firmware,
            RecordBody::Other { record_type, .. } => RecordType::from_code(*record_type),
        }
    }

    pub fn value(&self) -> Value {
        let mut v = vec![u(self.record_type().code()), u(self.seq), u(self.t)];
        match &self.body {
            RecordBody::Status(s) => {
                v.extend([i(s.lon), i(s.lat), u(s.acc_dm), opt(s.state.map(|st| u(st.code()))), u(s.bver), opt(s.battery.map(u)), opt(s.flags.map(u))]);
                v = trim(v);
            }
            RecordBody::Fixes(fixes) => {
                let first = fixes.first().copied().unwrap_or_default();
                v.extend([i(first.lon), i(first.lat), u(first.acc_dm)]);
                let mut rest = Vec::new();
                for w in fixes.windows(2) {
                    let (a, b) = (w[0], w[1]);
                    rest.extend([u(b.t.saturating_sub(a.t)), i(i64::from(b.lon) - i64::from(a.lon)), i(i64::from(b.lat) - i64::from(a.lat)), u(b.acc_dm)]);
                }
                v.push(Value::Array(rest));
            }
            RecordBody::Cue(c) => {
                v.extend([u(c.kind.code()), u(c.level), i(c.margin_cm), u(c.ring), u(c.bver), opt(c.dur_ms.map(u))]);
                v = trim(v);
            }
            RecordBody::Episode(e) => {
                v.extend([u(e.dur_s), u(e.bver), u(e.ring), u(e.cues), u(e.max_level), i(e.min_margin_cm), u(e.outcome.code())]);
            }
            RecordBody::Ack(a) => {
                v.extend([
                    u(a.message_type.code()),
                    u(a.version),
                    u(a.status.code()),
                    opt(a.code.map(|c| u(c.code()))),
                    opt(a.digest.map(|d| Value::bytes(d.to_vec()))),
                ]);
                v = trim(v);
            }
            RecordBody::Health(m) | RecordBody::Anchor(m) => v.push(m.clone().into_value()),
            RecordBody::Device(d) => v.push(d.value()),
            RecordBody::Slots(slots) => {
                v.push(Value::Array(slots.iter().map(|s| Value::Array(trim(vec![u(s.version), u(s.status.code()), opt(s.effective_at.map(u))]))).collect()));
            }
            RecordBody::Firmware(f) => {
                v.extend([
                    u(f.state.code()),
                    Value::text(f.version.clone()),
                    opt(f.digest.map(|d| Value::bytes(d.to_vec()))),
                    opt(f.code.map(|c| u(c.code()))),
                ]);
                v = trim(v);
            }
            RecordBody::Other { fields, .. } => v.extend(fields.iter().cloned()),
        }
        Value::Array(v)
    }

    pub fn encode(&self) -> Vec<u8> {
        cbor::encode(&self.value())
    }

    /// One record from its array. A record that doesn't fit its type's shape
    /// is `bad_cbor`; the caller keeps the others.
    pub fn from_value(v: &Value) -> Result<Self, Reject> {
        let a = v.as_array().ok_or(Reject::BadCbor)?;
        let [rt, seq, t, rest @ ..] = a else { return Err(Reject::BadCbor) };
        let rt = RecordType::from_code(rt.as_u64().ok_or(Reject::BadCbor)?);
        let seq = seq.as_u32().ok_or(Reject::BadCbor)?;
        let t = t.as_u64().ok_or(Reject::BadCbor)?;
        let at = |k: usize| rest.get(k).filter(|v| !v.is_null());
        let req_u = |k: usize| at(k).and_then(Value::as_u64).ok_or(Reject::BadCbor);
        let req_u32 = |k: usize| at(k).and_then(Value::as_u32).ok_or(Reject::BadCbor);
        let req_i32 = |k: usize| at(k).and_then(Value::as_i32).ok_or(Reject::BadCbor);
        let small = |k: usize| req_u(k).and_then(|n| u8::try_from(n).map_err(|_| Reject::BadCbor));
        let short = |k: usize| req_u(k).and_then(|n| u16::try_from(n).map_err(|_| Reject::BadCbor));
        let body = match rt {
            RecordType::Status => RecordBody::Status(Status {
                lon: req_i32(0)?,
                lat: req_i32(1)?,
                acc_dm: req_u32(2)?,
                state: at(3).and_then(Value::as_u64).map(FenceState::from_code),
                bver: req_u32(4)?,
                battery: at(5).and_then(Value::as_u64).and_then(|b| u8::try_from(b).ok()),
                flags: at(6).and_then(Value::as_u32),
            }),
            RecordType::Fixes => {
                let mut fix = Fix { t, lon: req_i32(0)?, lat: req_i32(1)?, acc_dm: req_u32(2)? };
                let mut fixes = vec![fix];
                let deltas = at(3).and_then(Value::as_array).ok_or(Reject::BadCbor)?;
                if deltas.len() % 4 != 0 {
                    return Err(Reject::BadCbor);
                }
                for d in deltas.chunks(4) {
                    let n = |k: usize| d[k].as_i64().ok_or(Reject::BadCbor);
                    let step = |base: i32, by: i64| i32::try_from(i64::from(base) + by).map_err(|_| Reject::BadCbor);
                    fix = Fix {
                        t: fix.t.checked_add(d[0].as_u64().ok_or(Reject::BadCbor)?).ok_or(Reject::BadCbor)?,
                        lon: step(fix.lon, n(1)?)?,
                        lat: step(fix.lat, n(2)?)?,
                        acc_dm: d[3].as_u32().ok_or(Reject::BadCbor)?,
                    };
                    fixes.push(fix);
                }
                RecordBody::Fixes(fixes)
            }
            RecordType::Cue => RecordBody::Cue(Cue {
                kind: CueKind::from_code(req_u(0)?),
                level: small(1)?,
                margin_cm: req_i32(2)?,
                ring: short(3)?,
                bver: req_u32(4)?,
                dur_ms: at(5).and_then(Value::as_u32),
            }),
            RecordType::Episode => RecordBody::Episode(Episode {
                dur_s: req_u32(0)?,
                bver: req_u32(1)?,
                ring: short(2)?,
                cues: req_u32(3)?,
                max_level: small(4)?,
                min_margin_cm: req_i32(5)?,
                outcome: EpisodeOutcome::from_code(req_u(6)?),
            }),
            RecordType::Ack => RecordBody::Ack(Ack {
                message_type: MessageType::from_code(req_u(0)?),
                version: req_u32(1)?,
                status: AckStatus::from_code(req_u(2)?),
                code: at(3).and_then(Value::as_u64).map(Reject::from_code),
                digest: at(4).and_then(Value::as_bytes).and_then(|d| d.try_into().ok()),
            }),
            RecordType::Health => RecordBody::Health(at(0).and_then(IntMap::from_value).ok_or(Reject::BadCbor)?),
            RecordType::Anchor => RecordBody::Anchor(at(0).and_then(IntMap::from_value).ok_or(Reject::BadCbor)?),
            RecordType::Device => RecordBody::Device(at(0).and_then(DeviceInfo::from_value).ok_or(Reject::BadCbor)?),
            RecordType::Slots => RecordBody::Slots(
                at(0)
                    .and_then(Value::as_array)
                    .ok_or(Reject::BadCbor)?
                    .iter()
                    .map(|s| {
                        let s = s.as_array().ok_or(Reject::BadCbor)?;
                        Ok(SlotEntry {
                            version: s.first().and_then(Value::as_u32).ok_or(Reject::BadCbor)?,
                            status: AckStatus::from_code(s.get(1).and_then(Value::as_u64).ok_or(Reject::BadCbor)?),
                            effective_at: s.get(2).and_then(Value::as_u64),
                        })
                    })
                    .collect::<Result<_, Reject>>()?,
            ),
            RecordType::Firmware => RecordBody::Firmware(FirmwareStatus {
                state: FirmwareState::from_code(req_u(0)?),
                version: at(1).and_then(Value::as_text).ok_or(Reject::BadCbor)?.to_owned(),
                digest: at(2).and_then(Value::as_bytes).and_then(|d| d.try_into().ok()),
                code: at(3).and_then(Value::as_u64).map(Reject::from_code),
            }),
            RecordType::Other(code) => RecordBody::Other { record_type: code, fields: rest.to_vec() },
        };
        Ok(Self { seq, t, body })
    }

    /// The JSON view.
    pub fn to_json(&self) -> Json {
        let mut o = serde_json::Map::new();
        o.insert("type".into(), json!(self.record_type().name()));
        o.insert("seq".into(), json!(self.seq));
        o.insert("t".into(), json!(self.t));
        let mut put = |k: &str, v: Option<Json>| {
            if let Some(v) = v {
                o.insert(k.into(), v);
            }
        };
        match &self.body {
            RecordBody::Status(s) => {
                put("lon", Some(json!(s.lon)));
                put("lat", Some(json!(s.lat)));
                put("acc_dm", Some(json!(s.acc_dm)));
                put("state", s.state.map(|st| json!(st.name())));
                put("bver", Some(json!(s.bver)));
                put("battery", s.battery.map(|b| json!(b)));
                put("flags", s.flags.map(|f| json!(f)));
            }
            RecordBody::Fixes(f) => put("fixes", Some(Json::Array(f.iter().map(|x| json!([x.t, x.lon, x.lat, x.acc_dm])).collect()))),
            RecordBody::Cue(c) => {
                put("kind", Some(json!(c.kind.name())));
                put("level", Some(json!(c.level)));
                put("margin_cm", Some(json!(c.margin_cm)));
                put("ring", Some(json!(c.ring)));
                put("bver", Some(json!(c.bver)));
                put("dur_ms", c.dur_ms.map(|d| json!(d)));
            }
            RecordBody::Episode(e) => {
                put("dur_s", Some(json!(e.dur_s)));
                put("bver", Some(json!(e.bver)));
                put("ring", Some(json!(e.ring)));
                put("cues", Some(json!(e.cues)));
                put("max_level", Some(json!(e.max_level)));
                put("min_margin_cm", Some(json!(e.min_margin_cm)));
                put("outcome", Some(json!(e.outcome.name())));
            }
            RecordBody::Ack(a) => {
                put("message_type", Some(json!(a.message_type.name())));
                put("version", Some(json!(a.version)));
                put("status", Some(json!(a.status.name())));
                put("code", a.code.map(|c| json!(c.name())));
                put("digest", a.digest.map(|d| json!(hex(&d))));
            }
            RecordBody::Health(m) => put("health", Some(table_json(m, HEALTH))),
            RecordBody::Anchor(m) => put("anchor", Some(table_json(m, ANCHOR))),
            RecordBody::Device(d) => put("device", Some(d.to_json())),
            RecordBody::Slots(s) => put(
                "slots",
                Some(Json::Array(
                    s.iter()
                        .map(|e| {
                            let mut x = json!({"version": e.version, "status": e.status.name()});
                            if let Some(t) = e.effective_at {
                                x["effective_at"] = json!(t);
                            }
                            x
                        })
                        .collect(),
                )),
            ),
            RecordBody::Firmware(f) => {
                put("state", Some(json!(f.state.name())));
                put("version", Some(json!(f.version)));
                put("digest", f.digest.map(|d| json!(hex(&d))));
                put("code", f.code.map(|c| json!(c.name())));
            }
            RecordBody::Other { record_type, fields } => {
                put("record_type", Some(json!(record_type)));
                put("fields", Some(Json::Array(fields.iter().map(super::json::value_json).collect())));
            }
        }
        Json::Object(o)
    }
}

/// Records as a frame carries them: one CBOR array of record arrays.
pub fn encode_records(records: &[Record]) -> Vec<u8> {
    cbor::encode(&Value::Array(records.iter().map(Record::value).collect()))
}

/// Read a frame's records. The outer array must be valid CBOR; each record
/// is read on its own, so one malformed record costs only itself.
pub fn decode_records(bytes: &[u8]) -> Result<Vec<Result<Record, Reject>>, Reject> {
    let v = cbor::decode(bytes).map_err(|_| Reject::BadCbor)?;
    Ok(v.as_array().ok_or(Reject::BadCbor)?.iter().map(Record::from_value).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_small_and_round_trips() {
        let r = Record {
            seq: 4321,
            t: 1_790_510_460,
            body: RecordBody::Status(Status {
                lon: -936_225_000,
                lat: 420_318_000,
                acc_dm: 18,
                state: Some(FenceState::Inside),
                bver: 57,
                battery: Some(81),
                flags: Some(flags::ARMED),
            }),
        };
        let b = r.encode();
        assert!(b.len() <= 30, "{} bytes", b.len());
        assert_eq!(Record::from_value(&cbor::decode(&b).unwrap()).unwrap(), r);
    }

    #[test]
    fn fixes_round_trip() {
        let fixes = vec![
            Fix { t: 100, lon: -936_225_000, lat: 420_318_000, acc_dm: 18 },
            Fix { t: 160, lon: -936_224_100, lat: 420_318_400, acc_dm: 21 },
            Fix { t: 220, lon: -936_225_300, lat: 420_317_700, acc_dm: 15 },
        ];
        let r = Record { seq: 1, t: 100, body: RecordBody::Fixes(fixes) };
        assert_eq!(Record::from_value(&r.value()).unwrap(), r);
    }

    #[test]
    fn unknown_records_and_values_are_kept() {
        let v = Value::Array(vec![Value::uint(99u8), Value::uint(1u8), Value::uint(2u8), Value::text("x")]);
        let r = Record::from_value(&v).unwrap();
        assert_eq!(r.record_type(), RecordType::Other(99));
        assert_eq!(r.value(), v);
        let cue = Value::Array([3, 5, 100, 7, 2, -150, 0, 57].map(Value::int).to_vec());
        let RecordBody::Cue(c) = Record::from_value(&cue).unwrap().body else { panic!() };
        assert_eq!((c.kind, c.kind.name()), (CueKind::Other(7), "other"));
    }

    #[test]
    fn one_bad_record_costs_only_itself() {
        let good = Record { seq: 1, t: 2, body: RecordBody::Health(IntMap::new()) };
        let bytes = cbor::encode(&Value::Array(vec![good.value(), Value::Array(vec![Value::uint(1u8)])]));
        let got = decode_records(&bytes).unwrap();
        assert_eq!(got[0].as_ref().unwrap(), &good);
        assert!(got[1].is_err());
    }
}
