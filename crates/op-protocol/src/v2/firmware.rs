//! Firmware releases (`opencollar/protocol/v2/firmware.md`): the release
//! manifest (message type 65, signed by a release or image key) and the
//! rules a collar applies before it installs one.

use ed25519_dalek::SigningKey;
use serde_json::{Value as Json, json};

use super::cbor::{self, IntMap, Value};
use super::cose::{self, Sign1};
use super::json::hex;
use super::registry::{DeviceClass, MessageType, Reject};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// 16: semver.
    pub version: String,
    /// 17: SHA-256 of the whole signed image file.
    pub digest: [u8; 32],
    /// 18: bytes.
    pub size: u64,
    /// 19: device classes it runs on.
    pub classes: Vec<DeviceClass>,
    /// 20: board revisions it runs on, exactly.
    pub hw_revs: Vec<String>,
    /// 21: anti-rollback counter; a collar refuses one below its own.
    pub security_counter: u32,
    /// 22: least battery to start the install, percent.
    pub min_battery: Option<u8>,
    /// 23: kids of firmware keys to stop trusting.
    pub revoke: Vec<[u8; 8]>,
}

impl Manifest {
    pub fn payload(&self) -> Vec<u8> {
        let mut m = IntMap::new();
        m.put(0, Value::uint(MessageType::Manifest.code()))
            .put(16, Value::text(self.version.clone()))
            .put(17, Value::bytes(self.digest.to_vec()))
            .put(18, Value::uint(self.size))
            .put(19, Value::Array(self.classes.iter().map(|c| Value::uint(c.code())).collect()))
            .put(20, Value::Array(self.hw_revs.iter().cloned().map(Value::Text).collect()))
            .put(21, Value::uint(self.security_counter))
            .put_opt(22, self.min_battery.map(Value::uint));
        if !self.revoke.is_empty() {
            m.put(23, Value::Array(self.revoke.iter().map(|k| Value::bytes(k.to_vec())).collect()));
        }
        cbor::encode(&m.into_value())
    }

    pub fn sign(&self, release_key: &SigningKey) -> Vec<u8> {
        cose::sign(&self.payload(), release_key)
    }

    /// Read a signed manifest (check the envelope with [`Sign1::verify`]
    /// against the trusted firmware key named by its kid).
    pub fn read(bytes: &[u8]) -> Result<(Sign1, Self), Reject> {
        let env = Sign1::parse(bytes)?;
        let m = env.payload_map()?;
        let bad = || Reject::BadCbor;
        if m.get(0).and_then(Value::as_u64) != Some(MessageType::Manifest.code()) {
            return Err(bad());
        }
        let list = |k| m.get(k).and_then(Value::as_array).ok_or_else(bad);
        let man = Self {
            version: m.get(16).and_then(Value::as_text).filter(|v| v.len() <= 32).ok_or_else(bad)?.to_owned(),
            digest: m.get(17).and_then(Value::as_bytes).and_then(|d| d.try_into().ok()).ok_or_else(bad)?,
            size: m.get(18).and_then(Value::as_u64).ok_or_else(bad)?,
            classes: list(19)?.iter().filter_map(Value::as_u64).map(DeviceClass::from_code).collect(),
            hw_revs: list(20)?.iter().filter_map(|h| h.as_text().map(str::to_owned)).collect(),
            security_counter: m.get(21).and_then(Value::as_u32).ok_or_else(bad)?,
            min_battery: m.get(22).and_then(Value::as_u64).and_then(|b| u8::try_from(b).ok()),
            revoke: m.get(23).and_then(Value::as_array).map(|a| a.iter().filter_map(|k| k.as_bytes()?.try_into().ok()).collect()).unwrap_or_default(),
        };
        Ok((env, man))
    }

    pub fn to_json(&self) -> Json {
        let mut o = json!({
            "type": "manifest",
            "version": self.version,
            "digest": hex(&self.digest),
            "size": self.size,
            "classes": self.classes.iter().map(|c| c.name()).collect::<Vec<_>>(),
            "hw_revs": self.hw_revs,
            "security_counter": self.security_counter,
        });
        if let Some(b) = self.min_battery {
            o["min_battery"] = json!(b);
        }
        if !self.revoke.is_empty() {
            o["revoke"] = json!(self.revoke.iter().map(|k| hex(k)).collect::<Vec<_>>());
        }
        o
    }
}

/// What a collar knows when it decides whether to install now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallState {
    pub class: DeviceClass,
    pub hw_rev: String,
    pub security_counter: u32,
    /// Percent; `None` when unknown (then it doesn't install).
    pub battery: Option<u8>,
    /// A staged boundary takes effect within the next hour, or a fast window
    /// (a move) is running.
    pub moving: bool,
    /// Outside its fence, in an escape episode, or holding a collar-scoped
    /// boundary (a pen).
    pub escaping: bool,
}

/// The collar's own floor for installing, whatever the manifest says.
pub const MIN_BATTERY_FLOOR: u8 = 40;

/// Whether to install now, in order: `incompatible` (class or board),
/// `downgrade` (counter below the collar's), then `not_now` (battery under
/// the manifest's minimum or the floor, a move or an escape). `not_now` is
/// not a refusal: the collar keeps the update and tries again.
pub fn check_install(m: &Manifest, s: &InstallState) -> Result<(), Reject> {
    if !m.classes.contains(&s.class) || !m.hw_revs.contains(&s.hw_rev) {
        return Err(Reject::Incompatible);
    }
    if m.security_counter < s.security_counter {
        return Err(Reject::Downgrade);
    }
    let need = m.min_battery.unwrap_or(0).max(MIN_BATTERY_FLOOR);
    if s.battery.is_none_or(|b| b < need) || s.moving || s.escaping {
        return Err(Reject::NotNow);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            version: "2.1.0".into(),
            digest: [3; 32],
            size: 210_000,
            classes: vec![DeviceClass::Node],
            hw_revs: vec!["N1.0".into()],
            security_counter: 2,
            min_battery: Some(50),
            revoke: vec![],
        }
    }

    #[test]
    fn install_rules_in_order() {
        let ok = InstallState { class: DeviceClass::Node, hw_rev: "N1.0".into(), security_counter: 2, battery: Some(80), moving: false, escaping: false };
        check_install(&manifest(), &ok).unwrap();
        assert_eq!(check_install(&manifest(), &InstallState { hw_rev: "N0.9".into(), ..ok.clone() }), Err(Reject::Incompatible));
        assert_eq!(check_install(&manifest(), &InstallState { security_counter: 3, battery: Some(10), ..ok.clone() }), Err(Reject::Downgrade));
        assert_eq!(check_install(&manifest(), &InstallState { battery: Some(49), ..ok.clone() }), Err(Reject::NotNow));
        assert_eq!(
            check_install(&Manifest { min_battery: None, ..manifest() }, &InstallState { battery: Some(39), ..ok.clone() }),
            Err(Reject::NotNow),
            "the floor"
        );
        assert_eq!(check_install(&manifest(), &InstallState { moving: true, ..ok.clone() }), Err(Reject::NotNow));
        assert_eq!(check_install(&manifest(), &InstallState { escaping: true, ..ok }), Err(Reject::NotNow));
    }

    #[test]
    fn sign_and_read() {
        let k = SigningKey::from_bytes(&[6; 32]);
        let (env, back) = Manifest::read(&manifest().sign(&k)).unwrap();
        env.verify(&k.verifying_key()).unwrap();
        assert_eq!(back, manifest());
    }
}
