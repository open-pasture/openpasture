//! The OpenCollar device protocol, v1 (field-ready contract §3; the V0 shape
//! of `opencollar/protocol/README.md` is a subset and serializes byte for byte
//! as before).
//!
//! Boundaries go down ([`BoundaryCommand`]), so does collar configuration
//! ([`ConfigCommand`], inside [`ReportResponse`]); acknowledgements ([`Ack`])
//! and position reports ([`PositionReport`]) come up. HTTP + JSON; the same
//! logical messages may map one to one onto CBOR later.
//!
//! Commands are signed with Ed25519 over their canonical JSON without `sig`
//! (see [`canonical_json`] and [`sign_command`]) so a collar only accepts its
//! own server's commands. A collar checks what it receives with
//! [`verify_wire`] (raw bytes, as the firmware scans them), then
//! [`BoundaryCommand::check`], then offers it to its [`SlotStore`].
//!
//! Every new field is optional and omitted when empty or default, and every
//! wire struct derives `Default`, so code elsewhere builds them with
//! `..Default::default()`.

mod canonical;
pub mod config;
mod sign;
pub mod slots;
pub mod wire;
pub mod wire_time;

use chrono::{DateTime, Utc};
use op_geo::shape::{self, ShapeCode};
use op_geo::{Episode, GeoError, Geofence, GeofenceConfig, LonLat, MAX_COLLAR_VERTICES, Polygon};
use serde::{Deserialize, Serialize};

pub use canonical::canonical_json;
pub use config::{ConfigCommand, sign_config, verify_config};
pub use ed25519_dalek::{SigningKey, VerifyingKey};
pub use op_geo::{CollarLimits, CueKind, CueMode, EpisodeOutcome};
pub use sign::{
    decode_public_key, decode_signing_key, encode_public_key, encode_signing_key, generate_signing_key, sign_command, signing_payload, verify_command,
    verify_json,
};
pub use slots::{Slot, SlotAck, SlotStore, Split, split_by_activation};
pub use wire::{Canonical, canonical_wire, verify_config_wire, verify_wire};

pub const MIN_VERTICES: usize = 3;
/// Most vertices of a V0-shaped (firmware 0.1) command: [`CollarLimits::LEGACY`].
pub const MAX_VERTICES: usize = MAX_COLLAR_VERTICES;
/// Most fixes accepted in one report.
pub const MAX_FIXES_PER_REPORT: usize = 10_000;
/// Longest `command_id`, `herd_id` or `collar_id`, in bytes. Server ids are ≤ 31.
pub const MAX_ID_BYTES: usize = 64;
/// Largest command a collar accepts, in bytes of wire text.
pub const MAX_COMMAND_BYTES: usize = 12_288;
/// Most top-level keys in a command.
pub const MAX_TOP_LEVEL_KEYS: usize = 16;

