use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::Value;

use crate::{BoundaryCommand, ProtocolError, canonical_json};

pub fn generate_signing_key() -> SigningKey {
    SigningKey::generate(&mut rand::rngs::OsRng)
}

/// Base64 of the 32-byte public key. Given to a collar at link time.
pub fn encode_public_key(key: &VerifyingKey) -> String {
    STANDARD.encode(key.as_bytes())
}

pub fn decode_public_key(s: &str) -> Result<VerifyingKey, ProtocolError> {
    let bytes: [u8; 32] = STANDARD.decode(s.trim()).ok().and_then(|b| b.try_into().ok()).ok_or(ProtocolError::BadKey)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| ProtocolError::BadKey)
}

/// Base64 of the 32-byte secret key.
pub fn encode_signing_key(key: &SigningKey) -> String {
    STANDARD.encode(key.to_bytes())
}

pub fn decode_signing_key(s: &str) -> Result<SigningKey, ProtocolError> {
    let bytes: [u8; 32] = STANDARD.decode(s.trim()).ok().and_then(|b| b.try_into().ok()).ok_or(ProtocolError::BadKey)?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// The bytes that get signed: canonical JSON of the command without `sig`.
pub fn signing_payload(command: &BoundaryCommand) -> Vec<u8> {
    payload(command)
}

/// Canonical JSON of any flat command without its `sig`.
pub(crate) fn payload<T: serde::Serialize>(command: &T) -> Vec<u8> {
    let mut value = serde_json::to_value(command).expect("command serializes");
    if let Value::Object(map) = &mut value {
        map.remove("sig");
    }
    canonical_json(&value).into_bytes()
}

/// Base64 Ed25519 signature over `payload`.
pub(crate) fn sign_payload(payload: &[u8], key: &SigningKey) -> String {
    STANDARD.encode(key.sign(payload).to_bytes())
}

/// Set `command.sig`.
pub fn sign_command(command: &mut BoundaryCommand, key: &SigningKey) {
    command.sig = None;
    command.sig = Some(sign_payload(&signing_payload(command), key));
}

pub fn verify_command(command: &BoundaryCommand, key: &VerifyingKey) -> Result<(), ProtocolError> {
    let sig = command.sig.as_deref().ok_or(ProtocolError::MissingSignature)?;
    verify_bytes(&signing_payload(command), sig, key)
}

/// Verify a command as received, before parsing it into a struct: drop
/// `sig`, canonicalise, check. It can't see duplicate keys (serde_json keeps
/// the last one), nesting or size; collars and collar-sim use
/// [`crate::verify_wire`], which can. Kept for existing callers.
pub fn verify_json(command: &Value, key: &VerifyingKey) -> Result<(), ProtocolError> {
    let mut value = command.clone();
    let sig = match &mut value {
        Value::Object(map) => match map.remove("sig") {
            Some(Value::String(s)) => s,
            _ => return Err(ProtocolError::MissingSignature),
        },
        _ => return Err(ProtocolError::Invalid("command")),
    };
    verify_bytes(canonical_json(&value).as_bytes(), &sig, key)
}

pub(crate) fn verify_bytes(payload: &[u8], sig: &str, key: &VerifyingKey) -> Result<(), ProtocolError> {
    let bytes: [u8; 64] = STANDARD.decode(sig).ok().and_then(|b| b.try_into().ok()).ok_or(ProtocolError::BadSignature)?;
    key.verify(payload, &Signature::from_bytes(&bytes)).map_err(|_| ProtocolError::BadSignature)
}
