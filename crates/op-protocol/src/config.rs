//! Collar configuration (protocol v1, §3.4): the signed command that sets a
//! collar's herd, endpoint and report cadence. It rides inline in
//! [`crate::ReportResponse::config`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::sign::{payload, sign_payload, verify_bytes};
use crate::{ProtocolError, RejectCode, SigningKey, VerifyingKey, id_ok, wire_time};

/// Shortest report or poll interval, seconds.
pub const MIN_INTERVAL_S: u32 = 10;
/// Longest report or poll interval, seconds.
pub const MAX_INTERVAL_S: u32 = 3600;
/// Longest `endpoint`, bytes.
pub const MAX_ENDPOINT_BYTES: usize = 256;

/// A flat object, signed and canonicalized exactly like boundary commands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigCommand {
    pub command_id: String,
    /// Required: absent is `bad_json`, another collar's is `wrong_collar`.
    pub collar_id: String,
    /// Per collar, strictly increasing: lower or equal is `stale`.
    pub version: u32,
    /// Replaces the provisioning herd for `wrong_herd` checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
    /// Absent: keep the current one. Present: `https://`. The collar switches
    /// after a successful report to it and falls back to the previous one if
    /// it fails for 24 h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Base report interval, 10-3600 s.
    pub report_s: u32,
    /// Base boundary poll interval, 10-3600 s.
    pub poll_s: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_report_s: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_poll_s: Option<u32>,
    /// The fast intervals apply until this GNSS time, then the base ones
    /// again without another command. Needs both fast intervals.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "wire_time::option")]
    pub fast_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl ConfigCommand {
    /// Ids of 1-64 bytes (`herd_id` when present).
    pub fn check_ids(&self) -> Result<(), RejectCode> {
        if id_ok(&self.command_id) && id_ok(&self.collar_id) && self.herd_id.as_deref().is_none_or(id_ok) { Ok(()) } else { Err(RejectCode::BadJson) }
    }

    /// What a collar checks once the signature verifies, in order: ids
    /// (`bad_json`), `wrong_collar`, `stale` (version ≤ `held_version`), then
    /// `bad_config`: an interval outside 10-3600 s, `fast_until` without both
    /// fast intervals, or an endpoint that isn't `https://…` of at most 256 bytes.
    pub fn check(&self, collar_id: &str, held_version: Option<u32>) -> Result<(), RejectCode> {
        self.check_ids()?;
        if self.collar_id != collar_id {
            return Err(RejectCode::WrongCollar);
        }
        if held_version.is_some_and(|h| self.version <= h) {
            return Err(RejectCode::Stale);
        }
        let interval_ok = |s: u32| (MIN_INTERVAL_S..=MAX_INTERVAL_S).contains(&s);
        let intervals = [Some(self.report_s), Some(self.poll_s), self.fast_report_s, self.fast_poll_s];
        if !intervals.into_iter().flatten().all(interval_ok) {
            return Err(RejectCode::BadConfig);
        }
        if self.fast_until.is_some() && (self.fast_report_s.is_none() || self.fast_poll_s.is_none()) {
            return Err(RejectCode::BadConfig);
        }
        if let Some(e) = &self.endpoint
            && (e.len() > MAX_ENDPOINT_BYTES || e.strip_prefix("https://").is_none_or(|host| host.is_empty()))
        {
            return Err(RejectCode::BadConfig);
        }
        Ok(())
    }

    /// Whether the fast intervals apply at GNSS time `now`.
    pub fn fast(&self, now: DateTime<Utc>) -> bool {
        self.fast_until.is_some_and(|t| now < t) && self.fast_report_s.is_some() && self.fast_poll_s.is_some()
    }

    /// `(report_s, poll_s)` in effect at GNSS time `now`.
    pub fn cadence(&self, now: DateTime<Utc>) -> (u32, u32) {
        match (self.fast(now), self.fast_report_s, self.fast_poll_s) {
            (true, Some(r), Some(p)) => (r, p),
            _ => (self.report_s, self.poll_s),
        }
    }
}

/// Set `config.sig`.
pub fn sign_config(config: &mut ConfigCommand, key: &SigningKey) {
    config.sig = None;
    config.sig = Some(sign_payload(&payload(config), key));
}

