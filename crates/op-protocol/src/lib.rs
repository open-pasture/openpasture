//! The OpenCollar device protocol, per `opencollar/protocol/README.md`.
//!
//! Boundaries go down ([`BoundaryCommand`]), acknowledgements ([`Ack`]) and
//! position reports ([`PositionReport`]) come up. V0 is HTTP + JSON; the same
//! logical messages will map one to one onto CBOR later.
//!
//! Boundaries are signed with Ed25519 over their canonical JSON without `sig`
//! (see [`canonical_json`] and [`sign_command`]) so a collar only accepts its
//! own server's boundaries.

mod canonical;
mod sign;
pub mod wire_time;

use chrono::{DateTime, Utc};
use op_geo::{GeoError, Geofence, GeofenceConfig, LonLat, MAX_COLLAR_VERTICES, Polygon};
use serde::{Deserialize, Serialize};

pub use canonical::canonical_json;
pub use ed25519_dalek::{SigningKey, VerifyingKey};
pub use sign::{
    decode_public_key, decode_signing_key, encode_public_key, encode_signing_key, generate_signing_key, sign_command, signing_payload, verify_command,
    verify_json,
};

pub const MIN_VERTICES: usize = 3;
pub const MAX_VERTICES: usize = MAX_COLLAR_VERTICES;
/// Most fixes accepted in one report.
pub const MAX_FIXES_PER_REPORT: usize = 10_000;

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
}

/// A boundary sent to a collar. `boundary` is one unclosed ring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundaryCommand {
    pub command_id: String,
    /// The herd this boundary is for. Signed with the rest, so a boundary
    /// for one herd can't be replayed to a collar in another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "wire_time::option")]
    pub effective_at: Option<DateTime<Utc>>,
    pub boundary: Vec<LonLat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hysteresis_m: Option<f64>,
    /// Ed25519 signature (base64) over the canonical JSON without `sig`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl BoundaryCommand {
    /// Build an unsigned command from a Polygon. Validates and normalises the ring.
    pub fn from_polygon(command_id: impl Into<String>, version: u32, polygon: &Polygon, effective_at: Option<DateTime<Utc>>) -> Result<Self, ProtocolError> {
        if polygon.coordinates.len() > 1 {
            return Err(ProtocolError::Holes);
        }
        let ring = op_geo::validate_ring(&polygon.outer_ring(), Some(MAX_VERTICES))?;
        Ok(Self {
            command_id: command_id.into(),
            herd_id: None,
            version,
            effective_at: effective_at.map(wire_time::trunc_secs),
            boundary: ring,
            warn_m: None,
            hysteresis_m: None,
            sig: None,
        })
    }

    /// What the collar checks before applying: vertex count, coordinate
    /// ranges, a ring that does not cross itself, sane margins, and a version
    /// newer than `current_version`.
    pub fn validate(&self, current_version: Option<u32>) -> Result<(), ProtocolError> {
        if self.command_id.trim().is_empty() || self.command_id.len() > 128 {
            return Err(ProtocolError::Invalid("command_id"));
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

    /// A collar in `herd_id` checks the command names its herd. Commands
    /// without a herd (older servers) pass.
    pub fn check_herd(&self, herd_id: &str) -> Result<(), ProtocolError> {
        match &self.herd_id {
            Some(h) if h != herd_id => Err(ProtocolError::WrongHerd),
            _ => Ok(()),
        }
    }

    pub fn polygon(&self) -> Polygon {
        Polygon::from_ring(self.boundary.clone())
    }

    /// A geofence for this boundary, with `warn_m` and `hysteresis_m`
    /// overriding the collar's defaults.
    pub fn geofence(&self, defaults: GeofenceConfig) -> Result<Geofence, ProtocolError> {
        let cfg = GeofenceConfig {
            warn_m: self.warn_m.unwrap_or(defaults.warn_m),
            hysteresis_m: self.hysteresis_m.unwrap_or(defaults.hysteresis_m),
            max_accuracy_m: defaults.max_accuracy_m,
        };
        Ok(Geofence::new(cfg, &self.boundary, self.version)?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AckStatus {
    /// Stored, waiting for `effective_at`.
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    /// Optional on the wire: the collar is known from its key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    pub command_id: String,
    pub version: u32,
    pub status: AckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(with = "wire_time", alias = "received_at")]
    pub at: DateTime<Utc>,
}

/// One GNSS fix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

/// A sound or stimulus the collar gave, and how far past (negative) or short
/// (positive) of the line the animal was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireCue {
    #[serde(with = "wire_time")]
    pub at: DateTime<Utc>,
    pub level: u8,
    #[serde(alias = "margin_meters")]
    pub margin_m: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<LonLat>,
}

/// Receiver health summary for the report period.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Health {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sats: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cn0: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttf_s: Option<f64>,
}

/// Sent on a schedule and on events. Fixes are batched to save power.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionReport {
    /// Optional on the wire: the collar is known from its key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    /// The fence the collar is enforcing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_version: Option<u32>,
    #[serde(default)]
    pub fixes: Vec<WireFix>,
    #[serde(default)]
    pub cues: Vec<WireCue>,
    /// 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,
}

impl PositionReport {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.fixes.len() > MAX_FIXES_PER_REPORT || self.cues.len() > MAX_FIXES_PER_REPORT {
            return Err(ProtocolError::Invalid("report: too many entries"));
        }
        let point_ok = |p: &LonLat| p[0].is_finite() && p[1].is_finite() && p[0].abs() <= 180.0 && p[1].abs() <= 90.0;
        for fix in &self.fixes {
            if !point_ok(&fix.point) {
                return Err(ProtocolError::Invalid("fix point"));
            }
            if !fix.accuracy_m.is_finite() || fix.accuracy_m < 0.0 {
                return Err(ProtocolError::Invalid("fix accuracy_m"));
            }
        }
        for cue in &self.cues {
            if !cue.margin_m.is_finite() || cue.point.as_ref().is_some_and(|p| !point_ok(p)) {
                return Err(ProtocolError::Invalid("cue"));
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

/// Server reply to a position report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportResponse {
    pub latest_version: Option<u32>,
}

#[cfg(test)]
mod tests;