/// Capabilities a collar reports in `device.caps`. The server never sends a
/// field that isn't in the collar's caps.
pub mod caps {
    /// Holds holes.
    pub const HOLES: &str = "holes";
    /// Holds staged slots and reports them.
    pub const SLOTS: &str = "slots";
    /// Accepts collar-scoped commands.
    pub const COLLAR_ID: &str = "collar_id";
    /// Accepts `cue_mode`.
    pub const CUE_MODE: &str = "cue_mode";
    /// Reports episodes.
    pub const EPISODES: &str = "episodes";
    /// Accepts config commands.
    pub const CONFIG: &str = "config";
    /// Everything firmware 0.2 does.
    pub const ALL: [&str; 6] = [HOLES, SLOTS, COLLAR_ID, CUE_MODE, EPISODES, CONFIG];
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProtocolError {
    #[error("{0}")]
    Geo(#[from] GeoError),
    #[error("The boundary has holes. V0 collars cannot enforce holes.")]
    Holes,
    #[error("Version {version} is not newer than the current version {current}.")]
    StaleVersion { version: u32, current: u32 },
    #[error("Invalid {0}.")]
    Invalid(&'static str),
    #[error("The boundary is not signed.")]
    MissingSignature,
    #[error("The boundary signature does not verify.")]
    BadSignature,
    #[error("Invalid key.")]
    BadKey,
    #[error("The boundary is for another herd.")]
    WrongHerd,
    #[error("{}", .0.message())]
    Rejected(RejectCode),
}

/// Why a collar rejected a command (the ack's `code`, protocol v1 §3.5).
/// Every code except `slots_full` is permanent: the server doesn't offer
/// that version to that collar again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    BadSig,
    WrongHerd,
    WrongCollar,
    BadJson,
    TooLarge,
    Stale,
    OutOfRange,
    TooFewVertices,
    TooManyVertices,
    TooManyHoles,
    SelfIntersecting,
    RingsCross,
    HoleOutside,
    HolesOverlap,
    ZeroArea,
    HoleTooSmall,
    HoleTooClose,
    BadMargins,
    SlotsFull,
    BadConfig,
}

impl RejectCode {
    pub const ALL: [RejectCode; 20] = [
        Self::BadSig,
        Self::WrongHerd,
        Self::WrongCollar,
        Self::BadJson,
        Self::TooLarge,
        Self::Stale,
        Self::OutOfRange,
        Self::TooFewVertices,
        Self::TooManyVertices,
        Self::TooManyHoles,
        Self::SelfIntersecting,
        Self::RingsCross,
        Self::HoleOutside,
        Self::HolesOverlap,
        Self::ZeroArea,
        Self::HoleTooSmall,
        Self::HoleTooClose,
        Self::BadMargins,
        Self::SlotsFull,
        Self::BadConfig,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadSig => "bad_sig",
            Self::WrongHerd => "wrong_herd",
            Self::WrongCollar => "wrong_collar",
            Self::BadJson => "bad_json",
            Self::TooLarge => "too_large",
            Self::Stale => "stale",
            Self::OutOfRange => "out_of_range",
            Self::TooFewVertices => "too_few_vertices",
            Self::TooManyVertices => "too_many_vertices",
            Self::TooManyHoles => "too_many_holes",
            Self::SelfIntersecting => "self_intersecting",
            Self::RingsCross => "rings_cross",
            Self::HoleOutside => "hole_outside",
            Self::HolesOverlap => "holes_overlap",
            Self::ZeroArea => "zero_area",
            Self::HoleTooSmall => "hole_too_small",
            Self::HoleTooClose => "hole_too_close",
            Self::BadMargins => "bad_margins",
            Self::SlotsFull => "slots_full",
            Self::BadConfig => "bad_config",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// Everything but `slots_full`, which clears once the collar has room.
    pub fn is_permanent(self) -> bool {
        self != Self::SlotsFull
    }

    /// One short sentence for people.
    pub fn message(self) -> &'static str {
        match self {
            Self::BadSig => "The signature does not verify.",
            Self::WrongHerd => "The command is for another herd.",
            Self::WrongCollar => "The command is for another collar.",
            Self::BadJson => "The command is not valid.",
            Self::TooLarge => "The command is too large.",
            Self::Stale => "The collar already holds a newer version.",
            Self::SlotsFull => "The collar has no room for another staged boundary.",
            Self::BadConfig => "The configuration is out of range.",
            Self::OutOfRange => ShapeCode::OutOfRange.message(),
            Self::TooFewVertices => ShapeCode::TooFewVertices.message(),
            Self::TooManyVertices => ShapeCode::TooManyVertices.message(),
            Self::TooManyHoles => ShapeCode::TooManyHoles.message(),
            Self::SelfIntersecting => ShapeCode::SelfIntersecting.message(),
            Self::RingsCross => ShapeCode::RingsCross.message(),
            Self::HoleOutside => ShapeCode::HoleOutside.message(),
            Self::HolesOverlap => ShapeCode::HolesOverlap.message(),
            Self::ZeroArea => ShapeCode::ZeroArea.message(),
            Self::HoleTooSmall => ShapeCode::HoleTooSmall.message(),
            Self::HoleTooClose => ShapeCode::HoleTooClose.message(),
            Self::BadMargins => ShapeCode::BadMargins.message(),
        }
    }
}

impl std::fmt::Display for RejectCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<ShapeCode> for RejectCode {
    fn from(c: ShapeCode) -> Self {
        match c {
            ShapeCode::BadMargins => Self::BadMargins,
            ShapeCode::TooManyHoles => Self::TooManyHoles,
            ShapeCode::OutOfRange => Self::OutOfRange,
            ShapeCode::TooFewVertices => Self::TooFewVertices,
            ShapeCode::TooManyVertices => Self::TooManyVertices,
            ShapeCode::SelfIntersecting => Self::SelfIntersecting,
            ShapeCode::RingsCross => Self::RingsCross,
            ShapeCode::HoleOutside => Self::HoleOutside,
            ShapeCode::HolesOverlap => Self::HolesOverlap,
            ShapeCode::ZeroArea => Self::ZeroArea,
            ShapeCode::HoleTooSmall => Self::HoleTooSmall,
            ShapeCode::HoleTooClose => Self::HoleTooClose,
        }
    }
}

impl From<RejectCode> for ProtocolError {
    fn from(c: RejectCode) -> Self {
        Self::Rejected(c)
    }
}

/// `Option<RejectCode>` that reads a code it doesn't know (a newer firmware's)
/// as `None` rather than refusing the whole message: a collar keeps a pending
/// ack until the server takes it, so a refused ack would be resent forever.
mod lenient_code {
    use super::RejectCode;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(c: &Option<RejectCode>, s: S) -> Result<S::Ok, S::Error> {
        match c {
            Some(c) => s.serialize_str(c.as_str()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<RejectCode>, D::Error> {
        Ok(Option::<String>::deserialize(d)?.and_then(|s| RejectCode::parse(&s)))
    }
}

fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES
}

/// A boundary sent to a collar (§3.2). `boundary` is the outer ring and
/// `holes` the holes, each unclosed, `[lon, lat]` with at most 7 decimals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BoundaryCommand {
    pub command_id: String,
    /// The herd this boundary is for. Signed with the rest, so a boundary
    /// for one herd can't be replayed to a collar in another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
    /// Only on one collar's own boundary (escape pens, per-collar restaged
    /// copies). Signed, so it can't be replayed to another collar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "wire_time::option")]
    pub effective_at: Option<DateTime<Utc>>,
    pub boundary: Vec<LonLat>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holes: Vec<Vec<LonLat>>,
    /// `audio` (the default, never serialized) or `track`.
    #[serde(default, skip_serializing_if = "CueMode::is_audio")]
    pub cue_mode: CueMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hysteresis_m: Option<f64>,
    /// Ed25519 signature (base64) over the canonical JSON without `sig`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl BoundaryCommand {
    /// Build an unsigned V0-shaped command from a Polygon: one ring of 3-64
    /// vertices, no holes. Validates and normalises the ring.
    pub fn from_polygon(command_id: impl Into<String>, version: u32, polygon: &Polygon, effective_at: Option<DateTime<Utc>>) -> Result<Self, ProtocolError> {
        if polygon.coordinates.len() > 1 {
            return Err(ProtocolError::Holes);
        }
        let ring = op_geo::validate_ring(&polygon.outer_ring(), Some(MAX_VERTICES))?;
        Ok(Self { command_id: command_id.into(), version, effective_at: effective_at.map(wire_time::trunc_secs), boundary: ring, ..Default::default() })
    }

    /// Build an unsigned command for a shape with holes: every ring rounded
    /// to 7 decimals, unclosed, consecutive duplicates removed. Limits and
    /// shape rules are [`BoundaryCommand::check`]'s job (fit the shape with
    /// [`op_geo::shape::fit`] first).
    pub fn from_shape(command_id: impl Into<String>, version: u32, polygon: &Polygon, effective_at: Option<DateTime<Utc>>) -> Result<Self, ProtocolError> {
        let round =
            |r: &Vec<LonLat>| op_geo::clean_ring(&r.iter().map(|p| [op_geo::projection::round7(p[0]), op_geo::projection::round7(p[1])]).collect::<Vec<_>>());
        let mut rings = polygon.coordinates.iter().map(round);
        let boundary = rings.next().ok_or(GeoError::Empty)?;
        if boundary.len() < MIN_VERTICES {
            return Err(GeoError::TooFewVertices.into());
        }
        Ok(Self {
            command_id: command_id.into(),
            version,
            effective_at: effective_at.map(wire_time::trunc_secs),
            boundary,
            holes: rings.collect(),
            ..Default::default()
        })
    }

    /// The V0 collar's checks: 3-64 vertices, no holes, coordinate ranges, a
    /// ring that does not cross itself, sane margins, ids of 1-64 bytes, and
    /// a version newer than `current_version`.
    pub fn validate(&self, current_version: Option<u32>) -> Result<(), ProtocolError> {
        if self.check_ids().is_err() {
            return Err(ProtocolError::Invalid("command_id"));
        }
        if !self.holes.is_empty() {
            return Err(ProtocolError::Holes);
        }
        op_geo::validate_ring(&self.boundary, Some(MAX_VERTICES))?;
        for (value, name) in [(self.warn_m, "warn_m"), (self.hysteresis_m, "hysteresis_m")] {
            if let Some(v) = value
                && (!v.is_finite() || !(0.0..=1000.0).contains(&v))
            {
                return Err(ProtocolError::Invalid(name));
            }
        }
        if let Some(current) = current_version
            && self.version <= current
        {
            return Err(ProtocolError::StaleVersion { version: self.version, current });
        }
        Ok(())
    }

    /// `command_id` of 1-64 bytes; `herd_id` and `collar_id`, when present, too.
    pub fn check_ids(&self) -> Result<(), RejectCode> {
        let opt_ok = |id: &Option<String>| id.as_deref().is_none_or(id_ok);
        if id_ok(&self.command_id) && opt_ok(&self.herd_id) && opt_ok(&self.collar_id) { Ok(()) } else { Err(RejectCode::BadJson) }
    }

    /// What a v1 collar checks once the signature verifies, in order: ids
    /// (`bad_json`), `wrong_herd`, `wrong_collar`, then the shape rules
    /// ([`op_geo::shape`]) with missing margins taken from `defaults`. A
    /// collar that doesn't know its own herd or id (`None`) skips that check.
    /// `stale` and `slots_full` are the [`SlotStore`]'s.
    pub fn check(&self, herd_id: Option<&str>, collar_id: Option<&str>, limits: &CollarLimits, defaults: &GeofenceConfig) -> Result<(), RejectCode> {
        self.check_ids()?;
        if let (Some(theirs), Some(ours)) = (self.herd_id.as_deref(), herd_id)
            && theirs != ours
        {
            return Err(RejectCode::WrongHerd);
        }
        if let (Some(theirs), Some(ours)) = (self.collar_id.as_deref(), collar_id)
            && theirs != ours
        {
            return Err(RejectCode::WrongCollar);
        }
        self.check_shape(limits, defaults)
    }

    /// The shape rules alone (collar side: no slack).
    pub fn check_shape(&self, limits: &CollarLimits, defaults: &GeofenceConfig) -> Result<(), RejectCode> {
        let (warn_m, hysteresis_m) = self.margins(defaults);
        shape::check_rings(&self.boundary, &self.holes, limits, warn_m, hysteresis_m, 0.0).map_err(RejectCode::from)
    }

    /// `warn_m` and `hysteresis_m`, or the collar's defaults.
    pub fn margins(&self, defaults: &GeofenceConfig) -> (f64, f64) {
        (self.warn_m.unwrap_or(defaults.warn_m), self.hysteresis_m.unwrap_or(defaults.hysteresis_m))
    }

    /// A collar in `herd_id` checks the command names its herd. Commands
    /// without a herd (older servers) pass.
    pub fn check_herd(&self, herd_id: &str) -> Result<(), ProtocolError> {
        match &self.herd_id {
            Some(h) if h != herd_id => Err(ProtocolError::WrongHerd),
            _ => Ok(()),
        }
    }

    /// Vertices over every ring as a collar stores them (e7, consecutive
    /// duplicates and closing vertices not counted).
    pub fn total_vertices(&self) -> usize {
        std::iter::once(&self.boundary).chain(&self.holes).map(|r| shape::e7_ring(r).len()).sum()
    }

    /// Flash bytes this command takes in a slot.
    pub fn record_bytes(&self) -> usize {
        CollarLimits::record_bytes(self.total_vertices())
    }

    /// When it takes effect: `effective_at`, else the time it was received.
    pub fn activation(&self, received_at: DateTime<Utc>) -> DateTime<Utc> {
        self.effective_at.unwrap_or(received_at)
    }

    /// The outer ring and holes as a closed Polygon.
    pub fn polygon(&self) -> Polygon {
        Polygon::from_rings(self.boundary.clone(), self.holes.iter().cloned())
    }

    /// A single-ring geofence for this boundary, as firmware 0.1 builds it,
    /// with `warn_m` and `hysteresis_m` overriding the collar's defaults.
    pub fn geofence(&self, defaults: GeofenceConfig) -> Result<Geofence, ProtocolError> {
        Ok(Geofence::new(self.fence_config(defaults), &self.boundary, self.version)?)
    }

    /// The geofence a v1 collar builds: every ring, within `limits` by count.
    pub fn fence(&self, defaults: GeofenceConfig, limits: &CollarLimits) -> Result<Geofence, ProtocolError> {
        Ok(Geofence::from_polygon(self.fence_config(defaults), &self.polygon(), self.version, limits)?)
    }

    fn fence_config(&self, defaults: GeofenceConfig) -> GeofenceConfig {
        let (warn_m, hysteresis_m) = self.margins(&defaults);
        GeofenceConfig { warn_m, hysteresis_m, max_accuracy_m: defaults.max_accuracy_m }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AckStatus {
    /// Stored, waiting for `effective_at`.
    #[default]
    Received,
    Applied,
    Rejected,
}

impl AckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "received" => Some(Self::Received),
            "applied" => Some(Self::Applied),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

/// A collar's acknowledgement of a boundary.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    /// Optional on the wire: the collar is known from its key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    pub command_id: String,
    pub version: u32,
    pub status: AckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Why it was rejected. A code this build doesn't know reads as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "lenient_code")]
    pub code: Option<RejectCode>,
    /// For `applied`, when the collar applied it (GNSS time, possibly in the
    /// past for a boundary applied offline).
    #[serde(with = "wire_time", alias = "received_at")]
    pub at: DateTime<Utc>,
}

/// One GNSS fix.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WireFix {
    #[serde(with = "wire_time")]
    pub at: DateTime<Utc>,
    pub point: LonLat,
    #[serde(alias = "accuracy_meters")]
    pub accuracy_m: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sats: Option<u32>,
    /// Mean carrier-to-noise density of the satellites used, dB-Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cn0: Option<f64>,
    /// Time to fix, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttf_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdop: Option<f64>,
    /// Only when it differs from the report's (a staged boundary applied
    /// offline can split one batch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_version: Option<u32>,
}

/// A cue the collar played, and how far past (negative) or short (positive)
/// of the line the animal was.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WireCue {
    #[serde(with = "wire_time")]
    pub at: DateTime<Utc>,
    /// Absent from firmware 0.1: readers take `outside` when `margin_m` < 0, else `warn`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<CueKind>,
    pub level: u8,
    /// Tone length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dur_ms: Option<u32>,
    #[serde(alias = "margin_meters")]
    pub margin_m: f64,
    /// Nearest ring: 0 outer, 1.. holes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ring: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<LonLat>,
}

/// A run of armed warning cues (§3.6).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WireEpisode {
    #[serde(with = "wire_time")]
    pub start: DateTime<Utc>,
    #[serde(with = "wire_time")]
    pub end: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_version: Option<u32>,
    pub ring: u16,
    pub cues: u32,
    pub max_level: u8,
    pub min_margin_m: f64,
    pub outcome: EpisodeOutcome,
}

impl WireEpisode {
    /// From an [`Episode`] whose times are Unix milliseconds.
    pub fn from_episode(e: &Episode, boundary_version: Option<u32>) -> Option<Self> {
        Some(Self {
            start: DateTime::from_timestamp_millis(e.start)?,
            end: DateTime::from_timestamp_millis(e.end)?,
            boundary_version,
            ring: u16::try_from(e.ring).ok()?,
            cues: e.cues,
            max_level: e.max_level,
            min_margin_m: e.min_margin_m,
            outcome: e.outcome,
        })
    }
}

/// Serving cell, when the modem measured one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CellInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rsrp_dbm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rsrq_db: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f64>,
    /// `ltem` or `nbiot`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tac: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "wire_time::option")]
    pub at: Option<DateTime<Utc>>,
}

