//! Re-arm command (op-architect record L): signed like config and boundaries —
//! Ed25519 over canonical JSON without `sig`. Carried in
//! [`crate::ReportResponse::rearm`] when a collar has the `rearm` cap and a
//! pending command that has not expired.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::sign::{payload, sign_payload, verify_bytes};
use crate::{ProtocolError, RejectCode, SigningKey, VerifyingKey, id_ok, wire_time};

/// Hours a queued re-arm stays deliverable after `issued_at`.
pub const REARM_TTL_HOURS: i64 = 24;

/// Short reason on the wire (free-text reason stays on the server audit row).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    AnimalBackIn,
    FenceFixed,
    FalseTrigger,
    #[default]
    Other,
}

impl ReasonCode {
    pub const ALL: [ReasonCode; 4] = [Self::AnimalBackIn, Self::FenceFixed, Self::FalseTrigger, Self::Other];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnimalBackIn => "animal_back_in",
            Self::FenceFixed => "fence_fixed",
            Self::FalseTrigger => "false_trigger",
            Self::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// Flat signed command delivered in the report response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RearmCommand {
    pub command_id: String,
    pub collar_id: String,
    pub herd_id: String,
    pub rearm_seq: u32,
    #[serde(with = "wire_time")]
    pub issued_at: DateTime<Utc>,
    #[serde(with = "wire_time")]
    pub expires_at: DateTime<Utc>,
    /// Collar applies only when its current shut-off matches exactly.
    #[serde(with = "wire_time")]
    pub shutoff_at: DateTime<Utc>,
    pub reason_code: ReasonCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl RearmCommand {
    pub fn check_ids(&self) -> Result<(), RejectCode> {
        if id_ok(&self.command_id) && id_ok(&self.collar_id) && id_ok(&self.herd_id) { Ok(()) } else { Err(RejectCode::BadJson) }
    }

    /// Collar-side checks once the signature verifies, in order: ids,
    /// `wrong_collar`, `wrong_herd`, `expired` (22), then `stale` (6) when
    /// `rearm_seq` is 0 or ≤ the held one, or when `shutoff_at` does not match
    /// (including when enforcement is on and there is no current shut-off).
    pub fn check(
        &self,
        collar_id: &str,
        herd_id: &str,
        current_shutoff: Option<DateTime<Utc>>,
        held_rearm_seq: u32,
        now: DateTime<Utc>,
    ) -> Result<(), RejectCode> {
        self.check_ids()?;
        if self.collar_id != collar_id {
            return Err(RejectCode::WrongCollar);
        }
        if self.herd_id != herd_id {
            return Err(RejectCode::WrongHerd);
        }
        if now >= self.expires_at {
            return Err(RejectCode::Expired);
        }
        if self.rearm_seq == 0 || self.rearm_seq <= held_rearm_seq {
            return Err(RejectCode::Stale);
        }
        match current_shutoff {
            Some(s) if s == self.shutoff_at => Ok(()),
            _ => Err(RejectCode::Stale),
        }
    }
}

/// Set `command.sig`.
pub fn sign_rearm(command: &mut RearmCommand, key: &SigningKey) {
    command.sig = None;
    command.sig = Some(sign_payload(&payload(command), key));
}

/// Check `command.sig` against the struct as it would serialize.
pub fn verify_rearm(command: &RearmCommand, key: &VerifyingKey) -> Result<(), ProtocolError> {
    let sig = command.sig.as_deref().ok_or(ProtocolError::MissingSignature)?;
    verify_bytes(&payload(command), sig, key)
}

/// Welfare snapshot in a position report (`welfare`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WelfareReport {
    /// Enforcement armed or not (`on` / `off`, or a bool).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforcement: Option<WelfareEnforcement>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "wire_time::option")]
    pub shutoff_at: Option<DateTime<Utc>>,
    /// Escape cycles since last re-arm. Wire name is `escape_cycles`; `cycles` is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "cycles")]
    pub escape_cycles: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rearm_seq: Option<u32>,
}

/// Lenient enforcement flag: string or bool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum WelfareEnforcement {
    #[default]
    On,
    Off,
    Other(String),
}

impl WelfareEnforcement {
    pub fn as_str(&self) -> &str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Other(s) => s,
        }
    }

    pub fn is_off(&self) -> bool {
        matches!(self, Self::Off) || self.as_str() == "off" || self.as_str() == "false"
    }
}

impl Serialize for WelfareEnforcement {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WelfareEnforcement {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        Ok(match v {
            serde_json::Value::Bool(true) => Self::On,
            serde_json::Value::Bool(false) => Self::Off,
            serde_json::Value::String(s) => match s.to_ascii_lowercase().as_str() {
                "on" | "true" | "armed" | "1" => Self::On,
                "off" | "false" | "disarmed" | "0" => Self::Off,
                _ => Self::Other(s),
            },
            other => Self::Other(other.to_string()),
        })
    }
}

