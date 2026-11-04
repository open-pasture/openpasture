//! Portable cross-farm transfer token: signed `OPT1:…`.
//!
//! Produced by a releasing farm with its Ed25519 farm key; claimed on the
//! receiving farm via `POST /api/fleet/claim` with `transfer_token`. The
//! signature binds serial, device key, collar id, single-use code, expiry and
//! the releasing farm's public key — forged or truncated tokens fail verify.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use op_protocol::decode_public_key;
use op_protocol::v2::identity::normalize_claim_code;
use sha2::{Digest, Sha256};

/// Payload bytes that are signed / verified.
pub fn signing_payload(serial: &str, device_pubkey: &str, collar_id: &str, code: &str, exp_unix: i64, farm_pubkey: &str) -> Vec<u8> {
    format!("OPT1\n{serial}\n{device_pubkey}\n{collar_id}\n{code}\n{exp_unix}\n{farm_pubkey}").into_bytes()
}

/// `OPT1:<serial>:<device_key>:<collar_id>:<code>:<exp>:<farm_pubkey>:<sig>`
pub fn format(serial: &str, device_pubkey: &str, collar_id: &str, code: &str, exp: DateTime<Utc>, farm_key: &SigningKey) -> String {
    let farm_pubkey = op_protocol::encode_public_key(&farm_key.verifying_key());
    let exp_unix = exp.timestamp();
    let payload = signing_payload(serial, device_pubkey, collar_id, code, exp_unix, &farm_pubkey);
    let sig = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, farm_key.sign(&payload).to_bytes());
    format!("OPT1:{serial}:{device_pubkey}:{collar_id}:{code}:{exp_unix}:{farm_pubkey}:{sig}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferToken {
    pub serial: String,
    pub device_pubkey: String,
    pub collar_id: String,
    pub transfer_code: String,
    pub expires_at: DateTime<Utc>,
    pub farm_pubkey: String,
    pub sig: String,
}

/// Parse and verify the Ed25519 signature. Returns `None` on malformed or forged input.
pub fn parse_and_verify(s: &str) -> Option<TransferToken> {
    let token = parse_unchecked(s)?;
    let pk = decode_public_key(&token.farm_pubkey).ok()?;
    verify(&token, &pk).then_some(token)
}

fn parse_unchecked(s: &str) -> Option<TransferToken> {
    let mut parts = s.trim().split(':');
    if parts.next()? != "OPT1" {
        return None;
    }
    let serial = parts.next()?.to_owned();
    let device_pubkey = parts.next()?.to_owned();
    let collar_id = parts.next()?.to_owned();
    let transfer_code = normalize_claim_code(parts.next()?)?;
    let exp: i64 = parts.next()?.parse().ok()?;
    let farm_pubkey = parts.next()?.to_owned();
    let sig = parts.next()?.to_owned();
    if parts.next().is_some() || farm_pubkey.is_empty() || sig.is_empty() {
        return None;
    }
    let expires_at = DateTime::from_timestamp(exp, 0)?;
    Some(TransferToken { serial, device_pubkey, collar_id, transfer_code, expires_at, farm_pubkey, sig })
}

fn verify(token: &TransferToken, farm_pk: &VerifyingKey) -> bool {
    let payload =
        signing_payload(&token.serial, &token.device_pubkey, &token.collar_id, &token.transfer_code, token.expires_at.timestamp(), &token.farm_pubkey);
    let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &token.sig) else {
        return false;
    };
    let Ok(sig_bytes) = <[u8; 64]>::try_from(bytes.as_slice()) else {
        return false;
    };
    farm_pk.verify(&payload, &Signature::from_bytes(&sig_bytes)).is_ok()
}

/// SHA-256 hex of the transfer code (for single-use tracking).
pub fn code_hash(code: &str) -> String {
    format!("{:x}", Sha256::digest(code.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use op_protocol::generate_signing_key;
    use op_protocol::v2::identity::new_claim_code;

    #[test]
    fn round_trip_and_rejects_forgery() {
        let key = generate_signing_key();
        let code = new_claim_code();
        let exp = DateTime::from_timestamp(1_900_000_000, 0).unwrap();
        let t = format("OCN-1", "pk", "col_1", &code, exp, &key);
        let p = parse_and_verify(&t).unwrap();
        assert_eq!(p.serial, "OCN-1");
        assert_eq!(p.collar_id, "col_1");
        assert_eq!(p.transfer_code, code);

        // Tamper with the collar id.
        let forged = t.replacen("col_1", "col_evil", 1);
        assert!(parse_and_verify(&forged).is_none());

        // Unsigned legacy shape.
        let legacy = format!("OPT1:OCN-1:pk:col_1:{code}:{}", exp.timestamp());
        assert!(parse_and_verify(&legacy).is_none());
    }
}
