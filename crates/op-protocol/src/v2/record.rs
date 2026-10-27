//! Uplink records: what a collar reports. Each is a CBOR array
//! `[type, seq, t, …]`: its record type, the collar's sequence number and a
//! GNSS time in Unix seconds, then fields by position. Optional fields at the
//! end are left off; one in the middle is `null`. Elements past the ones
//! this build knows are ignored, and a record type it doesn't know is kept as
//! [`RecordBody::Other`], so a newer collar's report is never refused.
//!
//! `seq` counts every record a collar makes, across all types, from 0, and
//! never goes back or repeats, through reboots, power cuts, release, reset
//! and firmware changes (`messages.md` §7.1, [`super::counter`]). A server
//! dedupes on `(device, seq)` and asks for gaps again with a resend command.
//!
//! Every record fits one uplink frame of the smallest plan
//! ([`MAX_RECORD_BYTES`]). Records that could outgrow it are made in parts:
//! a device record ([`DeviceInfo::split`]) or a slots list carries
//! `part = [index, count]`, and the parts made at one `t` are one snapshot.
//!
//! A field this version knows that is present with the wrong type or out of
//! its range makes the record `bad_cbor`; it is never read as absent.

use op_geo::CollarLimits;
use serde_json::{Value as Json, json};

use super::cbor::{self, IntMap, Value};
use super::json::{Field, Ty, hex, table_json, table_ok};
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
    Slots(Slots),
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

/// `[8, seq, t, [entry…], part?]`: the boundaries the collar holds. A list
/// too long for one frame goes in parts, made at one `t`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Slots {
    pub entries: Vec<SlotEntry>,
    /// `[index, count]`, absent when the list is whole.
    pub part: Option<(u8, u8)>,
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
    /// 16: `[index, count]` when the snapshot is in parts.
    pub part: Option<(u8, u8)>,
    /// 17: the device public key; only while a v1 collar moves to v2
    /// (`identity.md` §10).
    pub device_key: Option<[u8; 32]>,
    /// 18: channel plans it can use (`lora.md` §2).
    pub plans: Vec<u32>,
    /// 19: `[zones, zone_vertices]`: the most permanent no-go zones and
    /// vertices in all of them it holds.
    pub zone_limits: Option<[u32; 2]>,
}

/// The largest record (its CBOR bytes) that fits one uplink frame of plan 1
/// with an empty link section: 128 − 18 bytes of frame − 1 of array head.
pub const MAX_RECORD_BYTES: usize = 109;

/// Longest text in a record (`fw`, `hw_rev`, `boot`, a firmware version,
/// a cap, a health `reset`).
pub const MAX_TEXT: usize = 32;

/// Health keys. Units are in the names; `_x10` is tenths.
const fn f(key: u64, name: &'static str, ty: Ty) -> Field {
    Field { key, name, nested: None, ty }
}

pub const HEALTH: &[Field] = &[
    f(1, "fix_attempts", Ty::Uint),
    f(2, "fix_ok", Ty::Uint),
    f(3, "sats", Ty::Uint),
    f(4, "cn0_dbhz", Ty::Uint),
    f(5, "ttf_ds", Ty::Uint),
    f(6, "hdop_x10", Ty::Uint),
    f(7, "battery_mv", Ty::Uint),
    f(8, "charging", Ty::Bool),
    f(9, "temp_dc", Ty::Int),
    f(10, "uptime_s", Ty::Uint),
    f(11, "reset", Ty::Text),
    f(12, "still_s", Ty::Uint),
    f(13, "tilt_deg", Ty::Uint),
    Field { key: 14, name: "cell", nested: Some(CELL), ty: Ty::Map },
    Field { key: 15, name: "lora", nested: Some(LORA), ty: Ty::Map },
    f(16, "solar_mv", Ty::Uint),
];

pub const CELL: &[Field] = &[
    f(1, "rsrp_dbm", Ty::Int),
    f(2, "rsrq_db", Ty::Int),
    f(3, "snr_db", Ty::Int),
    f(4, "mode", Ty::Text),
    f(5, "band", Ty::Uint),
    f(6, "cell_id", Ty::Uint),
    f(7, "tac", Ty::Uint),
];