/// Receiver and device health for the report period.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Health {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sats: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cn0: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttf_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_ok: Option<u32>,
    /// Only when measured (never on GNSS-only boards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<CellInfo>,
    /// Seconds without movement (IMU boards only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub still_s: Option<u32>,
    /// Tilt from level (IMU boards only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tilt_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_v: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charging: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
    /// Cause of the last reset, e.g. `power_on`, `watchdog`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<String>,
}

/// A config the collar refused (reported once; the server stops resending it).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigReject {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "lenient_code")]
    pub code: Option<RejectCode>,
}

/// What the collar is (§3.8 `device`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fw: Option<String>,
    /// See [`caps`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<CollarLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_reject: Option<ConfigReject>,
}

impl DeviceInfo {
    pub fn has(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }

    /// No caps: [`CollarLimits::LEGACY`]; caps without limits: [`CollarLimits::V0`].
    pub fn limits_or_default(&self) -> CollarLimits {
        match (&self.limits, self.caps.is_empty()) {
            (_, true) => CollarLimits::LEGACY,
            (Some(l), false) => *l,
            (None, false) => CollarLimits::V0,
        }
    }
}

/// One boundary a collar holds (§3.8 `slots`): `applied` (the one in effect)
/// or `received` (staged).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlotReport {
    pub version: u32,
    pub status: AckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "wire_time::option")]
    pub effective_at: Option<DateTime<Utc>>,
}