/// Provisional welfare event kinds (registry.md not in this repo). Kind 3 is
/// treated as auto shut-off for alerting until op-architect confirms.
pub mod welfare_kind {
    pub const ENFORCEMENT_ON: i32 = 1;
    pub const ENFORCEMENT_OFF: i32 = 2;
    pub const AUTO_SHUTOFF: i32 = 3;
    pub const REARMED: i32 = 4;

    pub fn name(kind: i32) -> &'static str {
        match kind {
            ENFORCEMENT_ON => "enforcement_on",
            ENFORCEMENT_OFF => "enforcement_off",
            AUTO_SHUTOFF => "auto_shutoff",
            REARMED => "rearmed",
            _ => "unknown",
        }
    }

    pub fn is_auto_shutoff(kind: i32) -> bool {
        kind == AUTO_SHUTOFF
    }
}

/// One welfare event from a report. Unknown fields are kept in `extra`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WelfareEvent {
    /// Integer kind (1-4 known); unknown kinds are kept as sent.
    pub kind: i32,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "wire_time::option")]
    pub at: Option<DateTime<Utc>>,
    /// Anything else on the object (tolerant).
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sign_and_verify() {
        let key = SigningKey::from_bytes(&[9; 32]);
        let issued = wire_time::parse("2026-11-04T12:00:00Z").unwrap();
        let mut cmd = RearmCommand {
            command_id: "rrm_1".into(),
            collar_id: "col_a".into(),
            herd_id: "herd_1".into(),
            rearm_seq: 2,
            issued_at: issued,
            expires_at: issued + chrono::Duration::hours(REARM_TTL_HOURS),
            shutoff_at: issued - chrono::Duration::minutes(5),
            reason_code: ReasonCode::FenceFixed,
            sig: None,
        };
        assert_eq!(verify_rearm(&cmd, &key.verifying_key()), Err(ProtocolError::MissingSignature));
        sign_rearm(&mut cmd, &key);
        verify_rearm(&cmd, &key.verifying_key()).unwrap();
        let mut moved = cmd.clone();
        moved.herd_id = "herd_2".into();
        assert_eq!(verify_rearm(&moved, &key.verifying_key()), Err(ProtocolError::BadSignature));
    }

    #[test]
    fn check_stale_and_expired() {
        let issued = wire_time::parse("2026-11-04T12:00:00Z").unwrap();
        let shut = issued - chrono::Duration::minutes(1);
        let cmd = RearmCommand {
            command_id: "rrm_1".into(),
            collar_id: "col_a".into(),
            herd_id: "herd_1".into(),
            rearm_seq: 1,
            issued_at: issued,
            expires_at: issued + chrono::Duration::hours(REARM_TTL_HOURS),
            shutoff_at: shut,
            reason_code: ReasonCode::Other,
            sig: None,
        };
        cmd.check("col_a", "herd_1", Some(shut), 0, issued).unwrap();
        assert_eq!(cmd.check("col_b", "herd_1", Some(shut), 0, issued), Err(RejectCode::WrongCollar));
        assert_eq!(cmd.check("col_a", "herd_1", None, 0, issued), Err(RejectCode::Stale), "no shut-off / enforcement on");
        assert_eq!(cmd.check("col_a", "herd_1", Some(shut), 1, issued), Err(RejectCode::Stale), "rearm_seq <= held");
        assert_eq!(cmd.check("col_a", "herd_1", Some(shut), 0, cmd.expires_at), Err(RejectCode::Expired));
        assert_eq!(RejectCode::Stale.code(), 6);
        assert_eq!(RejectCode::Expired.code(), 22);
        assert_eq!(RejectCode::Unsupported.code(), 24);
        assert_eq!(RejectCode::NotNow.code(), 28);
        assert_eq!(RejectCode::from_code(6), Some(RejectCode::Stale));
    }

    #[test]
    fn welfare_accepts_cycles_alias_and_unknown_fields() {
        let w: WelfareReport = serde_json::from_value(json!({
            "enforcement": false,
            "shutoff_at": "2026-11-04T12:00:00Z",
            "cycles": 3,
            "rearm_seq": 7,
            "future_field": true
        }))
        .unwrap();
        assert!(w.enforcement.as_ref().unwrap().is_off());
        assert_eq!(w.escape_cycles, Some(3));
        assert_eq!(w.rearm_seq, Some(7));

        let w2: WelfareReport = serde_json::from_value(json!({ "enforcement": "on", "escape_cycles": 1 })).unwrap();
        assert_eq!(w2.escape_cycles, Some(1));
        assert!(!w2.enforcement.unwrap().is_off());
    }

    #[test]
    fn welfare_event_keeps_unknown_kind() {
        let e: WelfareEvent = serde_json::from_value(json!({ "kind": 99, "at": "2026-11-04T12:00:00Z", "note": "x" })).unwrap();
        assert_eq!(e.kind, 99);
        assert_eq!(e.extra.get("note").and_then(|v| v.as_str()), Some("x"));
        assert!(welfare_kind::is_auto_shutoff(3));
        assert!(!welfare_kind::is_auto_shutoff(99));
    }
}