/// Check `config.sig` against the struct as it would serialize.
pub fn verify_config(config: &ConfigCommand, key: &VerifyingKey) -> Result<(), ProtocolError> {
    let sig = config.sig.as_deref().ok_or(ProtocolError::MissingSignature)?;
    verify_bytes(&payload(config), sig, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ConfigCommand {
        ConfigCommand {
            command_id: "cfg_1".into(),
            collar_id: "col_a".into(),
            version: 3,
            herd_id: Some("herd_1".into()),
            endpoint: Some("https://farm.example.com/collar/v1".into()),
            report_s: 60,
            poll_s: 60,
            fast_report_s: Some(10),
            fast_poll_s: Some(10),
            fast_until: Some(wire_time::parse("2026-09-27T13:10:00Z").unwrap()),
            sig: None,
        }
    }

    #[test]
    fn contract_example_parses_and_checks() {
        let c: ConfigCommand = serde_json::from_str(
            r#"{"command_id":"cfg_01J9","collar_id":"col_01J9","version":3,"herd_id":"herd_01J7","endpoint":"https://farm.example.com/collar/v1",
                "report_s":60,"poll_s":60,"fast_report_s":10,"fast_poll_s":10,"fast_until":"2026-09-27T13:10:00Z","sig":"x"}"#,
        )
        .unwrap();
        c.check("col_01J9", Some(2)).unwrap();
        assert!(serde_json::from_str::<ConfigCommand>(r#"{"command_id":"c","version":1,"report_s":60,"poll_s":60}"#).is_err(), "collar_id is required");
    }

    #[test]
    fn check_codes_in_order() {
        let c = cfg();
        c.check("col_a", None).unwrap();
        c.check("col_a", Some(2)).unwrap();
        assert_eq!(c.check("col_b", Some(2)), Err(RejectCode::WrongCollar));
        assert_eq!(c.check("col_a", Some(3)), Err(RejectCode::Stale), "equal is stale");
        assert_eq!(c.check("col_a", Some(4)), Err(RejectCode::Stale));
        for (report_s, ok) in [(9, false), (10, true), (3600, true), (3601, false)] {
            assert_eq!(ConfigCommand { report_s, ..cfg() }.check("col_a", None).is_ok(), ok, "{report_s}");
        }
        assert_eq!(ConfigCommand { fast_poll_s: Some(5), ..cfg() }.check("col_a", None), Err(RejectCode::BadConfig));
        assert_eq!(ConfigCommand { fast_poll_s: None, ..cfg() }.check("col_a", None), Err(RejectCode::BadConfig));
        assert_eq!(ConfigCommand { endpoint: Some("http://farm.example.com/collar/v1".into()), ..cfg() }.check("col_a", None), Err(RejectCode::BadConfig));
        assert_eq!(ConfigCommand { endpoint: Some("https://".into()), ..cfg() }.check("col_a", None), Err(RejectCode::BadConfig));
        ConfigCommand { endpoint: None, fast_until: None, fast_report_s: None, fast_poll_s: None, ..cfg() }.check("col_a", None).unwrap();
        assert_eq!(ConfigCommand { collar_id: String::new(), ..cfg() }.check("", None), Err(RejectCode::BadJson));
        assert_eq!(ConfigCommand { herd_id: Some("h".repeat(65)), ..cfg() }.check("col_a", None), Err(RejectCode::BadJson));
    }

    #[test]
    fn fast_mode_expires_by_gnss_time() {
        let c = cfg();
        let until = c.fast_until.unwrap();
        assert_eq!(c.cadence(until - chrono::Duration::seconds(1)), (10, 10));
        assert_eq!(c.cadence(until), (60, 60));
        assert_eq!(ConfigCommand { fast_until: None, ..cfg() }.cadence(until - chrono::Duration::hours(1)), (60, 60));
    }

    #[test]
    fn sign_and_verify() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let mut c = cfg();
        assert_eq!(verify_config(&c, &key.verifying_key()), Err(ProtocolError::MissingSignature));
        sign_config(&mut c, &key);
        verify_config(&c, &key.verifying_key()).unwrap();
        let mut moved = c.clone();
        moved.herd_id = Some("herd_2".into());
        assert_eq!(verify_config(&moved, &key.verifying_key()), Err(ProtocolError::BadSignature));
        // Optional fields are omitted, not null.
        let mut bare = ConfigCommand { endpoint: None, herd_id: None, fast_until: None, fast_report_s: None, fast_poll_s: None, ..cfg() };
        sign_config(&mut bare, &key);
        let json = serde_json::to_string(&bare).unwrap();
        assert!(!json.contains("null") && !json.contains("endpoint"), "{json}");
    }
}