/// Most entries in a report's `slots`.
pub const MAX_SLOTS_REPORTED: usize = 256;

/// Sent on a schedule and on events. Fixes are batched to save power.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PositionReport {
    /// Optional on the wire: the collar is known from its key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    /// The fence the collar is enforcing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceInfo>,
    /// Present: every boundary the collar holds. Absent: unknown (firmware 0.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<Vec<SlotReport>>,
    #[serde(default)]
    pub fixes: Vec<WireFix>,
    #[serde(default)]
    pub cues: Vec<WireCue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub episodes: Vec<WireEpisode>,
    /// 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
}

impl PositionReport {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.fixes.len() > MAX_FIXES_PER_REPORT || self.cues.len() > MAX_FIXES_PER_REPORT || self.episodes.len() > MAX_FIXES_PER_REPORT {
            return Err(ProtocolError::Invalid("report: too many entries"));
        }
        let point_ok = |p: &LonLat| p[0].is_finite() && p[1].is_finite() && p[0].abs() <= 180.0 && p[1].abs() <= 90.0;
        let finite = |v: Option<f64>| v.is_none_or(f64::is_finite);
        for fix in &self.fixes {
            if !point_ok(&fix.point) {
                return Err(ProtocolError::Invalid("fix point"));
            }
            if !fix.accuracy_m.is_finite() || fix.accuracy_m < 0.0 {
                return Err(ProtocolError::Invalid("fix accuracy_m"));
            }
            if fix.hdop.is_some_and(|h| !h.is_finite() || h < 0.0) {
                return Err(ProtocolError::Invalid("fix hdop"));
            }
        }
        for cue in &self.cues {
            if !cue.margin_m.is_finite() || cue.point.as_ref().is_some_and(|p| !point_ok(p)) {
                return Err(ProtocolError::Invalid("cue"));
            }
        }
        for e in &self.episodes {
            if !e.min_margin_m.is_finite() || e.end < e.start {
                return Err(ProtocolError::Invalid("episode"));
            }
        }
        if let Some(slots) = &self.slots
            && (slots.len() > MAX_SLOTS_REPORTED || slots.iter().any(|s| s.status == AckStatus::Rejected))
        {
            return Err(ProtocolError::Invalid("slots"));
        }
        if let Some(d) = &self.device {
            let short = |s: &str| s.len() <= 32;
            if d.fw.as_deref().is_some_and(|f| !short(f)) || d.caps.len() > 16 || d.caps.iter().any(|c| !short(c)) {
                return Err(ProtocolError::Invalid("device"));
            }
            if let Some(l) = &d.limits
                && !limits_sane(l)
            {
                return Err(ProtocolError::Invalid("device limits"));
            }
        }
        if let Some(h) = &self.health {
            let cell = h.cell.as_ref();
            let floats =
                [h.cn0, h.ttf_s, h.tilt_deg, h.temp_c, h.battery_v, cell.and_then(|c| c.rsrp_dbm), cell.and_then(|c| c.rsrq_db), cell.and_then(|c| c.snr_db)];
            if !floats.into_iter().all(finite) {
                return Err(ProtocolError::Invalid("health"));
            }
        }
        if let Some(b) = self.battery
            && !(0.0..=1.0).contains(&b)
        {
            return Err(ProtocolError::Invalid("battery"));
        }
        Ok(())
    }
}

/// Limits a real collar could report: the server fits shapes to them.
fn limits_sane(l: &CollarLimits) -> bool {
    (3..=4096).contains(&l.outer)
        && l.holes <= 256
        && (l.holes == 0 || (3..=4096).contains(&l.hole_vertices))
        && l.total >= l.outer
        && l.total <= 65_536
        && (1..=1024).contains(&l.slots)
        && l.slot_bytes <= 1 << 30
}

/// Server reply to a position report.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportResponse {
    /// Highest alive version for this collar.
    pub latest_version: Option<u32>,
    /// The collar's current signed config, when its `device.config_version`
    /// is lower (or absent) and it has the `config` cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigCommand>,
}

#[cfg(test)]
mod tests;