pub const LORA: &[Field] = &[f(1, "rssi_dbm", Ty::Int), f(2, "snr_db", Ty::Int), f(3, "beacons", Ty::Uint), f(4, "missed", Ty::Uint), f(5, "frames", Ty::Uint)];

/// Anchor keys.
pub const ANCHOR: &[Field] = &[
    f(1, "active", Ty::Bool),
    f(2, "battery", Ty::Percent),
    f(3, "rsrp_dbm", Ty::Int),
    f(4, "heard", Ty::Uint),
    f(5, "rank", Ty::Uint),
    f(6, "queue", Ty::Uint),
];

// ---------------------------------------------------------------- encoding

fn u(v: impl Into<u64>) -> Value {
    Value::uint(v)
}

fn i(v: impl Into<i64>) -> Value {
    Value::int(v)
}

fn or_null(v: Option<Value>) -> Value {
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

/// An optional field: absent is `Some(None)`; present but unreadable is
/// `None` (the record is `bad_cbor`).
fn opt<T>(v: Option<&Value>, read: impl Fn(&Value) -> Option<T>) -> Option<Option<T>> {
    match v {
        None => Some(None),
        Some(v) => read(v).map(Some),
    }
}

fn kid8(v: &Value) -> Option<[u8; 8]> {
    v.as_bytes()?.try_into().ok()
}

fn text_of(v: &Value) -> Option<String> {
    v.as_text().filter(|t| t.len() <= MAX_TEXT).map(str::to_owned)
}

fn part_of(v: &Value) -> Option<(u8, u8)> {
    let [i, n] = v.as_array()? else { return None };
    let (i, n) = (u8::try_from(i.as_u64()?).ok()?, u8::try_from(n.as_u64()?).ok()?);
    (n >= 2 && i < n).then_some((i, n))
}

fn part_value((i, n): (u8, u8)) -> Value {
    Value::Array(vec![u(i), u(n)])
}

impl DeviceInfo {
    fn map(&self) -> IntMap {
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
            .put_opt(15, (!self.image_keys.is_empty()).then(|| Value::Array(self.image_keys.iter().map(|k| Value::bytes(k.to_vec())).collect())))
            .put_opt(16, self.part.map(part_value))
            .put_opt(17, self.device_key.map(|k| Value::bytes(k.to_vec())))
            .put_opt(18, (!self.plans.is_empty()).then(|| Value::Array(self.plans.iter().map(|p| u(*p)).collect())))
            .put_opt(19, self.zone_limits.map(|z| Value::Array(z.iter().map(|v| u(*v)).collect())));
        m
    }

    fn value(&self) -> Value {
        self.map().into_value()
    }

    /// Every known key that is present must read; unknown keys are ignored.
    fn from_value(v: &Value) -> Option<Self> {
        let m = IntMap::from_value(v)?;
        let list = |k: u64, read: &dyn Fn(&Value) -> Option<_>| -> Option<Vec<_>> {
            Some(opt(m.get(k), |v| v.as_array()?.iter().map(read).collect::<Option<Vec<_>>>())?.unwrap_or_default())
        };
        let caps: Vec<Value> = list(3, &|c: &Value| c.as_text().filter(|t| t.len() <= MAX_TEXT).map(|_| c.clone()))?;
        let image_keys: Vec<Value> = list(15, &|k: &Value| kid8(k).map(|_| k.clone()))?;
        let plans: Vec<Value> = list(18, &|p: &Value| p.as_u32().map(|_| p.clone()))?;
        Some(Self {
            class: opt(m.get(1), |v| v.as_u64().map(DeviceClass::from_code))?,
            fw: opt(m.get(2), text_of)?,
            caps: caps.iter().filter_map(|c| c.as_text().map(str::to_owned)).collect(),
            limits: opt(m.get(4), limits_from)?,
            hw_rev: opt(m.get(5), |v| v.as_text().filter(|t| t.len() <= 16).map(str::to_owned))?,
            server_kid: opt(m.get(6), kid8)?,
            pending_kid: opt(m.get(7), kid8)?,
            owner_kid: opt(m.get(8), kid8)?,
            versions: opt(m.get(9), |v| v.as_map()?.iter().map(|(t, v)| Some((MessageType::from_code(t.as_u64()?), v.as_u32()?))).collect::<Option<Vec<_>>>())?
                .unwrap_or_default(),
            herd: opt(m.get(10), |v| v.as_bytes().filter(|h| (1..=16).contains(&h.len())).map(<[u8]>::to_vec))?,
            short: opt(m.get(11), |v| v.as_u64().and_then(|s| u16::try_from(s).ok()).filter(|s| (1..=65_534).contains(s)))?,
            claim_nonce: opt(m.get(12), kid8)?,
            boot: opt(m.get(13), text_of)?,
            security_counter: opt(m.get(14), Value::as_u32)?,
            image_keys: image_keys.iter().filter_map(kid8).collect(),
            part: opt(m.get(16), part_of)?,
            device_key: opt(m.get(17), |v| v.as_bytes()?.try_into().ok())?,
            plans: plans.iter().filter_map(Value::as_u32).collect(),
            zone_limits: opt(m.get(19), |v| {
                let [z, n] = v.as_array()? else { return None };
                Some([z.as_u32()?, n.as_u32()?])
            })?,
        })
    }

    /// The snapshot in parts, each of whose device records (with `seq` and
    /// `t` up to 2³² − 1 and 2⁴⁰) is at most `max_record` bytes. Lists (caps,
    /// versions, image keys, plans) may be cut across parts; every other key
    /// goes whole in one. One part when it all fits.
    pub fn split(&self, max_record: usize) -> Vec<DeviceInfo> {
        // [7, seq, t, map]: array head, type, a 5-byte seq, a 9-byte t.
        let room = max_record.saturating_sub(1 + 1 + 5 + 9);
        let whole = self.map();
        if cbor::encode(&whole.clone().into_value()).len() <= room {
            return vec![DeviceInfo { part: None, ..self.clone() }];
        }
        // Atoms: whole keys, or single elements of the list keys.
        let mut atoms: Vec<(u64, Value)> = Vec::new();
        for (k, v) in whole.0.iter().filter(|(k, _)| *k != 16) {
            match (k, v) {
                (3 | 15 | 18, Value::Array(items)) => atoms.extend(items.iter().map(|i| (*k, Value::Array(vec![i.clone()])))),
                (9, Value::Map(entries)) => atoms.extend(entries.iter().map(|e| (9, Value::Map(vec![e.clone()])))),
                _ => atoms.push((*k, v.clone())),
            }
        }
        // A part leaves room for its own `part` key (at most 4 bytes).
        let room = room.saturating_sub(4);
        let mut parts: Vec<IntMap> = vec![IntMap::new()];
        for (k, v) in atoms {
            let mut trial = parts.last().unwrap().clone();
            join(&mut trial, k, v.clone());
            if cbor::encode(&trial.clone().into_value()).len() <= room || parts.last().unwrap().0.is_empty() {
                *parts.last_mut().unwrap() = trial;
            } else {
                let mut fresh = IntMap::new();
                join(&mut fresh, k, v);
                parts.push(fresh);
            }
        }
        let n = parts.len() as u8;
        parts
            .into_iter()
            .enumerate()
            .map(|(i, mut m)| {
                if n > 1 {
                    m.put(16, part_value((i as u8, n)));
                }
                DeviceInfo::from_value(&m.into_value()).expect("a split of a valid device record reads")
            })
            .collect()
    }

    /// One snapshot from its parts (any order): every index from 0 to
    /// count − 1 exactly once, the same count; lists joined, other keys from
    /// the one part that has them. `None` when parts are missing or disagree.
    pub fn merge(parts: &[DeviceInfo]) -> Option<DeviceInfo> {
        let count = match parts.first()?.part {
            None if parts.len() == 1 => return Some(parts[0].clone()),
            None => return None,
            Some((_, n)) => n,
        };
        let mut seen = vec![false; usize::from(count)];
        let mut out = IntMap::new();
        let mut parts = parts.to_vec();
        parts.sort_by_key(|p| p.part.map(|(i, _)| i));
        for p in &parts {
            let (i, n) = p.part?;
            if n != count || std::mem::replace(&mut seen[usize::from(i)], true) {
                return None;
            }
            for (k, v) in p.map().0.into_iter().filter(|(k, _)| *k != 16) {
                if !matches!(k, 3 | 9 | 15 | 18) && out.get(k).is_some() {
                    return None;
                }
                join(&mut out, k, v);
            }
        }
        if !seen.iter().all(|s| *s) {
            return None;
        }
        DeviceInfo::from_value(&out.into_value())
    }

    fn to_json(&self) -> Json {
        let mut o = serde_json::Map::new();
        let mut put = |k: &str, v: Option<Json>| {
            if let Some(v) = v {
                o.insert(k.into(), v);
            }
        };
        put("class", self.class.map(|c| c.to_json()));
        put("fw", self.fw.as_ref().map(|v| json!(v)));
        put("caps", (!self.caps.is_empty()).then(|| json!(self.caps)));
        put("limits", self.limits.as_ref().map(|l| json!(l)));
        put("hw_rev", self.hw_rev.as_ref().map(|v| json!(v)));
        put("server_kid", self.server_kid.map(|k| json!(hex(&k))));
        put("pending_kid", self.pending_kid.map(|k| json!(hex(&k))));
        put("owner_kid", self.owner_kid.map(|k| json!(hex(&k))));
        put(
            "versions",
            (!self.versions.is_empty()).then(|| {
                Json::Object(self.versions.iter().map(|(t, v)| (if t.is_known() { t.name().to_string() } else { t.code().to_string() }, json!(v))).collect())
            }),
        );
        put("herd", self.herd.as_ref().map(|h| json!(hex(h))));
        put("short", self.short.map(|v| json!(v)));
        put("claim_nonce", self.claim_nonce.map(|n| json!(hex(&n))));
        put("boot", self.boot.as_ref().map(|v| json!(v)));
        put("security_counter", self.security_counter.map(|v| json!(v)));
        put("image_keys", (!self.image_keys.is_empty()).then(|| Json::Array(self.image_keys.iter().map(|k| json!(hex(k))).collect())));
        put("part", self.part.map(|(i, n)| json!([i, n])));
        put("device_key", self.device_key.map(|k| json!(hex(&k))));
        put("plans", (!self.plans.is_empty()).then(|| json!(self.plans)));
        put("zone_limits", self.zone_limits.map(|z| json!(z)));
        Json::Object(o)
    }
}

/// Add `(k, v)` to a device map: list and map keys join, others are set.
fn join(m: &mut IntMap, k: u64, v: Value) {
    let merged = match (m.get(k), &v) {
        (Some(Value::Array(a)), Value::Array(b)) => Value::Array(a.iter().chain(b).cloned().collect()),
        (Some(Value::Map(a)), Value::Map(b)) => Value::Map(a.iter().chain(b).cloned().collect()),
        _ => v,
    };
    m.put(k, merged);
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
                v.extend([
                    i(s.lon),
                    i(s.lat),
                    u(s.acc_dm),
                    or_null(s.state.map(|st| u(st.code()))),
                    u(s.bver),
                    or_null(s.battery.map(u)),
                    or_null(s.flags.map(u)),
                ]);
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
                v.extend([u(c.kind.code()), u(c.level), i(c.margin_cm), u(c.ring), u(c.bver), or_null(c.dur_ms.map(u))]);
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
                    or_null(a.code.map(|c| u(c.code()))),
                    or_null(a.digest.map(|d| Value::bytes(d.to_vec()))),
                ]);
                v = trim(v);
            }
            RecordBody::Health(m) | RecordBody::Anchor(m) => v.push(m.clone().into_value()),
            RecordBody::Device(d) => v.push(d.value()),
            RecordBody::Slots(slots) => {
                v.push(Value::Array(
                    slots.entries.iter().map(|s| Value::Array(trim(vec![u(s.version), u(s.status.code()), or_null(s.effective_at.map(u))]))).collect(),
                ));
                if let Some(p) = slots.part {
                    v.push(part_value(p));
                }
            }
            RecordBody::Firmware(f) => {
                v.extend([
                    u(f.state.code()),
                    Value::text(f.version.clone()),
                    or_null(f.digest.map(|d| Value::bytes(d.to_vec()))),
                    or_null(f.code.map(|c| u(c.code()))),
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

    /// One record from its array. A record that doesn't fit its type's shape,
    /// or a known field present with the wrong type or out of range, is
    /// `bad_cbor`; the caller keeps the others.
    pub fn from_value(v: &Value) -> Result<Self, Reject> {
        let bad = Reject::BadCbor;
        let a = v.as_array().ok_or(bad)?;
        let [rt, seq, t, rest @ ..] = a else { return Err(bad) };
        let rt = RecordType::from_code(rt.as_u64().ok_or(bad)?);
        let seq = seq.as_u32().ok_or(bad)?;
        let t = t.as_u64().ok_or(bad)?;
        let at = |k: usize| rest.get(k).filter(|v| !v.is_null());
        let req_u = |k: usize| at(k).and_then(Value::as_u64).ok_or(bad);
        let req_u32 = |k: usize| at(k).and_then(Value::as_u32).ok_or(bad);
        let req_i32 = |k: usize| at(k).and_then(Value::as_i32).ok_or(bad);
        let short = |k: usize| req_u(k).and_then(|n| u16::try_from(n).map_err(|_| bad));
        let ranged = |k: usize, r: std::ops::RangeInclusive<u8>| req_u(k).ok().and_then(|n| u8::try_from(n).ok()).filter(|n| r.contains(n)).ok_or(bad);
        // Optional: absent or null is None; anything else must read.
        let maybe = |k: usize, read: &dyn Fn(&Value) -> Option<u64>| -> Result<Option<u64>, Reject> { at(k).map(|v| read(v).ok_or(bad)).transpose() };
        let u32_of = |v: &Value| v.as_u32().map(u64::from);
        let digest = |k: usize| at(k).map(|v| kid8(v).ok_or(bad)).transpose();
        let body = match rt {
            RecordType::Status => RecordBody::Status(Status {
                lon: req_i32(0)?,
                lat: req_i32(1)?,
                acc_dm: req_u32(2)?,
                state: maybe(3, &Value::as_u64)?.map(FenceState::from_code),
                bver: req_u32(4)?,
                battery: maybe(5, &|v| v.as_u64().filter(|b| *b <= 100))?.map(|b| b as u8),
                flags: maybe(6, &u32_of)?.map(|f| f as u32),
            }),
            RecordType::Fixes => {
                let mut fix = Fix { t, lon: req_i32(0)?, lat: req_i32(1)?, acc_dm: req_u32(2)? };
                let mut fixes = vec![fix];
                let deltas = at(3).and_then(Value::as_array).ok_or(bad)?;
                if deltas.len() % 4 != 0 {
                    return Err(bad);
                }
                for d in deltas.chunks(4) {
                    let n = |k: usize| d[k].as_i64().ok_or(bad);
                    // Checked: a hostile delta never overflows (Astra 01 F12).
                    let step = |base: i32, by: i64| i64::from(base).checked_add(by).and_then(|x| i32::try_from(x).ok()).ok_or(bad);
                    fix = Fix {
                        t: fix.t.checked_add(d[0].as_u64().ok_or(bad)?).ok_or(bad)?,
                        lon: step(fix.lon, n(1)?)?,
                        lat: step(fix.lat, n(2)?)?,
                        acc_dm: d[3].as_u32().ok_or(bad)?,
                    };
                    fixes.push(fix);
                }
                RecordBody::Fixes(fixes)
            }
            RecordType::Cue => RecordBody::Cue(Cue {
                kind: CueKind::from_code(req_u(0)?),
                level: ranged(1, 1..=4)?,
                margin_cm: req_i32(2)?,
                ring: short(3)?,
                bver: req_u32(4)?,
                dur_ms: maybe(5, &u32_of)?.map(|d| d as u32),
            }),
            RecordType::Episode => RecordBody::Episode(Episode {
                dur_s: req_u32(0)?,
                bver: req_u32(1)?,
                ring: short(2)?,
                cues: req_u32(3)?,
                max_level: ranged(4, 0..=4)?,
                min_margin_cm: req_i32(5)?,
                outcome: EpisodeOutcome::from_code(req_u(6)?),
            }),
            RecordType::Ack => RecordBody::Ack(Ack {
                message_type: MessageType::from_code(req_u(0)?),
                version: req_u32(1)?,
                status: AckStatus::from_code(req_u(2)?),
                code: maybe(3, &Value::as_u64)?.map(Reject::from_code),
                digest: digest(4)?,
            }),
            RecordType::Health => RecordBody::Health(at(0).and_then(IntMap::from_value).filter(|m| table_ok(m, HEALTH)).ok_or(bad)?),
            RecordType::Anchor => RecordBody::Anchor(at(0).and_then(IntMap::from_value).filter(|m| table_ok(m, ANCHOR)).ok_or(bad)?),
            RecordType::Device => RecordBody::Device(at(0).and_then(DeviceInfo::from_value).ok_or(bad)?),
            RecordType::Slots => RecordBody::Slots(Slots {
                entries: at(0)
                    .and_then(Value::as_array)
                    .ok_or(bad)?
                    .iter()
                    .map(|s| {
                        let s = s.as_array().ok_or(bad)?;
                        Ok(SlotEntry {
                            version: s.first().and_then(Value::as_u32).ok_or(bad)?,
                            status: AckStatus::from_code(s.get(1).and_then(Value::as_u64).ok_or(bad)?),
                            effective_at: s.get(2).filter(|v| !v.is_null()).map(|v| v.as_u64().ok_or(bad)).transpose()?,
                        })
                    })
                    .collect::<Result<_, Reject>>()?,
                part: at(1).map(|v| part_of(v).ok_or(bad)).transpose()?,
            }),
            RecordType::Firmware => RecordBody::Firmware(FirmwareStatus {
                state: FirmwareState::from_code(req_u(0)?),
                version: at(1).and_then(text_of).ok_or(bad)?,
                digest: digest(2)?,
                code: maybe(3, &Value::as_u64)?.map(Reject::from_code),
            }),
            RecordType::Other(code) => RecordBody::Other { record_type: code, fields: rest.to_vec() },
        };
        Ok(Self { seq, t, body })
    }

    /// The JSON view.
    pub fn to_json(&self) -> Json {
        let mut o = serde_json::Map::new();
        o.insert("type".into(), self.record_type().to_json());
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
                put("state", s.state.map(|st| st.to_json()));
                put("bver", Some(json!(s.bver)));
                put("battery", s.battery.map(|b| json!(b)));
                put("flags", s.flags.map(|f| json!(f)));
            }
            RecordBody::Fixes(f) => put("fixes", Some(Json::Array(f.iter().map(|x| json!([x.t, x.lon, x.lat, x.acc_dm])).collect()))),
            RecordBody::Cue(c) => {
                put("kind", Some(c.kind.to_json()));
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
                put("outcome", Some(e.outcome.to_json()));
            }
            RecordBody::Ack(a) => {
                put("message_type", Some(a.message_type.to_json()));
                put("version", Some(json!(a.version)));
                put("status", Some(a.status.to_json()));
                put("code", a.code.map(|c| c.to_json()));
                put("digest", a.digest.map(|d| json!(hex(&d))));
            }
            RecordBody::Health(m) => put("health", Some(table_json(m, HEALTH))),
            RecordBody::Anchor(m) => put("anchor", Some(table_json(m, ANCHOR))),
            RecordBody::Device(d) => put("device", Some(d.to_json())),
            RecordBody::Slots(s) => {
                put(
                    "slots",
                    Some(Json::Array(
                        s.entries
                            .iter()
                            .map(|e| {
                                let mut x = json!({"version": e.version, "status": e.status.to_json()});
                                if let Some(t) = e.effective_at {
                                    x["effective_at"] = json!(t);
                                }
                                x
                            })
                            .collect(),
                    )),
                );
                put("part", s.part.map(|(i, n)| json!([i, n])));
            }
            RecordBody::Firmware(f) => {
                put("state", Some(f.state.to_json()));
                put("version", Some(json!(f.version)));
                put("digest", f.digest.map(|d| json!(hex(&d))));
                put("code", f.code.map(|c| c.to_json()));
            }
            RecordBody::Other { fields, .. } => {
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

/// Read a frame's records (`messages.md` §6). The plaintext must start with
/// a definite array head; each record is then found by its extent and read
/// on its own, as deterministic, untagged CBOR of its type's shape. One
/// malformed record costs only itself (Astra 01 F12: a float in one record
/// used to drop its valid neighbours). Only a record whose extent can't be
/// found (truncated, indefinite) loses what follows it, as one `Err`.
pub fn decode_records(bytes: &[u8]) -> Result<Vec<Result<Record, Reject>>, Reject> {
    let (&first, rest) = bytes.split_first().ok_or(Reject::BadCbor)?;
    let width = match first & 0x1f {
        0..=23 => 0,
        24 => 1,
        25 => 2,
        26 => 4,
        27 => 8,
        _ => return Err(Reject::BadCbor),
    };
    if first >> 5 != 4 {
        return Err(Reject::BadCbor);
    }
    let count = match width {
        0 => u64::from(first & 0x1f),
        w => rest.get(..w).ok_or(Reject::BadCbor)?.iter().fold(0u64, |a, b| (a << 8) | u64::from(*b)),
    };
    let mut at = 1 + width;
    let mut out = Vec::new();
    for _ in 0..count {
        if at >= bytes.len() {
            out.push(Err(Reject::BadCbor));
            return Ok(out);
        }
        match cbor::item_len(&bytes[at..]) {
            Ok(n) => {
                let item = &bytes[at..at + n];
                out.push(cbor::decode_untagged(item).map_err(|_| Reject::BadCbor).and_then(|v| Record::from_value(&v)));
                at += n;
            }
            Err(_) => {
                out.push(Err(Reject::BadCbor));
                return Ok(out);
            }
        }
    }
    if at != bytes.len() {
        out.push(Err(Reject::BadCbor));
    }
    Ok(out)
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

    /// Astra 01 F12: a float in one record made the whole frame `bad_cbor`.
    #[test]
    fn a_record_outside_the_profile_costs_only_itself() {
        let good = Record { seq: 1, t: 2, body: RecordBody::Health(IntMap::new()) };
        let mut bytes = vec![0x83];
        bytes.extend(good.encode());
        bytes.extend([0x83, 0x01, 0x02, 0xf9, 0x3c, 0x00]); // [1, 2, 1.0]
        bytes.extend(good.encode());
        let got = decode_records(&bytes).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].as_ref().unwrap(), &good);
        assert_eq!(got[1], Err(Reject::BadCbor));
        assert_eq!(got[2].as_ref().unwrap(), &good, "the neighbour after it survives");
        // A tag inside a record is outside the profile too.
        let mut tagged = vec![0x82];
        tagged.extend([0x83, 0x06, 0x01, 0xc1, 0x00]);
        tagged.extend(good.encode());
        let got = decode_records(&tagged).unwrap();
        assert!(got[0].is_err() && got[1].is_ok());
        // Truncation loses only what follows.
        let got = decode_records(&bytes[..bytes.len() - 2]).unwrap();
        assert!(got[0].is_ok() && got[1].is_err() && got[2].is_err());
    }

    /// Astra 01 F12: base 1 plus an i64::MAX delta panicked in debug builds.
    #[test]
    fn fix_deltas_never_overflow() {
        let v = Value::Array(vec![
            Value::uint(2u8),
            Value::uint(1u8),
            Value::uint(100u8),
            Value::int(1),
            Value::int(1),
            Value::uint(1u8),
            Value::Array(vec![Value::uint(60u8), Value::int(i64::MAX), Value::int(0), Value::uint(1u8)]),
        ]);
        assert_eq!(Record::from_value(&v), Err(Reject::BadCbor));
    }

    /// Astra 01 F14: battery 255, a cue at level 255, a status `state` that
    /// isn't a number: each was read (or dropped to "absent") instead of refused.
    #[test]
    fn known_fields_out_of_range_are_refused() {
        let ints = |v: &[i64]| Value::Array(v.iter().map(|x| Value::int(*x)).collect());
        assert_eq!(Record::from_value(&ints(&[1, 1, 100, 1, 1, 18, 1, 57, 255])), Err(Reject::BadCbor), "battery 255");
        assert!(Record::from_value(&ints(&[1, 1, 100, 1, 1, 18, 1, 57, 100])).is_ok());
        assert_eq!(Record::from_value(&ints(&[3, 1, 100, 1, 255, -150, 0, 57])), Err(Reject::BadCbor), "cue level 255");
        assert_eq!(Record::from_value(&ints(&[3, 1, 100, 1, 0, -150, 0, 57])), Err(Reject::BadCbor), "cue level 0");
        let mut s = ints(&[1, 1, 100, 1, 1, 18, 1, 57]);
        if let Value::Array(a) = &mut s {
            a[6] = Value::text("inside");
        }
        assert_eq!(Record::from_value(&s), Err(Reject::BadCbor), "state as text");
        let health = Value::Array(vec![Value::uint(6u8), Value::uint(1u8), Value::uint(2u8), IntMap(vec![(8, Value::uint(1u8))]).into_value()]);
        assert_eq!(Record::from_value(&health), Err(Reject::BadCbor), "charging must be a bool");
        let mut dev = IntMap::new();
        dev.put(3, Value::Array(vec![Value::text("holes"), Value::uint(7u8)]));
        let device = Value::Array(vec![Value::uint(7u8), Value::uint(1u8), Value::uint(2u8), dev.into_value()]);
        assert_eq!(Record::from_value(&device), Err(Reject::BadCbor), "a cap that isn't text");
    }

    fn big_device() -> DeviceInfo {
        DeviceInfo {
            class: Some(DeviceClass::Anchor),
            fw: Some("2.0.0-rc.1+build.20261027".into()),
            caps: ["holes", "slots", "collar_id", "cue_mode", "episodes", "config", "v2", "fixes", "firmware", "handover", "owner", "imu", "zones"]
                .map(String::from)
                .to_vec(),
            limits: Some(CollarLimits::V0),
            hw_rev: Some("A1.0".into()),
            server_kid: Some([1; 8]),
            pending_kid: Some([2; 8]),
            owner_kid: Some([3; 8]),
            versions: (1..=13).map(|t| (MessageType::from_code(t), 4_000_000_000)).collect(),
            herd: Some(vec![0xab; 16]),
            short: Some(65_534),
            boot: Some("mcuboot 2.1.0".into()),
            security_counter: Some(4_000_000_000),
            image_keys: vec![[4; 8], [5; 8], [6; 8], [7; 8]],
            plans: vec![1, 2],
            zone_limits: Some([16, 512]),
            ..Default::default()
        }
    }

    /// Astra 01 F02: the published device record sealed to 183 bytes against
    /// a 128-byte uplink. Split, every part fits one frame, and the parts
    /// merge back into the snapshot.
    #[test]
    fn a_device_record_goes_in_parts_that_each_fit_a_frame() {
        let d = big_device();
        let whole = Record { seq: u32::MAX, t: 1 << 39, body: RecordBody::Device(d.clone()) };
        assert!(whole.encode().len() > MAX_RECORD_BYTES, "too big for one frame as a whole");
        let parts = d.split(MAX_RECORD_BYTES);
        assert!(parts.len() > 1);
        for p in &parts {
            let r = Record { seq: u32::MAX, t: 1 << 39, body: RecordBody::Device(p.clone()) };
            assert!(r.encode().len() <= MAX_RECORD_BYTES, "{} bytes", r.encode().len());
            assert_eq!(Record::from_value(&cbor::decode(&r.encode()).unwrap()).unwrap(), r);
        }
        let mut shuffled = parts.clone();
        shuffled.reverse();
        assert_eq!(DeviceInfo::merge(&shuffled), Some(DeviceInfo { part: None, ..d.clone() }));
        assert_eq!(DeviceInfo::merge(&parts[1..]), None, "a part missing");
        let small = DeviceInfo { class: Some(DeviceClass::Node), ..Default::default() };
        assert_eq!(small.split(MAX_RECORD_BYTES), vec![small.clone()], "what fits stays whole");
    }

    #[test]
    fn slots_in_parts() {
        let r = Record {
            seq: 9,
            t: 100,
            body: RecordBody::Slots(Slots { entries: vec![SlotEntry { version: 57, status: AckStatus::Applied, effective_at: None }], part: Some((0, 2)) }),
        };
        assert_eq!(Record::from_value(&r.value()).unwrap(), r);
        let bad = Value::Array(vec![
            Value::uint(8u8),
            Value::uint(9u8),
            Value::uint(100u8),
            Value::Array(vec![]),
            Value::Array(vec![Value::uint(2u8), Value::uint(2u8)]),
        ]);
        assert_eq!(Record::from_value(&bad), Err(Reject::BadCbor), "index past the count");
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
