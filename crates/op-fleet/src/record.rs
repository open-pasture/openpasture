//! Factory records (protocol v2 `identity.md` §3): a maker-signed document
//! naming the unit's serial, class, board, device key, firmware, claim check,
//! radios, the firmware keys its bootloader trusts and its test. A station
//! may send one its fixture signed; otherwise this server signs one with its
//! maker key at provisioning. A record from an unknown maker is kept and
//! shown as unverified.

use op_core::Ctx;
use op_protocol::v2::cbor::{IntMap, Value};
use op_protocol::v2::identity::{self, FactoryRecord};
use op_protocol::v2::registry::DeviceClass;
use serde::Serialize;
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use crate::keys::{self, hex, unhex};
use crate::runs::TestRun;
use crate::units::Unit;

/// A record as stored and shown.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordView {
    /// The COSE_Sign1 bytes, hex.
    pub cose: String,
    /// Its JSON view (protocol v2).
    pub record: Json,
    pub maker_kid: String,
    pub verified: bool,
}

/// A record read and checked.
pub struct Read {
    pub record: FactoryRecord,
    pub view: RecordView,
}

/// Whether `kid` is this server's maker key or a maker key the owner enrolled.
async fn maker_verifies(ctx: &Ctx, env: &op_protocol::v2::cose::Sign1) -> anyhow::Result<bool> {
    let own = keys::maker_key(ctx.data_dir())?.verifying_key();
    if env.verify(&own).is_ok() {
        return Ok(true);
    }
    for k in crate::firmware::trusted_keys(ctx, crate::firmware::Purpose::Maker).await? {
        if let Ok(key) = keys::parse_public_key(&k.public_key, "maker key")
            && env.verify(&key).is_ok()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Parse a record (hex COSE_Sign1) and check its maker signature.
pub async fn read(ctx: &Ctx, cose_hex: &str) -> Result<Read, String> {
    let bytes = unhex(cose_hex.trim()).ok_or("factory_record is the record's COSE bytes as hex.")?;
    let (env, record) = FactoryRecord::read(&bytes).map_err(|e| format!("factory_record doesn't read as a factory record ({}).", e.name()))?;
    let verified = maker_verifies(ctx, &env).await.map_err(|e| format!("{e:#}"))?;
    Ok(Read { view: RecordView { cose: hex(&bytes), record: record.to_json(), maker_kid: hex(&env.kid), verified }, record })
}

/// The record for an already stored hex, for display.
pub async fn view(ctx: &Ctx, cose_hex: &str) -> Option<RecordView> {
    read(ctx, cose_hex).await.ok().map(|r| r.view)
}

/// A BLE address as `D4:12:…` or plain hex, to its 6 bytes.
fn ble_bytes(s: &str) -> Option<Vec<u8>> {
    let h: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    unhex(&h.to_ascii_lowercase()).filter(|b| b.len() == 6)
}

/// Sign a record for `unit` with this server's maker key. `None` when the
/// unit's class isn't one protocol v2 names, or its serial or board don't
/// fit the record.
pub fn build(ctx: &Ctx, unit: &Unit, run: Option<&TestRun>, maker: &str, lora_region: &str) -> anyhow::Result<Option<(FactoryRecord, RecordView)>> {
    let Some(class) = DeviceClass::from_name(&unit.device_class) else { return Ok(None) };
    if !identity::serial_ok(&unit.serial) || unit.hw_rev.len() > 16 || unit.fw_version.len() > 32 {
        return Ok(None);
    }
    let key = keys::parse_public_key(&unit.device_pubkey, "device key").map_err(anyhow::Error::msg)?;
    let device_id = op_protocol::v2::crypto::kid(key.as_bytes());
    let claim_check = unit.claim_code.as_deref().map(|c| identity::claim_check(&identity::claim_key(c, &device_id)));
    let mut radio = IntMap::new();
    radio.put(1, Value::text(lora_region));
    if let Some(b) = unit.radios.ble_addr.as_deref().and_then(ble_bytes) {
        radio.put(2, Value::bytes(b));
    }
    radio.put_opt(3, unit.radios.imei.clone().map(Value::text)).put_opt(4, unit.radios.iccid.clone().map(Value::text));
    let test = run.map(|r| {
        let mut t = IntMap::new();
        t.put(1, Value::text(r.fixture_id.clone().or_else(|| r.station.clone()).unwrap_or_default()))
            .put(2, Value::Bool(r.result == crate::runs::RunResult::Pass))
            .put(3, Value::bytes(Sha256::digest(serde_json::to_vec(&r.steps).unwrap_or_default()).to_vec()))
            .put(4, Value::uint(r.finished_at.timestamp().max(0) as u64));
        t
    });
    let made_at = unit.created_at.timestamp().max(0) as u64;
    let record = FactoryRecord {
        serial: unit.serial.clone(),
        class,
        hw_rev: unit.hw_rev.clone(),
        device_key: key.to_bytes(),
        fw: unit.fw_version.clone(),
        made_at,
        maker: maker.chars().take(64).collect(),
        claim_check,
        radio: Some(radio),
        image_keys: unit.image_keys.iter().filter_map(|k| keys::parse_kid(k)).collect(),
        test,
        caps: unit.caps.clone(),
    };
    let maker_key = keys::maker_key(ctx.data_dir())?;
    let bytes = record.sign(&maker_key);
    let view = RecordView { cose: hex(&bytes), record: record.to_json(), maker_kid: keys::kid(&maker_key.verifying_key()), verified: true };
    Ok(Some((record, view)))
}

/// The card's QR text: `OC2:<serial>:<device key, base64url>:<claim code>`.
pub fn card(unit: &Unit, claim_code: &str) -> Option<String> {
    let key = keys::parse_public_key(&unit.device_pubkey, "device key").ok()?;
    Some(identity::Card { serial: unit.serial.clone(), device_key: key.to_bytes(), claim_code: claim_code.to_owned() }.text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ble_addresses_read_with_or_without_colons() {
        assert_eq!(ble_bytes("D4:12:AB:00:01:FF"), Some(vec![0xd4, 0x12, 0xab, 0, 1, 0xff]));
        assert_eq!(ble_bytes("d412ab0001ff"), Some(vec![0xd4, 0x12, 0xab, 0, 1, 0xff]));
        assert_eq!(ble_bytes("D4:12"), None);
    }
}
